//! The plugins' local socket: control to the agent's handlers, history into the spool, and acks
//! back to the plugin as frames are stored.
//!
//! After a gap, or with too many frames waiting to be stored, the relay takes no more history from
//! that plugin and ends its socket once the rest is stored. The plugin reconnects and resends from
//! its last ack.

use crate::sync::lock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::constants::{EVENT_BATCH_MAX_BYTES, EVENT_BATCH_MAX_FRAMES, PROTOCOL_HEADER, PROTOCOL_VERSION};
use ankor_protocol::helpers::is_compatible_protocol;
use ankor_protocol::schemas::s_event_frame;
use ankor_protocol::types::{AgentErrorCode, ServerState};
use ankor_protocol::validate::safe_parse;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request as HttpRequest, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_util::sync::CancellationToken;

use crate::contract::constants::{MAX_FRAME_BYTES, PLUGIN_WS_PATH, RELAY_QUEUE_MAX_BYTES, RELAY_QUEUE_MAX_FRAMES};
use crate::contract::schemas::s_plugin_to_agent_message;
use crate::contract::types::{AgentToPluginMessage, EventAck, PluginCommand, PluginGetServerStatus, PluginToAgentMessage};
use crate::event_spool::{SpoolStream, SpooledFrame};
use crate::failure::AgentFailure;
use crate::logger::Logger;
use crate::skipped_types::SkippedTypes;
use crate::spool_client::SpoolPort;

/// Query keys of the URL handed to a plugin in `ANKOR_WS_URL` and read back when it dials.
const SLOT_ID_PARAM: &str = "slot_id";
const KEY_PARAM: &str = "key";

/// Shared by the listener and the URL handed to a plugin, so the two agree.
const RELAY_HOST: &str = "127.0.0.1";

const LOCAL_FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Compares in full whatever the first difference, so the time taken reveals nothing.
fn holds_key(expected: &str, given: Option<&str>) -> bool {
    let given = given.unwrap_or_default();

    given.len() == expected.len() && given.bytes().zip(expected.bytes()).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// Anything on the host's loopback can dial the relay, a game of another slot included. Each launch's
/// key is random and lives only in its container, which no other game can read. 8 random bytes leave
/// nothing to guess at the rate a local socket allows.
#[must_use]
pub(crate) fn new_relay_key() -> String {
    hex::encode(rand::random::<[u8; 8]>())
}

pub(crate) type OnlineHook = Arc<dyn Fn(&str, bool) + Send + Sync>;
pub(crate) type MessageHook = Arc<dyn Fn(&str, PluginToAgentMessage) + Send + Sync>;

#[derive(Clone)]
pub struct RelayHooks {
    /// Told when a server's plugin connects (`true`) or disconnects (`false`).
    pub on_plugin_online: OnlineHook,
    /// Every plugin message but events, which go to the spool.
    pub on_plugin_message: MessageHook,
}

enum Outgoing {
    /// A JSON message for the plugin.
    Text(String),
    /// Ends the socket with a close frame, after what was queued before it.
    Close(Ending),
}

/// The plugin reconnects after each, and replays from its last ack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The agent is shutting down: 1001, going away.
    Shutdown,
    /// History waits for a replay from the plugin's last ack: 1000.
    Replay,
    /// A newer connection from the same server took its place: 1000.
    Replaced,
    /// The server's slot or launch is gone: 1000.
    Released,
}

impl Ending {
    fn frame(self) -> CloseFrame {
        let (code, reason) = match self {
            Self::Shutdown => (CloseCode::Away, "agent shutting down"),
            Self::Replay => (CloseCode::Normal, "replay from the last ack"),
            Self::Replaced => (CloseCode::Normal, "replaced by a newer connection"),
            Self::Released => (CloseCode::Normal, "server released"),
        };

        CloseFrame { code, reason: reason.into() }
    }
}

#[derive(Clone)]
struct Producer {
    /// Tells this socket from a newer one on the same connection.
    id: u64,
    /// Queues messages for the socket's writer.
    sender: mpsc::UnboundedSender<Outgoing>,
    /// Wakes the reader so that it stops reading.
    stop: Arc<Notify>,
    /// Where the plugin's link and messages go, from the listener that accepted it.
    hooks: RelayHooks,
}

impl Producer {
    fn terminate(&self, ending: Ending) {
        let _ = self.sender.send(Outgoing::Close(ending));
        self.stop.notify_one();
    }
}

