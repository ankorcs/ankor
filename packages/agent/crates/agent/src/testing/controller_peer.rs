//! The service side of the agent socket: the bearer and protocol checks on the upgrade, a welcome
//! for each hello, one reply per request id, and a record of every frame and close. Enough of a controller
//! service for an agent to run against.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::constants::{DEFAULT_EVENTS_PATH, DEFAULT_SOCKET_PATH, PROTOCOL_HEADER, close_codes};

use crate::controller_connection::close_code;
use ankor_protocol::helpers::{is_compatible_protocol, node_authorization, wire_name};
use ankor_protocol::schemas::s_agent_to_controller_message;
use ankor_protocol::types::AgentToControllerMessage;
use ankor_protocol::validate::{format_issues, safe_parse};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::{agent_message, lock, welcome};

#[derive(Clone, Debug)]
pub struct PeerOptions {
    /// The only bearer token upgraded; `None` accepts every token.
    pub token: Option<String>,
    /// Welcome each hello; false leaves the welcome to the test.
    pub welcome: bool,
    /// Close the socket on `shutdown`, as a controller that does not wait for the agent's close would;
    /// by default the agent closes it, as the contract has the controller wait.
    pub close_on_shutdown: bool,
    /// Where the agent socket is served.
    pub socket_path: String,
    /// Where event batches are taken.
    pub events_path: String,
}

impl Default for PeerOptions {
    fn default() -> Self {
        Self { token: None, welcome: true, close_on_shutdown: false, socket_path: DEFAULT_SOCKET_PATH.to_owned(), events_path: DEFAULT_EVENTS_PATH.to_owned() }
    }
}

enum Command {
    /// Sends a text frame.
    Send(String),
    /// Sends a binary frame.
    Binary(Vec<u8>),
    /// Sends a close frame with this code and reason.
    Close(u16, String),
    /// Drops the connection without a close frame.
    Terminate,
    /// Stops reading the socket, so pings and a close go unanswered.
    Pause,
    /// Reads the socket again after a pause.
    Resume,
}

#[derive(Default)]
struct State {
    /// Every valid frame the agent sent, in order.
    received: Vec<Value>,
    /// Each frame the contract refuses, with the reasons.
    refused: Vec<String>,
    /// Sockets upgraded, closed ones included.
    accepted: usize,
    /// Hellos received.
    hellos: usize,
    /// The `Authorization` header of the last upgrade.
    auth: Option<String>,
    /// The close code and reason each socket ended with.
    closes: Vec<(u16, String)>,
    /// Why each socket ended: the shutdown signal, or `connection_lost`.
    disconnects: Vec<String>,
    /// Pings the agent sent.
    pings: usize,
    /// Commands for the open socket; `None` while none is open.
    current: Option<mpsc::UnboundedSender<Command>>,
    /// Requests waiting for a reply, by id.
    replies: HashMap<String, oneshot::Sender<Value>>,
    /// Every event batch posted, refused or not, with its `Authorization` header.
    uploads: Vec<(Option<String>, Value)>,
    /// Answers batches with a receipt rather than 503.
    accept_uploads: bool,
}

pub struct ControllerPeer {
    /// `ws://127.0.0.1:<port><socket path>`.
    pub url: String,
    /// The loopback port the peer listens on.
    pub port: u16,
    /// What the peer recorded, and the open socket.
    state: Arc<Mutex<State>>,
    /// The number in the next request's id.
    next_id: AtomicU64,
}

