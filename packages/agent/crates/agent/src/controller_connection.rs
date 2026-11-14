//! The outbound socket to the controller, authenticated with the node token.
//!
//! One task owns the socket. Until the controller welcomes a socket's `hello`, only that hello and replies go
//! out, because the hello carries what other frames would report. A refusal that retrying cannot
//! change (a token the controller does not know, another protocol version, a refused hello, a deleted node)
//! stops the connection until the process restarts.

use crate::sync::lock;
use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::constants::{
    AGENT_ERROR_DETAIL_MAX_CHARS, AGENT_HANDSHAKE_TIMEOUT, AGENT_HELLO_REFUSED_CLOSE, AGENT_PING_INTERVAL, AGENT_RECONNECT_MAX, AGENT_RECONNECT_MIN,
    AGENT_WELCOME_TIMEOUT, AGENT_WELCOME_TIMEOUT_REASON, CONTROLLER_FRAME_MAX_BYTES, PROTOCOL_HEADER, PROTOCOL_VERSION, SHUTDOWN_REQUEST_WAIT, close_codes,
};
use ankor_protocol::helpers::{backoff_delay, from_wire_name, node_authorization, wire_name};
use ankor_protocol::schemas::s_controller_to_agent_message;
use ankor_protocol::types::{
    AgentError, AgentErrorCode, AgentReply, AgentReplyFailure, AgentShutdown, AgentShutdownSignal, AgentToControllerMessage, CloseReason,
    ControllerToAgentMessage, ControllerWatchLogs, ControllerWatchResources, RequestType,
};
use ankor_protocol::validate::{format_issues, safe_parse};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::task::AbortOnDropHandle;

use crate::failure::{agent_error, truncate};
use crate::logger::Logger;
use crate::skipped_types::SkippedTypes;

/// How long a close the agent starts may take before it cuts the socket off.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

pub use futures_util::future::BoxFuture;
pub type HandlerError = crate::failure::OpError;

/// Work a request handler leaves for after its reply has gone out, such as a redial or a container
/// swap that must not pre-empt the reply.
#[derive(Clone, Default)]
pub struct Defer(Arc<Mutex<Vec<BoxFuture<'static, ()>>>>);

impl Defer {
    pub fn push(&self, work: impl Future<Output = ()> + Send + 'static) {
        lock(&self.0).push(Box::pin(work));
    }

    pub(crate) fn take(&self) -> Vec<BoxFuture<'static, ()>> {
        std::mem::take(&mut *lock(&self.0))
    }
}

pub struct Request {
    pub id: String,
    pub request_type: RequestType,
    pub message: ControllerToAgentMessage,
}

/// The `reply` frame that fails a request, built from the contract's types.
fn failure_reply(id: String, request: Option<RequestType>, code: AgentErrorCode, detail: &str) -> Value {
    reply_frame(AgentReply::Failure(AgentReplyFailure {
        reply_to: id,
        request,
        error: AgentError { code, detail: truncate(detail, AGENT_ERROR_DETAIL_MAX_CHARS) },
    }))
}

/// The `reply` frame that answers a request with its data. The handler builds each request's data
/// as JSON, which the typed success variants cannot hold, so the envelope is written here, once.
fn success_reply(id: &str, request: RequestType, data: &Value) -> Value {
    json!({ "type": "reply", "reply_to": id, "request": request, "ok": true, "data": data })
}

fn reply_frame(reply: AgentReply) -> Value {
    serde_json::to_value(AgentToControllerMessage::Reply(reply)).unwrap_or(Value::Null)
}

pub enum Watch {
    Resources(ControllerWatchResources),
    Logs(ControllerWatchLogs),
}

/// What the agent does with the connection's events. Hooks that return futures run on their own
/// task, so they may send through the connection.
pub trait Hooks: Send + Sync + 'static {
    /// The `ws(s)://` URL with the agent path, read on every dial.
    fn url(&self) -> String;
    /// Runs after each connect, and sends the `hello`.
    fn on_open(&self, connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>>;
    /// After the controller accepted this socket's hello.
    fn on_welcome(&self, connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>>;
    fn on_close(&self, _code: u16, _reason: &str) {}
    fn on_watch(&self, _watch: Watch) {}
    /// The controller refused this node until the process restarts.
    fn on_failed(&self, _reason: &str) {}
    fn on_check_cs2_build(&self) {}
    /// Runs a request and returns its reply's `data`.
    fn handle(&self, request: Request, defer: Defer) -> BoxFuture<'static, Result<Value, HandlerError>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Sigint,
    Sigterm,
    Sigquit,
}

impl Signal {
    /// The contract's name, such as `SIGTERM`, used as the close reason and in the log.
    #[must_use]
    pub fn name(self) -> String {
        wire_name(self.wire())
    }

    fn wire(self) -> AgentShutdownSignal {
        match self {
            Self::Sigint => AgentShutdownSignal::Sigint,
            Self::Sigterm => AgentShutdownSignal::Sigterm,
            Self::Sigquit => AgentShutdownSignal::Sigquit,
        }
    }
}

/// Timings the tests shorten.
#[derive(Clone, Debug)]
pub struct Timings {
    /// The shortest reconnect delay.
    pub min_backoff: Duration,
    /// The longest reconnect delay.
    pub max_backoff: Duration,
    /// How often the agent pings. A write that waits longer counts as stalled.
    pub ping_interval: Duration,
    /// How long the agent waits for `welcome` after it sent `hello`.
    pub welcome_timeout: Duration,
    /// How long after `hello_refused` the agent waits for the controller to close first.
    pub refused_close: Duration,
    /// How long the opening handshake may take.
    pub handshake_timeout: Duration,
    /// The backoff's jitter, from 0 to 1.
    pub random: fn() -> f64,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            min_backoff: AGENT_RECONNECT_MIN,
            max_backoff: AGENT_RECONNECT_MAX,
            ping_interval: AGENT_PING_INTERVAL,
            welcome_timeout: AGENT_WELCOME_TIMEOUT,
            refused_close: AGENT_HELLO_REFUSED_CLOSE,
            handshake_timeout: AGENT_HANDSHAKE_TIMEOUT,
            random: rand::random::<f64>,
        }
    }
}

