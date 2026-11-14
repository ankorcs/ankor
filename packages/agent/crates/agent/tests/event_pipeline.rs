//! The whole event path against real sockets: plugins into the relay, the relay into a real spool,
//! the uploader out of it to a controller that answers, refuses, drops or never answers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ankor_agent::contract::types::{AgentToPluginMessage, EventAck, ExecCommand, PluginCommand, PluginGetServerStatus, PluginStartMatch, PluginToAgentMessage};
use ankor_agent::event_spool::{SpoolAck, SpoolAppend, SpoolBacklog, SpoolError, SpoolFrame, SpoolStream, SpooledFrame};
use ankor_agent::event_uploader::{EventUploader, EventsTarget, HttpPost, UploaderParts, nothing_stuck};
use ankor_agent::logger::Logger;
use ankor_agent::plugin_relay::{PluginRelay, RelayHooks};
use ankor_agent::spool_client::{SpoolClient, SpoolPort};
use ankor_agent::testing::fixtures::MATCH_ID;
use ankor_agent::testing::{plugin_message, until};
use ankor_protocol::constants::{EVENT_BATCH_MAX_FRAMES, PROTOCOL_HEADER, PROTOCOL_VERSION};
use ankor_protocol::helpers::{is_compatible_protocol, node_authorization};
use ankor_protocol::types::{AgentErrorCode, EventBacklog, ServerExit};
use async_trait::async_trait;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const ALLOCATION: &str = "00000000-0000-4000-8000-000000000001";
const OTHER_ALLOCATION: &str = "00000000-0000-4000-8000-000000000002";
const TOKEN: &str = "agent-secret";
/// The key the relay hands each test plugin.
const RELAY_KEY: &str = "0123456789abcdef";

fn frame(seq: i64) -> Value {
    json!({ "seq": seq, "event": { "type": "log", "match_id": MATCH_ID, "tick": 640, "timestamp": 1000, "level": "info", "message": format!("event-{seq}") } })
}

fn status() -> Value {
    json!({ "type": "server_status", "request_id": null, "map_name": "de_mirage", "match": null, "players": [], "config_error": null, "ready": true, "server_steam_id": null })
}

fn exec(command: &str) -> PluginCommand {
    PluginCommand::Exec(ExecCommand { command: command.to_owned(), request_id: "r1".to_owned() })
}

/// A latch a test opens; everything waiting on it continues.
#[derive(Clone)]
struct Gate(Arc<watch::Sender<bool>>);

impl Gate {
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    fn release(&self) {
        self.0.send_replace(true);
    }

    async fn wait(&self) {
        let _ = self.0.subscribe().wait_for(|open| *open).await;
    }
}

/// How the controller answers a batch.
enum Reply {
    Answer(u16, String),
    /// Ends the connection without an answer.
    Drop,
    /// Never answers.
    Hang,
}

