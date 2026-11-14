//! The event pipeline as the agent starts it: the plugins stay linked whether or not the spool
//! opened, and only a spool that opened acknowledges their events.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::contract::types::{AgentToPluginMessage, EventAck, ExecCommand, PluginCommand, PluginToAgentMessage};
use ankor_agent::event_spool::{SPOOL_SCHEMA_VERSION, SpoolAck, SpoolAppend, SpoolBacklog, SpoolError, SpoolFrame, SpoolStream};
use ankor_agent::event_uploader::{BacklogHook, EventsTarget, TargetFn};
use ankor_agent::events::start_events;
use ankor_agent::logger::Logger;
use ankor_agent::plugin_relay::RelayHooks;
use ankor_agent::spool_client::{SpoolClient, SpoolPort};
use ankor_agent::testing::fixtures::MATCH_ID;
use ankor_agent::testing::{plugin_message, until};
use ankor_protocol::constants::{PROTOCOL_HEADER, PROTOCOL_VERSION};
use ankor_protocol::types::{NodeUnavailableCode, ServerExit};
use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const ALLOCATION: &str = "00000000-0000-4000-8000-000000000001";
/// The key the relay is told a launch's plugin dials with.
const RELAY_KEY: &str = "0123456789abcdef";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A plugin's link, and every message the agent sent down it.
struct Plugin {
    sink: futures_util::stream::SplitSink<Socket, Message>,
    messages: Arc<Mutex<Vec<Value>>>,
    open: Arc<Mutex<bool>>,
}

impl Plugin {
    async fn connect(url: &str) -> Self {
        let mut request = url.into_client_request().unwrap();

        request.headers_mut().insert(PROTOCOL_HEADER, PROTOCOL_VERSION.parse().unwrap());

        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let (sink, mut stream) = socket.split();
        let messages: Arc<Mutex<Vec<Value>>> = Arc::default();
        let open = Arc::new(Mutex::new(true));
        let (seen, still_open) = (Arc::clone(&messages), Arc::clone(&open));

        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                if let Message::Text(text) = message {
                    seen.lock().unwrap().push(serde_json::from_str(&text).unwrap());
                }
            }

            *still_open.lock().unwrap() = false;
        });

        Self { sink, messages, open }
    }

    async fn send(&mut self, message: &Value) {
        self.sink.send(Message::Text(message.to_string().into())).await.unwrap();
    }

    /// The messages the relay sent whose message `matches`.
    fn received_where(&self, matches: impl Fn(&AgentToPluginMessage) -> bool) -> Vec<Value> {
        self.messages.lock().unwrap().iter().filter(|frame| plugin_message(frame).is_some_and(|message| matches(&message))).cloned().collect()
    }
}

fn status() -> Value {
    json!({ "type": "server_status", "request_id": null, "map_name": "de_mirage", "match": null, "players": [], "config_error": null, "ready": true, "server_steam_id": null })
}

fn event(seq: i64) -> Value {
    json!({ "type": "event", "seq": seq, "event": { "type": "log", "match_id": MATCH_ID, "tick": 640, "timestamp": 1000, "level": "info", "message": format!("event-{seq}") } })
}

fn unreachable_controller(asked: Arc<AtomicUsize>) -> TargetFn {
    Arc::new(move || {
        asked.fetch_add(1, Ordering::SeqCst);
        EventsTarget { host: "127.0.0.1:1".to_owned(), tls: false, events_path: "/api/node/events".to_owned() }
    })
}

fn ignore_backlog() -> BacklogHook {
    Arc::new(|_| {})
}

#[tokio::test]
async fn keeps_the_plugins_linked_when_the_spool_will_not_open_holding_back_acks_so_a_plugin_keeps_its_events() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("streams.sqlite");

    rusqlite::Connection::open(&file).unwrap().execute_batch(&format!("PRAGMA user_version = {};", SPOOL_SCHEMA_VERSION + 1)).unwrap();

    let links: Arc<Mutex<Vec<bool>>> = Arc::default();
    let received: Arc<Mutex<Vec<PluginToAgentMessage>>> = Arc::default();
    let asked = Arc::new(AtomicUsize::new(0));
    let (seen_links, seen) = (Arc::clone(&links), Arc::clone(&received));
    let hooks = RelayHooks {
        on_plugin_online: Arc::new(move |_, connected| seen_links.lock().unwrap().push(connected)),
        on_plugin_message: Arc::new(move |_, message| seen.lock().unwrap().push(message)),
    };
    let spool: Arc<dyn SpoolPort> = Arc::new(SpoolClient::start(file, Logger::null()));
    let (events, listener) =
        start_events(spool, 0, "agent-secret", unreachable_controller(Arc::clone(&asked)), ignore_backlog(), &Logger::null()).await.unwrap();

    listener.serve(hooks);

    assert_eq!(events.unavailable.as_ref().map(|unavailable| unavailable.code), Some(NodeUnavailableCode::EventStorage));
    // No upload loop runs over a spool that never opened: only making the uploader read the address.
    assert_eq!(asked.load(Ordering::SeqCst), 1);

    let url = events.relay.prepare("a", ALLOCATION, RELAY_KEY);
    let mut first = Plugin::connect(&url).await;

    first.send(&status()).await;
    until("the status at the agent", || received.lock().unwrap().len() == 1).await;

    for seq in 0..3 {
        first.send(&event(seq)).await;
    }

    assert!(events.relay.command(ALLOCATION, &PluginCommand::Exec(ExecCommand { command: "status".to_owned(), request_id: "r1".to_owned() })));
    until("the command at the plugin", || !first.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_))).is_empty()).await;
    // Past the relay's local flush, when a commit that failed would drop the producer.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(*first.open.lock().unwrap());
    assert!(first.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty());

    first.sink.close().await.unwrap();
    until("the plugin link ending", || links.lock().unwrap().last() == Some(&false)).await;

    let mut second = Plugin::connect(&url).await;

    second.send(&status()).await;
    until("the status after a reconnect", || received.lock().unwrap().len() == 2).await;
    assert_eq!(*links.lock().unwrap(), [true, false, true]);
    assert!(second.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty());
    let _ = events.close().await;
}

