//! The connection to the controller against a real WebSocket peer: the handshake and its refusals, the
//! welcome gate, backoff, request correlation, redials and shutdown.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ankor_agent::controller_connection::{BoxFuture, ControllerConnection, Defer, HandlerError, Hooks, Request, Signal, Timings, Watch, close_code};
use ankor_agent::logger::{Entry, Level, Logger};
use ankor_agent::testing::controller_peer::{ControllerPeer, PeerOptions};
use ankor_agent::testing::fixtures::launch_config;
use ankor_agent::testing::slots::launch_server;
use ankor_agent::testing::start_connection;
use ankor_agent::testing::wire;
use ankor_agent::testing::{agent_message, until, welcome};
use ankor_protocol::constants::{AGENT_ERROR_DETAIL_MAX_CHARS, AGENT_WELCOME_TIMEOUT_REASON, CONTROLLER_FRAME_MAX_BYTES, SHUTDOWN_REQUEST_WAIT, close_codes};
use ankor_protocol::helpers::{backoff_delay, node_authorization};
use ankor_protocol::types::{AgentErrorCode, AgentToControllerMessage, CloseReason, RequestType, SlotState};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

type OpenHook = Arc<dyn Fn(ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> + Send + Sync>;
type Handler = Arc<dyn Fn(Request) -> BoxFuture<'static, Result<Value, HandlerError>> + Send + Sync>;
type Gates = Arc<Mutex<Vec<(String, watch::Sender<bool>)>>>;

/// The reply to the `delete_demo` these tests send to see a request through.
fn answer() -> Handler {
    Arc::new(|_| Box::pin(async { Ok(json!({ "slot_id": "a", "file_name": "x.dem" })) }))
}

/// A `delete_demo` request with this id.
fn delete_demo(id: &str) -> Value {
    json!({ "type": "delete_demo", "id": id, "slot_id": "a", "file_name": "x.dem" })
}

struct TestHooks {
    url: Arc<Mutex<String>>,
    on_open: Option<OpenHook>,
    on_welcome: Option<OpenHook>,
    handler: Handler,
    closes: Arc<Mutex<Vec<(u16, String)>>>,
    failures: Arc<Mutex<Vec<String>>>,
    watches: Arc<Mutex<Vec<Value>>>,
    checks: Arc<AtomicUsize>,
}

impl TestHooks {
    fn new(url: &str) -> Self {
        Self {
            url: Arc::new(Mutex::new(url.to_owned())),
            on_open: None,
            on_welcome: None,
            handler: answer(),
            closes: Arc::default(),
            failures: Arc::default(),
            watches: Arc::default(),
            checks: Arc::default(),
        }
    }
}

impl Hooks for TestHooks {
    fn url(&self) -> String {
        self.url.lock().unwrap().clone()
    }

    fn on_open(&self, connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> {
        match &self.on_open {
            Some(hook) => hook(connection),
            None => Box::pin(async { Ok(()) }),
        }
    }

    fn on_welcome(&self, connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> {
        match &self.on_welcome {
            Some(hook) => hook(connection),
            None => Box::pin(async { Ok(()) }),
        }
    }

    fn on_close(&self, code: u16, reason: &str) {
        self.closes.lock().unwrap().push((code, reason.to_owned()));
    }

    fn on_watch(&self, watch: Watch) {
        let value = match watch {
            Watch::Resources(watch) => serde_json::to_value(watch).unwrap(),
            Watch::Logs(watch) => serde_json::to_value(watch).unwrap(),
        };

        self.watches.lock().unwrap().push(value);
    }

    fn on_failed(&self, reason: &str) {
        self.failures.lock().unwrap().push(reason.to_owned());
    }

    fn on_check_cs2_build(&self) {
        self.checks.fetch_add(1, Ordering::SeqCst);
    }

    fn handle(&self, request: Request, _defer: Defer) -> BoxFuture<'static, Result<Value, HandlerError>> {
        (self.handler)(request)
    }
}

/// Warnings and errors as the connection logs them: where an operator reads what to do.
fn recording() -> (Logger, Arc<Mutex<Vec<String>>>) {
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&lines);
    let log = Logger::new(
        "controller",
        Some(Arc::new(move |entry: Entry| {
            let message = entry.message.trim_start_matches("[controller] ");

            match entry.level {
                Level::Warn => seen.lock().unwrap().push(format!("warn {message}")),
                Level::Error => seen.lock().unwrap().push(format!("error {message}")),
                // The one info line kept: the backoff it waits, which tests read rather than time.
                Level::Info if message.starts_with("reconnecting in ") => seen.lock().unwrap().push(format!("info {message}")),
                _ => {}
            }
        })),
    );

    (log, lines)
}

/// The backoffs the agent waited, as it logged them.
fn backoffs(lines: &Mutex<Vec<String>>) -> Vec<String> {
    lines.lock().unwrap().iter().filter_map(|line| line.strip_prefix("info reconnecting in ")).map(str::to_owned).collect()
}

fn timings(min_ms: u64, max_ms: u64) -> Timings {
    Timings { min_backoff: Duration::from_millis(min_ms), max_backoff: Duration::from_millis(max_ms), random: || 0.5, ..Timings::default() }
}

fn start(hooks: TestHooks, timings: Timings) -> (ControllerConnection, Arc<TestHooks>) {
    start_logged(hooks, timings, Logger::null())
}

fn start_logged(hooks: TestHooks, timings: Timings, log: Logger) -> (ControllerConnection, Arc<TestHooks>) {
    let hooks = Arc::new(hooks);

    (start_connection("test-token".to_owned(), Arc::clone(&hooks) as Arc<dyn Hooks>, timings, log), hooks)
}

/// A `hello` the contract takes.
fn hello() -> AgentToControllerMessage {
    serde_json::from_value(json!({
        "type": "hello",
        "agent": { "version": "0.1.0", "image": "ankor-agent:latest", "image_id": format!("sha256:{}", "a".repeat(64)) },
        "host": { "cpu_count": 4, "os_name": null, "kernel": "6.12.48" },
        "public_ip": "203.0.113.7", "country": "DK",
        "config": { "slots": [], "pinned_ip": null, "controller": { "host": "127.0.0.1", "tls": false, "socket_path": "/ws/agent", "events_path": "/api/node/events" }, "auto_update": false, "demo_upload": null },
        "slot_status": [], "cs2": { "build_id": null, "latest_build_id": "19001", "image": "ankor-cs2:latest", "image_id": null, "update": null, "image_update": null },
        "unavailable": null, "demos": [], "server_exits": [],
        "event_backlog": { "servers": [], "server_count": 0, "event_count": 0, "reason": null },
        "agent_update": null,
    }))
    .unwrap()
}

fn says_hello() -> OpenHook {
    Arc::new(|connection| {
        Box::pin(async move {
            connection.push(&hello()).await;
            Ok(())
        })
    })
}

fn slot_status() -> AgentToControllerMessage {
    serde_json::from_value(json!({
        "type": "slot_status", "slot_id": "a", "state": "idle", "allocation_id": null, "match_id": null, "plugin_online": false, "gslt_state": "unchecked", "reserved": false, "server_steam_id": null,
    }))
    .unwrap()
}

/// Milliseconds between consecutive dials. A dial waits at least its backoff, so these are lower bounds.
fn gaps(dialled: &[Instant]) -> Vec<u128> {
    dialled.windows(2).map(|pair| (pair[1] - pair[0]).as_millis()).collect()
}

fn is_reply(id: &str) -> impl Fn(&Value) -> bool + '_ {
    move |message| matches!(agent_message(message), Some(AgentToControllerMessage::Reply(_))) && message["reply_to"] == id
}

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[tokio::test]
async fn increases_and_caps_backoff_when_hello_is_rejected_resetting_only_after_welcome() {
    let peer = Arc::new(ControllerPeer::start(PeerOptions { welcome: false, ..PeerOptions::default() }).await);
    let dialled: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let welcomed = Arc::new(AtomicUsize::new(0));
    let mut hooks = TestHooks::new(&peer.url);
    let (dials, answering, counted) = (Arc::clone(&dialled), Arc::clone(&peer), Arc::clone(&welcomed));

    hooks.on_open = Some(Arc::new(move |connection| {
        dials.lock().unwrap().push(Instant::now());

        let peer = Arc::clone(&answering);

        Box::pin(async move {
            let accepted = peer.accepted();
            let hellos = peer.hellos();

            connection.push(&hello()).await;
            until("the hello at the peer", || peer.hellos() > hellos).await;

            if accepted <= 4 {
                peer.close(u16::from(CloseCode::Again), "hello rejected");
            } else {
                peer.send_json(&welcome());
            }

            Ok(())
        })
    }));
    hooks.on_welcome = Some(Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }));

    let (log, lines) = recording();
    let (_connection, _) = start_logged(hooks, timings(40, 160), log);

    until("acceptance after four rejected hellos", || welcomed.load(Ordering::SeqCst) == 1).await;

    let rejected = gaps(&dialled.lock().unwrap());

    // Each dial waited out its backoff: 40, 80, 160, then capped at 160 rather than 320.
    assert!(rejected.iter().zip([40, 80, 160, 160]).all(|(gap, least)| *gap >= least), "{rejected:?}");
    assert_eq!(backoffs(&lines), ["40ms (attempt 1)", "80ms (attempt 2)", "160ms (attempt 3)", "160ms (attempt 4)"]);

    peer.terminate();
    until("reconnect after an accepted hello", || welcomed.load(Ordering::SeqCst) == 2).await;
    // Back to the first step after the welcome.
    assert_eq!(backoffs(&lines).last().map(String::as_str), Some("40ms (attempt 1)"));
    peer.expect_valid_frames();
}