struct Connection {
    /// The slot the server runs in.
    slot_id: String,
    /// The allocation the slot was prepared for.
    allocation_id: String,
    /// The key the allocation's plugin dials with.
    key: String,
    /// The plugin's current socket, or `None` while it is not connected.
    producer: Option<Producer>,
    /// Events waiting for the next commit.
    queued_frames: Vec<SpooledFrame>,
    /// The agent's `get_server_status` requests that wait for an answer, by request id.
    status_requests: HashMap<String, oneshot::Sender<Result<ServerState, AgentFailure>>>,
    /// Commands that wait for the plugin's result, by request id.
    command_requests: HashMap<String, oneshot::Sender<Result<(), AgentFailure>>>,
    /// Bytes of the queued frames and of those awaiting their commit.
    pending_bytes: usize,
    /// The number of queued frames and of those awaiting their commit.
    pending_frames: usize,
    /// Set after a gap or a full queue. History is refused until the plugin replays from its last ack.
    replay_required: bool,
}

type Shared = Arc<Mutex<Connection>>;

struct Inner {
    /// Where history is stored, or `None` when the spool did not open.
    spool: Option<Arc<dyn SpoolPort>>,
    /// Receives the relay's lines.
    log: Logger,
    /// Each prepared slot's connection, by slot id.
    slots: Mutex<HashMap<String, Shared>>,
    /// Connections with frames waiting for the next commit, released ones included.
    queued: Mutex<Vec<Shared>>,
    /// Set from a flush until its commit takes the queue, so one commit task is spawned at a time.
    flush_scheduled: AtomicBool,
    /// Set while a delayed flush is pending.
    flush_timer: AtomicBool,
    /// One commit at a time, in order.
    commits: tokio::sync::Mutex<()>,
    /// Set once `close` has begun. New sockets are refused and incoming frames ignored.
    stopped: AtomicBool,
    /// Ends the listener once `close` has stored what was received.
    closed: CancellationToken,
    /// The bound port, which `prepare` writes into the plugin's URL.
    port: AtomicU16,
    /// The id the next socket receives.
    next_producer: AtomicU64,
    /// Message types a newer plugin sent that this agent does not know.
    skipped: Mutex<SkippedTypes>,
}

#[derive(Clone)]
pub struct PluginRelay {
    inner: Arc<Inner>,
}

/// A bound port that accepts no plugin until [`RelayListener::serve`].
pub struct RelayListener {
    relay: PluginRelay,
    listener: TcpListener,
    port: u16,
}

impl RelayListener {
    pub fn serve(self, hooks: RelayHooks) -> u16 {
        let Self { relay, listener, port } = self;

        tokio::spawn(async move {
            let (closed, log) = (relay.inner.closed.clone(), relay.inner.log.clone());

            accept_until_closed(
                &closed,
                &log,
                || listener.accept(),
                |(stream, _)| {
                    let (relay, hooks) = (relay.clone(), hooks.clone());

                    tokio::spawn(async move { relay.handshake(stream, hooks).await });
                },
            )
            .await;
        });

        port
    }
}

/// An accept that fails immediately, as when the agent is out of file descriptors, would otherwise spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// A failed accept is logged once per run of failures and retried, because giving up would leave
/// every plugin offline until a restart.
async fn accept_until_closed<S, F>(closed: &CancellationToken, log: &Logger, mut accept: impl FnMut() -> F, mut serve: impl FnMut(S))
where
    F: Future<Output = std::io::Result<S>>,
{
    let mut failing = false;

    loop {
        let accepted = tokio::select! {
            biased;
            () = closed.cancelled() => return,
            accepted = accept() => accepted,
        };

        match accepted {
            Ok(stream) => {
                failing = false;
                serve(stream);
            }
            Err(error) => {
                if !failing {
                    log.warn(format!("the relay could not accept a plugin: {error}; trying again"));
                    failing = true;
                }

                if closed.run_until_cancelled(tokio::time::sleep(ACCEPT_RETRY)).await.is_none() {
                    return;
                }
            }
        }
    }
}