fn receipt() -> Reply {
    Reply::Answer(200, r#"{"rejected":[]}"#.to_owned())
}

fn unavailable() -> Reply {
    Reply::Answer(503, String::new())
}

type Respond = Box<dyn Fn(&Value) -> Reply + Send + Sync>;

/// A controller over plain HTTP/1.1, so a test decides what each batch's connection sees.
struct Controller {
    host: String,
    path: String,
    batches: Mutex<Vec<Value>>,
    respond: Mutex<Respond>,
    /// Requests that did not come the way an agent sends them.
    wrong: Mutex<Vec<String>>,
}

impl Controller {
    async fn start(path: &str) -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let controller = Arc::new(Self {
            host: listener.local_addr().unwrap().to_string(),
            path: path.to_owned(),
            batches: Mutex::default(),
            respond: Mutex::new(Box::new(|_| receipt())),
            wrong: Mutex::default(),
        });
        let serving = Arc::clone(&controller);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(Arc::clone(&serving).serve(stream));
            }
        });
        controller
    }

    async fn serve(self: Arc<Self>, stream: TcpStream) {
        let mut reader = BufReader::new(stream);

        loop {
            let mut line = String::new();

            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }

            let target = line.split(' ').nth(1).unwrap_or_default().to_owned();
            let mut headers = Vec::new();

            loop {
                let mut header = String::new();

                reader.read_line(&mut header).await.unwrap();

                let header = header.trim_end();

                if header.is_empty() {
                    break;
                }

                let (name, value) = header.split_once(':').unwrap();

                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }

            let header = |name: &str| headers.iter().find(|(each, _)| each == name).map(|(_, value)| value.clone());
            let length: usize = header("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
            let mut body = vec![0; length];

            reader.read_exact(&mut body).await.unwrap();

            if target != self.path
                || header("authorization") != Some(node_authorization("agent-secret"))
                || !is_compatible_protocol(header(&PROTOCOL_HEADER.to_ascii_lowercase()).as_deref())
            {
                self.wrong.lock().unwrap().push(format!("{target} {headers:?}"));
            }

            let batch: Value = serde_json::from_slice(&body).unwrap();

            self.batches.lock().unwrap().push(batch.clone());

            let reply = (self.respond.lock().unwrap())(&batch);

            match reply {
                Reply::Answer(status, body) => {
                    let response = format!("HTTP/1.1 {status} Answer\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());

                    if reader.get_mut().write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
                Reply::Drop => return,
                Reply::Hang => std::future::pending::<()>().await,
            }
        }
    }

    fn set_respond(&self, respond: impl Fn(&Value) -> Reply + Send + Sync + 'static) {
        *self.respond.lock().unwrap() = Box::new(respond);
    }

    fn batches(&self) -> Vec<Value> {
        assert_eq!(*self.wrong.lock().unwrap(), Vec::<String>::new());
        self.batches.lock().unwrap().clone()
    }

    fn frames(&self) -> Vec<Value> {
        self.batches().iter().flat_map(|batch| batch["frames"].as_array().unwrap().clone()).collect()
    }

    fn seqs(&self) -> Vec<i64> {
        self.frames().iter().map(|frame| frame["seq"].as_i64().unwrap()).collect()
    }
}

type Fut<T> = Pin<Box<dyn Future<Output = Result<T, SpoolError>> + Send>>;
type Hook<A, R> = Arc<dyn Fn(Arc<SpoolClient>, A) -> Fut<R> + Send + Sync>;

/// The real spool, with any of its calls replaced by a test's.
#[derive(Default)]
struct Hooks {
    append: Option<Hook<Vec<SpoolStream>, Vec<SpoolAppend>>>,
    pending: Option<Hook<String, Vec<SpoolFrame>>>,
    backlog: Option<Hook<(), Vec<SpoolBacklog>>>,
    prune: Option<Hook<Vec<String>, Vec<SpoolBacklog>>>,
}

struct Wrapped {
    inner: Arc<SpoolClient>,
    hooks: Hooks,
}

#[async_trait]
impl SpoolPort for Wrapped {
    async fn version(&self) -> Result<u32, SpoolError> {
        self.inner.version().await
    }

    async fn position(&self, allocation_id: String) -> Result<Option<SpoolAck>, SpoolError> {
        self.inner.position(allocation_id).await
    }

    async fn append(&self, streams: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>, SpoolError> {
        match &self.hooks.append {
            Some(hook) => hook(Arc::clone(&self.inner), streams).await,
            None => self.inner.append(streams).await,
        }
    }

    async fn pending(&self, allocation_id: String) -> Result<Vec<SpoolFrame>, SpoolError> {
        match &self.hooks.pending {
            Some(hook) => hook(Arc::clone(&self.inner), allocation_id).await,
            None => self.inner.pending(allocation_id).await,
        }
    }

    async fn backlog(&self) -> Result<Vec<SpoolBacklog>, SpoolError> {
        match &self.hooks.backlog {
            Some(hook) => hook(Arc::clone(&self.inner), ()).await,
            None => self.inner.backlog().await,
        }
    }

    async fn acknowledge(&self, allocation_id: String, to_seq: i64) -> Result<(), SpoolError> {
        self.inner.acknowledge(allocation_id, to_seq).await
    }

    async fn prune(&self, active: Vec<String>) -> Result<Vec<SpoolBacklog>, SpoolError> {
        match &self.hooks.prune {
            Some(hook) => hook(Arc::clone(&self.inner), active).await,
            None => self.inner.prune(active).await,
        }
    }

    async fn record_exit(&self, exit: ServerExit) -> Result<(), SpoolError> {
        self.inner.record_exit(exit).await
    }

    async fn exits(&self) -> Result<Vec<ServerExit>, SpoolError> {
        self.inner.exits().await
    }

    async fn forget_exits(&self, allocation_ids: Vec<String>) -> Result<(), SpoolError> {
        self.inner.forget_exits(allocation_ids).await
    }

    async fn allocation_used(&self, allocation_id: String) -> Result<bool, SpoolError> {
        self.inner.allocation_used(allocation_id).await
    }

    async fn record_allocation(&self, allocation_id: String) -> Result<(), SpoolError> {
        self.inner.record_allocation(allocation_id).await
    }

    async fn close(&self) -> Result<(), SpoolError> {
        self.inner.close().await
    }
}

/// A spool file in a directory that lives as long as the test.
struct SpoolFile {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn spool_file() -> SpoolFile {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool.sqlite");

    SpoolFile { _dir: dir, path }
}

/// What the relay told the agent: links made and ended, statuses, and match progress.
#[derive(Default)]
struct Heard {
    links: Mutex<Vec<bool>>,
    statuses: Mutex<Vec<Value>>,
    progress: Mutex<Vec<Value>>,
}

impl Heard {
    fn links(&self) -> Vec<bool> {
        self.links.lock().unwrap().clone()
    }

    fn connected(&self) -> usize {
        self.links().into_iter().filter(|link| *link).count()
    }

    fn statuses(&self) -> Vec<Value> {
        self.statuses.lock().unwrap().clone()
    }
}

struct Pipeline {
    relay: PluginRelay,
    uploader: EventUploader,
    spool: Arc<SpoolClient>,
    port: Arc<dyn SpoolPort>,
    local: String,
    heard: Arc<Heard>,
    target: Arc<Mutex<EventsTarget>>,
}

impl Pipeline {
    async fn start(controller: &Controller, file: &Path, hooks: Hooks, reports: Option<Arc<Mutex<Vec<EventBacklog>>>>) -> Self {
        let spool = Arc::new(SpoolClient::start(file.to_owned(), Logger::null()));
        let port: Arc<dyn SpoolPort> = Arc::new(Wrapped { inner: Arc::clone(&spool), hooks });
        let heard = Arc::new(Heard::default());
        let (linked, told) = (Arc::clone(&heard), Arc::clone(&heard));
        let relay_hooks = RelayHooks {
            on_plugin_online: Arc::new(move |_, connected| linked.links.lock().unwrap().push(connected)),
            on_plugin_message: Arc::new(move |_, message| {
                let frame = serde_json::to_value(&message).unwrap();

                match message {
                    PluginToAgentMessage::ServerStatus(_) => told.statuses.lock().unwrap().push(frame),
                    PluginToAgentMessage::PhaseChange(_) | PluginToAgentMessage::Score(_) => told.progress.lock().unwrap().push(frame),
                    _ => {}
                }
            }),
        };
        let relay = PluginRelay::new(Some(Arc::clone(&port)), Logger::null());
        let target = Arc::new(Mutex::new(EventsTarget { host: controller.host.clone(), tls: false, events_path: controller.path.clone() }));
        let (reading, attached) = (Arc::clone(&target), relay.clone());
        let uploader = EventUploader::from_parts(UploaderParts {
            spool: Arc::clone(&port),
            token: TOKEN.to_owned(),
            target: Arc::new(move || reading.lock().unwrap().clone()),
            attached: Arc::new(move || attached.attached_allocations()),
            on_backlog: Arc::new(move |report| {
                if let Some(reports) = &reports {
                    reports.lock().unwrap().push(report);
                }
            }),
            log: Logger::null(),
            post: Arc::new(HttpPost::new()),
            random: Arc::new(|| 0.5),
        });

        uploader.start().await;
        relay.bind(0).await.unwrap().serve(relay_hooks);

        let local = relay.prepare("a", ALLOCATION, RELAY_KEY);

        Self { relay, uploader, spool, port, local, heard, target }
    }

    async fn plain(controller: &Controller, file: &Path) -> Self {
        Self::start(controller, file, Hooks::default(), None).await
    }

    /// A new target, as `set_config` leaves one: the uploader must notice the change itself.
    fn move_to(&self, controller: &Controller) {
        *self.target.lock().unwrap() = EventsTarget { host: controller.host.clone(), tls: false, events_path: controller.path.clone() };
    }

    async fn close(&self) {
        self.relay.close().await;
        self.uploader.close().await;
        let _ = self.port.close().await;
    }

    async fn pending(&self, allocation: &str) -> usize {
        self.spool.pending(allocation.to_owned()).await.unwrap().len()
    }

    async fn until_drained(&self, allocation: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);

        while self.pending(allocation).await > 0 {
            assert!(Instant::now() < deadline, "timed out waiting for the receipted events to leave the spool");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A plugin's link, and every message the agent sent down it.
struct Plugin {
    sink: SplitSink<Socket, Message>,
    messages: Arc<Mutex<Vec<Value>>>,
    open: Arc<AtomicBool>,
    /// The code and reason of the close frame the relay ended the socket with.
    closed_with: Arc<Mutex<Option<(u16, String)>>>,
}

async fn dial(url: &str, protocol: bool) -> Result<Socket, u16> {
    let mut request = url.into_client_request().unwrap();

    if protocol {
        request.headers_mut().insert(PROTOCOL_HEADER, PROTOCOL_VERSION.parse().unwrap());
    }

    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(error) => panic!("{error}"),
    }
}

impl Plugin {
    async fn connect(url: &str) -> Self {
        let (sink, mut stream) = dial(url, true).await.unwrap().split();
        let messages: Arc<Mutex<Vec<Value>>> = Arc::default();
        let open = Arc::new(AtomicBool::new(true));
        let closed_with: Arc<Mutex<Option<(u16, String)>>> = Arc::default();
        let (seen, still_open, closing) = (Arc::clone(&messages), Arc::clone(&open), Arc::clone(&closed_with));

        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                match message {
                    Message::Text(text) => seen.lock().unwrap().push(serde_json::from_str(&text).unwrap()),
                    Message::Close(Some(frame)) => *closing.lock().unwrap() = Some((u16::from(frame.code), frame.reason.to_string())),
                    _ => {}
                }
            }

            still_open.store(false, Ordering::SeqCst);
        });

        Self { sink, messages, open, closed_with }
    }

    async fn send(&mut self, frame: &Value) {
        let mut message = json!({ "type": "event" });

        message.as_object_mut().unwrap().extend(frame.as_object().unwrap().clone());
        self.send_raw(&message.to_string()).await;
    }

    async fn send_raw(&mut self, text: &str) {
        let _ = self.sink.send(Message::Text(text.to_owned().into())).await;
    }

    async fn close(&mut self) {
        let _ = self.sink.close().await;
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    fn closed_with(&self) -> Option<(u16, String)> {
        self.closed_with.lock().unwrap().clone()
    }

    async fn until_closed(&self) {
        until("the plugin socket to close", || !self.is_open()).await;
    }

    fn received_where(&self, matches: impl Fn(&AgentToPluginMessage) -> bool) -> Vec<Value> {
        self.messages.lock().unwrap().iter().filter(|frame| plugin_message(frame).is_some_and(|message| matches(&message))).cloned().collect()
    }

    fn acked(&self, to_seq: i64) -> bool {
        self.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).iter().any(|ack| ack["to_seq"] == to_seq)
    }

    async fn until_acked(&self, to_seq: i64) {
        until(&format!("an acknowledgement through {to_seq}"), || self.acked(to_seq)).await;
    }
}

fn gated_append(gate: &Gate, committing: &Arc<AtomicBool>) -> Hooks {
    let (gate, committing) = (gate.clone(), Arc::clone(committing));

    Hooks {
        append: Some(Arc::new(move |spool, streams| {
            let (gate, committing) = (gate.clone(), Arc::clone(&committing));

            Box::pin(async move {
                committing.store(true, Ordering::SeqCst);
                gate.wait().await;
                spool.append(streams).await
            })
        })),
        ..Hooks::default()
    }
}

#[tokio::test]
async fn acknowledges_only_after_the_local_sqlite_commit_without_waiting_for_http_delivery() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let (file, gate, committing) = (spool_file(), Gate::new(), Arc::new(AtomicBool::new(false)));
    let pipeline = Pipeline::start(&controller, &file.path, gated_append(&gate, &committing), None).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    until("the spool append to reach the commit gate", || committing.load(Ordering::SeqCst)).await;
    assert!(producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty());
    gate.release();
    producer.until_acked(0).await;
    until("the controller to receive an upload", || !controller.batches().is_empty()).await;
    assert_eq!(pipeline.pending(ALLOCATION).await, 1);
    pipeline.close().await;
}

#[tokio::test]
async fn commits_every_games_queued_frames_in_one_spool_transaction_and_acknowledges_each_producer() {
    let controller = Controller::start("/api/node/events").await;
    let commits: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let recorded = Arc::clone(&commits);
    let hooks = Hooks {
        append: Some(Arc::new(move |spool, streams: Vec<SpoolStream>| {
            recorded.lock().unwrap().push(streams.iter().map(|stream| stream.allocation_id.clone()).collect());
            Box::pin(async move { spool.append(streams).await })
        })),
        ..Hooks::default()
    };
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, hooks, None).await;
    let mut first = Plugin::connect(&pipeline.local).await;
    let mut second = Plugin::connect(&pipeline.relay.prepare("b", OTHER_ALLOCATION, RELAY_KEY)).await;

    first.send(&frame(0)).await;
    second.send(&frame(0)).await;
    first.until_acked(0).await;
    second.until_acked(0).await;
    assert_eq!(*commits.lock().unwrap(), [[ALLOCATION, OTHER_ALLOCATION]]);
    pipeline.close().await;
}