#[tokio::test]
async fn is_welcomed_only_from_the_controllers_welcome_until_the_socket_closes() {
    let peer = ControllerPeer::start(PeerOptions { welcome: false, ..PeerOptions::default() }).await;
    let mut hooks = TestHooks::new(&peer.url);
    let welcomed = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&welcomed);

    hooks.on_open = Some(says_hello());
    hooks.on_welcome = Some(Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }));

    // No redial while the test looks at the closed socket.
    let (connection, _) = start(hooks, timings(60_000, 60_000));

    until("the hello", || peer.hellos() == 1).await;
    assert_eq!((connection.connected(), connection.welcomed()), (true, false));
    peer.send_json(&welcome());
    until("the welcome", || welcomed.load(Ordering::SeqCst) == 1).await;
    assert!(connection.welcomed());
    connection.push(&hello()).await;
    assert!(!connection.welcomed());
    peer.terminate();
    until("the close", || !connection.connected()).await;
    assert!(!connection.welcomed());
}

/// The welcome lapses with its socket, so a frame pushed during the backoff is refused when it is pushed
/// instead of waiting for the driver's turn.
#[tokio::test]
async fn refuses_a_frame_when_it_is_pushed_after_the_welcomed_socket_closes() {
    use futures_util::FutureExt as _;

    let peer = ControllerPeer::start(PeerOptions { welcome: false, ..PeerOptions::default() }).await;
    let mut hooks = TestHooks::new(&peer.url);

    hooks.on_open = Some(says_hello());

    // No redial while the test pushes into the closed socket.
    let (connection, _) = start(hooks, timings(60_000, 60_000));

    until("the hello", || peer.hellos() == 1).await;
    peer.send_json(&welcome());
    until("the welcome", || connection.welcomed()).await;
    peer.terminate();
    until("the close", || !connection.connected()).await;
    assert_eq!(connection.push(&slot_status()).now_or_never(), Some(false));
}

#[tokio::test]
async fn backs_off_when_hello_is_left_unanswered_and_cancels_its_deadline_after_welcome() {
    let peer = Arc::new(ControllerPeer::start(PeerOptions { welcome: false, ..PeerOptions::default() }).await);
    let dialled: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let welcomed = Arc::new(AtomicUsize::new(0));
    let mut hooks = TestHooks::new(&peer.url);
    let (dials, answering, counted) = (Arc::clone(&dialled), Arc::clone(&peer), Arc::clone(&welcomed));

    hooks.on_open = Some(Arc::new(move |connection| {
        dials.lock().unwrap().push(Instant::now());

        let peer = Arc::clone(&answering);

        Box::pin(async move {
            let third = peer.accepted() == 3;

            connection.push(&hello()).await;

            if third {
                until("the third hello", || peer.hellos() == 3).await;
                peer.send_json(&welcome());
            }

            Ok(())
        })
    }));
    hooks.on_welcome = Some(Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }));

    let (connection, _) = start(hooks, Timings { welcome_timeout: Duration::from_millis(40), ..timings(50, 400) });

    until("acceptance after two unanswered hellos", || welcomed.load(Ordering::SeqCst) == 1).await;

    // The 40 ms deadline, then a backoff of 50 and of 100: the second wait grew.
    let waited = gaps(&dialled.lock().unwrap());

    assert!(waited[0] >= 90 && waited[1] >= 140, "{waited:?}");
    pause(80).await;
    assert_eq!(peer.request(delete_demo("")).await["ok"], true);
    assert_eq!(peer.accepted(), 3);
    // Each unanswered hello ended with the agent's close, not a cut connection.
    assert_eq!(
        peer.closes(),
        [
            (u16::from(close_code(close_codes::NORMAL)), AGENT_WELCOME_TIMEOUT_REASON.to_owned()),
            (u16::from(close_code(close_codes::NORMAL)), AGENT_WELCOME_TIMEOUT_REASON.to_owned())
        ]
    );

    // A config hello on the welcomed socket waits for a welcome of its own.
    connection.push(&hello()).await;
    until("the redial after the config hello's deadline", || peer.accepted() == 4).await;
}

#[tokio::test]
async fn sends_the_bearer_token_on_the_upgrade_and_calls_on_open() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let mut hooks = TestHooks::new(&peer.url);

    hooks.on_open = Some(Arc::new(|connection| {
        Box::pin(async move {
            let reply = serde_json::from_value(json!({ "type": "reply", "reply_to": "r0", "request": "reserve_slot", "ok": true, "data": { "slot_id": "a" } }))
                .unwrap();

            connection.push(&reply).await;
            Ok(())
        })
    }));

    let hooks = Arc::new(hooks);
    let connection = start_connection(
        "agent-token-1".to_owned(),
        Arc::clone(&hooks) as Arc<dyn Hooks>,
        Timings { welcome_timeout: Duration::from_millis(20), ..Timings::default() },
        Logger::null(),
    );

    peer.wait_for("the reply", |message| matches!(agent_message(message), Some(AgentToControllerMessage::Reply(_)))).await;
    assert_eq!(peer.auth_header(), Some(node_authorization("agent-token-1")));

    // Only a hello starts the welcome deadline.
    pause(60).await;
    assert_eq!((peer.accepted(), connection.connected()), (1, true));
}

#[tokio::test]
async fn holds_back_what_the_node_reports_on_a_new_socket_until_the_controller_welcomes_its_hello() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let sent: Arc<Mutex<Vec<bool>>> = Arc::default();
    let mut hooks = TestHooks::new(&peer.url);
    let (before, after) = (Arc::clone(&sent), Arc::clone(&sent));

    hooks.on_open = Some(Arc::new(move |connection| {
        let sent = Arc::clone(&before);

        Box::pin(async move {
            let early = connection.push(&slot_status()).await;

            sent.lock().unwrap().push(early);
            connection.push(&hello()).await;

            // Written, but not yet welcomed.
            let unwelcomed = connection.push(&slot_status()).await;

            sent.lock().unwrap().push(unwelcomed);
            Ok(())
        })
    }));
    hooks.on_welcome = Some(Arc::new(move |connection| {
        let sent = Arc::clone(&after);

        Box::pin(async move {
            let welcomed = connection.push(&slot_status()).await;

            sent.lock().unwrap().push(welcomed);
            Ok(())
        })
    }));

    let (_connection, _) = start(hooks, Timings::default());

    peer.wait_for("the slot_status after welcome", |message| matches!(agent_message(message), Some(AgentToControllerMessage::SlotStatus(_)))).await;
    until("all three sends", || sent.lock().unwrap().len() == 3).await;
    assert_eq!(*sent.lock().unwrap(), [false, false, true]);
    assert!(matches!(
        peer.received().iter().map(agent_message).collect::<Vec<_>>().as_slice(),
        [Some(AgentToControllerMessage::Hello(_)), Some(AgentToControllerMessage::SlotStatus(_))]
    ));
}

#[tokio::test]
async fn reconnects_when_preparing_the_first_hello_fails() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let welcomed = Arc::new(AtomicUsize::new(0));
    let mut hooks = TestHooks::new(&peer.url);
    let (tried, counted) = (Arc::clone(&attempts), Arc::clone(&welcomed));

    hooks.on_open = Some(Arc::new(move |connection| {
        let first = tried.fetch_add(1, Ordering::SeqCst) == 0;

        Box::pin(async move {
            if first {
                return Err("Docker unavailable".into());
            }

            connection.push(&hello()).await;
            Ok(())
        })
    }));
    hooks.on_welcome = Some(Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }));

    let (_connection, hooks) = start(hooks, timings(10, 20));

    until("a successful hello after reconnect", || welcomed.load(Ordering::SeqCst) == 1).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!((peer.accepted(), peer.hellos()), (2, 1));
    // The failed hook dropped its socket as a dead one, without a close frame.
    assert_eq!(*hooks.closes.lock().unwrap(), [(u16::from(CloseCode::Abnormal), String::new())]);
    assert_eq!(peer.closes().first().map(|close| close.0), Some(u16::from(CloseCode::Abnormal)));
}