impl PluginRelay {
    /// `spool` is `None` when it did not open. Control still flows, and events stay unacknowledged
    /// with their plugin. No plugin reaches the relay before [`RelayListener::serve`].
    #[must_use]
    pub fn new(spool: Option<Arc<dyn SpoolPort>>, log: Logger) -> Self {
        Self {
            inner: Arc::new(Inner {
                spool,
                log,
                slots: Mutex::default(),
                queued: Mutex::default(),
                flush_scheduled: AtomicBool::new(false),
                flush_timer: AtomicBool::new(false),
                commits: tokio::sync::Mutex::new(()),
                stopped: AtomicBool::new(false),
                closed: CancellationToken::new(),
                port: AtomicU16::new(0),
                next_producer: AtomicU64::new(0),
                skipped: Mutex::default(),
            }),
        }
    }

    /// Port 0 picks a free port, which [`PluginRelay::prepare`] then names. Plugins that dial wait
    /// until [`RelayListener::serve`].
    ///
    /// # Errors
    ///
    /// Fails when the port cannot be bound.
    pub async fn bind(&self, port: u16) -> std::io::Result<RelayListener> {
        let listener = TcpListener::bind((RELAY_HOST, port))
            .await
            .map_err(|error| std::io::Error::new(error.kind(), format!("cannot listen on {RELAY_HOST}:{port}: {error}")))?;
        let bound = listener.local_addr()?.port();

        self.inner.port.store(bound, Ordering::SeqCst);

        Ok(RelayListener { relay: self.clone(), listener, port: bound })
    }