#[tokio::test]
async fn posts_batches_to_the_configured_events_path() {
    let controller = Controller::start("/ankor/node-events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    until("the controller to receive an upload at its own path", || !controller.batches().is_empty()).await;
    assert_eq!(controller.seqs()[0], 0);
    pipeline.close().await;
}

#[tokio::test]
async fn collects_local_commits_into_one_http_upload_per_second_while_acknowledging_each_commit_promptly() {
    let controller = Controller::start("/api/node/events").await;
    let arrivals: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let arrived = Arc::clone(&arrivals);

    controller.set_respond(move |_| {
        arrived.lock().unwrap().push(Instant::now());
        receipt()
    });

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    for seq in 0..4 {
        let sent = Instant::now();

        producer.send(&frame(seq)).await;
        producer.until_acked(seq).await;
        assert!(sent.elapsed() < Duration::from_millis(500), "acknowledged {seq} after {:?}", sent.elapsed());
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    pipeline.until_drained(ALLOCATION).await;
    assert_eq!(controller.seqs(), [0, 1, 2, 3]);

    let arrivals = arrivals.lock().unwrap().clone();

    // Four commits over 1.6 s: fewer uploads than commits, at most one a second.
    assert!(arrivals.len() < 4, "{} uploads", arrivals.len());
    assert!(arrivals.windows(2).all(|pair| pair[1] - pair[0] > Duration::from_millis(900)), "{arrivals:?}");
    pipeline.close().await;
}

#[tokio::test]
async fn drains_a_backlog_in_bounded_requests_during_one_upload_cycle() {
    let controller = Controller::start("/api/node/events").await;
    let arrivals: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let arrived = Arc::clone(&arrivals);

    controller.set_respond(move |_| {
        arrived.lock().unwrap().push(Instant::now());
        receipt()
    });

    // The uploader finds the backlog only once all of it is stored.
    let gate = Gate::new();
    let held = gate.clone();
    let hooks = Hooks {
        backlog: Some(Arc::new(move |spool, ()| {
            let gate = held.clone();

            Box::pin(async move {
                gate.wait().await;
                spool.backlog().await
            })
        })),
        ..Hooks::default()
    };
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, hooks, None).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    let last = i64::try_from(EVENT_BATCH_MAX_FRAMES).unwrap();

    for seq in 0..=last {
        producer.send(&frame(seq)).await;
    }

    producer.until_acked(last).await;
    assert_eq!(controller.batches(), Vec::<Value>::new());
    gate.release();
    until("the controller to receive the second upload", || controller.batches().len() == 2).await;
    assert_eq!(controller.batches().iter().map(|batch| batch["frames"].as_array().unwrap().len()).collect::<Vec<_>>(), [usize::try_from(last).unwrap(), 1]);
    assert_eq!(controller.seqs(), (0..=last).collect::<Vec<_>>());

    let arrivals = arrivals.lock().unwrap().clone();

    assert!(arrivals[1] - arrivals[0] < Duration::from_millis(500), "the second request waited for another cycle");
    pipeline.until_drained(ALLOCATION).await;
    pipeline.close().await;
}

#[tokio::test]
async fn keeps_a_new_commit_scheduled_when_an_earlier_pending_read_returns_an_empty_queue() {
    let controller = Controller::start("/api/node/events").await;
    let gate = Gate::new();
    let held = Arc::new(AtomicBool::new(false));
    // Armed once frame 0 is delivered: the next read answers as if it ran before frame 1's commit.
    let stale_read = Arc::new(AtomicBool::new(false));
    let (holding, stale, waiting) = (Arc::clone(&held), Arc::clone(&stale_read), gate.clone());
    let hooks = Hooks {
        pending: Some(Arc::new(move |spool, allocation| {
            let (gate, holding, stale) = (waiting.clone(), Arc::clone(&holding), Arc::clone(&stale));

            Box::pin(async move {
                if stale.swap(false, Ordering::SeqCst) {
                    holding.store(true, Ordering::SeqCst);
                    gate.wait().await;
                    return Ok(Vec::new());
                }

                spool.pending(allocation).await
            })
        })),
        ..Hooks::default()
    };
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, hooks, None).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.until_acked(0).await;
    until("the controller to receive the first upload", || controller.batches().len() == 1).await;
    stale_read.store(true, Ordering::SeqCst);
    producer.send(&frame(1)).await;
    producer.until_acked(1).await;
    until("the stale pending read to reach its gate", || held.load(Ordering::SeqCst)).await;
    gate.release();
    until("the controller to receive the second upload", || controller.batches().len() == 2).await;
    assert_eq!(controller.batches()[1]["frames"].as_array().unwrap().iter().map(|frame| frame["seq"].clone()).collect::<Vec<_>>(), [1]);
    pipeline.close().await;
}

#[tokio::test]
async fn retries_a_lost_http_response_with_identical_identities_and_deletes_only_after_the_exact_receipt() {
    let controller = Controller::start("/api/node/events").await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);

    controller.set_respond(move |_| if counted.fetch_add(1, Ordering::SeqCst) == 0 { Reply::Drop } else { receipt() });

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    until("the lost-response upload to retry", || attempts.load(Ordering::SeqCst) >= 2).await;
    pipeline.until_drained(ALLOCATION).await;

    let batches = controller.batches();

    assert_eq!(batches[0], batches[1]);
    assert_eq!(batches[0]["frames"][0]["event"]["tick"], 640);
    pipeline.close().await;
}

#[tokio::test]
async fn backs_off_when_queue_discovery_fails_then_delivers_retained_events_when_reads_recover() {
    let controller = Controller::start("/api/node/events").await;
    let reads: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let failing = Arc::new(AtomicBool::new(true));
    let started = Instant::now();
    let (read, fails) = (Arc::clone(&reads), Arc::clone(&failing));
    let hooks = Hooks {
        backlog: Some(Arc::new(move |spool, ()| {
            read.lock().unwrap().push(started.elapsed());

            let fails = fails.load(Ordering::SeqCst);

            Box::pin(async move { if fails { Err(SpoolError::Unavailable("spool temporarily unavailable".to_owned())) } else { spool.backlog().await } })
        })),
        ..Hooks::default()
    };
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, hooks, None).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    // Passes run on the one-second tick, so a retry lands on the first tick past its delay.
    let gap = |from: Duration, to: Duration, min: u64, max: u64| (Duration::from_millis(min)..=Duration::from_millis(max)).contains(&to.saturating_sub(from));

    producer.send(&frame(0)).await;
    producer.until_acked(0).await;
    // Each failure also tries to read the backlog for reporting; that read fails too.
    until("the second failed pass", || reads.lock().unwrap().len() == 4).await;

    let first = reads.lock().unwrap().clone();

    assert!(gap(first[0], first[1], 0, 50) && gap(first[0], first[2], 1_000, 2_100) && gap(first[2], first[3], 0, 50), "{first:?}");
    assert_eq!(pipeline.pending(ALLOCATION).await, 1);
    assert_eq!(controller.batches(), Vec::<Value>::new());

    failing.store(false, Ordering::SeqCst);
    until("the retained event to upload", || controller.batches().len() == 1).await;
    pipeline.until_drained(ALLOCATION).await;

    let all = reads.lock().unwrap().clone();

    // The second failure waits two seconds.
    assert!(gap(all[2], all[4], 2_000, 3_100), "{all:?}");
    assert_eq!(controller.seqs(), [0]);
    pipeline.close().await;
}

#[tokio::test]
async fn tells_the_controller_how_many_events_are_stuck_after_a_failed_upload_and_nothing_once_one_succeeds() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let reports: Arc<Mutex<Vec<EventBacklog>>> = Arc::default();
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, Hooks::default(), Some(Arc::clone(&reports))).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.send(&frame(1)).await;
    until("the failed upload to report its backlog", || reports.lock().unwrap().iter().any(|report| report.event_count == 2)).await;
    assert_eq!(
        serde_json::to_value(reports.lock().unwrap().iter().find(|report| report.event_count == 2).unwrap()).unwrap(),
        json!({ "servers": [{ "allocation_id": ALLOCATION, "pending": 2 }], "server_count": 1, "event_count": 2, "reason": "event backend returned HTTP 503" })
    );

    controller.set_respond(|_| receipt());
    until("the backlog to be reported empty", || reports.lock().unwrap().last().is_some_and(|report| report.server_count == 0)).await;
    assert_eq!(reports.lock().unwrap().last(), Some(&nothing_stuck()));

    let cleared = reports.lock().unwrap().len();

    // Healthy delivery reports nothing stuck.
    producer.send(&frame(2)).await;
    until("the next upload", || controller.seqs().contains(&2)).await;
    pipeline.until_drained(ALLOCATION).await;
    assert!(reports.lock().unwrap()[cleared - 1..].iter().all(|report| *report == nothing_stuck()));
    pipeline.close().await;
}