#[tokio::test]
async fn reconnects_if_preparing_the_post_welcome_state_fails() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut hooks = TestHooks::new(&peer.url);
    let tried = Arc::clone(&attempts);

    hooks.on_welcome = Some(Arc::new(move |_| {
        let first = tried.fetch_add(1, Ordering::SeqCst) == 0;

        Box::pin(async move { if first { Err("Docker unavailable".into()) } else { Ok(()) } })
    }));

    let (connection, _) = start(hooks, timings(10, 20));

    until("the first connection", || connection.connected()).await;
    peer.send_json(&welcome());
    until("reconnect after the failed report", || peer.accepted() == 2 && connection.connected()).await;
    peer.send_json(&welcome());
    until("the second welcome", || attempts.load(Ordering::SeqCst) == 2).await;
    assert!(connection.connected());
}

#[tokio::test]
async fn does_not_close_the_replacement_socket_when_an_earlier_on_open_fails() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (release, released) = watch::channel(false);
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut hooks = TestHooks::new(&peer.url);
    let tried = Arc::clone(&attempts);

    hooks.on_open = Some(Arc::new(move |_| {
        let first = tried.fetch_add(1, Ordering::SeqCst) == 0;
        let mut released = released.clone();

        Box::pin(async move {
            if first {
                let _ = released.wait_for(|done| *done).await;
                return Err("old Docker lookup failed".into());
            }

            Ok(())
        })
    }));

    let (connection, _) = start(hooks, timings(10, 20));

    until("the first on_open", || attempts.load(Ordering::SeqCst) == 1).await;
    connection.redial();
    until("the replacement socket", || attempts.load(Ordering::SeqCst) == 2).await;
    release.send_replace(true);
    pause(20).await;
    assert_eq!(peer.request(delete_demo("")).await["ok"], true);
    assert!(connection.connected());
    // The failed hook asked for a redial, which a stale socket's hook must not cause.
    assert_eq!(peer.accepted(), 2);
}

#[tokio::test]
async fn ignores_a_request_whose_id_is_still_running_and_takes_the_id_again_once_replied() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (log, lines) = recording();
    let (release, released) = watch::channel(false);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let mut hooks = TestHooks::new(&peer.url);

    hooks.handler = Arc::new(move |_| {
        let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
        let mut released = released.clone();

        Box::pin(async move {
            if first {
                let _ = released.wait_for(|done| *done).await;
            }

            Ok(json!({ "slot_id": "a", "file_name": "x.dem" }))
        })
    });

    let (connection, _) = start_logged(hooks, timings(10, 20), log);

    until("the connection", || connection.connected()).await;
    peer.send_json(&delete_demo("dup"));
    until("the first request at the handler", || calls.load(Ordering::SeqCst) == 1).await;
    peer.send_json(&delete_demo("dup"));
    until("the duplicate warning", || lines.lock().unwrap().iter().any(|line| line.contains("dup"))).await;
    assert_eq!(*lines.lock().unwrap(), ["warn ignoring delete_demo dup: a request with that id is still running"]);
    release.send_replace(true);
    until("the first reply", || peer.received().iter().filter(|message| is_reply("dup")(message)).count() == 1).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    peer.send_json(&delete_demo("dup"));
    until("the reply to the reused id", || peer.received().iter().filter(|message| is_reply("dup")(message)).count() == 2).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn hands_a_watch_to_on_watch_and_never_routes_it_to_the_handler() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let handled = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&handled);
    let mut hooks = TestHooks::new(&peer.url);

    hooks.handler = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Value::Null) })
    });

    let (_connection, hooks) = start(hooks, Timings::default());
    let logs = json!({ "type": "watch_logs", "source": { "kind": "cs2_update", "update_id": "00000000-0000-4000-8000-00000000a0a0" }, "replay_id": "r1" });

    peer.wait_for_connection().await;
    peer.send_json(&json!({ "type": "watch_resources", "replay": false }));
    peer.send_json(&logs);
    until("the watches", || hooks.watches.lock().unwrap().len() == 2).await;

    let mut expected = logs;

    expected.as_object_mut().unwrap().remove("type");
    assert_eq!(*hooks.watches.lock().unwrap(), [json!({ "replay": false }), expected]);
    assert_eq!(handled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hands_check_cs2_build_to_its_hook_and_answers_nothing() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (_connection, hooks) = start(TestHooks::new(&peer.url), Timings::default());

    peer.wait_for_connection().await;
    peer.send_json(&json!({ "type": "check_cs2_build" }));
    until("the check", || hooks.checks.load(Ordering::SeqCst) == 1).await;
    pause(50).await;
    assert!(peer.received_where(|message| matches!(message, AgentToControllerMessage::Reply(_))).is_empty());
}

#[tokio::test]
async fn a_connection_made_before_its_driver_refuses_frames_and_dials_only_once_started() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (connection, driver) = ControllerConnection::new("test-token".to_owned(), Timings::default(), Logger::null());

    assert!(!connection.push(&slot_status()).await);
    pause(50).await;
    assert_eq!(peer.accepted(), 0);
    assert!(!connection.started());

    let mut hooks = TestHooks::new(&peer.url);

    hooks.on_open = Some(says_hello());
    driver.start(Arc::new(hooks));
    assert!(connection.started());
    until("the welcome", || connection.welcomed()).await;
    assert!(connection.push(&slot_status()).await);
}

#[tokio::test]
async fn a_slow_hello_for_a_socket_that_dropped_never_reaches_the_next_one() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    // The build read both sockets' hellos wait on, released once the second socket is open.
    let (read, reading) = watch::channel(false);
    let mut hooks = TestHooks::new(&peer.url);

    hooks.on_open = Some(Arc::new(move |connection| {
        let mut reading = reading.clone();

        Box::pin(async move {
            let _ = reading.wait_for(|done| *done).await;
            connection.push(&hello()).await;
            Ok(())
        })
    }));

    let (_connection, _) = start(hooks, timings(20, 40));

    peer.wait_for_connection().await;
    peer.terminate();
    until("a second socket", || peer.accepted() == 2).await;
    read.send_replace(true);
    until("the hello", || peer.hellos() >= 1).await;
    pause(100).await;
    assert_eq!(peer.hellos(), 1);
}

#[tokio::test]
async fn pings_the_controller_on_its_own_timer() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (connection, _) = start(TestHooks::new(&peer.url), Timings { ping_interval: Duration::from_millis(20), ..Timings::default() });

    peer.wait_for_connection().await;
    until("two pings", || peer.pings() >= 2).await;
    assert!(connection.connected());
}

#[tokio::test]
async fn drops_a_socket_whose_controller_stops_answering_pings_and_dials_again() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (_connection, _) = start(TestHooks::new(&peer.url), Timings { ping_interval: Duration::from_millis(20), ..timings(20, 40) });

    peer.wait_for_connection().await;
    // The socket stays open but nothing behind it answers: what a half-open link looks like.
    peer.pause();
    until("a second socket", || peer.accepted() == 2).await;
}

#[tokio::test]
async fn drops_a_socket_whose_controller_stops_reading_while_a_frame_is_being_written() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (connection, _) = start(TestHooks::new(&peer.url), Timings { ping_interval: Duration::from_millis(100), ..timings(20, 40) });

    peer.wait_for_connection().await;
    peer.pause();

    // Frames large enough to fill both socket buffers, so a write waits on the controller.
    let mut big = serde_json::to_value(hello()).unwrap();

    big["host"]["kernel"] = json!("x".repeat(1 << 20));

    let big: AgentToControllerMessage = serde_json::from_value(big).unwrap();
    let writer = connection.clone();

    tokio::spawn(async move {
        for _ in 0..64 {
            writer.push(&big).await;
        }
    });
    until("a second socket", || peer.accepted() == 2).await;
}

#[tokio::test]
async fn a_reply_leaves_after_what_its_request_queued_before_it() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let cell: Arc<std::sync::OnceLock<ControllerConnection>> = Arc::default();
    let reaching = Arc::clone(&cell);
    let mut hooks = TestHooks::new(&peer.url);

    hooks.on_open = Some(says_hello());
    hooks.handler = Arc::new(move |_| {
        let connection = reaching.get().cloned();

        Box::pin(async move {
            if let Some(connection) = connection {
                drop(connection.push(&slot_status()));
            }

            Ok(json!({ "slot_id": "a", "file_name": "x.dem" }))
        })
    });

    let (connection, _) = start(hooks, Timings::default());

    let _ = cell.set(connection.clone());
    peer.wait_for_connection().await;
    until("the welcome", || connection.welcomed()).await;

    for _ in 0..20 {
        assert_eq!(peer.request(delete_demo("")).await["ok"], true);
    }

    let messages: Vec<AgentToControllerMessage> =
        peer.received().iter().filter_map(agent_message).filter(|message| !matches!(message, AgentToControllerMessage::Hello(_))).collect();

    assert_eq!(messages.len(), 40);
    assert!(messages.chunks(2).all(|pair| matches!(pair, [AgentToControllerMessage::SlotStatus(_), AgentToControllerMessage::Reply(_)])));
}