/// What the socket does differently for a frame, read from its message variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    /// Starts a new handshake: the socket waits for its `welcome` again.
    Hello,
    /// Leaves before the `welcome`, like the `hello` it answers alongside.
    Reply,
    /// Waits for the `welcome`.
    Other,
}

impl FrameKind {
    fn of(message: &AgentToControllerMessage) -> Self {
        match message {
            AgentToControllerMessage::Hello(_) => Self::Hello,
            AgentToControllerMessage::Reply(_) => Self::Reply,
            _ => Self::Other,
        }
    }

    /// Whether the frame may leave before the controller has welcomed the socket.
    fn before_welcome(self) -> bool {
        self != Self::Other
    }
}

enum Command {
    /// A frame, its kind, and the bytes it counts against the log backlog: its size for a log frame,
    /// else 0.
    Send(Value, FrameKind, oneshot::Sender<bool>, usize),
    /// Closes the socket and dials again.
    Redial,
    /// Drops the socket with this number, if it is still the open one.
    Terminate(u64),
    /// A reply for the socket with this number. It travels with the frames, so it leaves after
    /// everything queued before it.
    Reply(u64, Value, oneshot::Sender<bool>),
    /// Sends `shutdown`, closes, and answers once the socket ended.
    Shutdown(Signal, oneshot::Sender<()>),
}

/// Most bytes of log frames that wait to be written; past it, new ones are dropped. About five
/// seconds of the node log's budget: a controller that reads slower than the node logs then costs
/// lost log lines, not memory, and replies and reports never queue behind minutes of logs.
const LOG_BACKLOG_MAX_BYTES: usize = 1 << 20;

/// The log frames waiting to be written, which may be dropped, unlike state frames and replies.
#[derive(Default)]
struct LogBacklog {
    /// Bytes of log frames queued and not yet written.
    bytes: AtomicUsize,
    /// Log frames dropped since the backlog last filled up; while it is more than 0, the agent drops.
    dropped: AtomicU64,
}

/// A handle to the connection task. Cloning shares the connection.
#[derive(Clone)]
pub struct ControllerConnection {
    /// Feeds the driver.
    commands: mpsc::UnboundedSender<Command>,
    /// Log frames queued and not yet written.
    log_backlog: Arc<LogBacklog>,
    /// Receives the backlog's warnings.
    log: Logger,
    /// Whether a socket is open.
    connected: Arc<AtomicBool>,
    /// Whether the controller has welcomed the latest `hello` on the open socket.
    welcomed: Arc<AtomicBool>,
    /// Counts sockets, so a hook that fails after its socket was replaced redials nothing.
    socket: Arc<AtomicU64>,
    /// Set once the driver runs.
    started: Arc<AtomicBool>,
}

/// A connection's driver before it runs: it dials once [`Unstarted::start`] hands it the hooks.
pub struct Unstarted {
    token: String,
    timings: Timings,
    log: Logger,
    receiver: mpsc::UnboundedReceiver<Command>,
    handle: ControllerConnection,
}

impl Unstarted {
    pub fn start(self, hooks: Arc<dyn Hooks>) {
        let Self { token, timings, log, receiver, handle } = self;

        handle.started.store(true, Ordering::SeqCst);
        tokio::spawn(
            Driver { token, hooks, timings, log, receiver, handle, attempt: 0, failed: false, stopped: false, skipped: SkippedTypes::default() }.run(),
        );
    }
}

impl ControllerConnection {
    /// The handle exists before anything dials, so the parts that report to the controller can hold
    /// it. Until a socket is welcomed, it refuses every frame but a hello or a reply.
    #[must_use]
    pub fn new(token: String, timings: Timings, log: Logger) -> (Self, Unstarted) {
        crate::init_tls();
        let (commands, receiver) = mpsc::unbounded_channel();
        let handle = Self {
            commands,
            log_backlog: Arc::default(),
            log: log.clone(),
            connected: Arc::default(),
            welcomed: Arc::default(),
            socket: Arc::default(),
            started: Arc::default(),
        };

        (handle.clone(), Unstarted { token, timings, log, receiver, handle })
    }