#[tokio::test]
async fn backs_off_for_the_whole_node_a_failed_pass_stops_at_the_oldest_server_and_reports_every_waiting_one() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let reports: Arc<Mutex<Vec<EventBacklog>>> = Arc::default();
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, Hooks::default(), Some(Arc::clone(&reports))).await;
    let mut first = Plugin::connect(&pipeline.local).await;
    let mut second = Plugin::connect(&pipeline.relay.prepare("b", OTHER_ALLOCATION, RELAY_KEY)).await;

    first.send(&frame(0)).await;
    first.until_acked(0).await;
    second.send(&frame(0)).await;
    until("the second server to report its backlog", || {
        reports.lock().unwrap().iter().any(|report| report.servers.iter().any(|stuck| stuck.allocation_id == OTHER_ALLOCATION))
    })
    .await;
    assert_eq!(
        reports.lock().unwrap().last().unwrap().servers.iter().map(|stuck| stuck.allocation_id.as_str()).collect::<Vec<_>>(),
        [ALLOCATION, OTHER_ALLOCATION]
    );
    assert!(controller.batches().iter().all(|batch| batch["allocation_id"] == ALLOCATION));

    let failed = controller.batches().len();

    controller.set_respond(|_| receipt());
    until("both servers to be reported delivered", || reports.lock().unwrap().last().is_some_and(|report| report.server_count == 0)).await;
    assert_eq!(controller.batches()[failed..].iter().map(|batch| batch["allocation_id"].clone()).collect::<Vec<_>>(), [ALLOCATION, OTHER_ALLOCATION]);
    pipeline.close().await;
}

#[tokio::test]
async fn drains_the_persistent_queue_to_its_saved_endpoint_after_restart_without_a_running_game() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    until("the controller to receive an upload", || !controller.batches().is_empty()).await;
    pipeline.close().await;
    controller.set_respond(|_| receipt());

    let recovered = Pipeline::plain(&controller, &file.path).await;

    recovered.until_drained(ALLOCATION).await;
    assert!(controller.batches().len() > 1);
    recovered.close().await;
}

#[tokio::test]
async fn discards_controller_quarantined_payloads_on_receipt() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|batch| {
        let quarantined = batch["frames"].as_array().unwrap().iter().any(|frame| frame["seq"] == 1);
        let rejected = if quarantined { json!([{ "seq": 1, "reason": "Invalid event: event.tick: too small" }]) } else { json!([]) };

        Reply::Answer(200, json!({ "rejected": rejected }).to_string())
    });

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.send(&frame(1)).await;
    until("the controller to receive both event frames", || controller.frames().len() == 2).await;
    pipeline.until_drained(ALLOCATION).await;
    // Nothing was resent: every batch the controller saw carried new frames.
    assert_eq!(controller.seqs(), [0, 1]);
    pipeline.close().await;
}

#[tokio::test]
async fn takes_a_receipt_with_any_2xx_status() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| Reply::Answer(201, r#"{"rejected":[]}"#.to_owned()));

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.send(&frame(1)).await;
    until("the controller to receive both event frames", || controller.frames().len() == 2).await;
    pipeline.until_drained(ALLOCATION).await;
    assert_eq!(controller.seqs(), [0, 1]);
    pipeline.close().await;
}

async fn retains_on(reply: fn() -> Reply) {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(move |_| reply());

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    until("the controller to receive an upload retry", || controller.batches().len() >= 2).await;
    assert_eq!(pipeline.pending(ALLOCATION).await, 1);
    pipeline.close().await;
}

#[tokio::test]
async fn retains_a_batch_the_controller_could_not_read_409_and_retries_it() {
    retains_on(|| Reply::Answer(409, r#"{"code":"event_integrity_error","detail":"Invalid event batch envelope"}"#.to_owned())).await;
}

#[tokio::test]
async fn keeps_a_batch_spooled_on_a_404() {
    retains_on(|| Reply::Answer(404, "Not Found".to_owned())).await;
}

#[tokio::test]
async fn retains_events_when_the_receipt_is_not_json() {
    retains_on(|| Reply::Answer(200, "not JSON".to_owned())).await;
}

#[tokio::test]
async fn retains_events_when_the_receipt_fails_its_schema() {
    retains_on(|| Reply::Answer(200, json!({ "allocation_id": ALLOCATION, "to_seq": 0 }).to_string())).await;
}

#[tokio::test]
async fn relays_controller_commands_to_the_linked_plugin_and_reports_the_link_ending() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    until("the plugin link", || pipeline.heard.connected() > 0).await;
    assert!(pipeline.relay.command(ALLOCATION, &exec("status")));
    until("the plugin to receive the status command", || {
        producer.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_))).iter().any(|message| message["command"] == "status")
    })
    .await;
    // Only a SQLite commit acknowledges the producer, and a command is never one.
    assert!(serde_json::from_value::<PluginCommand>(json!({ "type": "ack", "to_seq": 99 })).is_err());
    producer.close().await;
    until("the plugin disconnect report", || pipeline.heard.links().last() == Some(&false)).await;
    pipeline.close().await;
}

#[tokio::test]
async fn answers_a_status_request_with_the_status_that_carries_its_request_id_without_the_plugin_socket_fields() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    until("the plugin link", || pipeline.heard.connected() > 0).await;

    let relay = pipeline.relay.clone();
    let asked = tokio::spawn(async move { relay.request_status(ALLOCATION, Duration::from_secs(5)).await });

    until("the plugin to be asked", || !producer.received_where(|message| matches!(message, AgentToPluginMessage::GetServerStatus(_))).is_empty()).await;

    let request_id = producer.received_where(|message| matches!(message, AgentToPluginMessage::GetServerStatus(_)))[0]["request_id"].clone();

    assert!(request_id.is_string());

    let mut answer = status();

    producer.send_raw(&answer.to_string()).await;
    answer["request_id"] = request_id;
    producer.send_raw(&answer.to_string()).await;
    assert_eq!(
        serde_json::to_value(asked.await.unwrap().unwrap()).unwrap(),
        json!({ "config_error": null, "map_name": "de_mirage", "players": [], "match": null, "ready": true, "server_steam_id": null })
    );
    // Every status also reaches the agent's handlers.
    until("both statuses at the handlers", || pipeline.heard.statuses().len() == 2).await;
    pipeline.close().await;
}

#[tokio::test]
async fn refuses_a_status_request_while_no_plugin_is_linked_and_fails_one_whose_plugin_leaves_or_stays_silent() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let long = Duration::from_secs(5);

    assert_eq!(pipeline.relay.request_status(ALLOCATION, long).await.unwrap_err().code, AgentErrorCode::PluginOffline);
    assert_eq!(pipeline.relay.request_status("00000000-0000-4000-8000-000000000009", long).await.unwrap_err().code, AgentErrorCode::PluginOffline);

    let mut producer = Plugin::connect(&pipeline.local).await;

    until("the plugin link", || pipeline.heard.connected() > 0).await;
    assert_eq!(pipeline.relay.request_status(ALLOCATION, Duration::from_millis(50)).await.unwrap_err().code, AgentErrorCode::PluginStatusTimeout);

    let relay = pipeline.relay.clone();
    let leaving = tokio::spawn(async move { relay.request_status(ALLOCATION, long).await });

    until("the plugin to be asked again", || producer.received_where(|message| matches!(message, AgentToPluginMessage::GetServerStatus(_))).len() == 2).await;
    producer.close().await;
    assert_eq!(leaving.await.unwrap().unwrap_err().code, AgentErrorCode::PluginOffline);
    pipeline.close().await;
}

fn start_match(request_id: String) -> AgentToPluginMessage {
    AgentToPluginMessage::StartMatch(PluginStartMatch { request_id })
}