    async fn handshake(&self, mut stream: tokio::net::TcpStream, hooks: RelayHooks) {
        // Anything but a socket upgrade gets an empty 404, which the WebSocket handshake would not send.
        if !tokio::time::timeout(UPGRADE_WAIT, is_upgrade(&stream)).await.unwrap_or(false) {
            let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            return;
        }

        let chosen: Arc<Mutex<Option<Shared>>> = Arc::default();
        let found = Arc::clone(&chosen);
        let relay = self.clone();
        let callback = move |request: &HttpRequest, response: Response| -> Result<Response, ErrorResponse> {
            let refuse = |status: StatusCode| {
                let mut refusal = ErrorResponse::new(None);

                *refusal.status_mut() = status;
                Err(refusal)
            };
            let query: HashMap<String, String> =
                request.uri().query().map(|query| url::form_urlencoded::parse(query.as_bytes()).into_owned().collect()).unwrap_or_default();
            let slot = query.get(SLOT_ID_PARAM).and_then(|slot| lock(&relay.inner.slots).get(slot).cloned());
            let Some(connection) = slot.filter(|_| request.uri().path() == PLUGIN_WS_PATH && !relay.inner.stopped.load(Ordering::SeqCst)) else {
                return refuse(StatusCode::NOT_FOUND);
            };
            let (slot_id, key) = {
                let connection = lock(&connection);

                (connection.slot_id.clone(), connection.key.clone())
            };

            if !holds_key(&key, query.get(KEY_PARAM).map(String::as_str)) {
                relay.inner.log.warn(format!("slot {slot_id}: refused a plugin connection without its launch's key"));
                return refuse(StatusCode::UNAUTHORIZED);
            }

            let header = request.headers().get(PROTOCOL_HEADER).and_then(|value| value.to_str().ok());

            if !is_compatible_protocol(header) {
                relay
                    .inner
                    .log
                    .warn(format!("slot {slot_id}: the plugin's protocol version is incompatible with this agent's {PROTOCOL_VERSION}; update the cs2 image"));
                return refuse(StatusCode::UPGRADE_REQUIRED);
            }

            *lock(&found) = Some(connection);
            Ok(response)
        };
        let config = WebSocketConfig::default().max_message_size(Some(usize::try_from(MAX_FRAME_BYTES).unwrap_or(usize::MAX)));

        match tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config)).await {
            Ok(socket) => {
                let connection = lock(&chosen).take();

                if let Some(connection) = connection {
                    self.attach_producer(&connection, socket, hooks);
                }
            }
            Err(error) => {
                if lock(&chosen).is_none() {
                    return;
                }

                self.inner.log.warn(format!("plugin handshake failed: {error}"));
            }
        }
    }

    /// A new allocation on the slot retires the previous one's connection.
    #[must_use]
    pub fn prepare(&self, slot_id: &str, allocation_id: &str, key: &str) -> String {
        // Checked and replaced in one hold, so two prepares cannot both install an entry.
        let replaced = {
            let mut slots = lock(&self.inner.slots);

            if slots.get(slot_id).is_some_and(|connection| lock(connection).allocation_id == allocation_id) {
                None
            } else {
                slots.insert(
                    slot_id.to_owned(),
                    Arc::new(Mutex::new(Connection {
                        slot_id: slot_id.to_owned(),
                        allocation_id: allocation_id.to_owned(),
                        key: key.to_owned(),
                        producer: None,
                        queued_frames: Vec::new(),
                        status_requests: HashMap::new(),
                        command_requests: HashMap::new(),
                        pending_bytes: 0,
                        pending_frames: 0,
                        replay_required: false,
                    })),
                )
            }
        };

        self.close_entry(replaced);

        let query = url::form_urlencoded::Serializer::new(String::new()).append_pair(SLOT_ID_PARAM, slot_id).append_pair(KEY_PARAM, key).finish();

        format!("ws://{RELAY_HOST}:{}{PLUGIN_WS_PATH}?{query}", self.inner.port.load(Ordering::SeqCst))
    }

    /// A newer launch's entry stays.
    pub fn release_allocation(&self, slot_id: &str, allocation_id: &str) {
        let removed = {
            let mut slots = lock(&self.inner.slots);
            let current = slots.get(slot_id).is_some_and(|connection| lock(connection).allocation_id == allocation_id);

            if current { slots.remove(slot_id) } else { None }
        };

        self.close_entry(removed);
    }

    fn close_entry(&self, removed: Option<Shared>) {
        let Some(connection) = removed else {
            return;
        };
        let producer = lock(&connection).producer.clone();

        if let Some(producer) = producer {
            producer.terminate(Ending::Released);
            Self::unlink(&connection, producer.id, &producer.hooks);
        }

        self.flush();
    }

    fn connection_of(&self, allocation_id: &str) -> Option<Shared> {
        lock(&self.inner.slots).values().find(|connection| lock(connection).allocation_id == allocation_id).cloned()
    }

    /// False when the plugin is not connected.
    #[must_use]
    pub fn command(&self, allocation_id: &str, command: &PluginCommand) -> bool {
        let producer = self.connection_of(allocation_id).and_then(|connection| lock(&connection).producer.clone());

        send_to_plugin(producer.as_ref(), command)
    }

    /// The plugin's answer to a `get_server_status` of the agent's own.
    ///
    /// # Errors
    ///
    /// Refused when the plugin is not connected, leaves or stays silent past `timeout`.
    pub async fn request_status(&self, allocation_id: &str, timeout: Duration) -> Result<ServerState, AgentFailure> {
        let request_id = uuid();
        let (answer, answered) = oneshot::channel();
        let connection = self.open_request(allocation_id, |connection| {
            connection.status_requests.insert(request_id.clone(), answer);
        })?;
        let producer = lock(&connection).producer.clone();

        send_to_plugin(producer.as_ref(), &AgentToPluginMessage::GetServerStatus(PluginGetServerStatus { request_id: Some(request_id.clone()) }));
        await_answer(answered, timeout, || {
            lock(&connection).status_requests.remove(&request_id);
        })
        .await
    }

    /// Resolves once the plugin reports the command's result.
    ///
    /// # Errors
    ///
    /// Refused when the plugin declines, is not connected, leaves or stays silent past `timeout`.
    pub async fn run_command(&self, allocation_id: &str, build: impl FnOnce(String) -> AgentToPluginMessage, timeout: Duration) -> Result<(), AgentFailure> {
        let request_id = uuid();
        let (answer, answered) = oneshot::channel();
        let connection = self.open_request(allocation_id, |connection| {
            connection.command_requests.insert(request_id.clone(), answer);
        })?;
        let producer = lock(&connection).producer.clone();

        send_to_plugin(producer.as_ref(), &build(request_id.clone()));
        await_answer(answered, timeout, || {
            lock(&connection).command_requests.remove(&request_id);
        })
        .await
    }

    fn open_request(&self, allocation_id: &str, register: impl FnOnce(&mut Connection)) -> Result<Shared, AgentFailure> {
        let connection = self.connection_of(allocation_id).ok_or_else(|| AgentFailure::code(AgentErrorCode::PluginOffline))?;

        {
            let mut guard = lock(&connection);

            if guard.producer.is_none() {
                return Err(AgentFailure::code(AgentErrorCode::PluginOffline));
            }

            register(&mut guard);
        }

        Ok(connection)
    }

    /// These plugins can still replay what the spool holds.
    #[must_use]
    pub fn attached_allocations(&self) -> Vec<String> {
        lock(&self.inner.slots)
            .values()
            .filter_map(|connection| {
                let connection = lock(connection);

                connection.producer.as_ref().map(|_| connection.allocation_id.clone())
            })
            .collect()
    }

    /// Ends every plugin socket and waits for received history to be stored.
    pub async fn close(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);

        let connections: Vec<Shared> = lock(&self.inner.slots).values().cloned().collect();

        for connection in connections {
            // Taken out of the hold first, as terminating sends to the plugin's writer.
            let producer = lock(&connection).producer.clone();

            if let Some(producer) = producer {
                producer.terminate(Ending::Shutdown);
            }
        }

        self.inner.flush_timer.store(false, Ordering::SeqCst);

        {
            // After any commit under way, whatever is still queued, before the spool closes.
            let _turn = self.inner.commits.lock().await;

            if !lock(&self.inner.queued).is_empty() {
                self.commit_queued().await;
            }
        }

        self.inner.closed.cancel();
    }

    /// Checked and cleared in one hold, so a newer socket that took its place meanwhile stays linked.
    fn unlink(connection: &Shared, producer_id: u64, hooks: &RelayHooks) {
        let allocation_id = {
            let mut guard = lock(connection);

            if guard.producer.as_ref().is_none_or(|producer| producer.id != producer_id) {
                return;
            }

            guard.producer = None;
            fail_requests(&mut guard);
            guard.allocation_id.clone()
        };

        (hooks.on_plugin_online)(&allocation_id, false);
    }

    fn attach_producer(&self, connection: &Shared, socket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, hooks: RelayHooks) {
        let (mut sink, mut stream) = socket.split();
        let (sender, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
        let stop = Arc::new(Notify::new());
        let producer = Producer { id: self.inner.next_producer.fetch_add(1, Ordering::SeqCst), sender, stop: Arc::clone(&stop), hooks: hooks.clone() };
        let (slot_id, allocation_id) = {
            let mut guard = lock(connection);

            if let Some(previous) = guard.producer.take() {
                previous.terminate(Ending::Replaced);
            }

            fail_requests(&mut guard);
            guard.producer = Some(producer.clone());

            if guard.pending_frames == 0 {
                guard.replay_required = false;
            }

            (guard.slot_id.clone(), guard.allocation_id.clone())
        };

        (hooks.on_plugin_online)(&allocation_id, true);

        match &self.inner.spool {
            None => self
                .inner
                .log
                .warn(format!("slot {slot_id}: the event spool is closed; the plugin keeps its events until an agent with an open spool acknowledges them")),
            Some(spool) => {
                // How far the server's events are stored, so a reconnecting plugin resends only the rest.
                let spool = Arc::clone(spool);
                let relay = self.clone();
                let producer = producer.clone();
                let allocation = allocation_id.clone();

                tokio::spawn(async move {
                    let _turn = relay.inner.commits.lock().await;

                    match spool.position(allocation).await {
                        Ok(Some(ack)) => {
                            send_to_plugin(Some(&producer), &AgentToPluginMessage::Ack(EventAck { to_seq: ack.to_seq }));
                        }
                        Ok(None) => {}
                        Err(error) => relay.inner.log.error(format!("local event batch failed: {error}")),
                    }
                });
            }
        }

        tokio::spawn(async move {
            while let Some(outgoing) = outgoing.recv().await {
                let message = match outgoing {
                    Outgoing::Text(text) => Message::Text(text.into()),
                    Outgoing::Close(ending) => Message::Close(Some(ending.frame())),
                };
                let closing = matches!(message, Message::Close(_));

                if sink.send(message).await.is_err() || closing {
                    break;
                }
            }

            let _ = sink.close().await;
        });

        let relay = self.clone();
        let connection = Arc::clone(connection);
        let producer_id = producer.id;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = stop.notified() => break,
                    frame = stream.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            let current = lock(&connection).producer.as_ref().is_some_and(|producer| producer.id == producer_id);

                            if current && !relay.inner.stopped.load(Ordering::SeqCst) {
                                relay.incoming(&connection, producer_id, &text, &hooks);
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {
                            relay.inner.log.warn(format!("slot {}: ended the plugin's socket: it sent a binary frame, which the protocol does not use", lock(&connection).slot_id));
                            break;
                        }
                        // What a misbehaving plugin causes, as a frame over the limit. A server that
                        // exits or is killed resets its socket without a close, which needs no line.
                        Some(Err(error @ (WsError::Capacity(_) | WsError::Protocol(_) | WsError::Utf8(_))))
                            if !matches!(error, WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake)) =>
                        {
                            relay.inner.log.warn(format!("slot {}: ended the plugin's socket: {error}", lock(&connection).slot_id));
                            break;
                        }
                        Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                }
            }

            Self::unlink(&connection, producer_id, &hooks);
        });
    }

    fn incoming(&self, connection: &Shared, producer_id: u64, raw: &str, hooks: &RelayHooks) {
        let slot_id = lock(connection).slot_id.clone();
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return self.drop_frame(&slot_id, raw, "not JSON");
        };

        // The relay reads an event's position alone; the controller validates the event. The key already
        // proved which server sent it, and the frame is stored as uploaded, without its type.
        if value.get("type").and_then(Value::as_str) == Some("event") {
            match safe_parse(s_event_frame, value.clone()).ok().and_then(|frame| frame.get("seq").and_then(Value::as_i64)) {
                None => self.drop_frame(&slot_id, raw, "no seq"),
                Some(seq) => {
                    let mut stored = value;

                    if let Some(fields) = stored.as_object_mut() {
                        fields.shift_remove("type");
                    }

                    let raw = stored.to_string();

                    self.queue_frame(connection, producer_id, SpooledFrame { seq, raw });
                }
            }

            return;
        }

        // A newer plugin may send a message this agent does not have yet.
        if let Some(kind) = value.get("type").and_then(Value::as_str)
            && !PluginToAgentMessage::TAGS.contains(&kind)
        {
            if let Some(kind) = lock(&self.inner.skipped).first_sighting(kind) {
                self.inner.log.info(format!("slot {slot_id}: skipping {kind} frames, which this agent does not know"));
            }

            return;
        }

        let Some(message) =
            safe_parse(s_plugin_to_agent_message, value.clone()).ok().and_then(|parsed| serde_json::from_value::<PluginToAgentMessage>(parsed).ok())
        else {
            return self.drop_frame(&slot_id, raw, "an unreadable message");
        };

        match &message {
            PluginToAgentMessage::ServerStatus(status) => {
                if let Some(request_id) = &status.request_id {
                    let request = lock(connection).status_requests.remove(request_id);

                    if let Some(request) = request {
                        // The message passed the contract, and a server_status is the server's state
                        // with a type and a request id, so the state needs no check of its own.
                        let state = serde_json::from_value::<ServerState>(value).ok();

                        let _ = request.send(state.ok_or_else(|| AgentFailure::code(AgentErrorCode::Failed)));
                    }
                }
            }
            PluginToAgentMessage::CommandResult(result) => {
                let request = lock(connection).command_requests.remove(&result.request_id);

                if let Some(request) = request {
                    let outcome = match result.error {
                        None => Ok(()),
                        Some(refusal) => {
                            let code = serde_json::to_value(refusal).and_then(serde_json::from_value::<AgentErrorCode>).unwrap_or(AgentErrorCode::Failed);

                            Err(AgentFailure::code(code))
                        }
                    };

                    let _ = request.send(outcome);
                }
            }
            _ => {}
        }

        let allocation_id = lock(connection).allocation_id.clone();

        (hooks.on_plugin_message)(&allocation_id, message);
    }

    /// Without a position a frame has no place in its server's stream, so the controller could never get it.
    fn drop_frame(&self, slot_id: &str, raw: &str, reason: &str) {
        self.inner.log.warn(format!("slot {slot_id}: dropped a producer frame with {reason}: {}", crate::failure::truncate(raw, 200)));
    }

    fn queue_frame(&self, connection: &Shared, producer_id: u64, frame: SpooledFrame) {
        let flush_now = {
            let mut guard = lock(connection);

            // The reader checked its socket was current in another hold; a newer socket's replay
            // may have started since, and this frame would land after it.
            if guard.producer.as_ref().is_none_or(|producer| producer.id != producer_id) {
                return;
            }

            // After a missed frame, history is taken again only on replay; live control keeps flowing.
            if guard.replay_required || self.inner.spool.is_none() {
                return;
            }

            let bytes = frame.raw.len();

            if guard.pending_bytes + bytes > usize::try_from(RELAY_QUEUE_MAX_BYTES).unwrap_or(usize::MAX)
                || guard.pending_frames >= usize::try_from(RELAY_QUEUE_MAX_FRAMES).unwrap_or(usize::MAX)
            {
                guard.replay_required = true;
                self.inner.log.warn(format!("slot {}: history queue full; replay will resume after pending commits finish", guard.slot_id));
                return;
            }

            guard.queued_frames.push(frame);
            guard.pending_bytes += bytes;
            guard.pending_frames += 1;
            guard.queued_frames.len() >= usize::try_from(EVENT_BATCH_MAX_FRAMES).unwrap_or(usize::MAX)
        };

        {
            let mut queued = lock(&self.inner.queued);

            if !queued.iter().any(|other| Arc::ptr_eq(other, connection)) {
                queued.push(Arc::clone(connection));
            }
        }

        if flush_now {
            self.flush();
        } else if !self.inner.flush_timer.swap(true, Ordering::SeqCst) {
            let relay = self.clone();

            tokio::spawn(async move {
                tokio::time::sleep(LOCAL_FLUSH_INTERVAL).await;
                relay.flush();
            });
        }
    }

    fn flush(&self) {
        self.inner.flush_timer.store(false, Ordering::SeqCst);

        if lock(&self.inner.queued).is_empty() || self.inner.flush_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }

        let relay = self.clone();

        tokio::spawn(async move {
            let _turn = relay.inner.commits.lock().await;

            relay.commit_queued().await;
        });
    }

    /// Every slot's queued frames in one commit, so the disk syncs once however many games run.
    async fn commit_queued(&self) {
        self.inner.flush_scheduled.store(false, Ordering::SeqCst);

        let Some(spool) = self.inner.spool.clone() else {
            return;
        };
        let batches: Vec<(Shared, Vec<SpooledFrame>)> = std::mem::take(&mut *lock(&self.inner.queued))
            .into_iter()
            .map(|connection| {
                let frames = std::mem::take(&mut lock(&connection).queued_frames);

                (connection, frames)
            })
            .collect();
        let owners: Vec<&Shared> = batches.iter().filter(|(_, frames)| !frames.is_empty()).map(|(connection, _)| connection).collect();
        let streams: Vec<SpoolStream> = batches
            .iter()
            .filter(|(_, frames)| !frames.is_empty())
            .map(|(connection, frames)| SpoolStream { allocation_id: lock(connection).allocation_id.clone(), frames: frames.clone() })
            .collect();
        // Nothing to store needs no commit; the accounting below still runs for every connection.
        let results = if streams.is_empty() {
            Vec::new()
        } else {
            match spool.append(streams.clone()).await {
                Ok(results) => results,
                Err(error) => streams.iter().map(|_| Err(error.to_string())).collect(),
            }
        };

        // A broken stream (a gap) costs its plugin the socket, and it replays from its ack.
        for (connection, result) in owners.iter().zip(results) {
            let mut guard = lock(connection);

            match result {
                Err(error) => {
                    self.inner.log.warn(format!("slot {}: event commit failed, replay required: {error}", guard.slot_id));
                    guard.replay_required = true;

                    // Queued while this commit ran, so they follow the lost frames. Committed next,
                    // they would start a new stream mid-way and lose its first events. The plugin
                    // replays them with the rest.
                    let after_gap = std::mem::take(&mut guard.queued_frames);

                    guard.pending_frames -= after_gap.len();
                    guard.pending_bytes -= after_gap.iter().map(|frame| frame.raw.len()).sum::<usize>();
                }
                Ok(Some(ack)) => {
                    send_to_plugin(guard.producer.as_ref(), &AgentToPluginMessage::Ack(EventAck { to_seq: ack.to_seq }));
                }
                Ok(None) => {}
            }
        }

        for (connection, frames) in &batches {
            let mut guard = lock(connection);

            guard.pending_frames -= frames.len();
            guard.pending_bytes -= frames.iter().map(|frame| frame.raw.len()).sum::<usize>();

            // The plugin replays unacknowledged history on reconnect. Waiting for the backlog first
            // keeps a blocked disk from causing a reconnect loop that also disrupts live control.
            if guard.replay_required
                && guard.pending_frames == 0
                && let Some(producer) = &guard.producer
            {
                producer.terminate(Ending::Replay);
            }
        }
    }
}

