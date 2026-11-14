//! The fake controller: the agent socket, the event batch and demo upload endpoints, and the
//! control socket through which scenarios drive the game servers.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ankor_protocol::helpers::node_authorization;
use ankor_protocol::types::{AgentToControllerMessage, ControllerToAgentMessage, ControllerWelcome};
use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::scenario::agent_message;
use crate::stubs::Gate;
use crate::transcript::{Recorder, channels, controller_entry};

/// The path the game servers dial for their instructions.
pub const PLUGIN_CONTROL_PATH: &str = "/conformance/plugin";
/// The query parameter of a plugin control dial that names the plugin's allocation.
pub const PLUGIN_ALLOCATION_QUERY: &str = "allocation";

/// The path demos are uploaded to.
pub const DEMO_PATH: &str = "/demos/upload";

enum Outgoing {
    Frame(Value),
    /// Ends the TCP connection without a close frame.
    Drop,
}

struct AgentConn {
    id: usize,
    tx: mpsc::UnboundedSender<Outgoing>,
}

struct ControllerState {
    rec: Recorder,
    token: Mutex<String>,
    agent: Mutex<Option<AgentConn>>,
    auto_welcome: AtomicBool,
    refuse: AtomicBool,
    connections: AtomicUsize,
    event_answers: Mutex<VecDeque<(u16, Value)>>,
    demo_answers: Mutex<VecDeque<u16>>,
    plugins: Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
    demos: Gate,
    /// What the agent sent that the contract refuses, so a run fails instead of recording it as
    /// the golden transcript.
    contract_issues: Mutex<Vec<String>>,
}

/// Records `value`'s issues against `schema`, as a controller that parses with the contract would
/// refuse it.
fn check_contract(state: &ControllerState, what: &str, schema: fn(&mut ankor_protocol::validate::Payload), value: &Value) {
    if let Err(issues) = ankor_protocol::validate::safe_parse(schema, value.clone()) {
        lock(&state.contract_issues).push(format!("{what}: {}", ankor_protocol::validate::format_issues(&issues)));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The port the fake controller listens on, the one `agent.yml` implies with `tls: false`.
pub const CONTROLLER_PORT: u16 = 80;

/// A handle on the running fake controller.
#[derive(Clone)]
pub struct Controller {
    state: Arc<ControllerState>,
}

impl Controller {
    pub async fn start(rec: Recorder) -> Result<Self> {
        let state = Arc::new(ControllerState {
            rec,
            token: Mutex::new(String::new()),
            agent: Mutex::new(None),
            auto_welcome: AtomicBool::new(true),
            refuse: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
            event_answers: Mutex::new(VecDeque::new()),
            demo_answers: Mutex::new(VecDeque::new()),
            plugins: Mutex::new(HashMap::new()),
            demos: Gate::new(),
            contract_issues: Mutex::default(),
        });
        let router = Router::new()
            .route(ankor_protocol::constants::DEFAULT_SOCKET_PATH, get(agent_socket))
            .route(ankor_protocol::constants::DEFAULT_EVENTS_PATH, any(event_batch))
            .route(DEMO_PATH, any(demo_upload))
            .route(PLUGIN_CONTROL_PATH, get(plugin_control))
            .fallback(any(unknown))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], CONTROLLER_PORT))).await.context("bind fake controller")?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self { state })
    }

    /// The contract issues of what the agent sent since the last call, which empties them.
    pub fn take_contract_issues(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.state.contract_issues))
    }

    /// The node token the socket and the event endpoint expect.
    pub fn set_token(&self, token: &str) {
        token.clone_into(&mut lock(&self.state.token));
    }

    /// Whether a `hello` is answered with `welcome` on its own.
    pub fn set_auto_welcome(&self, on: bool) {
        self.state.auto_welcome.store(on, Ordering::SeqCst);
    }

    /// Whether new agent sockets are turned away with `503`, unrecorded, as an unreachable controller would.
    pub fn set_refuse(&self, on: bool) {
        self.state.refuse.store(on, Ordering::SeqCst);
    }

    /// Scripted answers for the next event batch posts: status and body. Afterwards `200 {"rejected":[]}`.
    pub fn answer_events(&self, answers: impl IntoIterator<Item = (u16, Value)>) {
        lock(&self.state.event_answers).extend(answers);
    }

    /// Scripted statuses for the next demo uploads. Afterwards `200`.
    pub fn answer_demos(&self, answers: impl IntoIterator<Item = u16>) {
        lock(&self.state.demo_answers).extend(answers);
    }

    /// Holds demo uploads: their answers wait until the gate opens.
    pub fn demo_gate(&self) -> &Gate {
        &self.state.demos
    }

    /// Sends a frame to the agent on its current socket.
    pub fn send(&self, frame: Value) -> Result<()> {
        let agent = lock(&self.state.agent);
        let conn = agent.as_ref().ok_or_else(|| anyhow!("no agent socket is open"))?;
        conn.tx.send(Outgoing::Frame(frame)).map_err(|_| anyhow!("the agent socket closed"))
    }

    /// Ends the agent's socket without a close frame, as a broken link would.
    pub fn drop_agent(&self) -> Result<()> {
        let agent = lock(&self.state.agent).take().ok_or_else(|| anyhow!("no agent socket is open"))?;
        agent.tx.send(Outgoing::Drop).map_err(|_| anyhow!("the agent socket closed"))
    }

    /// Sends an instruction to the game server of `allocation`.
    pub fn plugin(&self, allocation: &str, op: Value) -> Result<()> {
        let plugins = lock(&self.state.plugins);
        let tx = plugins.get(allocation).ok_or_else(|| anyhow!("the plugin of {allocation} is not connected"))?;
        tx.send(op).map_err(|_| anyhow!("the plugin of {allocation} left"))
    }

    /// Forgets the agent socket, the scripted answers and the plugins, for a new run.
    pub fn reset(&self) {
        *lock(&self.state.agent) = None;
        lock(&self.state.event_answers).clear();
        lock(&self.state.demo_answers).clear();
        lock(&self.state.plugins).clear();
        self.state.auto_welcome.store(true, Ordering::SeqCst);
        self.state.refuse.store(false, Ordering::SeqCst);
        self.state.connections.store(0, Ordering::SeqCst);
        self.state.demos.open();
    }
}