#[tokio::test]
async fn settles_a_command_on_the_plugins_result_for_its_request_id_and_fails_one_whose_plugin_leaves_or_stays_silent() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let long = Duration::from_secs(5);
    let run = |timeout: Duration| {
        let relay = pipeline.relay.clone();

        tokio::spawn(async move { relay.run_command(ALLOCATION, start_match, timeout).await })
    };

    assert_eq!(pipeline.relay.run_command(ALLOCATION, start_match, long).await.unwrap_err().code, AgentErrorCode::PluginOffline);

    let mut producer = Plugin::connect(&pipeline.local).await;

    until("the plugin link", || pipeline.heard.connected() > 0).await;

    let started = run(long);

    until("the plugin to be asked", || producer.received_where(|message| matches!(message, AgentToPluginMessage::StartMatch(_))).len() == 1).await;

    let first = producer.received_where(|message| matches!(message, AgentToPluginMessage::StartMatch(_)))[0]["request_id"].clone();

    producer.send_raw(&json!({ "type": "command_result", "request_id": "another", "error": "team_empty" }).to_string()).await;
    producer.send_raw(&json!({ "type": "command_result", "request_id": first, "error": null }).to_string()).await;
    assert!(started.await.unwrap().is_ok());

    let refused = run(long);

    until("the second request", || producer.received_where(|message| matches!(message, AgentToPluginMessage::StartMatch(_))).len() == 2).await;

    let second = producer.received_where(|message| matches!(message, AgentToPluginMessage::StartMatch(_)))[1]["request_id"].clone();

    producer.send_raw(&json!({ "type": "command_result", "request_id": second, "error": "not_in_warmup" }).to_string()).await;
    assert_eq!(refused.await.unwrap().unwrap_err().code, AgentErrorCode::NotInWarmup);
    assert_eq!(run(Duration::from_millis(50)).await.unwrap().unwrap_err().code, AgentErrorCode::PluginStatusTimeout);

    let leaving = run(long);

    until("the fourth request", || producer.received_where(|message| matches!(message, AgentToPluginMessage::StartMatch(_))).len() == 4).await;
    producer.close().await;
    assert_eq!(leaving.await.unwrap().unwrap_err().code, AgentErrorCode::PluginOffline);
    pipeline.close().await;
}

#[tokio::test]
async fn sends_a_command_or_status_request_to_the_addressed_games_plugin_alone_and_drops_one_for_a_game_with_none() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let second = Plugin::connect(&pipeline.relay.prepare("b", OTHER_ALLOCATION, RELAY_KEY)).await;

    until("b's plugin link", || pipeline.heard.connected() == 1).await;
    // Slot a's game has no plugin yet, and nothing runs under the third allocation.
    assert!(!pipeline.relay.command(ALLOCATION, &exec("status")));
    assert!(!pipeline.relay.command("00000000-0000-4000-8000-000000000009", &exec("status")));
    assert_eq!(pipeline.relay.request_status(ALLOCATION, Duration::from_secs(5)).await.unwrap_err().code, AgentErrorCode::PluginOffline);

    let first = Plugin::connect(&pipeline.local).await;

    until("a's plugin link", || pipeline.heard.connected() == 2).await;
    assert!(pipeline.relay.command(OTHER_ALLOCATION, &exec("mp_restartgame 1")));
    let _ = pipeline.relay.request_status(OTHER_ALLOCATION, Duration::from_millis(50)).await;
    assert!(!second.received_where(|message| matches!(message, AgentToPluginMessage::GetServerStatus(_))).is_empty());
    assert_eq!(
        second.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_))).iter().map(|message| message["command"].clone()).collect::<Vec<_>>(),
        ["mp_restartgame 1"]
    );
    assert!(
        first.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_))).is_empty()
            && first.received_where(|message| matches!(message, AgentToPluginMessage::GetServerStatus(_))).is_empty()
    );
    pipeline.close().await;
}

#[tokio::test]
async fn closes_a_producer_with_a_sequence_gap_without_acknowledging_or_posting_it() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.until_acked(0).await;
    until("the controller to receive the first event", || controller.seqs() == [0]).await;
    producer.send(&frame(2)).await;
    producer.until_closed().await;
    assert_eq!(producer.closed_with(), Some((1000, "replay from the last ack".to_owned())));
    assert_eq!(producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).len(), 1);
    assert_eq!(controller.seqs(), [0]);
    pipeline.close().await;
}

/// A plugin learns the agent went away rather than seeing its socket reset.
#[tokio::test]
async fn ends_a_plugin_socket_with_going_away_when_the_agent_shuts_down() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let producer = Plugin::connect(&pipeline.local).await;

    until("the plugin to be linked", || pipeline.heard.links() == [true]).await;
    pipeline.close().await;
    producer.until_closed().await;
    assert_eq!(producer.closed_with(), Some((1001, "agent shutting down".to_owned())));
}

/// A plugin acknowledged by a spool since deleted resends only what came after; the spool takes
/// it from there rather than refusing the rest of the match.
#[tokio::test]
async fn stores_a_server_whose_history_the_spool_lost_from_its_first_resent_event() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(5)).await;
    producer.send(&frame(6)).await;
    producer.until_acked(6).await;
    until("the controller to receive the resent events", || controller.seqs() == [5, 6]).await;
    pipeline.close().await;
}

#[tokio::test]
async fn forwards_an_invalid_event_for_the_controller_to_quarantine_and_drops_a_frame_with_no_seq() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    let mut invalid = frame(1);

    invalid["event"]["tick"] = json!(-1);
    producer.send(&frame(0)).await;
    producer.send(&invalid).await;
    producer.send_raw(&json!({ "type": "event", "event": { "type": "log" } }).to_string()).await;
    producer.send_raw("not json").await;
    producer.send(&frame(2)).await;
    producer.until_acked(2).await;
    until("the controller to receive all three event frames", || controller.frames().len() == 3).await;
    assert_eq!(controller.seqs(), [0, 1, 2]);
    assert_eq!(controller.frames()[1], invalid);
    assert!(producer.is_open());
    pipeline.close().await;
}

#[tokio::test]
async fn drops_a_plugin_frame_that_is_no_object_or_of_a_type_this_agent_does_not_know_and_keeps_the_plugin() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    // A newer cs2 image may send what this agent does not know yet.
    producer.send_raw("null").await;
    producer.send_raw("[]").await;
    producer.send_raw(&json!({ "type": "future_message", "detail": 1 }).to_string()).await;
    producer.send(&frame(0)).await;
    producer.until_acked(0).await;
    assert!(producer.is_open());
    assert_eq!(pipeline.heard.statuses(), Vec::<Value>::new());
    pipeline.close().await;
}

#[tokio::test]
async fn drops_a_plugin_that_sends_a_binary_frame_without_acknowledging_it() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    let mut message = json!({ "type": "event" });

    message.as_object_mut().unwrap().extend(frame(0).as_object().unwrap().clone());
    producer.sink.send(Message::Binary(message.to_string().into_bytes().into())).await.unwrap();
    producer.until_closed().await;
    assert!(producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty());
    pipeline.close().await;
}

#[tokio::test]
async fn hands_a_slot_to_the_plugins_new_socket_failing_what_the_old_one_was_asked_and_stays_linked_when_the_old_one_closes() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let first = Plugin::connect(&pipeline.local).await;

    until("the first link", || pipeline.heard.connected() == 1).await;

    let relay = pipeline.relay.clone();
    let asked = tokio::spawn(async move { relay.request_status(ALLOCATION, Duration::from_mins(1)).await });

    until("the first socket to be asked", || !first.received_where(|message| matches!(message, AgentToPluginMessage::GetServerStatus(_))).is_empty()).await;

    // The old socket will never answer, so the request fails now rather than at its timeout.
    let second = Plugin::connect(&pipeline.local).await;

    assert_eq!(asked.await.unwrap().unwrap_err().code, AgentErrorCode::PluginOffline);
    first.until_closed().await;
    assert_eq!(first.closed_with(), Some((1000, "replaced by a newer connection".to_owned())));
    assert_eq!(pipeline.heard.links(), [true, true]);
    assert!(pipeline.relay.command(ALLOCATION, &exec("status")));
    until("the new socket to receive the command", || !second.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_))).is_empty()).await;
    assert!(first.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_))).is_empty());
    pipeline.close().await;
}

#[tokio::test]
async fn ignores_a_status_answer_whose_request_already_timed_out_and_answers_the_next_request() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    let request_ids = |plugin: &Plugin| {
        plugin
            .received_where(|message| matches!(message, AgentToPluginMessage::GetServerStatus(_)))
            .iter()
            .map(|message| message["request_id"].clone())
            .collect::<Vec<_>>()
    };

    until("the plugin link", || pipeline.heard.connected() > 0).await;
    assert_eq!(pipeline.relay.request_status(ALLOCATION, Duration::from_millis(50)).await.unwrap_err().code, AgentErrorCode::PluginStatusTimeout);

    let mut late = status();

    late["request_id"] = request_ids(&producer)[0].clone();
    producer.send_raw(&late.to_string()).await;

    let relay = pipeline.relay.clone();
    let asked = tokio::spawn(async move { relay.request_status(ALLOCATION, Duration::from_secs(5)).await });

    until("the second request", || request_ids(&producer).len() == 2).await;

    let mut answer = status();

    answer["request_id"] = request_ids(&producer)[1].clone();
    producer.send_raw(&answer.to_string()).await;

    let state = asked.await.unwrap().unwrap();

    assert_eq!((state.map_name.as_str(), state.ready), ("de_mirage", true));
    assert!(producer.is_open());
    pipeline.close().await;
}