    /// Whether the driver has started, with or without an open socket.
    #[must_use]
    pub fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// The controller has welcomed this socket's hello, so every frame goes out.
    #[must_use]
    pub fn welcomed(&self) -> bool {
        self.connected() && self.welcomed.load(Ordering::SeqCst)
    }

    /// Queues the frame now, ahead of any sent later, and resolves once it has left the socket;
    /// false when it never did.
    pub fn push(&self, message: &AgentToControllerMessage) -> impl Future<Output = bool> + Send + 'static {
        let (ack, answer) = oneshot::channel();
        // Refused now, not when the socket's turn comes: what is pushed before the welcome is
        // replayed by the welcome's own reports, so it must not also leave from the queue.
        let kind = FrameKind::of(message);
        let logs = matches!(message, AgentToControllerMessage::LogBatch(_) | AgentToControllerMessage::LogIndex(_));
        let queued = serde_json::to_value(message).is_ok_and(|value| {
            if !kind.before_welcome() && !self.welcomed.load(Ordering::SeqCst) {
                return false;
            }

            let log_bytes = if logs { serde_json::to_string(&value).map_or(0, |text| text.len()) } else { 0 };

            if log_bytes > 0 && !self.hold_log_frame(log_bytes) {
                return false;
            }

            self.commands.send(Command::Send(value, kind, ack, log_bytes)).is_ok()
        });