impl ControllerPeer {
    /// # Panics
    ///
    /// When no loopback port is free.
    pub async fn start(options: PeerOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
        let port = listener.local_addr().expect("a bound port").port();
        let state: Arc<Mutex<State>> = Arc::default();
        let accepting = Arc::clone(&state);
        let url = format!("ws://127.0.0.1:{port}{}", options.socket_path);
        let options = Arc::new(options);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&accepting), Arc::clone(&options)));
            }
        });

        Self { url, port, state, next_id: AtomicU64::new(1) }
    }

    /// A slot's state as the last hello and the `slot_status` frames after it left it.
    #[must_use]
    pub fn status(&self, slot_id: &str) -> Option<Value> {
        let received = self.received();
        let since = received.iter().rposition(|frame| matches!(agent_message(frame), Some(AgentToControllerMessage::Hello(_))))?;
        let from_hello = received.get(since).and_then(|hello| hello["slot_status"].as_array()?.iter().find(|status| status["slot_id"] == slot_id).cloned());

        received
            .iter()
            .skip(since)
            .rev()
            .find(|frame| matches!(agent_message(frame), Some(AgentToControllerMessage::SlotStatus(status)) if status.slot_id == slot_id))
            .cloned()
            .or(from_hello)
    }

    /// Every valid frame the agent sent, in order.
    #[must_use]
    pub fn received(&self) -> Vec<Value> {
        lock(&self.state).received.clone()
    }

    /// Every frame the agent sent that the contract refuses, with the reasons: what a test expects
    /// to stay empty.
    #[must_use]
    pub fn refused(&self) -> Vec<String> {
        lock(&self.state).refused.clone()
    }

    /// The frames whose message `matches`, such as `|message| matches!(message, AgentToControllerMessage::Reply(_))`,
    /// so a test picks frames by the contract's message kinds rather than by their `type` text.
    #[must_use]
    pub fn received_where(&self, matches: impl Fn(&AgentToControllerMessage) -> bool) -> Vec<Value> {
        self.received().into_iter().filter(|frame| agent_message(frame).is_some_and(|message| matches(&message))).collect()
    }

    /// Sockets the agent has opened, closed ones included.
    #[must_use]
    pub fn accepted(&self) -> usize {
        lock(&self.state).accepted
    }

    #[must_use]
    pub fn hellos(&self) -> usize {
        lock(&self.state).hellos
    }

    /// The `Authorization` header of the last upgrade.
    #[must_use]
    pub fn auth_header(&self) -> Option<String> {
        lock(&self.state).auth.clone()
    }

    /// The close code and reason each socket ended with, in order.
    #[must_use]
    pub fn closes(&self) -> Vec<(u16, String)> {
        lock(&self.state).closes.clone()
    }

    /// Why each socket ended: the shutdown signal, or `connection_lost`.
    #[must_use]
    pub fn disconnects(&self) -> Vec<String> {
        lock(&self.state).disconnects.clone()
    }

    #[must_use]
    pub fn pings(&self) -> usize {
        lock(&self.state).pings
    }

    fn command(&self, command: Command) {
        if let Some(current) = &lock(&self.state).current {
            let _ = current.send(command);
        }
    }

    /// Sends a raw frame on the current socket.
    pub fn send(&self, text: &str) {
        self.command(Command::Send(text.to_owned()));
    }

    pub fn send_json(&self, value: &Value) {
        self.send(&value.to_string());
    }

    /// Sends a binary frame on the current socket, which the contract has no use for.
    pub fn send_binary(&self, bytes: Vec<u8>) {
        self.command(Command::Binary(bytes));
    }

    #[must_use]
    pub fn connected(&self) -> bool {
        lock(&self.state).current.is_some()
    }

    pub fn close(&self, code: u16, reason: &str) {
        self.command(Command::Close(code, reason.to_owned()));
    }

    /// Drops the current connection without a close frame.
    pub fn terminate(&self) {
        self.command(Command::Terminate);
    }

    /// Stops reading the current socket: what a half-open link looks like.
    pub fn pause(&self) {
        self.command(Command::Pause);
    }

    /// Reads the current socket again after [`ControllerPeer::pause`]: a slow link catching up.
    pub fn resume(&self) {
        self.command(Command::Resume);
    }

    /// Sends a request with a new id and waits for its reply.
    ///
    /// # Panics
    ///
    /// When no reply comes within eight seconds of wall-clock time. The wait polls as
    /// [`crate::testing::until`] does: on paused time a tokio timeout would run out at once while
    /// the agent's answer waits on a thread of its own, such as the spool's.
    pub async fn request(&self, mut frame: Value) -> Value {
        let id = format!("peer-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let (answer, mut answered) = oneshot::channel();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);

        frame["id"] = json!(id);
        lock(&self.state).replies.insert(id, answer);
        self.send_json(&frame);

        loop {
            match answered.try_recv() {
                Ok(reply) => return reply,
                Err(oneshot::error::TryRecvError::Closed) => panic!("the peer dropped the reply"),
                Err(oneshot::error::TryRecvError::Empty) => {
                    assert!(std::time::Instant::now() < deadline, "no reply in time");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }

    /// Waits for a frame `matches` accepts, received before or after the call.
    ///
    /// # Panics
    ///
    /// When none comes within eight seconds.
    pub async fn wait_for(&self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        let mut found = None;

        super::until(what, || {
            found = self.received().into_iter().find(|message| matches(message));
            found.is_some()
        })
        .await;
        found.expect("found")
    }

    pub async fn wait_for_connection(&self) {
        super::until("a connection", || lock(&self.state).current.is_some()).await;
    }

    /// Answers the following event batches with a receipt instead of 503.
    pub fn accept_uploads(&self) {
        lock(&self.state).accept_uploads = true;
    }

    /// Every event batch posted, refused or not, with its `Authorization` header.
    #[must_use]
    pub fn uploads(&self) -> Vec<(Option<String>, Value)> {
        lock(&self.state).uploads.clone()
    }

    /// Fails if the agent sent a frame the contract refuses.
    ///
    /// # Panics
    ///
    /// When it did.
    pub fn expect_valid_frames(&self) {
        assert_eq!(lock(&self.state).refused, Vec::<String>::new());
    }
}

/// Every test checks what the agent sent: a frame the contract refuses fails the test when its peer
/// goes, unless the test already failed.
impl Drop for ControllerPeer {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.expect_valid_frames();
        }
    }
}

async fn serve(stream: TcpStream, state: Arc<Mutex<State>>, options: Arc<PeerOptions>) {
    if !is_upgrade(&stream).await {
        serve_http(stream, &state, &options).await;
        return;
    }

    let checked = Arc::clone(&state);
    let expected = options.token.clone();
    let socket_path = options.socket_path.clone();
    let callback = move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let refuse = |status: StatusCode| {
            let mut refusal = ErrorResponse::new(None);

            *refusal.status_mut() = status;
            Err(refusal)
        };
        let header = request.headers().get("authorization").and_then(|value| value.to_str().ok()).map(str::to_owned);

        if request.uri().path() != socket_path || expected.as_ref().is_some_and(|token| header.as_deref() != Some(&node_authorization(token))) {
            return refuse(StatusCode::UNAUTHORIZED);
        }

        if !is_compatible_protocol(request.headers().get(PROTOCOL_HEADER).and_then(|value| value.to_str().ok())) {
            return refuse(StatusCode::UPGRADE_REQUIRED);
        }

        lock(&checked).auth = header;
        Ok(response)
    };
    let Ok(socket) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
        return;
    };
    let (commands, receiver) = mpsc::unbounded_channel();

    {
        let mut state = lock(&state);

        state.accepted += 1;
        state.current = Some(commands.clone());
    }

    let (close, reason) = run(socket, receiver, &state, &options).await;
    let mut state = lock(&state);

    state.closes.push(close);
    state.disconnects.push(reason);

    // Forgotten once it ends, so waiting for a connection waits for a new one; a newer socket stays.
    if state.current.as_ref().is_some_and(|current| current.same_channel(&commands)) {
        state.current = None;
    }
}