#[tokio::test]
async fn correlates_concurrent_requests_by_id_one_reply_each() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let gates: Gates = Arc::default();
    let opened = Arc::clone(&gates);
    let mut hooks = TestHooks::new(&peer.url);

    hooks.handler = Arc::new(move |request| {
        let (release, mut released) = watch::channel(false);
        let message = serde_json::to_value(&request.message).unwrap();

        opened.lock().unwrap().push((request.id.clone(), release));

        Box::pin(async move {
            let _ = released.wait_for(|done| *done).await;

            Ok(if request.request_type == RequestType::StartServer {
                json!({ "allocation_id": message["allocation_id"], "slot_id": "a" })
            } else {
                json!({ "allocation_id": "00000000-0000-4000-8000-000000000001" })
            })
        })
    });

    let (_connection, _) = start(hooks, Timings::default());
    let release = |id: &str| gates.lock().unwrap().iter().find(|(each, _)| each == id).unwrap().1.send_replace(true);

    peer.wait_for_connection().await;
    peer.send_json(
        &json!({ "type": "start_server", "id": "req-slow", "allocation_id": "11111111-1111-4111-8111-111111111111", "launch": launch_config(&json!({})) }),
    );
    peer.send_json(&json!({ "type": "stop_server", "id": "req-fast", "allocation_id": "00000000-0000-4000-8000-000000000001" }));
    // Both handlers in flight, then released out of order.
    until("both requests at the handler", || gates.lock().unwrap().len() == 2).await;
    release("req-fast");

    let fast = peer.wait_for("the reply to req-fast", is_reply("req-fast")).await;

    release("req-slow");

    let slow = peer.wait_for("the reply to req-slow", is_reply("req-slow")).await;

    assert_eq!(
        fast,
        json!({ "type": "reply", "reply_to": "req-fast", "request": "stop_server", "ok": true, "data": { "allocation_id": "00000000-0000-4000-8000-000000000001" } })
    );
    assert_eq!(
        slow,
        json!({ "type": "reply", "reply_to": "req-slow", "request": "start_server", "ok": true, "data": { "allocation_id": "11111111-1111-4111-8111-111111111111", "slot_id": "a" } })
    );
    assert_eq!(peer.received_where(|message| matches!(message, AgentToControllerMessage::Reply(_))).len(), 2);
}

#[tokio::test]
async fn replies_ok_false_with_the_handler_error_message() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let mut hooks = TestHooks::new(&peer.url);

    hooks.handler = Arc::new(|_| Box::pin(async { Err("slot a is running, refusing start".into()) }));

    let (_connection, _) = start(hooks, Timings::default());

    peer.wait_for_connection().await;
    peer.send_json(
        &json!({ "type": "start_server", "id": "req-1", "allocation_id": "11111111-1111-4111-8111-111111111111", "launch": launch_config(&json!({})) }),
    );

    let reply = peer.wait_for("the reply to req-1", is_reply("req-1")).await;

    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"], json!({ "code": AgentErrorCode::Failed, "detail": "slot a is running, refusing start" }));
}

#[tokio::test]
async fn replies_ok_false_for_a_schema_invalid_request_that_still_carries_an_id() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let handled = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&handled);
    let mut hooks = TestHooks::new(&peer.url);

    hooks.handler = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Value::Null) })
    });

    let (_connection, _) = start(hooks, Timings::default());
    let mut launch = serde_json::to_value(launch_config(&json!({}))).unwrap();

    launch["max_players"] = json!("two");
    peer.wait_for_connection().await;
    peer.send_json(&json!({ "type": "start_server", "id": "req-bad", "allocation_id": "not-a-uuid", "launch": launch }));

    let reply = peer.wait_for("the reply to req-bad", is_reply("req-bad")).await;
    let detail = reply["error"]["detail"].as_str().unwrap();

    assert_eq!(
        (reply["request"].clone(), reply["ok"].clone(), reply["error"]["code"].clone()),
        (json!(RequestType::StartServer), json!(false), json!(AgentErrorCode::InvalidRequest))
    );
    // Each refused field by its path, so the controller can tell what it sent wrong.
    assert!(regex::Regex::new(r"^allocation_id: \S.*; launch\.max_players: \S").unwrap().is_match(detail), "{detail}");
    assert_eq!(handled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn refuses_a_request_that_only_the_contract_finds_wrong() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let handled = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&handled);
    let mut hooks = TestHooks::new(&peer.url);

    hooks.handler = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Value::Null) })
    });

    let (_connection, _) = start(hooks, Timings::default());
    let launch = serde_json::to_value(launch_config(&json!({}))).unwrap();

    // A string reads into the request's type, so only the contract's UUID rule refuses it.
    peer.wait_for_connection().await;
    peer.send_json(&json!({ "type": "start_server", "id": "req-uuid", "allocation_id": "not-a-uuid", "launch": launch }));

    let reply = peer.wait_for("the reply to req-uuid", is_reply("req-uuid")).await;

    assert_eq!((reply["ok"].clone(), reply["error"]["code"].clone()), (json!(false), json!(AgentErrorCode::InvalidRequest)));
    assert!(reply["error"]["detail"].as_str().unwrap().starts_with("allocation_id: "), "{reply}");
    assert_eq!(handled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn answers_a_request_with_an_id_but_no_string_type_with_invalid_request() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (_connection, _) = start(TestHooks::new(&peer.url), Timings::default());

    peer.wait_for_connection().await;
    peer.send_json(&json!({ "id": "req-untyped" }));
    peer.send_json(&json!({ "id": "req-numbered", "type": 7 }));

    for id in ["req-untyped", "req-numbered"] {
        let reply = peer.wait_for(id, is_reply(id)).await;

        assert_eq!(
            (reply["request"].clone(), reply["ok"].clone(), reply["error"]["code"].clone()),
            (Value::Null, json!(false), json!(AgentErrorCode::InvalidRequest))
        );
    }
}

#[tokio::test]
async fn answers_a_request_of_a_type_it_does_not_know_with_unsupported_request() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (_connection, _) = start(TestHooks::new(&peer.url), Timings::default());
    let limit = usize::try_from(AGENT_ERROR_DETAIL_MAX_CHARS).unwrap();

    peer.wait_for_connection().await;
    peer.send_json(&json!({ "type": "restart_server", "id": "req-new", "allocation_id": "00000000-0000-4000-8000-000000000001" }));

    let reply = peer.wait_for("the reply to req-new", is_reply("req-new")).await;

    assert_eq!(reply["request"], Value::Null);
    assert_eq!(reply["error"], json!({ "code": AgentErrorCode::UnsupportedRequest, "detail": "unsupported request type restart_server" }));

    peer.send_json(&json!({ "type": "x".repeat(limit), "id": "req-long" }));

    let long = peer.wait_for("the reply to req-long", is_reply("req-long")).await;
    let expected = format!("{:x<limit$}", "unsupported request type ");

    assert_eq!(long["error"], json!({ "code": AgentErrorCode::UnsupportedRequest, "detail": expected }));
}

#[tokio::test]
async fn reads_a_frame_of_controller_frame_max_bytes_and_closes_with_1009_and_dials_again_on_one_byte_more() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (connection, _) = start(TestHooks::new(&peer.url), timings(10, 20));
    let limit = usize::try_from(CONTROLLER_FRAME_MAX_BYTES).unwrap();
    // A request padded with a key the contract ignores to exactly `bytes` on the wire.
    let padded = |id: &str, bytes: usize| {
        let frame = |pad: &str| json!({ "type": "delete_demo", "id": id, "slot_id": "a", "file_name": "x.dem", "pad": pad }).to_string();

        frame(&"x".repeat(bytes - frame("").len()))
    };

    until("the first connection", || connection.connected()).await;
    peer.send(&padded("req-at-limit", limit));
    peer.wait_for("the reply to a frame at the limit", is_reply("req-at-limit")).await;
    peer.send(&padded("req-over-limit", limit + 1));
    until("the close", || peer.closes().len() == 1).await;
    assert_eq!(peer.closes()[0].0, u16::from(close_code(close_codes::MESSAGE_TOO_BIG)));
    until("the redial", || peer.accepted() == 2 && connection.connected()).await;
    assert!(!peer.received().iter().any(is_reply("req-over-limit")));
}

#[tokio::test]
async fn drops_garbage_frames_without_replying() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (_connection, hooks) = start(TestHooks::new(&peer.url), Timings::default());

    peer.wait_for_connection().await;
    peer.send("not json at all");
    peer.send("null");
    peer.send_json(&json!({ "type": "unknown_type" }));
    peer.send_json(&json!({ "type": "watch_resources", "replay": false }));
    until("the watch", || hooks.watches.lock().unwrap().len() == 1).await;
    assert!(peer.received_where(|message| matches!(message, AgentToControllerMessage::Reply(_))).is_empty());
}

#[tokio::test]
async fn reconnects_after_the_server_drops_the_socket() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (connection, hooks) = start(TestHooks::new(&peer.url), timings(20, 40));

    until("the first connection", || connection.connected()).await;
    peer.close(u16::from(CloseCode::Error), "server restart");
    until("the disconnect", || !connection.connected()).await;
    until("the reconnect", || connection.connected()).await;
    peer.send_json(&json!({ "type": "watch_resources", "replay": false }));
    until("a watch after reconnect", || hooks.watches.lock().unwrap().len() == 1).await;
}