#[tokio::test]
async fn preserves_escaped_text_nested_extra_payload_fields_and_event_order_through_serialization() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    let mut first = frame(0);

    first["event"]["type"] = json!("player_connect");
    first["event"]["rejected"] = json!(false);
    first["event"]["player"] =
        json!({ "steam_id": "76561198000000001", "name": "Ægir \"quoted\" \\ path", "side": "t", "team": 1, "extra": { "detail": ["retained"] } });
    first["event"]["future"] = json!({ "values": [null, true, { "text": "日本語\nnext line" }] });

    let second = frame(1);

    producer.send(&first).await;
    producer.send(&second).await;
    until("the controller to receive both event frames", || controller.frames().len() == 2).await;
    assert_eq!(controller.frames(), [first, second]);
    pipeline.close().await;
}

/// Servers with a stream row, in the order they were first seen.
fn spool_streams(path: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open(path).unwrap();
    let mut statement = db.prepare("SELECT allocation FROM server_streams ORDER BY rowid").unwrap();

    statement.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

#[tokio::test]
async fn moves_retained_events_to_the_configured_backend_without_rewriting_sqlite_routes() {
    let first = Controller::start("/api/node/events").await;

    first.set_respond(|_| unavailable());

    let file = spool_file();
    let pipeline = Pipeline::plain(&first, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.until_acked(0).await;
    until("the original backend to receive an upload", || !first.batches().is_empty()).await;

    let before = spool_streams(&file.path);
    let second = Controller::start("/elsewhere/events").await;

    pipeline.move_to(&second);
    until("the replacement backend to receive an upload", || !second.batches().is_empty()).await;
    pipeline.until_drained(ALLOCATION).await;
    assert_eq!(second.batches()[0]["allocation_id"], ALLOCATION);
    assert_eq!(spool_streams(&file.path), before);
    assert!(!pipeline.local.contains("token"));
    pipeline.close().await;
}

#[tokio::test]
async fn abandons_an_upload_the_old_service_never_answers_once_the_node_is_moved() {
    let first = Controller::start("/api/node/events").await;

    first.set_respond(|_| Reply::Hang);

    let file = spool_file();
    let pipeline = Pipeline::plain(&first, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    until("an upload left hanging", || !first.batches().is_empty()).await;

    let second = Controller::start("/elsewhere/events").await;

    pipeline.move_to(&second);
    until("the replacement backend to receive an upload", || !second.batches().is_empty()).await;
    pipeline.until_drained(ALLOCATION).await;
    pipeline.close().await;
}

fn with_query(url: &str, edit: impl FnOnce(&mut Vec<(String, String)>)) -> String {
    let mut parsed = url::Url::parse(url).unwrap();
    let mut pairs: Vec<(String, String)> = parsed.query_pairs().map(|(name, value)| (name.into_owned(), value.into_owned())).collect();

    edit(&mut pairs);
    parsed.query_pairs_mut().clear().extend_pairs(&pairs);
    parsed.to_string()
}

#[tokio::test]
async fn refuses_a_plugin_connection_with_no_key_or_another_launchs_key() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let without = with_query(&pipeline.local, |pairs| pairs.retain(|(name, _)| name != "key"));
    let another = with_query(&pipeline.local, |pairs| pairs.iter_mut().filter(|(name, _)| name == "key").for_each(|(_, value)| *value = "f".repeat(64)));

    assert_eq!(dial(&without, true).await.err(), Some(401));
    assert_eq!(dial(&another, true).await.err(), Some(401));
    pipeline.close().await;
}

#[tokio::test]
async fn refuses_a_plugin_connection_for_an_unknown_slot_another_path_or_a_released_launch_as_not_found() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let unknown = with_query(&pipeline.local, |pairs| pairs.iter_mut().filter(|(name, _)| name == "slot_id").for_each(|(_, value)| *value = "zz".to_owned()));
    let mut elsewhere = url::Url::parse(&pipeline.local).unwrap();

    elsewhere.set_path("/missing");
    assert_eq!(dial(&unknown, true).await.err(), Some(404));
    assert_eq!(dial(elsewhere.as_str(), true).await.err(), Some(404));
    pipeline.relay.release_allocation("a", ALLOCATION);
    assert_eq!(dial(&pipeline.local, true).await.err(), Some(404));
    assert_eq!(pipeline.heard.links(), Vec::<bool>::new());
    pipeline.close().await;
}

fn origin(url: &str) -> String {
    let parsed = url::Url::parse(url).unwrap();

    format!("http://{}:{}", parsed.host_str().unwrap(), parsed.port().unwrap())
}

#[tokio::test]
async fn answers_a_request_that_is_not_a_socket_upgrade_with_404() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let response = reqwest::get(origin(&pipeline.local)).await.unwrap();

    assert_eq!(response.status().as_u16(), 404);
    assert_eq!(response.text().await.unwrap(), "");
    pipeline.close().await;
}

#[tokio::test]
async fn fails_to_start_on_a_port_another_relay_holds() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let port = url::Url::parse(&pipeline.local).unwrap().port().unwrap();
    let error = PluginRelay::new(None, Logger::null()).bind(port).await.err().unwrap();

    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    pipeline.close().await;
}

#[tokio::test]
async fn refuses_a_plugin_that_does_not_name_the_protocol_before_any_frame() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;

    assert_eq!(dial(&pipeline.local, false).await.err(), Some(426));
    assert_eq!(spool_streams(&file.path), Vec::<String>::new());
    pipeline.close().await;
}

/// Sends an upgrade request by hand and returns the connection, for answers a client library refuses to make.
async fn raw_upgrade(url: &str, path_and_query: &str, protocol: bool) -> TcpStream {
    let parsed = url::Url::parse(url).unwrap();
    let host = format!("{}:{}", parsed.host_str().unwrap(), parsed.port().unwrap());
    let mut stream = TcpStream::connect(&host).await.unwrap();
    let mut request = format!(
        "GET {path_and_query} HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: MDEyMzQ1Njc4OWFiY2RlZg==\r\n"
    );

    if protocol {
        request.push_str("Ankor-Protocol: ");
        request.push_str(PROTOCOL_VERSION);
        request.push_str("\r\n");
    }

    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

fn path_and_query(url: &str) -> String {
    let parsed = url::Url::parse(url).unwrap();

    format!("{}?{}", parsed.path(), parsed.query().unwrap_or_default())
}

#[tokio::test]
async fn refuses_an_invalid_upgrade_url_without_failing_the_listener() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut stream = raw_upgrade(&pipeline.local, "//", true).await;
    let mut answer = String::new();

    BufReader::new(&mut stream).read_line(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
    drop(stream);

    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.until_acked(0).await;
    pipeline.close().await;
}

#[tokio::test]
async fn survives_a_reset_while_refusing_a_plugin_with_the_wrong_route_key_or_protocol() {
    for invalid in ["route", "key", "protocol"] {
        let controller = Controller::start("/api/node/events").await;
        let file = spool_file();
        let pipeline = Pipeline::plain(&controller, &file.path).await;
        let target = match invalid {
            "route" => format!("/missing?{}", url::Url::parse(&pipeline.local).unwrap().query().unwrap()),
            "key" => path_and_query(&with_query(&pipeline.local, |pairs| pairs.retain(|(name, _)| name != "key"))),
            _ => path_and_query(&pipeline.local),
        };
        let stream = raw_upgrade(&pipeline.local, &target, invalid != "protocol").await;

        stream.set_zero_linger().unwrap();
        drop(stream);

        // A reset during the rejected write must not take the listener down. The relay still
        // admits the legitimate producer afterwards, with no history from the refused one.
        let mut producer = Plugin::connect(&pipeline.local).await;

        producer.send(&frame(0)).await;
        producer.until_acked(0).await;
        pipeline.close().await;
    }
}

#[tokio::test]
async fn passes_a_matchs_phase_and_score_to_its_handler_without_spooling_them() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    let phase = json!({ "type": "phase_change", "phase": "knife" });
    let score = json!({ "type": "score", "team1_score": 3, "team2_score": 2 });

    until("the plugin link", || pipeline.heard.connected() == 1).await;
    producer.send_raw(&phase.to_string()).await;
    producer.send_raw(&score.to_string()).await;
    until("the progress at the handler", || pipeline.heard.progress.lock().unwrap().len() == 2).await;
    assert_eq!(*pipeline.heard.progress.lock().unwrap(), [phase, score]);
    assert_eq!(pipeline.pending(ALLOCATION).await, 0);
    assert_eq!(controller.batches(), Vec::<Value>::new());
    pipeline.close().await;
}