        async move { queued && answer.await.unwrap_or(false) }
    }

    fn hold_log_frame(&self, bytes: usize) -> bool {
        let backlog = &self.log_backlog;
        // Checked and counted in one step, so frames from several threads cannot all pass the check.
        let held = backlog.bytes.try_update(Ordering::SeqCst, Ordering::SeqCst, |held| held.checked_add(bytes).filter(|total| *total <= LOG_BACKLOG_MAX_BYTES));

        if held.is_err() {
            if backlog.dropped.fetch_add(1, Ordering::SeqCst) == 0 {
                self.log.warn("the controller reads slower than the node sends its logs; dropping log frames until it catches up");
            }

            return false;
        }

        true
    }

    /// Once the backlog is down to half, logs how many frames were dropped.
    fn release_log_frame(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }

        let backlog = &self.log_backlog;
        let left = backlog.bytes.fetch_sub(bytes, Ordering::SeqCst).saturating_sub(bytes);

        if left <= LOG_BACKLOG_MAX_BYTES / 2 {
            let dropped = backlog.dropped.swap(0, Ordering::SeqCst);

            if dropped > 0 {
                self.log.info(format!("sending log frames again; dropped {dropped} while the controller was behind"));
            }
        }
    }

    /// Closes the current socket normally and dials again, from the URL the hooks give now.
    pub fn redial(&self) {
        let _ = self.commands.send(Command::Redial);
    }

    /// Sends `shutdown`, closes with the signal as reason, and waits for the close, bounded.
    pub async fn shutdown(&self, signal: Signal) {
        let (ack, done) = oneshot::channel();

        if self.commands.send(Command::Shutdown(signal, ack)).is_ok() {
            let _ = done.await;
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How a socket ended: as the close or the shutdown under way says, else as `otherwise` does. A
/// socket the controller closes while shutdown waits for its requests still ends the shutdown, not in a
/// redial.
fn ended_as(closing: Option<(Instant, Ended)>, stopping: Option<(Instant, oneshot::Sender<()>, Signal)>, otherwise: impl FnOnce() -> Ended) -> Ended {
    closing.map(|(_, ended)| ended).or_else(|| stopping.map(|(_, ack, _)| Ended::Shutdown(ack))).unwrap_or_else(otherwise)
}

/// The requests a socket is running.
#[derive(Default)]
struct Requests {
    /// Ids of the requests still running.
    open: Mutex<HashSet<String>>,
    /// Told each time a request ends, so a shutdown that waits for them notices the last.
    settled: Notify,
}

enum Ended {
    Closed(CloseCode, String),
    Redial,
    Shutdown(oneshot::Sender<()>),
    Stop,
}

struct Driver {
    /// The node token sent on every dial.
    token: String,
    /// The agent's handlers for the connection's events.
    hooks: Arc<dyn Hooks>,
    /// Backoff, ping and timeout durations.
    timings: Timings,
    /// Receives connection progress.
    log: Logger,
    /// Commands from every handle.
    receiver: mpsc::UnboundedReceiver<Command>,
    /// The handle whose state the driver keeps current.
    handle: ControllerConnection,
    /// Dials since the last welcome or redial, which set the backoff.
    attempt: u32,
    /// Set by a refusal that retrying cannot change. Nothing dials again until the process restarts.
    failed: bool,
    /// Set once a shutdown or the last handle ended the connection. Nothing dials again.
    stopped: bool,
    /// Message types a newer controller sent that this agent does not know.
    skipped: SkippedTypes,
}

enum Dialled {
    /// The socket opened.
    Open(Box<Socket>),
    /// A refusal retrying cannot change: why, and what the operator can do.
    Refused(String, &'static str),
    /// A failure that the backoff retries.
    Retry,
}

impl Driver {
    async fn run(mut self) {
        'dialling: loop {
            if self.failed || self.stopped {
                match self.receiver.recv().await {
                    None => return,
                    Some(Command::Send(_, _, ack, log_bytes)) => {
                        self.handle.release_log_frame(log_bytes);
                        let _ = ack.send(false);
                    }
                    Some(Command::Reply(_, _, ack)) => {
                        let _ = ack.send(false);
                    }
                    Some(Command::Shutdown(_, ack)) => {
                        let _ = ack.send(());
                    }
                    Some(Command::Redial | Command::Terminate(_)) => {}
                }

                continue;
            }

            let url = self.hooks.url();
            let (token, timings, log) = (self.token.clone(), self.timings.clone(), self.log.clone());
            let dialling = dial(&url, &token, &timings, &log);

            tokio::pin!(dialling);

            // A frame sent meanwhile is refused; only a redial, a stop or a shutdown gives the dial up.
            let dialled = loop {
                tokio::select! {
                    dialled = &mut dialling => break dialled,
                    command = self.receiver.recv() => {
                        let redial = matches!(command, Some(Command::Redial));

                        if !self.idle_command(command) {
                            return;
                        }

                        if redial || self.stopped {
                            continue 'dialling;
                        }
                    }
                }
            };

            let ended = match dialled {
                Dialled::Open(socket) => {
                    let ended = self.session(*socket, &url).await;

                    match &ended {
                        Ended::Closed(code, reason) => self.hooks.on_close(u16::from(*code), reason),
                        Ended::Shutdown(_) => self.hooks.on_close(u16::from(close_code(close_codes::NORMAL)), ""),
                        Ended::Redial | Ended::Stop => {}
                    }

                    ended
                }
                Dialled::Refused(reason, remedy) => {
                    self.fail(&reason, remedy);
                    self.hooks.on_close(u16::from(CloseCode::Abnormal), "");
                    continue;
                }
                // A dial that never opened closes as an abnormal 1006, as a socket that dropped does.
                Dialled::Retry => {
                    self.hooks.on_close(u16::from(CloseCode::Abnormal), "");
                    Ended::Closed(CloseCode::Abnormal, String::new())
                }
            };

            self.connected(false);

            match ended {
                Ended::Redial => self.attempt = 0,
                Ended::Stop => self.stopped = true,
                Ended::Shutdown(ack) => {
                    self.stopped = true;
                    let _ = ack.send(());
                }
                Ended::Closed(code, reason) => {
                    if !self.after_close(code, &reason) {
                        continue;
                    }

                    if !self.backoff().await {
                        return;
                    }
                }
            }
        }
    }

    /// A command while no socket is open. Returns false when every handle is gone.
    fn idle_command(&mut self, command: Option<Command>) -> bool {
        match command {
            None => return false,
            Some(Command::Send(_, _, ack, log_bytes)) => {
                self.handle.release_log_frame(log_bytes);
                let _ = ack.send(false);
            }
            Some(Command::Reply(_, _, ack)) => {
                let _ = ack.send(false);
            }
            Some(Command::Shutdown(_, ack)) => {
                self.stopped = true;
                let _ = ack.send(());
            }
            Some(Command::Redial) => self.attempt = 0,
            Some(Command::Terminate(_)) => {}
        }

        true
    }

    fn connected(&self, open: bool) {
        self.handle.connected.store(open, Ordering::SeqCst);

        if !open {
            self.handle.welcomed.store(false, Ordering::SeqCst);
        }
    }

    /// Stops dialling until the agent restarts, on purpose: a fleet that retried a refusal would
    /// keep hammering the controller for as long as the refusal lasts.
    fn fail(&mut self, reason: &str, remedy: &str) {
        self.failed = true;
        self.log.error(format!("{reason}; not reconnecting: {remedy}"));
        self.hooks.on_failed(reason);
    }

    /// Waits out the backoff while answering commands. Returns false when every handle is gone.
    async fn backoff(&mut self) -> bool {
        self.attempt += 1;

        let delay = backoff_delay(self.attempt, self.timings.min_backoff, self.timings.max_backoff, self.timings.random);
        let deadline = Instant::now() + delay;

        self.log.info(format!("reconnecting in {}ms (attempt {})", delay.as_millis(), self.attempt));

        loop {
            tokio::select! {
                () = sleep_until(deadline) => return true,
                command = self.receiver.recv() => {
                    let redial = matches!(command, Some(Command::Redial));

                    if !self.idle_command(command) {
                        return false;
                    }

                    if redial || self.stopped {
                        return true;
                    }
                }
            }
        }
    }

    /// Decides what a close means. Returns false when the connection must not redial.
    fn after_close(&mut self, code: CloseCode, reason: &str) -> bool {
        if self.stopped || self.failed {
            return false;
        }

        // The controller ends a connection on purpose with a policy close and a `CloseReason`.
        if code != close_code(close_codes::POLICY) {
            return true;
        }

        match from_wire_name::<CloseReason>(reason) {
            Some(CloseReason::AgentDeleted) => {
                self.fail(&format!("the controller at {} deleted this node", self.hooks.url()), "add the node again and set this host up with its new token");
                return false;
            }
            Some(CloseReason::NodeConnected) => {
                self.log.warn("the controller refused this connection: the node is already connected, from this host or another; run one agent per node, and set the other host up with its own node token");
            }
            _ => {}
        }

        true
    }

    #[allow(clippy::too_many_lines, reason = "one select loop over every event a socket sees")]
    async fn session(&mut self, socket: Socket, url: &str) -> Ended {
        let (mut sink, mut stream) = socket.split();
        let requests: Arc<Requests> = Arc::default();
        // A shutdown waiting for the requests the socket runs: until when, and who to tell.
        let mut stopping: Option<(Instant, oneshot::Sender<()>, Signal)> = None;
        let mut greeted = false;
        let mut welcome_deadline: Option<Instant> = None;
        let mut refused_close: Option<Instant> = None;
        let mut closing: Option<(Instant, Ended)> = None;
        let mut ping = tokio::time::interval_at(Instant::now() + self.timings.ping_interval, self.timings.ping_interval);
        // A tick the loop missed while busy is not made up: two back to back would see the first
        // ping unanswered before its pong could be read.
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut answered = true;

        let replies = Replies { commands: self.handle.commands.clone(), socket: self.handle.socket.fetch_add(1, Ordering::SeqCst) + 1 };
        self.connected(true);
        self.log.info(format!("connected to {url}"));

        // The socket's hooks end with it: one still running when the socket closes is cancelled, so
        // it cannot act on the next socket.
        let mut hooks = vec![self.run_hook(true)];

        loop {
            let deadline = closing.as_ref().map(|(at, _)| *at).or(stopping.as_ref().map(|(at, _, _)| *at)).or(refused_close).or(welcome_deadline);

            tokio::select! {
                frame = stream.next() => match frame {
                    Some(Err(tungstenite::Error::Capacity(error))) => {
                        self.log.warn(format!("closing the socket: {error}"));
                        close(&mut sink, close_code(close_codes::MESSAGE_TOO_BIG), "").await;

                        return ended_as(closing, stopping, || Ended::Closed(close_code(close_codes::MESSAGE_TOO_BIG), String::new()));
                    }
                    None | Some(Err(_)) => return ended_as(closing, stopping, || Ended::Closed(CloseCode::Abnormal, String::new())),
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame.map_or((CloseCode::Status, String::new()), |frame| (frame.code, frame.reason.to_string()));

                        return ended_as(closing, stopping, || Ended::Closed(code, reason));
                    }
                    // A pong that waits behind a write until the next tick reads as missed; a socket
                    // that slow is near its write-stall limit, so the reconnect costs nothing.
                    Some(Ok(Message::Pong(_))) => answered = true,
                    Some(Ok(Message::Text(text))) => {
                        // Once the controller refused the hello, nothing it sends is answered.
                        if closing.is_none() && !self.failed {
                            self.incoming(&text, &replies, &requests, stopping.is_some(), &mut greeted, &mut welcome_deadline, &mut refused_close, &mut hooks);
                        }
                    }
                    Some(Ok(_)) => {}
                },
                _ = ping.tick() => {
                    if !answered {
                        self.log.warn("the controller missed a pong; reconnecting");
                        return ended_as(closing, stopping, || Ended::Closed(CloseCode::Abnormal, String::new()));
                    }

                    answered = false;

                    if timeout(self.timings.ping_interval, sink.send(Message::Ping(Vec::new().into()))).await.is_err() {
                        return ended_as(closing, stopping, || self.stalled());
                    }
                },
                command = self.receiver.recv() => match command {
                    None => return Ended::Stop,
                    // Log frames still go out while shutdown waits for requests; a controller that stops
                    // reading then is cut off by Docker's stop timeout.
                    Some(Command::Send(value, kind, ack, log_bytes)) => {
                        if kind == FrameKind::Hello {
                            greeted = false;
                            self.handle.welcomed.store(false, Ordering::SeqCst);
                        }

                        if closing.is_some() || (!greeted && !kind.before_welcome()) {
                            self.handle.release_log_frame(log_bytes);
                            let _ = ack.send(false);
                            continue;
                        }

                        let written = write(&mut sink, &value, &self.log, self.timings.ping_interval).await;

                        self.handle.release_log_frame(log_bytes);

                        if written == Written::Sent && kind == FrameKind::Hello && welcome_deadline.is_none() {
                            welcome_deadline = Some(Instant::now() + self.timings.welcome_timeout);
                        }

                        let _ = ack.send(written == Written::Sent);

                        if written == Written::Stalled {
                            return ended_as(closing, stopping, || self.stalled());
                        }
                    }
                    Some(Command::Reply(socket, value, ack)) => {
                        if socket != replies.socket {
                            let _ = ack.send(false);
                            continue;
                        }

                        let written = write(&mut sink, &value, &self.log, self.timings.ping_interval).await;
                        let _ = ack.send(written == Written::Sent);

                        if written == Written::Stalled {
                            return ended_as(closing, stopping, || self.stalled());
                        }
                    }
                    Some(Command::Terminate(socket)) => {
                        if socket == self.handle.socket.load(Ordering::SeqCst) {
                            return ended_as(closing, stopping, || Ended::Closed(CloseCode::Abnormal, String::new()));
                        }
                    }
                    // A shutdown under way keeps its close; dialling another host would outlive the agent.
                    Some(Command::Redial) if closing.is_none() && stopping.is_none() => {
                        close(&mut sink, close_code(close_codes::NORMAL), "").await;
                        closing = Some((Instant::now() + CLOSE_GRACE, Ended::Redial));
                        self.connected(false);
                    }
                    Some(Command::Redial) => {}
                    // The controller hears at once that the agent stops; the requests it runs get their
                    // replies before the close, for up to the contract's wait.
                    Some(Command::Shutdown(signal, ack)) => {
                        let frame = AgentToControllerMessage::Shutdown(AgentShutdown { signal: signal.wire() });
                        let value = serde_json::to_value(&frame).unwrap_or(Value::Null);

                        if write(&mut sink, &value, &self.log, CLOSE_GRACE).await != Written::Sent {
                            return Ended::Shutdown(ack);
                        }

                        if lock(&requests.open).is_empty() {
                            closing = Some(self.close_for_shutdown(&mut sink, ack, signal).await);
                        } else {
                            stopping = Some((Instant::now() + SHUTDOWN_REQUEST_WAIT, ack, signal));
                        }
                    }
                },
                () = requests.settled.notified(), if stopping.is_some() => {
                    if lock(&requests.open).is_empty()
                        && let Some((_, ack, signal)) = stopping.take()
                    {
                        closing = Some(self.close_for_shutdown(&mut sink, ack, signal).await);
                    }
                },
                // Without a deadline the branch is off and its sleep never polled.
                () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    if let Some((_, ended)) = closing.take() {
                        return ended;
                    }

                    // Requests still running when the wait ends are cut off.
                    if let Some((_, ack, signal)) = stopping.take() {
                        closing = Some(self.close_for_shutdown(&mut sink, ack, signal).await);
                        continue;
                    }

                    if refused_close.take().is_some() {
                        close(&mut sink, close_code(close_codes::NORMAL), "").await;
                        closing = Some((Instant::now() + CLOSE_GRACE, Ended::Stop));
                        continue;
                    }

                    welcome_deadline = None;

                    if !greeted {
                        self.log.warn("the controller did not accept this node's hello before the deadline; reconnecting");
                        close(&mut sink, close_code(close_codes::NORMAL), AGENT_WELCOME_TIMEOUT_REASON).await;
                        closing = Some((Instant::now() + CLOSE_GRACE, Ended::Closed(close_code(close_codes::NORMAL), AGENT_WELCOME_TIMEOUT_REASON.to_owned())));
                    }
                },
            }
        }
    }

    async fn close_for_shutdown(&self, sink: &mut Sink, ack: oneshot::Sender<()>, signal: Signal) -> (Instant, Ended) {
        close(sink, close_code(close_codes::NORMAL), &signal.name()).await;
        self.connected(false);

        (Instant::now() + CLOSE_GRACE, Ended::Shutdown(ack))
    }

    /// A write that the controller did not take within a ping interval: the socket is as dead as one that
    /// missed a pong, which the loop could not notice while the write waited.
    fn stalled(&self) -> Ended {
        self.log.warn("the controller stopped reading this node's frames; reconnecting");
        Ended::Closed(CloseCode::Abnormal, String::new())
    }

    fn run_hook(&self, open: bool) -> AbortOnDropHandle<()> {
        let hooks = Arc::clone(&self.hooks);
        let handle = self.handle.clone();
        let log = self.log.clone();
        let name = if open { "onOpen" } else { "onWelcome" };
        let socket = handle.socket.load(Ordering::SeqCst);

        AbortOnDropHandle::new(tokio::spawn(async move {
            let result = if open { hooks.on_open(handle.clone()).await } else { hooks.on_welcome(handle.clone()).await };

            if let Err(error) = result {
                log.error(format!("{name} failed: {error}"));
                // Drops the socket the hook ran for, as a dead one: 1006, then the backoff.
                let _ = handle.commands.send(Command::Terminate(socket));
            }
        }))
    }

    #[allow(clippy::too_many_arguments, reason = "the per-socket state the session loop owns")]
    fn incoming(
        &mut self,
        text: &str,
        replies: &Replies,
        requests: &Arc<Requests>,
        stopping: bool,
        greeted: &mut bool,
        welcome_deadline: &mut Option<Instant>,
        refused_close: &mut Option<Instant>,
        hooks: &mut Vec<AbortOnDropHandle<()>>,
    ) {
        let Ok(raw) = serde_json::from_str::<Value>(text) else {
            self.log.warn("dropping non-JSON frame");
            return;
        };
        let id = raw.get("id").and_then(Value::as_str).map(str::to_owned);
        let kind = raw.get("type").and_then(Value::as_str).map(str::to_owned);
        let parsed =
            safe_parse(s_controller_to_agent_message, raw.clone()).ok().and_then(|value| serde_json::from_value::<ControllerToAgentMessage>(value).ok());

        let Some(message) = parsed else {
            let detail = safe_parse(s_controller_to_agent_message, raw).err().map(|issues| format_issues(&issues)).unwrap_or_default();

            match id {
                Some(id) => {
                    let known = kind.as_deref().is_some_and(is_controller_message_type);
                    let (code, detail) = match &kind {
                        Some(kind) if !known => (AgentErrorCode::UnsupportedRequest, format!("unsupported request type {kind}")),
                        _ => (AgentErrorCode::InvalidRequest, detail),
                    };
                    let request = kind.as_deref().and_then(from_wire_name::<RequestType>);

                    send_reply(replies, failure_reply(id, request, code, &detail));
                }
                // A newer controller may send a message this agent does not have yet.
                None => match kind {
                    Some(kind) if !is_controller_message_type(&kind) => {
                        if let Some(kind) = self.skipped.first_sighting(&kind) {
                            self.log.info(format!("skipping {kind} frames, which this agent does not know"));
                        }
                    }
                    _ => self.log.warn(format!("dropping invalid frame: {detail}")),
                },
            }

            return;
        };

        match message {
            ControllerToAgentMessage::WatchResources(watch) => self.hooks.on_watch(Watch::Resources(watch)),
            ControllerToAgentMessage::WatchLogs(watch) => self.hooks.on_watch(Watch::Logs(watch)),
            ControllerToAgentMessage::Welcome(_) => {
                if !*greeted {
                    *greeted = true;
                    self.handle.welcomed.store(true, Ordering::SeqCst);
                    self.attempt = 0;
                    *welcome_deadline = None;
                    hooks.push(self.run_hook(false));
                }
            }
            ControllerToAgentMessage::HelloRefused(refused) => {
                let reason = wire_name(refused.reason);

                self.fail(
                    &format!("the controller at {} refused this node's hello ({reason}): {}", self.hooks.url(), refused.detail),
                    "run node-setup.sh --update to take the agent image the controller names, or restart the agent container to try again",
                );
                // The controller closes next; closing from here at once would cross its close and lose its
                // code.
                *refused_close = Some(Instant::now() + self.timings.refused_close);
            }
            ControllerToAgentMessage::CheckCs2Build(_) => self.hooks.on_check_cs2_build(),
            message => {
                let id = id.unwrap_or_default();
                // Every message that reaches here parsed as a request, so its type is one the contract names.
                let Some(request_type) = kind.as_deref().and_then(from_wire_name::<RequestType>) else {
                    send_reply(replies, failure_reply(id, None, AgentErrorCode::UnsupportedRequest, "not a request"));
                    return;
                };

                // Once shutdown began, a new request is refused instead of run.
                if stopping {
                    send_reply(replies, failure_reply(id, Some(request_type), AgentErrorCode::ShuttingDown, "the agent is shutting down"));
                    return;
                }

                if !lock(&requests.open).insert(id.clone()) {
                    self.log.warn(format!("ignoring {} {id}: a request with that id is still running", wire_name(request_type)));
                    return;
                }

                let hooks = Arc::clone(&self.hooks);
                let log = self.log.clone();
                let replies = replies.clone();
                let requests = Arc::clone(requests);

                tokio::spawn(async move {
                    let defer = Defer::default();
                    let result = hooks.handle(Request { id: id.clone(), request_type, message }, defer.clone()).await;
                    let reply = match result {
                        Ok(data) => success_reply(&id, request_type, &data),
                        Err(error) => {
                            log.warn(format!("{} failed: {error}", wire_name(request_type)));
                            reply_frame(AgentReply::Failure(AgentReplyFailure {
                                reply_to: id.clone(),
                                request: Some(request_type),
                                error: agent_error(&error),
                            }))
                        }
                    };
                    let (ack, sent) = oneshot::channel();

                    if replies.send(reply, ack) {
                        let _ = sent.await;
                    }

                    lock(&requests.open).remove(&id);
                    requests.settled.notify_one();

                    for work in defer.take() {
                        tokio::spawn(work);
                    }
                });
            }
        }
    }
}