// An event is stored as its frame without the frame's `type`, so it is smaller than the frame,
// which the socket caps at MAX_FRAME_BYTES. That cap keeps every stored event inside one upload
// batch; an event larger than a batch could never be uploaded and would stall its server's events.
const _: () = assert!(MAX_FRAME_BYTES <= EVENT_BATCH_MAX_BYTES, "a frame the relay accepts must fit an upload batch once stored");

fn fail_requests(connection: &mut Connection) {
    for (_, request) in connection.status_requests.drain() {
        let _ = request.send(Err(AgentFailure::code(AgentErrorCode::PluginOffline)));
    }

    for (_, request) in connection.command_requests.drain() {
        let _ = request.send(Err(AgentFailure::code(AgentErrorCode::PluginOffline)));
    }
}

const UPGRADE_WAIT: Duration = Duration::from_secs(1);

/// Peeks without reading, so the handshake still sees the whole request. The caller bounds the wait
/// with [`UPGRADE_WAIT`].
async fn is_upgrade(stream: &tokio::net::TcpStream) -> bool {
    let mut buffer = vec![0; 16 * 1024];

    loop {
        let Ok(read) = stream.peek(&mut buffer).await else {
            return false;
        };
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut headers);

        match request.parse(buffer.get(..read).unwrap_or_default()) {
            Ok(httparse::Status::Complete(_)) => {
                return request.headers.iter().any(|header| header.name.eq_ignore_ascii_case("upgrade") && header.value.eq_ignore_ascii_case(b"websocket"));
            }
            Ok(httparse::Status::Partial) if read > 0 && read < buffer.len() => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            _ => return false,
        }
    }
}