/// Serves one socket; returns its close and why it ended.
async fn run(
    mut socket: WebSocketStream<TcpStream>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    state: &Arc<Mutex<State>>,
    options: &PeerOptions,
) -> ((u16, String), String) {
    let mut reason = "connection_lost".to_owned();
    let mut paused = false;

    loop {
        tokio::select! {
            frame = socket.next(), if !paused => match frame {
                Some(Ok(Message::Text(text))) => {
                    if let Some(signal) = receive(&mut socket, &text, state, options).await {
                        reason = signal;
                    }
                }
                Some(Ok(Message::Ping(_))) => lock(state).pings += 1,
                Some(Ok(Message::Close(frame))) => {
                    let close = frame.map_or((u16::from(CloseCode::Status), String::new()), |frame| (u16::from(frame.code), frame.reason.to_string()));

                    // Answer the close, then wait for the agent to end the connection.
                    while let Some(Ok(_)) = socket.next().await {}

                    return (close, reason);
                }
                Some(Ok(_)) => {}
                None | Some(Err(_)) => return ((u16::from(CloseCode::Abnormal), String::new()), reason),
            },
            command = commands.recv() => match command {
                Some(Command::Send(text)) => drop(socket.send(Message::Text(text.into())).await),
                Some(Command::Binary(bytes)) => drop(socket.send(Message::Binary(bytes.into())).await),
                Some(Command::Close(code, why)) => drop(socket.send(Message::Close(Some(CloseFrame { code: CloseCode::from(code), reason: why.into() }))).await),
                Some(Command::Pause) => paused = true,
                Some(Command::Resume) => paused = false,
                Some(Command::Terminate) | None => return ((u16::from(CloseCode::Abnormal), String::new()), reason),
            },
        }
    }
}