async fn dial(url: &str, token: &str, timings: &Timings, log: &Logger) -> Dialled {
    let mut request = match url.into_client_request() {
        Ok(request) => request,
        Err(error) => {
            log.warn(format!("socket error: {error}"));
            return Dialled::Retry;
        }
    };
    let headers = request.headers_mut();

    let Ok(authorization) = node_authorization(token).parse() else {
        return Dialled::Refused(
            "the node token holds characters an HTTP header cannot carry".to_owned(),
            "set this host up again with the token the controller gave the node",
        );
    };

    headers.insert("Authorization", authorization);
    headers.insert(PROTOCOL_HEADER, reqwest::header::HeaderValue::from_static(PROTOCOL_VERSION));

    let config = WebSocketConfig::default().max_message_size(Some(usize::try_from(CONTROLLER_FRAME_MAX_BYTES).unwrap_or(usize::MAX)));
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);

    match timeout(timings.handshake_timeout, connect).await {
        Ok(Ok((socket, _))) => Dialled::Open(Box::new(socket)),
        Ok(Err(tungstenite::Error::Http(response))) => {
            let status = response.status();

            // Only the controller's own verdicts end the dialling. A 404 or a 5xx is what a misconfigured or
            // failing proxy answers, so it is retried with the backoff.
            match status {
                StatusCode::UNAUTHORIZED => Dialled::Refused(
                    format!("the controller at {url} refused this node's token (HTTP {})", status.as_u16()),
                    "restart the agent container to try again; if the node was deleted, add it again and set this host up with its token",
                ),
                StatusCode::UPGRADE_REQUIRED => Dialled::Refused(
                    format!("the controller at {url} uses a protocol version incompatible with this agent's {PROTOCOL_VERSION} (HTTP {})", status.as_u16()),
                    "run node-setup.sh --update to take the agent image the controller names, or restart the agent container to try again",
                ),
                _ => {
                    log.warn(format!("the controller at {url} answered HTTP {} instead of opening the socket", status.as_u16()));
                    Dialled::Retry
                }
            }
        }
        Ok(Err(error)) => {
            log.warn(format!("socket error: {error}"));
            Dialled::Retry
        }
        Err(_) => {
            log.warn("socket error: Opening handshake has timed out");
            Dialled::Retry
        }
    }
}