#[tokio::test]
async fn acknowledges_what_the_open_spool_committed_and_closes_the_spool_with_the_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let spool = Arc::new(SpoolClient::start(dir.path().join("streams.sqlite"), Logger::null()));
    let hooks = RelayHooks { on_plugin_online: Arc::new(|_, _| {}), on_plugin_message: Arc::new(|_, _| {}) };
    let (events, listener) =
        start_events(Arc::clone(&spool) as Arc<dyn SpoolPort>, 0, "agent-secret", unreachable_controller(Arc::default()), ignore_backlog(), &Logger::null())
            .await
            .unwrap();

    listener.serve(hooks);

    assert_eq!(events.unavailable, None);

    let mut linked = Plugin::connect(&events.relay.prepare("a", ALLOCATION, RELAY_KEY)).await;

    linked.send(&event(0)).await;
    until("the ack", || !linked.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty()).await;
    assert_eq!(
        linked.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).first(),
        Some(&json!(AgentToPluginMessage::Ack(EventAck { to_seq: 0 })))
    );
    assert_eq!(spool.pending(ALLOCATION.to_owned()).await.unwrap().len(), 1);

    let _ = events.close().await;
    assert!(spool.version().await.is_err());
}

/// The real spool, except that its file does not close.
struct FailingClose(Arc<SpoolClient>);

#[async_trait]
impl SpoolPort for FailingClose {
    async fn version(&self) -> Result<u32, SpoolError> {
        self.0.version().await
    }

    async fn position(&self, allocation_id: String) -> Result<Option<SpoolAck>, SpoolError> {
        self.0.position(allocation_id).await
    }

    async fn append(&self, streams: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>, SpoolError> {
        self.0.append(streams).await
    }

    async fn pending(&self, allocation_id: String) -> Result<Vec<SpoolFrame>, SpoolError> {
        self.0.pending(allocation_id).await
    }

    async fn backlog(&self) -> Result<Vec<SpoolBacklog>, SpoolError> {
        self.0.backlog().await
    }

    async fn acknowledge(&self, allocation_id: String, to_seq: i64) -> Result<(), SpoolError> {
        self.0.acknowledge(allocation_id, to_seq).await
    }

    async fn prune(&self, active: Vec<String>) -> Result<Vec<SpoolBacklog>, SpoolError> {
        self.0.prune(active).await
    }

    async fn record_exit(&self, exit: ServerExit) -> Result<(), SpoolError> {
        self.0.record_exit(exit).await
    }

    async fn exits(&self) -> Result<Vec<ServerExit>, SpoolError> {
        self.0.exits().await
    }

    async fn forget_exits(&self, allocation_ids: Vec<String>) -> Result<(), SpoolError> {
        self.0.forget_exits(allocation_ids).await
    }

    async fn allocation_used(&self, allocation_id: String) -> Result<bool, SpoolError> {
        self.0.allocation_used(allocation_id).await
    }

    async fn record_allocation(&self, allocation_id: String) -> Result<(), SpoolError> {
        self.0.record_allocation(allocation_id).await
    }

    async fn close(&self) -> Result<(), SpoolError> {
        let _ = self.0.close().await;

        Err(SpoolError::Unavailable("disk I/O error".to_owned()))
    }
}

#[tokio::test]
async fn reports_a_spool_that_did_not_close() {
    let dir = tempfile::tempdir().unwrap();
    let spool = Arc::new(SpoolClient::start(dir.path().join("streams.sqlite"), Logger::null()));
    let (events, listener) =
        start_events(Arc::new(FailingClose(spool)), 0, "agent-secret", unreachable_controller(Arc::default()), ignore_backlog(), &Logger::null())
            .await
            .unwrap();

    listener.serve(RelayHooks { on_plugin_online: Arc::new(|_, _| {}), on_plugin_message: Arc::new(|_, _| {}) });
    assert_eq!(events.unavailable, None);
    assert_eq!(events.close().await.unwrap_err().to_string(), "disk I/O error");
}