#[tokio::test]
async fn relays_queried_status_with_or_without_a_loaded_match_without_spooling_events() {
    for loaded in [false, true] {
        let controller = Controller::start("/api/node/events").await;
        let file = spool_file();
        let pipeline = Pipeline::plain(&controller, &file.path).await;
        let mut producer = Plugin::connect(&pipeline.local).await;

        until("the plugin link", || pipeline.heard.connected() == 1).await;
        assert!(pipeline.relay.command(ALLOCATION, &PluginCommand::GetServerStatus(PluginGetServerStatus { request_id: None })));
        until("the request at the plugin", || {
            producer
                .received_where(|message| matches!(message, AgentToPluginMessage::GetServerStatus(_)))
                .contains(&json!(AgentToPluginMessage::GetServerStatus(PluginGetServerStatus { request_id: None })))
        })
        .await;

        let mut answer = status();

        if loaded {
            answer["match"] = json!({ "match_id": MATCH_ID, "phase": "live", "team1_side": "t", "team2_side": "ct", "team1_score": 3, "team2_score": 2 });
        }

        producer.send_raw(&answer.to_string()).await;
        until("the status at the handler", || pipeline.heard.statuses().len() == 1).await;
        assert_eq!(pipeline.heard.statuses(), [answer]);
        assert_eq!(pipeline.pending(ALLOCATION).await, 0);
        assert_eq!(pipeline.spool.position(ALLOCATION.to_owned()).await.unwrap(), None);
        assert_eq!(controller.batches(), Vec::<Value>::new());
        pipeline.close().await;
    }
}

#[tokio::test]
async fn bypasses_the_spool_for_status_including_after_restart() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let file = spool_file();
    let first = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&first.local).await;

    until("the plugin link", || first.heard.connected() == 1).await;
    producer.send_raw(&status().to_string()).await;
    until("the status at the handler", || first.heard.statuses().len() == 1).await;
    assert_eq!(first.pending(ALLOCATION).await, 0);
    first.close().await;

    let second = Pipeline::plain(&controller, &file.path).await;
    let mut reconnected = Plugin::connect(&second.local).await;

    until("the plugin link after restart", || second.heard.connected() == 1).await;
    reconnected.send_raw(&status().to_string()).await;
    until("the status after restart", || second.heard.statuses().len() == 1).await;
    assert!(second.relay.command(ALLOCATION, &exec("status")));
    until("the reconnected plugin to receive the command", || {
        !reconnected.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_))).is_empty()
    })
    .await;
    second.close().await;
    assert_eq!([first.heard.statuses(), second.heard.statuses()].concat(), [status(), status()]);
    assert_eq!(controller.batches(), Vec::<Value>::new());
    assert!(
        producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty()
            && reconnected.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty()
    );

    let db = rusqlite::Connection::open(&file.path).unwrap();

    for table in ["server_events", "server_streams"] {
        let count: i64 = db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap();

        assert_eq!(count, 0, "{table}");
    }
}

#[tokio::test]
async fn delivers_status_without_waiting_for_a_blocked_spool_commit() {
    let controller = Controller::start("/api/node/events").await;
    let (file, gate, committing) = (spool_file(), Gate::new(), Arc::new(AtomicBool::new(false)));
    let pipeline = Pipeline::start(&controller, &file.path, gated_append(&gate, &committing), None).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    let event = json!({ "type": "map_end", "match_id": MATCH_ID, "tick": 64, "timestamp": 1000, "reason": "played", "winner_team": 1, "team1_score": 13, "team2_score": 7, "teams": null });

    until("the plugin link", || pipeline.heard.connected() == 1).await;
    producer.send(&json!({ "seq": 0, "event": event })).await;
    until("the SQLite commit to block", || committing.load(Ordering::SeqCst)).await;
    producer.send_raw(&status().to_string()).await;
    until("the status at the handler", || pipeline.heard.statuses().len() == 1).await;
    assert!(producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty());
    gate.release();
    producer.until_acked(0).await;
    until("the controller to receive the first upload", || controller.batches().len() == 1).await;
    assert_eq!(controller.frames()[0]["event"], event);
    pipeline.close().await;
}

#[tokio::test]
async fn drops_the_producer_of_a_commit_the_disk_refused_without_an_ack_and_takes_its_replay() {
    let controller = Controller::start("/api/node/events").await;
    let refuse = Arc::new(AtomicBool::new(true));
    let refusing = Arc::clone(&refuse);
    let hooks = Hooks {
        append: Some(Arc::new(move |spool, streams| {
            let refused = refusing.swap(false, Ordering::SeqCst);

            Box::pin(async move { if refused { Err(SpoolError::Unavailable("disk I/O error".to_owned())) } else { spool.append(streams).await } })
        })),
        ..Hooks::default()
    };
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, hooks, None).await;
    let mut first = Plugin::connect(&pipeline.local).await;

    first.send(&frame(0)).await;
    first.until_closed().await;
    assert!(first.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty());

    let mut replay = Plugin::connect(&pipeline.local).await;

    replay.send(&frame(0)).await;
    replay.until_acked(0).await;
    assert_eq!(pipeline.pending(ALLOCATION).await, 1);
    pipeline.close().await;
}

/// Frames that arrive while a stream's first commit fails come after the lost ones; stored next,
/// they would start the stream from its middle and its first events would be lost for good.
#[tokio::test]
async fn takes_a_replay_from_the_start_when_frames_arrived_while_the_first_commit_failed() {
    let controller = Controller::start("/api/node/events").await;
    let (gate, committing) = (Gate::new(), Arc::new(AtomicBool::new(false)));
    let refuse = Arc::new(AtomicBool::new(true));
    let (waiting, started, refusing) = (gate.clone(), Arc::clone(&committing), Arc::clone(&refuse));
    let hooks = Hooks {
        append: Some(Arc::new(move |spool, streams| {
            let (gate, started, refused) = (waiting.clone(), Arc::clone(&started), refusing.swap(false, Ordering::SeqCst));

            Box::pin(async move {
                if !refused {
                    return spool.append(streams).await;
                }

                started.store(true, Ordering::SeqCst);
                gate.wait().await;
                Err(SpoolError::Unavailable("disk I/O error".to_owned()))
            })
        })),
        ..Hooks::default()
    };
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, hooks, None).await;
    let mut first = Plugin::connect(&pipeline.local).await;

    first.send(&frame(0)).await;
    until("the first commit to start", || committing.load(Ordering::SeqCst)).await;

    for seq in 1..4 {
        first.send(&frame(seq)).await;
    }

    // Long enough for the relay to queue them behind the commit under way.
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.release();
    first.until_closed().await;
    assert!(first.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty(), "frames after the lost ones were acknowledged");

    let mut replay = Plugin::connect(&pipeline.local).await;

    for seq in 0..4 {
        replay.send(&frame(seq)).await;
    }

    replay.until_acked(3).await;
    assert_eq!(pipeline.pending(ALLOCATION).await, 4);
    pipeline.close().await;
}

async fn bounds_pending_history(by_bytes: bool) {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let (gate, committing) = (Gate::new(), Arc::new(AtomicBool::new(false)));
    let committed: Arc<Mutex<Vec<i64>>> = Arc::default();
    let (held, blocking, recorded) = (gate.clone(), Arc::clone(&committing), Arc::clone(&committed));
    let hooks = Hooks {
        append: Some(Arc::new(move |spool, streams: Vec<SpoolStream>| {
            let (gate, committing, committed) = (held.clone(), Arc::clone(&blocking), Arc::clone(&recorded));

            Box::pin(async move {
                committing.store(true, Ordering::SeqCst);
                gate.wait().await;

                let seqs: Vec<i64> = streams.iter().flat_map(|stream| stream.frames.iter().map(|frame: &SpooledFrame| frame.seq)).collect();
                let results = spool.append(streams).await;

                committed.lock().unwrap().extend(seqs);
                results
            })
        })),
        ..Hooks::default()
    };
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, hooks, None).await;
    let mut producer = Plugin::connect(&pipeline.local).await;
    let count = if by_bytes { 12 } else { 520 };
    let padding = if by_bytes { "x".repeat(512 * 1024) } else { String::new() };
    let mut raws: Vec<String> = (0..count)
        .map(|seq| {
            let mut message = json!({ "type": "event", "padding": padding });

            message.as_object_mut().unwrap().extend(frame(seq).as_object().unwrap().clone());
            message.to_string()
        })
        .collect();
    let accepted = if by_bytes { 7 } else { 512 };

    // A frame that would fit again after the overflow waits for the replay too.
    let mut last = json!({ "type": "event" });

    last.as_object_mut().unwrap().extend(frame(count).as_object().unwrap().clone());
    raws.push(last.to_string());
    producer.send_raw(&raws[0]).await;
    until("the first commit to block", || committing.load(Ordering::SeqCst)).await;

    for raw in &raws[1..] {
        producer.send_raw(raw).await;
    }

    producer.send_raw(&status().to_string()).await;
    until("status after the history limit", || pipeline.heard.statuses().len() == 1).await;
    assert!(producer.is_open());
    assert!(producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty());
    gate.release();
    producer.until_closed().await;
    assert_eq!(*committed.lock().unwrap(), (0..accepted).collect::<Vec<_>>());
    assert_eq!(pipeline.spool.position(ALLOCATION.to_owned()).await.unwrap().map(|ack| ack.to_seq), Some(accepted - 1));

    let mut replay = Plugin::connect(&pipeline.local).await;

    for raw in &raws[usize::try_from(accepted).unwrap()..] {
        replay.send_raw(raw).await;
    }

    replay.until_acked(count).await;
    assert_eq!(*committed.lock().unwrap(), (0..=count).collect::<Vec<_>>());
    assert_eq!(pipeline.heard.statuses(), [status()]);
    pipeline.close().await;
}