fn send_reply(replies: &Replies, reply: Value) {
    let (ack, _) = oneshot::channel();
    let _ = replies.send(reply, ack);
}

/// Where a socket's request tasks send their replies.
#[derive(Clone)]
struct Replies {
    commands: mpsc::UnboundedSender<Command>,
    socket: u64,
}

impl Replies {
    fn send(&self, reply: Value, ack: oneshot::Sender<bool>) -> bool {
        self.commands.send(Command::Reply(self.socket, reply, ack)).is_ok()
    }
}

type Sink = futures_util::stream::SplitSink<Socket, Message>;

#[derive(PartialEq, Eq)]
enum Written {
    /// The frame went out.
    Sent,
    /// The socket refused the frame.
    Failed,
    /// Still not on its way after the limit: the controller stopped reading the socket.
    Stalled,
}

async fn write(sink: &mut Sink, value: &Value, log: &Logger, limit: Duration) -> Written {
    match timeout(limit, sink.send(Message::Text(value.to_string().into()))).await {
        Ok(Ok(())) => Written::Sent,
        Ok(Err(error)) => {
            log.warn(format!("send failed: {error}"));
            Written::Failed
        }
        Err(_) => Written::Stalled,
    }
}

/// A close code from the contract's `close_codes`, as tungstenite takes it.
#[must_use]
pub fn close_code(code: u64) -> CloseCode {
    u16::try_from(code).map_or(CloseCode::Abnormal, CloseCode::from)
}