/// A controller that answers every upgrade with one HTTP status, counting the dials.
async fn refusing_controller(status: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws/agent", listener.local_addr().unwrap());
    let dials = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&dials);

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);

            let mut head = [0; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut head).await;
            let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nConnection: close\r\ncontent-length: 0\r\n\r\n").as_bytes()).await;
        }
    });

    (url, dials)
}

#[tokio::test]
async fn stops_dialling_for_good_when_the_controller_answers_401_or_426_and_tells_the_operator_what_to_do() {
    let cases = [
        ("401 Unauthorized", "refused this node's token", "add it again and set this host up with its token"),
        ("426 Upgrade Required", "uses a protocol version incompatible with this agent's", "run node-setup.sh --update"),
    ];

    for (status, reason, remedy) in cases {
        let (url, dials) = refusing_controller(status).await;
        let (log, lines) = recording();
        let (connection, hooks) = start_logged(TestHooks::new(&url), timings(5, 10), log);

        until("the refusal", || hooks.failures.lock().unwrap().len() == 1).await;
        connection.redial();
        pause(100).await;
        assert_eq!(dials.load(Ordering::SeqCst), 1);
        assert!(hooks.failures.lock().unwrap()[0].contains(reason), "{:?}", hooks.failures);
        assert!(lines.lock().unwrap().iter().any(|line| line.contains(remedy)), "{lines:?}");
        // The dial that never opened ends as an abnormal close.
        assert_eq!(*hooks.closes.lock().unwrap(), [(u16::from(CloseCode::Abnormal), String::new())]);
    }
}

#[tokio::test]
async fn keeps_dialling_a_controller_that_answers_503_or_429() {
    for status in ["503 Service Unavailable", "429 Too Many Requests"] {
        let (url, dials) = refusing_controller(status).await;
        let (_connection, hooks) = start(TestHooks::new(&url), timings(5, 10));

        // Still dialling is the proof: a refusal stops the dials for good.
        until("three dials", || dials.load(Ordering::SeqCst) >= 3).await;
        assert!(hooks.failures.lock().unwrap().is_empty());
        assert!(hooks.closes.lock().unwrap().iter().all(|close| *close == (u16::from(CloseCode::Abnormal), String::new())));
        assert!(!hooks.closes.lock().unwrap().is_empty());
    }
}

/// What a scripted controller does with each socket.
#[derive(Clone)]
enum Step {
    Send(Value),
    Close(CloseCode, CloseReason),
}

/// A controller that runs `steps` on every socket, and records the dials, the frames and the close codes
/// the agent sent.
struct Scripted {
    url: String,
    dials: Arc<Mutex<Vec<Instant>>>,
    received: Arc<Mutex<Vec<Value>>>,
    closed_by: Arc<Mutex<Vec<u16>>>,
}

async fn scripted_controller(steps: Vec<Step>) -> Scripted {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let scripted = Scripted {
        url: format!("ws://{}/ws/agent", listener.local_addr().unwrap()),
        dials: Arc::default(),
        received: Arc::default(),
        closed_by: Arc::default(),
    };
    let (dials, received, closed_by) = (Arc::clone(&scripted.dials), Arc::clone(&scripted.received), Arc::clone(&scripted.closed_by));

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                continue;
            };
            let (steps, received, closed_by) = (steps.clone(), Arc::clone(&received), Arc::clone(&closed_by));

            dials.lock().unwrap().push(Instant::now());
            tokio::spawn(async move {
                for step in steps {
                    let message = match step {
                        Step::Send(value) => Message::Text(value.to_string().into()),
                        Step::Close(code, reason) => Message::Close(Some(CloseFrame { code, reason: wire(reason).into() })),
                    };
                    let _ = socket.send(message).await;
                }

                while let Some(Ok(message)) = socket.next().await {
                    match message {
                        Message::Text(text) => received.lock().unwrap().push(serde_json::from_str(&text).unwrap()),
                        Message::Close(Some(frame)) => closed_by.lock().unwrap().push(u16::from(frame.code)),
                        _ => {}
                    }
                }
            });
        }
    });

    scripted
}

fn hello_refused(detail: &str) -> Step {
    Step::Send(json!({ "type": "hello_refused", "reason": "invalid_hello", "detail": detail }))
}

#[tokio::test]
async fn stops_dialling_for_good_when_the_controller_refuses_its_hello() {
    let controller =
        scripted_controller(vec![hello_refused("missing host.cpu_count"), Step::Close(close_code(close_codes::POLICY), CloseReason::HelloRefused)]).await;
    let (log, lines) = recording();
    let (_connection, hooks) = start_logged(TestHooks::new(&controller.url), timings(5, 10), log);

    until("the refusal and the close", || hooks.failures.lock().unwrap().len() == 1 && hooks.closes.lock().unwrap().len() == 1).await;
    pause(100).await;
    // The controller's close, not one the agent raced it with.
    assert_eq!(*hooks.closes.lock().unwrap(), [(u16::from(close_code(close_codes::POLICY)), wire(CloseReason::HelloRefused))]);
    assert_eq!(controller.dials.lock().unwrap().len(), 1);
    assert!(hooks.failures.lock().unwrap()[0].contains("refused this node's hello (invalid_hello): missing host.cpu_count"));
    // A hello this agent cannot get right is fixed by the image the controller names.
    assert!(lines.lock().unwrap().iter().any(|line| line.contains("run node-setup.sh --update")));
}

#[tokio::test]
async fn closes_the_socket_itself_when_the_controller_refuses_its_hello_but_leaves_the_socket_open() {
    let controller = scripted_controller(vec![hello_refused("missing config")]).await;
    let (log, lines) = recording();
    let (_connection, hooks) = start_logged(TestHooks::new(&controller.url), Timings { refused_close: Duration::from_millis(20), ..timings(5, 10) }, log);

    until("the agent's close", || controller.closed_by.lock().unwrap().len() == 1).await;
    pause(100).await;
    assert_eq!(*controller.closed_by.lock().unwrap(), [u16::from(close_code(close_codes::NORMAL))]);
    assert!(hooks.failures.lock().unwrap()[0].contains("(invalid_hello): missing config"));
    assert!(lines.lock().unwrap().iter().any(|line| line.contains("node-setup.sh --update")));
    assert_eq!(controller.dials.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn keeps_dialling_with_its_backoff_when_the_controller_refuses_a_second_socket_for_the_node() {
    let controller = scripted_controller(vec![Step::Close(close_code(close_codes::POLICY), CloseReason::NodeConnected)]).await;
    let (log, lines) = recording();
    let (_connection, _) = start_logged(TestHooks::new(&controller.url), timings(20, 200), log);

    until("three dials", || controller.dials.lock().unwrap().len() >= 3).await;
    assert_eq!(backoffs(&lines)[..2], ["20ms (attempt 1)", "40ms (attempt 2)"]);
    assert!(lines.lock().unwrap().iter().any(|line| line.starts_with("warn ") && line.contains("already connected") && line.contains("one agent per node")));
}

#[tokio::test]
async fn dials_again_when_the_controller_closes_for_a_duplicate_hello() {
    let controller = scripted_controller(vec![Step::Close(close_code(close_codes::POLICY), CloseReason::DuplicateHello)]).await;
    let (_connection, hooks) = start(TestHooks::new(&controller.url), timings(20, 200));

    until("a second dial", || controller.dials.lock().unwrap().len() >= 2).await;
    assert!(hooks.failures.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stops_dialling_when_the_controller_closes_for_a_deleted_node() {
    let controller = scripted_controller(vec![Step::Close(close_code(close_codes::POLICY), CloseReason::AgentDeleted)]).await;
    let (log, lines) = recording();
    let (_connection, hooks) = start_logged(TestHooks::new(&controller.url), timings(20, 200), log);

    until("the failure", || hooks.failures.lock().unwrap().len() == 1).await;
    pause(100).await;
    assert!(hooks.failures.lock().unwrap()[0].contains("deleted this node"));
    assert!(lines.lock().unwrap().iter().any(|line| line.contains("add the node again")));
    assert_eq!(controller.dials.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn keeps_dialling_when_the_controller_closes_with_agent_deleted_under_a_code_other_than_1008() {
    let controller = scripted_controller(vec![Step::Close(close_code(close_codes::NORMAL), CloseReason::AgentDeleted)]).await;
    let (log, lines) = recording();
    let (_connection, hooks) = start_logged(TestHooks::new(&controller.url), timings(5, 10), log);

    until("three dials", || controller.dials.lock().unwrap().len() >= 3).await;
    assert!(hooks.failures.lock().unwrap().is_empty());
    // Only the controller's 1008 says another agent holds the node.
    assert!(!lines.lock().unwrap().iter().any(|line| line.contains("already connected")));
}

#[tokio::test]
async fn answers_nothing_more_once_the_controller_has_refused_its_hello() {
    let controller = scripted_controller(vec![hello_refused("missing config"), Step::Send(delete_demo("req-late"))]).await;
    let handled = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&handled);
    let mut hooks = TestHooks::new(&controller.url);

    hooks.handler = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Value::Null) })
    });

    let (_connection, hooks) = start(hooks, Timings { refused_close: Duration::from_secs(10), ..Timings::default() });

    until("the refusal", || hooks.failures.lock().unwrap().len() == 1).await;
    pause(100).await;
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    assert_eq!(*controller.received.lock().unwrap(), Vec::<Value>::new());
}