#[tokio::test]
async fn bounds_pending_history_by_frames_while_keeping_status_live_then_replays_the_overflow() {
    bounds_pending_history(false).await;
}

#[tokio::test]
async fn bounds_pending_history_by_bytes_while_keeping_status_live_then_replays_the_overflow() {
    bounds_pending_history(true).await;
}

fn query_value(url: &str, name: &str) -> Option<String> {
    url::Url::parse(url).unwrap().query_pairs().find(|(each, _)| each == name).map(|(_, value)| value.into_owned())
}

#[tokio::test]
async fn gives_each_launch_on_a_slot_its_own_key_keeping_successive_launches_and_their_history_separate() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut first = Plugin::connect(&pipeline.local).await;

    assert_eq!(query_value(&pipeline.local, "slot_id").as_deref(), Some("a"));
    first.send(&frame(0)).await;
    first.until_acked(0).await;

    let replacement = pipeline.relay.prepare("a", OTHER_ALLOCATION, "fedcba9876543210");

    assert_eq!(query_value(&replacement, "slot_id").as_deref(), Some("a"));
    assert_ne!(query_value(&replacement, "key"), query_value(&pipeline.local, "key"));
    first.until_closed().await;

    let mut second = Plugin::connect(&replacement).await;

    second.send(&frame(0)).await;
    second.until_acked(0).await;
    assert_eq!(pipeline.spool.pending(ALLOCATION.to_owned()).await.unwrap()[0].allocation_id, ALLOCATION);
    assert_eq!(pipeline.spool.pending(OTHER_ALLOCATION.to_owned()).await.unwrap()[0].allocation_id, OTHER_ALLOCATION);
    pipeline.relay.release_allocation("a", OTHER_ALLOCATION);
    second.until_closed().await;
    assert_eq!(pipeline.pending(OTHER_ALLOCATION).await, 1);
    pipeline.close().await;
}

#[tokio::test]
async fn keeps_the_linked_plugin_when_its_slot_is_prepared_again_for_the_same_launch() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.until_acked(0).await;
    assert_eq!(pipeline.relay.prepare("a", ALLOCATION, RELAY_KEY), pipeline.local);
    producer.send(&frame(1)).await;
    producer.until_acked(1).await;
    assert_eq!(pipeline.heard.links(), [true]);
    pipeline.close().await;
}

#[tokio::test]
async fn refuses_new_plugins_and_drops_linked_ones_while_it_closes_behind_a_blocked_commit() {
    let controller = Controller::start("/api/node/events").await;
    let (file, gate, committing) = (spool_file(), Gate::new(), Arc::new(AtomicBool::new(false)));
    let pipeline = Arc::new(Pipeline::start(&controller, &file.path, gated_append(&gate, &committing), None).await);
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    until("the commit to block", || committing.load(Ordering::SeqCst)).await;

    let closing = tokio::spawn({
        let pipeline = Arc::clone(&pipeline);

        async move { pipeline.close().await }
    });

    tokio::task::yield_now().await;
    assert_eq!(dial(&pipeline.local, true).await.err(), Some(404));
    gate.release();
    closing.await.unwrap();
    producer.until_closed().await;
}

#[tokio::test]
async fn delivers_what_a_server_spooled_after_its_allocation_ends() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.send(&frame(1)).await;
    producer.until_acked(1).await;
    pipeline.relay.release_allocation("a", ALLOCATION);
    controller.set_respond(|_| receipt());
    pipeline.until_drained(ALLOCATION).await;
    assert_eq!(controller.batches().last(), Some(&json!({ "allocation_id": ALLOCATION, "frames": [frame(0), frame(1)] })));
    pipeline.close().await;
}

#[tokio::test]
async fn acknowledges_a_reconnecting_producer_how_far_its_server_is_stored_and_no_other_stream_in_the_spool() {
    let controller = Controller::start("/api/node/events").await;

    controller.set_respond(|_| unavailable());

    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    producer.send(&frame(0)).await;
    producer.send(&frame(1)).await;
    producer.until_acked(1).await;
    pipeline
        .spool
        .append(vec![SpoolStream {
            allocation_id: OTHER_ALLOCATION.to_owned(),
            frames: (0..3).map(|seq| SpooledFrame { seq, raw: frame(seq).to_string() }).collect(),
        }])
        .await
        .unwrap();
    producer.close().await;
    producer.until_closed().await;

    let again = Plugin::connect(&pipeline.local).await;

    until("the reconnected producer to receive its acknowledgement", || {
        !again.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(again.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))), [json!(AgentToPluginMessage::Ack(EventAck { to_seq: 1 }))]);
    pipeline.close().await;
}

#[tokio::test]
async fn acknowledges_a_producer_the_position_an_earlier_agent_stored_on_its_first_connection_to_this_one() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let earlier = SpoolClient::start(file.path.clone(), Logger::null());

    controller.set_respond(|_| unavailable());
    earlier
        .append(vec![SpoolStream {
            allocation_id: ALLOCATION.to_owned(),
            frames: (0..3).map(|seq| SpooledFrame { seq, raw: frame(seq).to_string() }).collect(),
        }])
        .await
        .unwrap();
    let _ = earlier.close().await;

    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    until("the producer to receive the stored position", || !producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty())
        .await;
    assert_eq!(producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))), [json!(AgentToPluginMessage::Ack(EventAck { to_seq: 2 }))]);
    // The replay of what was stored is skipped, and what follows it is taken.
    producer.send(&frame(2)).await;
    producer.send(&frame(3)).await;
    producer.until_acked(3).await;
    assert_eq!(pipeline.spool.backlog().await.unwrap(), [SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 4 }]);
    pipeline.close().await;
}

#[tokio::test]
async fn acknowledges_nothing_to_a_producer_whose_server_has_stored_no_event() {
    let controller = Controller::start("/api/node/events").await;
    let file = spool_file();
    let pipeline = Pipeline::plain(&controller, &file.path).await;
    let mut producer = Plugin::connect(&pipeline.local).await;

    until("the plugin link", || pipeline.heard.connected() == 1).await;
    producer.send_raw(&status().to_string()).await;
    until("the status, behind any acknowledgement", || pipeline.heard.statuses().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(producer.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).is_empty());
    pipeline.close().await;
}

#[tokio::test(start_paused = true)]
async fn prunes_what_no_attached_producer_can_replay_at_boot_while_a_game_runs_and_after_its_plugin_leaves() {
    let controller = Controller::start("/api/node/events").await;
    let pruned: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let recorded = Arc::clone(&pruned);
    let hooks = Hooks {
        prune: Some(Arc::new(move |spool, allocations: Vec<String>| {
            recorded.lock().unwrap().push(allocations.clone());
            Box::pin(async move { spool.prune(allocations).await })
        })),
        ..Hooks::default()
    };
    let file = spool_file();
    let pipeline = Pipeline::start(&controller, &file.path, hooks, None).await;

    assert_eq!(*pruned.lock().unwrap(), [Vec::<String>::new()]);

    let mut producer = Plugin::connect(&pipeline.local).await;

    // Paused time runs on while the test waits on real sockets, so an hourly prune may already have
    // run before the link, naming nothing. The prune that counts is one with the producer attached.
    until("the plugin link", || pipeline.heard.connected() == 1).await;
    tokio::time::sleep(Duration::from_hours(1)).await;
    until("a periodic prune that keeps the attached producer's events", || pruned.lock().unwrap().last().is_some_and(|kept| kept == &[ALLOCATION])).await;

    producer.close().await;
    until("the plugin disconnect report", || pipeline.heard.links().last() == Some(&false)).await;
    tokio::time::sleep(Duration::from_hours(1)).await;
    until("a prune after the plugin left that keeps nothing", || pruned.lock().unwrap().last().is_some_and(Vec::is_empty)).await;
    pipeline.close().await;
}