async fn close(sink: &mut Sink, code: CloseCode, reason: &str) {
    let _ = timeout(CLOSE_GRACE, sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() })))).await;
}

fn is_controller_message_type(kind: &str) -> bool {
    ControllerToAgentMessage::TAGS.contains(&kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_log_frames_up_to_the_bound_whatever_threads_send_them() {
        let (connection, _driver) = ControllerConnection::new("t".to_owned(), Timings::default(), Logger::null());
        let frame = 1000;

        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..LOG_BACKLOG_MAX_BYTES / frame {
                        let _ = connection.hold_log_frame(frame);
                    }
                });
            }
        });

        assert!(connection.log_backlog.bytes.load(Ordering::SeqCst) <= LOG_BACKLOG_MAX_BYTES);
        assert!(!connection.hold_log_frame(frame));
        connection.release_log_frame(LOG_BACKLOG_MAX_BYTES / 2);
        assert!(connection.hold_log_frame(frame));
    }

    #[test]
    fn logs_the_dropped_frames_once_the_backlog_is_down_to_half_and_only_after_a_drop() {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&lines);
        let log = Logger::new("controller", Some(Arc::new(move |entry: crate::logger::Entry| lock(&seen).push(entry.message))));
        let (connection, _driver) = ControllerConnection::new("t".to_owned(), Timings::default(), log);
        let resumed = || lock(&lines).iter().filter(|line| line.contains("sending log frames again")).cloned().collect::<Vec<_>>();

        assert!(connection.hold_log_frame(1000));
        connection.release_log_frame(1000);
        assert!(resumed().is_empty(), "nothing was dropped");

        assert!(connection.hold_log_frame(LOG_BACKLOG_MAX_BYTES));
        assert!(!connection.hold_log_frame(1));
        connection.release_log_frame(1);
        assert!(resumed().is_empty(), "the backlog is still over half");

        connection.release_log_frame(LOG_BACKLOG_MAX_BYTES / 2 - 1);
        assert_eq!(resumed(), ["[controller] sending log frames again; dropped 1 while the controller was behind"]);
    }

    #[test]
    fn knows_which_types_the_controller_sends() {
        assert!(is_controller_message_type("start_server"));
        assert!(is_controller_message_type("welcome"));
        assert!(!is_controller_message_type("make_coffee"));
    }
}