/// Records a frame and answers it as the controller would; returns the signal of a `shutdown`.
async fn receive(socket: &mut WebSocketStream<TcpStream>, text: &str, state: &Arc<Mutex<State>>, options: &PeerOptions) -> Option<String> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        lock(state).refused.push(text.to_owned());
        return None;
    };

    if let Err(issues) = safe_parse(s_agent_to_controller_message, value.clone()) {
        lock(state).refused.push(format!("{text}\n{}", format_issues(&issues)));
        return None;
    }

    lock(state).received.push(value.clone());

    match agent_message(&value) {
        Some(AgentToControllerMessage::Hello(_)) => {
            lock(state).hellos += 1;

            if options.welcome {
                let _ = socket.send(Message::Text(welcome().to_string().into())).await;
            }

            None
        }
        Some(AgentToControllerMessage::Shutdown(shutdown)) => {
            let signal = wire_name(shutdown.signal);

            if options.close_on_shutdown {
                let _ = socket.send(Message::Close(Some(CloseFrame { code: close_code(close_codes::NORMAL), reason: signal.clone().into() }))).await;
            }

            Some(signal)
        }
        Some(AgentToControllerMessage::Reply(_)) => {
            let id = value["reply_to"].as_str().unwrap_or_default().to_owned();

            if let Some(answer) = lock(state).replies.remove(&id) {
                let _ = answer.send(value);
            }

            None
        }
        _ => None,
    }
}

/// Whether the request waiting on `stream` asks for a WebSocket, read without taking it off the stream.
async fn is_upgrade(stream: &TcpStream) -> bool {
    let mut buffer = vec![0; 16 * 1024];

    for _ in 0..200 {
        let Ok(read) = stream.peek(&mut buffer).await else {
            return false;
        };
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut headers);

        match request.parse(buffer.get(..read).unwrap_or_default()) {
            Ok(httparse::Status::Complete(_)) => {
                return request.headers.iter().any(|header| header.name.eq_ignore_ascii_case("upgrade") && header.value.eq_ignore_ascii_case(b"websocket"));
            }
            Ok(httparse::Status::Partial) if read > 0 => tokio::time::sleep(Duration::from_millis(5)).await,
            _ => return false,
        }
    }

    false
}

/// The event-batch endpoint: each batch is recorded, then answered with 503 or a receipt.
async fn serve_http(stream: TcpStream, state: &Arc<Mutex<State>>, options: &PeerOptions) {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

    let mut reader = tokio::io::BufReader::new(stream);

    loop {
        let mut line = String::new();

        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }

        let mut parts = line.split_whitespace();
        let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
        let mut headers: Vec<(String, String)> = Vec::new();

        loop {
            let mut header = String::new();

            if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                return;
            }

            let header = header.trim_end();

            if header.is_empty() {
                break;
            }

            if let Some((name, value)) = header.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }

        let header = |name: &str| headers.iter().find(|(each, _)| each == name).map(|(_, value)| value.clone());
        let length: usize = header("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
        let mut body = vec![0; length];

        if reader.read_exact(&mut body).await.is_err() {
            return;
        }

        let (status, answer) = if method != "POST" || path != options.events_path {
            (404, String::new())
        } else if !is_compatible_protocol(header(&PROTOCOL_HEADER.to_ascii_lowercase()).as_deref()) {
            (426, String::new())
        } else {
            let batch = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let mut state = lock(state);

            state.uploads.push((header("authorization"), batch));

            if state.accept_uploads { (200, json!({ "rejected": [] }).to_string()) } else { (503, String::new()) }
        };
        let response = format!("HTTP/1.1 {status} Answer\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{answer}", answer.len());

        if reader.get_mut().write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}