fn header_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers.iter().map(|(name, value)| (name.as_str().to_owned(), String::from_utf8_lossy(value.as_bytes()).into_owned())).collect()
}

fn bearer_ok(state: &ControllerState, headers: &HeaderMap) -> bool {
    let expected = node_authorization(&lock(&state.token));
    headers.get("authorization").and_then(|value| value.to_str().ok()) == Some(expected.as_str())
}

async fn agent_socket(State(state): State<Arc<ControllerState>>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    if state.refuse.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let authorized = bearer_ok(&state, &headers);
    let protocol = headers.get(ankor_protocol::constants::PROTOCOL_HEADER).and_then(|value| value.to_str().ok()).map(str::to_owned);
    state.rec.record(channels::CONTROLLER, json!({ "connect": { "authorized": authorized, "protocol": protocol } }));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade.on_upgrade(move |socket| run_agent_socket(state, socket))
}

async fn run_agent_socket(state: Arc<ControllerState>, socket: WebSocket) {
    let id = state.connections.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, mut rx) = mpsc::unbounded_channel();
    *lock(&state.agent) = Some(AgentConn { id, tx: tx.clone() });
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let frame: Value = serde_json::from_str(text.as_str()).unwrap_or_else(|_| json!({ "not_json": text.as_str() }));
                        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("untyped");
                        check_contract(&state, &format!("{kind} frame"), ankor_protocol::schemas::s_agent_to_controller_message, &frame);
                        let hello = matches!(agent_message(&frame), Some(AgentToControllerMessage::Hello(_)));
                        state.rec.record(channels::CONTROLLER, json!({ (controller_entry::AGENT): frame }));
                        if hello && state.auto_welcome.load(Ordering::SeqCst) {
                            let _ = tx.send(Outgoing::Frame(json!(ControllerToAgentMessage::Welcome(ControllerWelcome {}))));
                        }
                    }
                    Some(Ok(Message::Binary(data))) => state.rec.record(channels::CONTROLLER, json!({ (controller_entry::AGENT_BINARY): data.len() })),
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame.as_ref().map_or((None, String::new()), |frame: &CloseFrame| (Some(frame.code), frame.reason.to_string()));
                        state.rec.record(channels::CONTROLLER, json!({ (controller_entry::AGENT_CLOSED): { "code": code, "reason": reason } }));
                        let _ = sink.send(Message::Close(frame)).await;
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        state.rec.record(channels::CONTROLLER, json!({ (controller_entry::AGENT_GONE): true }));
                        break;
                    }
                }
            }
            outgoing = rx.recv() => {
                match outgoing {
                    Some(Outgoing::Frame(frame)) => {
                        state.rec.record(channels::CONTROLLER, json!({ (controller_entry::CONTROLLER): frame }));
                        if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Outgoing::Drop) => {
                        state.rec.record(channels::CONTROLLER, json!({ "controller_dropped": true }));
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    let mut agent = lock(&state.agent);
    if agent.as_ref().is_some_and(|conn| conn.id == id) {
        *agent = None;
    }
}

async fn event_batch(State(state): State<Arc<ControllerState>>, headers: HeaderMap, body: Bytes) -> Response {
    let authorized = bearer_ok(&state, &headers);
    let parsed: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({ "not_json": String::from_utf8_lossy(&body) }));
    check_contract(&state, "event batch", ankor_protocol::schemas::s_event_batch, &parsed);
    // The batch checks only each frame's position, so one bad event cannot fail it; each frame is
    // parsed on its own, as a controller does before it quarantines one.
    for frame in parsed["frames"].as_array().into_iter().flatten() {
        let what = format!("event {}", frame["seq"]);
        check_contract(&state, &what, ankor_protocol::schemas::s_event_batch_frame, frame);
    }
    let (status, answer) = lock(&state.event_answers).pop_front().unwrap_or_else(|| (200, json!({ "rejected": [] })));
    state.rec.record(
        channels::EVENTS,
        json!({
            "authorized": authorized,
            "protocol": headers.get(ankor_protocol::constants::PROTOCOL_HEADER).and_then(|value| value.to_str().ok()),
            "content_type": headers.get("content-type").and_then(|value| value.to_str().ok()),
            "body": parsed,
            "answered": status,
        }),
    );
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(answer)).into_response()
}