/// False when there is no plugin or its writer is gone. A request ignores the result, because a
/// departed plugin fails its waiting requests as `plugin_offline` once its reader unlinks it. That
/// follows the writer immediately.
fn send_to_plugin(producer: Option<&Producer>, message: &impl serde::Serialize) -> bool {
    let Some(producer) = producer else {
        return false;
    };

    serde_json::to_string(message).is_ok_and(|text| producer.sender.send(Outgoing::Text(text)).is_ok())
}

async fn await_answer<T>(answered: oneshot::Receiver<Result<T, AgentFailure>>, timeout: Duration, forget: impl FnOnce()) -> Result<T, AgentFailure> {
    match tokio::time::timeout(timeout, answered).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(AgentFailure::code(AgentErrorCode::PluginOffline)),
        Err(_) => {
            forget();
            Err(AgentFailure::code(AgentErrorCode::PluginStatusTimeout))
        }
    }
}

/// A random version 4 UUID.
pub(crate) fn uuid() -> String {
    let mut bytes: [u8; 16] = rand::random();

    // The version in the high nibble of byte 6, the RFC 4122 variant in the top bits of byte 8.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let part = |range: std::ops::Range<usize>| bytes.get(range).map(hex::encode).unwrap_or_default();

    format!("{}-{}-{}-{}-{}", part(0..4), part(4..6), part(6..8), part(8..10), part(10..16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn keeps_accepting_after_a_failed_accept_and_logs_once_per_run_of_failures() {
        let warnings: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&warnings);
        let log = Logger::new("relay", Some(Arc::new(move |entry: crate::logger::Entry| lock(&seen).push(entry.message))));
        let closed = CancellationToken::new();
        let answers =
            Mutex::new(vec![Ok(3), Err(std::io::Error::other("too many open files")), Err(std::io::Error::other("too many open files")), Ok(2), Ok(1)]);
        let served: Arc<Mutex<Vec<u8>>> = Arc::default();
        let (serving, stopping) = (Arc::clone(&served), closed.clone());

        accept_until_closed(
            &closed,
            &log,
            || {
                let next = lock(&answers).pop();

                async move {
                    match next {
                        Some(answer) => answer,
                        None => std::future::pending().await,
                    }
                }
            },
            |stream: u8| {
                lock(&serving).push(stream);

                if stream == 3 {
                    stopping.cancel();
                }
            },
        )
        .await;

        assert_eq!(*lock(&served), [1, 2, 3]);
        assert_eq!(*lock(&warnings), ["[relay] the relay could not accept a plugin: too many open files; trying again"]);
    }

    #[test]
    fn keys_are_16_hex_characters_and_differ_per_launch() {
        let key = new_relay_key();

        assert_eq!(key.len(), 16);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(key, new_relay_key());
    }

    #[test]
    fn request_ids_are_v4_uuids() {
        let id = uuid();

        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    }
}