#[tokio::test]
async fn does_not_reconnect_after_shutdown() {
    let peer = ControllerPeer::start(PeerOptions::default()).await;
    let (connection, _) = start(TestHooks::new(&peer.url), timings(20, 40));

    until("the first connection", || connection.connected()).await;
    connection.shutdown(Signal::Sigterm).await;
    pause(150).await;
    assert!(!connection.connected());
    assert_eq!(peer.accepted(), 1);
}

#[tokio::test]
async fn send_reports_failure_instead_of_failing_while_disconnected() {
    let (connection, _) = start(TestHooks::new("ws://127.0.0.1:1/ws/agent"), timings(10_000, 10_000));
    let reply =
        serde_json::from_value(json!({ "type": "reply", "reply_to": "r0", "request": "reserve_slot", "ok": true, "data": { "slot_id": "a" } })).unwrap();

    assert!(!connection.push(&reply).await);
}

mod redial {
    use super::*;

    #[tokio::test]
    async fn closes_the_old_socket_with_1000_as_it_dials_the_new_address() {
        let old = ControllerPeer::start(PeerOptions::default()).await;
        let next = ControllerPeer::start(PeerOptions::default()).await;
        let (connection, hooks) = start(TestHooks::new(&old.url), timings(20, 40));

        until("the first connection", || connection.connected()).await;
        *hooks.url.lock().unwrap() = next.url.clone();
        connection.redial();
        until("the old socket closed", || old.closes().len() == 1).await;
        assert_eq!(old.closes()[0], (u16::from(close_code(close_codes::NORMAL)), String::new()));
        until("the new address", || next.accepted() == 1 && connection.connected()).await;
        assert_eq!(old.accepted(), 1);
    }

    #[tokio::test]
    async fn dials_nothing_once_shut_down() {
        let peer = ControllerPeer::start(PeerOptions::default()).await;
        let (connection, _) = start(TestHooks::new(&peer.url), timings(20, 40));

        until("the first connection", || connection.connected()).await;
        connection.shutdown(Signal::Sigterm).await;
        connection.redial();
        pause(100).await;
        assert_eq!(peer.accepted(), 1);
    }

    /// A `set_config` that moves the controller while the agent stops: the shutdown keeps its close, and the
    /// new address is never dialled.
    #[tokio::test]
    async fn keeps_the_shutdown_close_when_a_redial_comes_during_it() {
        let old = ControllerPeer::start(PeerOptions::default()).await;
        let next = ControllerPeer::start(PeerOptions::default()).await;
        let (connection, hooks) = start(TestHooks::new(&old.url), timings(20, 40));

        until("the first connection", || connection.connected()).await;
        *hooks.url.lock().unwrap() = next.url.clone();

        // Joined in order: the shutdown is under way when the redial comes.
        tokio::join!(connection.shutdown(Signal::Sigterm), async { connection.redial() });
        until("the old socket closed", || old.closes().len() == 1).await;
        pause(150).await;
        assert_eq!(next.accepted(), 0);
        assert_eq!(old.closes()[0], (u16::from(close_code(close_codes::NORMAL)), Signal::Sigterm.name()));
    }

    #[tokio::test]
    async fn dials_at_once_during_a_backoff_and_the_backoff_dials_nothing_after_it() {
        let peer = ControllerPeer::start(PeerOptions::default()).await;
        let (connection, _) = start(TestHooks::new(&peer.url), timings(200, 200));

        until("the first connection", || connection.connected()).await;
        peer.close(u16::from(CloseCode::Error), "server restart");
        until("the disconnect", || !connection.connected()).await;
        connection.redial();
        until("the redial", || peer.accepted() == 2).await;
        // Past the backoff that was pending when the redial came.
        pause(300).await;
        assert_eq!(peer.accepted(), 2);
        assert!(connection.connected());
    }

    #[tokio::test]
    async fn gives_the_new_sockets_hello_its_own_deadline_when_the_old_hello_was_still_waiting_for_its_welcome() {
        let peer = ControllerPeer::start(PeerOptions { welcome: false, ..PeerOptions::default() }).await;
        let mut hooks = TestHooks::new(&peer.url);

        hooks.on_open = Some(says_hello());

        let (connection, _) = start(hooks, Timings { welcome_timeout: Duration::from_millis(100), ..timings(20, 40) });

        until("the first hello", || peer.hellos() == 1).await;
        connection.redial();
        until("the hello on the new socket", || peer.hellos() == 2).await;
        until("the redial after the new hello's deadline", || peer.accepted() == 3).await;
    }

    #[tokio::test]
    async fn leaves_a_request_that_settles_after_the_redial_unanswered_on_the_new_socket() {
        for fails in [false, true] {
            let peer = ControllerPeer::start(PeerOptions::default()).await;
            let (release, released) = watch::channel(false);
            let mut hooks = TestHooks::new(&peer.url);

            hooks.handler = Arc::new(move |_| {
                let mut released = released.clone();

                Box::pin(async move {
                    let _ = released.wait_for(|done| *done).await;

                    if fails { Err("too late".into()) } else { Ok(json!({ "slot_id": "a", "file_name": "x.dem" })) }
                })
            });

            let (connection, _) = start(hooks, timings(20, 40));

            until("the first connection", || connection.connected()).await;
            peer.send_json(&delete_demo("req-old"));
            pause(50).await;
            connection.redial();
            until("the new socket", || peer.accepted() == 2 && connection.connected()).await;
            release.send_replace(true);
            pause(100).await;
            assert!(peer.received_where(|message| matches!(message, AgentToControllerMessage::Reply(_))).is_empty());
        }
    }
}

#[test]
fn backoff_delay_doubles_from_min_to_max_and_stays_inside_the_jitter_band() {
    let at = |attempt: u32, random: f64| backoff_delay(attempt, Duration::from_secs(1), Duration::from_secs(30), || random);

    assert_eq!((1..=7).map(|attempt| at(attempt, 0.5).as_secs()).collect::<Vec<_>>(), [1, 2, 4, 8, 16, 30, 30]);

    for attempt in 1..=8 {
        let middle = at(attempt, 0.5);

        assert!(at(attempt, 0.0) <= middle && middle <= at(attempt, 1.0));
        assert!(at(attempt, 0.0) >= Duration::from_secs(1));
    }
}

mod intentional_shutdown {
    use super::*;

    #[tokio::test]
    async fn flushes_its_signal_before_closing() {
        for (signal, name) in [(Signal::Sigint, "SIGINT"), (Signal::Sigterm, "SIGTERM"), (Signal::Sigquit, "SIGQUIT")] {
            let peer = ControllerPeer::start(PeerOptions::default()).await;
            let (connection, _) = start(TestHooks::new(&peer.url), Timings::default());

            peer.wait_for_connection().await;
            until("the connection", || connection.connected()).await;

            let started = Instant::now();

            connection.shutdown(signal).await;
            // The controller's close ends the wait, not the two-second bound.
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(peer.received().contains(&json!({ "type": "shutdown", "signal": name })));
            assert!(!connection.connected());
        }
    }

    #[tokio::test]
    async fn dials_nothing_more_when_it_shuts_down_during_a_backoff() {
        let peer = ControllerPeer::start(PeerOptions::default()).await;
        let (connection, _) = start(TestHooks::new(&peer.url), timings(100, 100));

        until("the connection", || connection.connected()).await;
        peer.close(u16::from(CloseCode::Error), "server restart");
        until("the disconnect", || !connection.connected()).await;
        connection.shutdown(Signal::Sigterm).await;
        pause(250).await;
        assert_eq!(peer.accepted(), 1);
    }