async fn demo_upload(State(state): State<Arc<ControllerState>>, headers: HeaderMap, body: Bytes) -> Response {
    state.demos.wait().await;
    let status = lock(&state.demo_answers).pop_front().unwrap_or(200);
    state.rec.record(
        channels::DEMOS,
        json!({
            "headers": header_map(&headers),
            "bytes": body.len(),
            "sha256": hex::encode(Sha256::digest(&body)),
            "answered": status,
        }),
    );
    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR).into_response()
}

async fn unknown(State(state): State<Arc<ControllerState>>, uri: axum::http::Uri, method: axum::http::Method) -> Response {
    state.rec.record(channels::CONTROLLER_HTTP, json!({ "unknown": format!("{method} {uri}") }));
    StatusCode::NOT_FOUND.into_response()
}

async fn plugin_control(State(state): State<Arc<ControllerState>>, Query(query): Query<HashMap<String, String>>, upgrade: WebSocketUpgrade) -> Response {
    let allocation = query.get(PLUGIN_ALLOCATION_QUERY).cloned().unwrap_or_default();
    upgrade.on_upgrade(move |socket| run_plugin_control(state, allocation, socket))
}

async fn run_plugin_control(state: Arc<ControllerState>, allocation: String, socket: WebSocket) {
    let channel = channels::plugin(&allocation);
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    lock(&state.plugins).insert(allocation.clone(), tx);
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let report: Value = serde_json::from_str(text.as_str()).unwrap_or_else(|_| json!({ "not_json": text.as_str() }));
                        state.rec.record(channel.clone(), report);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            op = rx.recv() => {
                let Some(op) = op else { break };
                if sink.send(Message::Text(op.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    state.rec.record(channel, json!({ "control": "closed" }));
}