    /// A controller that closes on `shutdown` instead of waiting still ends the shutdown: the
    /// connection does not redial while the request it was running is cut off.
    #[tokio::test]
    async fn ends_the_shutdown_without_a_redial_when_the_controller_closes_while_a_request_runs() {
        let peer = ControllerPeer::start(PeerOptions { close_on_shutdown: true, ..PeerOptions::default() }).await;
        let (release, released) = watch::channel(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let mut hooks = TestHooks::new(&peer.url);

        hooks.handler = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);

            let mut released = released.clone();

            Box::pin(async move {
                let _ = released.wait_for(|done| *done).await;

                Ok(json!({ "slot_id": "a", "file_name": "x.dem" }))
            })
        });

        let (connection, _) = start(hooks, Timings::default());

        peer.wait_for_connection().await;
        until("the connection", || connection.connected()).await;
        peer.send_json(&delete_demo("held"));
        until("the request at the handler", || calls.load(Ordering::SeqCst) == 1).await;
        connection.shutdown(Signal::Sigterm).await;
        pause(250).await;
        assert_eq!(peer.accepted(), 1, "no redial after the shutdown");
        release.send_replace(true);
    }

    /// A request still running when the shutdown's wait runs out is cut off. The socket stays open
    /// while the request may still finish, then the agent closes it with the signal, and the request
    /// gets no reply.
    #[tokio::test]
    async fn closes_with_the_signal_once_the_wait_for_a_request_still_running_runs_out() {
        let peer = ControllerPeer::start(PeerOptions { close_on_shutdown: false, ..PeerOptions::default() }).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let mut hooks = TestHooks::new(&peer.url);

        hooks.handler = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);

            Box::pin(std::future::pending())
        });

        let (connection, _) = start(hooks, Timings::default());

        peer.wait_for_connection().await;
        until("the connection", || connection.connected()).await;
        peer.send_json(&delete_demo("held"));
        until("the request at the handler", || calls.load(Ordering::SeqCst) == 1).await;

        let started = Instant::now();
        let shutdown = tokio::spawn({
            let connection = connection.clone();

            async move { connection.shutdown(Signal::Sigterm).await }
        });

        until("the shutdown frame", || peer.received().contains(&json!({ "type": "shutdown", "signal": "SIGTERM" }))).await;
        pause(1000).await;
        assert!(peer.closes().is_empty(), "the socket stays open while the request may still finish");
        shutdown.await.unwrap();

        let waited = started.elapsed();

        assert!(waited >= SHUTDOWN_REQUEST_WAIT, "{waited:?}");
        assert!(waited < SHUTDOWN_REQUEST_WAIT + Duration::from_secs(3), "{waited:?}");
        until("the close at the peer", || peer.closes().len() == 1).await;
        assert_eq!(peer.closes(), [(u16::from(close_code(close_codes::NORMAL)), Signal::Sigterm.name())]);
        assert!(!peer.received().iter().any(|frame| frame["reply_to"] == "held"), "the cut-off request has no reply");
    }

    #[tokio::test]
    async fn closes_the_socket_itself_with_1000_and_the_signal_as_its_reason() {
        let peer = ControllerPeer::start(PeerOptions { close_on_shutdown: false, ..PeerOptions::default() }).await;
        let (connection, _) = start(TestHooks::new(&peer.url), Timings::default());

        peer.wait_for_connection().await;
        until("the connection", || connection.connected()).await;
        connection.shutdown(Signal::Sigterm).await;
        until("the close at the peer", || peer.closes().len() == 1).await;
        assert_eq!(peer.closes(), [(u16::from(close_code(close_codes::NORMAL)), Signal::Sigterm.name())]);
    }

    #[tokio::test]
    async fn gives_up_on_a_peer_that_never_answers_the_close_after_two_seconds() {
        let peer = ControllerPeer::start(PeerOptions { close_on_shutdown: false, ..PeerOptions::default() }).await;
        let (connection, _) = start(TestHooks::new(&peer.url), Timings::default());

        peer.wait_for_connection().await;
        until("the connection", || connection.connected()).await;
        // The peer reads nothing more, so it never answers the agent's close frame.
        peer.pause();
        pause(20).await;

        let started = Instant::now();

        connection.shutdown(Signal::Sigterm).await;
        assert!(started.elapsed() >= Duration::from_millis(1_995), "{:?}", started.elapsed());
        assert!(!connection.connected());
    }
}

mod reports_around_the_welcome {
    use ankor_agent::agent_reporting::{ChangeReporter, HelloInput, SendFrame, build_hello};
    use ankor_agent::event_uploader::nothing_stuck;
    use ankor_agent::testing::fixtures::{agent_config, idle_statuses, ready_status};
    use ankor_agent::testing::slots::make_slots;
    use ankor_protocol::config::AgentConfig;
    use ankor_protocol::types::{
        AgentInfo, AgentNodeStatus, AgentSlotStatus, Cs2State, HostInfo, MaintenanceOperation, NodeUnavailable, NodeUnavailableCode, SlotStatus,
    };
    use futures_util::FutureExt as _;

    use super::*;

    /// The connection the reporters send through, once it is started.
    type Late = Arc<Mutex<Option<ControllerConnection>>>;

    fn through<T: Send + 'static>(late: &Late, wrap: fn(T) -> AgentToControllerMessage) -> SendFrame<T> {
        let late = Arc::clone(late);

        Arc::new(move |frame| match late.lock().unwrap().clone() {
            Some(connection) => connection.push(&wrap(frame)).boxed(),
            None => async { false }.boxed(),
        })
    }

    fn wire(status: SlotStatus) -> AgentSlotStatus {
        AgentSlotStatus {
            slot_id: status.slot_id,
            state: status.state,
            allocation_id: status.allocation_id,
            match_id: status.match_id,
            plugin_online: status.plugin_online,
            gslt_state: status.gslt_state,
            reserved: status.reserved,
            server_steam_id: status.server_steam_id.clone(),
        }
    }

    fn cs2(build_id: Option<&str>, image: &str) -> Cs2State {
        Cs2State {
            build_id: build_id.map(str::to_owned),
            latest_build_id: Some("19001".to_owned()),
            image: image.to_owned(),
            image_id: None,
            update: None,
            image_update: None,
        }
    }

    fn hello_of(config: AgentConfig, slot_status: Vec<SlotStatus>, cs2: Cs2State, unavailable: Option<NodeUnavailable>) -> AgentToControllerMessage {
        build_hello(HelloInput {
            config,
            agent: AgentInfo { version: "0.1.0".to_owned(), image: "ankor-agent:latest".to_owned(), image_id: format!("sha256:{}", "a".repeat(64)) },
            host: HostInfo { cpu_count: 4, os_name: None, kernel: "6.12.48".to_owned() },
            public_ip: Some("203.0.113.7".to_owned()),
            country: Some("DK".to_owned()),
            slot_status,
            cs2,
            unavailable,
            demos: Vec::new(),
            server_exits: Vec::new(),
            event_backlog: nothing_stuck(),
            agent_update: None,
        })
    }

    /// The slot as the controller holds it: from the last `slot_status` for it, or the last hello.
    fn held_status(peer: &ControllerPeer, slot_id: &str) -> Option<Value> {
        peer.received().into_iter().rev().find_map(|frame| match agent_message(&frame) {
            Some(AgentToControllerMessage::SlotStatus(status)) if status.slot_id == slot_id => Some(frame),
            Some(AgentToControllerMessage::Hello(_)) => {
                frame["slot_status"].as_array().and_then(|slots| slots.iter().find(|slot| slot["slot_id"] == slot_id).cloned())
            }
            _ => None,
        })
    }

    #[tokio::test]
    async fn sends_a_slot_that_exits_between_hello_and_welcome_once_and_leaves_the_unchanged_slot_to_the_hello() {
        let peer = ControllerPeer::start(PeerOptions { welcome: false, ..PeerOptions::default() }).await;
        let config = agent_config(&json!({}));
        let base = Arc::new(make_slots(config.clone()));
        let allocation_id = "44444444-4444-4444-8444-444444444444";

        launch_server(&base.slots, launch_config(&json!({})), allocation_id).await.unwrap();
        base.slots.plugin_status(allocation_id, &ready_status());

        let late: Late = Arc::default();
        let reports = ChangeReporter::new(|frame: &AgentSlotStatus| frame.slot_id.clone(), through(&late, AgentToControllerMessage::SlotStatus));
        let welcomed = Arc::new(AtomicUsize::new(0));
        let mut hooks = TestHooks::new(&peer.url);
        let (opening, open_slots, open_config) = (reports.clone(), Arc::clone(&base), config.clone());
        let (welcoming, welcome_slots, counted) = (reports.clone(), Arc::clone(&base), Arc::clone(&welcomed));

        hooks.on_open = Some(Arc::new(move |connection| {
            let slot_status = open_slots.slots.statuses();

            opening.helloed(&slot_status.iter().cloned().map(wire).collect::<Vec<_>>());

            let hello = hello_of(open_config.clone(), slot_status, cs2(None, &open_config.cs2_image), None);

            Box::pin(async move {
                connection.push(&hello).await;
                Ok(())
            })
        }));
        hooks.on_welcome = Some(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);

            for status in welcome_slots.slots.statuses() {
                welcoming.report(wire(status));
            }

            Box::pin(async { Ok(()) })
        }));

        let (connection, _) = start(hooks, Timings::default());

        *late.lock().unwrap() = Some(connection.clone());
        until("the initial hello", || peer.hellos() == 1).await;

        let held = held_status(&peer, "a").unwrap();

        assert_eq!((&held["state"], &held["allocation_id"]), (&json!(SlotState::Running), &json!(allocation_id)));
        base.docker.exit(base.docker.last(), Some(0));
        reports.report(wire(base.status("a")));
        peer.send_json(&welcome());
        until("the current slot status", || held_status(&peer, "a").is_some_and(|status| status["state"] == json!(SlotState::Idle))).await;

        let held = held_status(&peer, "a").unwrap();

        assert_eq!((&held["state"], &held["allocation_id"]), (&json!(SlotState::Idle), &Value::Null));
        peer.send_json(&welcome());
        assert_eq!(peer.request(delete_demo("r1")).await["ok"], true);
        assert_eq!(
            peer.received_where(|message| matches!(message, AgentToControllerMessage::SlotStatus(_))),
            [
                json!({ "type": "slot_status", "slot_id": "a", "state": "idle", "allocation_id": null, "match_id": null, "plugin_online": false, "gslt_state": "ok", "reserved": false, "server_steam_id": null })
            ]
        );
        assert_eq!(welcomed.load(Ordering::SeqCst), 1);
        assert_eq!(peer.hellos(), 1);
    }

    #[tokio::test]
    async fn sends_the_node_status_after_welcome_only_when_it_changed_since_the_hello_also_after_a_config_hello() {
        #[derive(Clone)]
        struct Node(Arc<Mutex<(bool, String)>>);

        impl Node {
            fn runtime(&self) -> AgentNodeStatus {
                let (running, build_id) = self.0.lock().unwrap().clone();
                let unavailable = running.then(|| NodeUnavailable {
                    code: NodeUnavailableCode::Maintenance,
                    operation: Some(MaintenanceOperation::Cs2Update),
                    detail: "CS2 update".to_owned(),
                });

                AgentNodeStatus { cs2: cs2(Some(&build_id), "ankor-cs2:latest"), unavailable, agent_update: None }
            }

            fn set(&self, running: bool, build_id: &str) {
                *self.0.lock().unwrap() = (running, build_id.to_owned());
            }
        }

        let peer = ControllerPeer::start(PeerOptions { welcome: false, ..PeerOptions::default() }).await;
        let node = Node(Arc::new(Mutex::new((true, "19000".to_owned()))));
        let late: Late = Arc::default();
        let reports = ChangeReporter::new(|_: &AgentNodeStatus| "node".to_owned(), through(&late, AgentToControllerMessage::NodeStatus));
        let config = agent_config(&json!({}));
        let hello = {
            let (reports, node) = (reports.clone(), node.clone());

            move |connection: ControllerConnection| {
                let runtime = node.runtime();

                reports.helloed(std::slice::from_ref(&runtime));

                let hello = hello_of(config.clone(), idle_statuses(&config), runtime.cs2, runtime.unavailable);

                async move {
                    connection.push(&hello).await;
                }
            }
        };
        let mut hooks = TestHooks::new(&peer.url);
        let (opening, welcoming, welcome_node) = (hello.clone(), reports.clone(), node.clone());

        hooks.on_open = Some(Arc::new(move |connection| {
            let sent = opening(connection);

            Box::pin(async move {
                sent.await;
                Ok(())
            })
        }));
        hooks.on_welcome = Some(Arc::new(move |_| {
            welcoming.report(welcome_node.runtime());
            Box::pin(async { Ok(()) })
        }));

        let (connection, _) = start(hooks, Timings::default());

        *late.lock().unwrap() = Some(connection.clone());
        until("initial hello", || peer.hellos() == 1).await;
        node.set(false, "19001");
        reports.report(node.runtime());
        peer.send_json(&welcome());
        peer.wait_for("current node status", |frame| matches!(agent_message(frame), Some(AgentToControllerMessage::NodeStatus(_)))).await;
        assert_eq!(
            peer.received_where(|message| matches!(message, AgentToControllerMessage::NodeStatus(_))),
            [
                json!({ "type": "node_status", "cs2": { "build_id": "19001", "latest_build_id": "19001", "image": "ankor-cs2:latest", "image_id": null, "update": null, "image_update": null }, "unavailable": null, "agent_update": null })
            ]
        );
        assert_eq!(peer.hellos(), 1);

        hello(connection.clone()).await;
        until("configuration hello", || peer.hellos() == 2).await;
        peer.send_json(&welcome());
        assert_eq!(peer.request(delete_demo("r1")).await["ok"], true);
        assert_eq!(peer.received_where(|message| matches!(message, AgentToControllerMessage::NodeStatus(_))).len(), 1);

        hello(connection.clone()).await;
        until("second configuration hello", || peer.hellos() == 3).await;
        node.set(true, "19001");
        reports.report(node.runtime());
        peer.send_json(&welcome());
        peer.wait_for(
            "current maintenance",
            |frame| matches!(agent_message(frame), Some(AgentToControllerMessage::NodeStatus(status)) if status.unavailable.is_some()),
        )
        .await;
        assert_eq!(peer.received_where(|message| matches!(message, AgentToControllerMessage::NodeStatus(_))).len(), 2);
    }
}

#[tokio::test]
async fn refuses_frames_sent_while_it_dials_and_opens_one_socket_for_them() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws/agent", listener.local_addr().unwrap());
    let upgrades = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&upgrades);

    // The controller takes its time answering the upgrade.
    tokio::spawn(async move {
        let mut sockets = Vec::new();

        while let Ok((stream, _)) = listener.accept().await {
            pause(100).await;

            if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                counted.fetch_add(1, Ordering::SeqCst);
                sockets.push(socket);
            }
        }
    });

    let (connection, _) = start(TestHooks::new(&url), timings(20, 40));

    pause(20).await;
    assert!(!connection.push(&slot_status()).await);
    assert!(!connection.push(&slot_status()).await);
    until("the connection", || connection.connected()).await;
    pause(150).await;
    assert_eq!(upgrades.load(Ordering::SeqCst), 1);
}

mod log_backlog {
    use futures_util::FutureExt as _;

    use super::*;

    /// A `log_batch` of about 40 KB, so a few dozen fill the backlog.
    fn log_batch(index: usize) -> AgentToControllerMessage {
        let lines: Vec<Value> =
            (0..20).map(|line| json!({ "at": 1_700_000_014_000_i64, "level": null, "line": format!("{index}.{line} {}", "x".repeat(1_900)) })).collect();

        serde_json::from_value(json!({
            "type": "log_batch",
            "source": { "kind": "server", "allocation_id": "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e" },
            "replay_id": null,
            "reset": false,
            "lines": lines,
        }))
        .unwrap()
    }

    fn event_backlog() -> AgentToControllerMessage {
        serde_json::from_value(json!({ "type": "event_backlog", "servers": [], "server_count": 0, "event_count": 0, "reason": null })).unwrap()
    }

    /// Every line the connection logs, at any level.
    fn every_line() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&lines);

        (Logger::new("controller", Some(Arc::new(move |entry: Entry| seen.lock().unwrap().push(entry.message)))), lines)
    }

    /// The controller reads slower than the node logs: log frames past the backlog's bound are dropped at
    /// once and the drop is logged once, a state frame is never dropped, and once the controller catches up
    /// the agent says how many it dropped and queues log frames again.
    #[tokio::test]
    async fn drops_log_frames_past_the_bound_keeps_state_frames_and_logs_both_ends() {
        let peer = ControllerPeer::start(PeerOptions::default()).await;
        let (log, lines) = every_line();
        let (connection, _) = start_logged(TestHooks::new(&peer.url), Timings::default(), log);

        peer.wait_for_connection().await;
        peer.send_json(&welcome());
        until("the welcome", || connection.welcomed()).await;
        peer.pause();

        // Pushed without yielding, so nothing is written yet: every frame waits in the backlog.
        let mut pending = Vec::new();
        let mut dropped = 0;

        for index in 0..60 {
            let mut pushed = connection.push(&log_batch(index)).boxed();

            match (&mut pushed).now_or_never() {
                Some(false) => dropped += 1,
                Some(true) => {}
                None => pending.push(pushed),
            }
        }

        let mut state = connection.push(&event_backlog()).boxed();

        assert!(dropped > 0 && dropped < 60, "{dropped} of 60 dropped");
        assert_eq!((&mut state).now_or_never(), None, "a state frame waits in the queue instead of being dropped");

        let dropping: Vec<String> = lines.lock().unwrap().iter().filter(|line| line.contains("dropping log frames")).cloned().collect();

        assert_eq!(dropping.len(), 1, "{:?}", lines.lock().unwrap());

        peer.resume();
        until("the backlog written", || lines.lock().unwrap().iter().any(|line| line.contains(&format!("dropped {dropped} while the controller was behind"))))
            .await;
        assert!(state.await, "the state frame was written");
        assert!(futures_util::future::join_all(pending).await.into_iter().all(|written| written), "every log frame kept was written");
        assert!(connection.push(&log_batch(60)).await, "log frames are queued again");
        until("every log frame kept at the controller", || {
            peer.received_where(|message| matches!(message, AgentToControllerMessage::LogBatch(_))).len() == 60 - dropped + 1
        })
        .await;
        peer.expect_valid_frames();
    }
}
