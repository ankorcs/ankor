//! The connection against a local fake controller.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
#![allow(clippy::result_large_err, clippy::unused_async, reason = "the hooks' signatures are the trait's")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::controller_connection::{BoxFuture, ControllerConnection, Defer, HandlerError, Hooks, Request, Signal, Timings, close_code};
use ankor_agent::logger::{Entry, Logger};
use ankor_agent::testing::start_connection;
use ankor_agent::testing::{agent_message, welcome, wire};
use ankor_protocol::constants::{AGENT_WELCOME_TIMEOUT_REASON, PROTOCOL_HEADER, PROTOCOL_VERSION, close_codes};
use ankor_protocol::helpers::node_authorization;
use ankor_protocol::types::{
    AgentErrorCode, AgentShutdown, AgentShutdownSignal, AgentToControllerMessage, CloseReason, ControllerHelloRefused, ControllerToAgentMessage,
    HelloRefusedReason, RequestType,
};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request as HttpRequest, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

#[derive(Debug, PartialEq)]
enum Event {
    Welcomed,
    Closed(u16, String),
    Failed(String),
    Handled(String),
}

struct TestHooks {
    url: String,
    events: mpsc::UnboundedSender<Event>,
    log_frame: AgentToControllerMessage,
}

impl Hooks for TestHooks {
    fn url(&self) -> String {
        self.url.clone()
    }

    fn on_open(&self, connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> {
        let frame = self.log_frame.clone();

        let events = self.events.clone();

        // A hook's panic ends its task alone, so a failed check is an event the test sees.
        Box::pin(async move {
            // Not a hello, so it waits for the welcome.
            if connection.push(&frame).await {
                let _ = events.send(Event::Failed("a frame before the hello went out".to_owned()));
            }
            connection.push(&AgentToControllerMessage::Shutdown(AgentShutdown { signal: AgentShutdownSignal::Sigint })).await;
            let hello: AgentToControllerMessage = serde_json::from_value(hello_json()).unwrap();

            if !connection.push(&hello).await {
                let _ = events.send(Event::Failed("the hello did not go out".to_owned()));
            }
            Ok(())
        })
    }

    fn on_welcome(&self, connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> {
        let events = self.events.clone();
        let frame = self.log_frame.clone();

        Box::pin(async move {
            let sent = connection.push(&frame).await;
            let _ = events.send(if sent { Event::Welcomed } else { Event::Failed("the frame after the welcome did not go out".to_owned()) });
            Ok(())
        })
    }

    fn on_close(&self, code: u16, reason: &str) {
        let _ = self.events.send(Event::Closed(code, reason.to_owned()));
    }

    fn on_failed(&self, reason: &str) {
        let _ = self.events.send(Event::Failed(reason.to_owned()));
    }

    fn handle(&self, request: Request, defer: Defer) -> BoxFuture<'static, Result<Value, HandlerError>> {
        let events = self.events.clone();

        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            defer.push(async move {
                let _ = events.send(Event::Handled(request.id));
            });

            if request.request_type == RequestType::GetServerStatus { Err("no such slot".into()) } else { Ok(Value::Null) }
        })
    }
}

fn hello_json() -> Value {
    json!({
        "type": "hello",
        "agent": { "version": "0.3.0", "image": "ghcr.io/ankorcs/agent:0.3", "image_id": format!("sha256:{}", "a".repeat(64)) },
        "host": { "cpu_count": 4, "os_name": null, "kernel": "test" },
        "public_ip": null, "country": null,
        "config": { "slots": [], "pinned_ip": null, "controller": { "host": "127.0.0.1", "tls": false, "socket_path": "/ws/agent", "events_path": "/api/node/events" }, "auto_update": false, "demo_upload": null },
        "slot_status": [], "cs2": { "build_id": null, "latest_build_id": null, "image": "ghcr.io/ankorcs/cs2:0", "image_id": null, "update": null, "image_update": null },
        "unavailable": null, "demos": [], "server_exits": [],
        "event_backlog": { "servers": [], "server_count": 0, "event_count": 0, "reason": null },
        "agent_update": null,
    })
}

fn log_frame() -> AgentToControllerMessage {
    serde_json::from_value(json!({ "type": "event_backlog", "servers": [], "server_count": 0, "event_count": 0, "reason": null })).unwrap()
}

fn timings() -> Timings {
    Timings {
        min_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(100),
        ping_interval: Duration::from_secs(30),
        welcome_timeout: Duration::from_millis(300),
        refused_close: Duration::from_millis(100),
        handshake_timeout: Duration::from_secs(2),
        ..Timings::default()
    }
}

async fn start(listener: &TcpListener) -> (ControllerConnection, mpsc::UnboundedReceiver<Event>) {
    let (connection, events, _) = start_logged(listener).await;

    (connection, events)
}

/// As [`start`], with the lines the connection logs.
async fn start_logged(listener: &TcpListener) -> (ControllerConnection, mpsc::UnboundedReceiver<Event>, Arc<Mutex<Vec<String>>>) {
    let (events, received) = mpsc::unbounded_channel();
    let url = format!("ws://{}/ws/agent", listener.local_addr().unwrap());
    let hooks = Arc::new(TestHooks { url, events, log_frame: log_frame() });
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let logged = Arc::clone(&lines);
    let log = Logger::new("controller", Some(Arc::new(move |entry: Entry| logged.lock().unwrap().push(entry.message))));

    (start_connection("t".repeat(64), hooks, timings(), log), received, lines)
}

/// Accepts one socket, recording the request's headers.
async fn accept(listener: &TcpListener, headers: Arc<Mutex<Vec<(String, String)>>>, status: Option<StatusCode>) -> Option<WebSocketStream<TcpStream>> {
    let (stream, _) = listener.accept().await.unwrap();
    let callback = move |request: &HttpRequest, response: Response| -> Result<Response, ErrorResponse> {
        headers.lock().unwrap().extend(request.headers().iter().map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_owned())));

        match status {
            Some(status) => {
                let mut refusal = ErrorResponse::new(None);

                *refusal.status_mut() = status;
                Err(refusal)
            }
            None => Ok(response),
        }
    };

    tokio_tungstenite::accept_hdr_async(stream, callback).await.ok()
}

async fn next_json(socket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        match socket.next().await.expect("frame").expect("frame") {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Close(_) => panic!("closed"),
            _ => {}
        }
    }
}

/// The controller's refusal of the hello, with the detail `no`.
fn hello_refused() -> String {
    json!(ControllerToAgentMessage::HelloRefused(ControllerHelloRefused { reason: HelloRefusedReason::InvalidHello, detail: "no".to_owned() })).to_string()
}

async fn next_event(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(3), events.recv()).await.expect("event in time").expect("event")
}

#[tokio::test]
async fn hello_first_then_everything_after_welcome() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connection, mut events) = start(&listener).await;
    let headers = Arc::default();
    let mut socket = accept(&listener, Arc::clone(&headers), None).await.unwrap();

    assert!(matches!(agent_message(&next_json(&mut socket).await), Some(AgentToControllerMessage::Hello(_))));

    {
        let headers = headers.lock().unwrap();

        assert!(headers.contains(&("authorization".to_owned(), node_authorization(&"t".repeat(64)))));
        assert!(headers.contains(&(PROTOCOL_HEADER.to_lowercase(), PROTOCOL_VERSION.to_string())));
    }

    socket.send(Message::Text(welcome().to_string().into())).await.unwrap();
    assert_eq!(next_event(&mut events).await, Event::Welcomed);
    assert!(matches!(agent_message(&next_json(&mut socket).await), Some(AgentToControllerMessage::EventBacklog(_))));
    assert!(connection.welcomed());

    connection.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn skips_a_notice_a_newer_controller_sends_and_logs_its_type_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connection, mut events, lines) = start_logged(&listener).await;
    let mut socket = accept(&listener, Arc::default(), None).await.unwrap();

    next_json(&mut socket).await;
    socket.send(Message::Text(welcome().to_string().into())).await.unwrap();
    next_event(&mut events).await;
    next_json(&mut socket).await;

    for _ in 0..2 {
        socket.send(Message::Text(json!({ "type": "node_weather", "sky": "clear" }).to_string().into())).await.unwrap();
    }

    // A request after them is answered, so both were read before it.
    socket.send(Message::Text(json!({ "type": "make_coffee", "id": "a" }).to_string().into())).await.unwrap();
    assert_eq!(next_json(&mut socket).await["error"]["code"], json!(AgentErrorCode::UnsupportedRequest));

    let lines = lines.lock().unwrap().clone();

    assert_eq!(lines.iter().filter(|line| line.contains("skipping node_weather frames, which this agent does not know")).count(), 1, "{lines:?}");
    assert!(!lines.iter().any(|line| line.contains("dropping invalid frame")), "{lines:?}");
    connection.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn drops_an_invalid_notice_of_a_type_it_knows_instead_of_skipping_the_type() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connection, mut events, lines) = start_logged(&listener).await;
    let mut socket = accept(&listener, Arc::default(), None).await.unwrap();

    next_json(&mut socket).await;
    socket.send(Message::Text(welcome().to_string().into())).await.unwrap();
    next_event(&mut events).await;
    next_json(&mut socket).await;
    socket.send(Message::Text(json!({ "type": "watch_resources", "replay": "yes" }).to_string().into())).await.unwrap();

    // A request after it is answered, so the notice was read before it.
    socket.send(Message::Text(json!({ "type": "make_coffee", "id": "a" }).to_string().into())).await.unwrap();
    assert_eq!(next_json(&mut socket).await["error"]["code"], json!(AgentErrorCode::UnsupportedRequest));

    let lines = lines.lock().unwrap().clone();

    assert_eq!(lines.iter().filter(|line| line.contains("dropping invalid frame")).count(), 1, "{lines:?}");
    assert!(!lines.iter().any(|line| line.contains("skipping")), "{lines:?}");
    connection.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn requests_get_replies_and_bad_ones_get_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connection, mut events) = start(&listener).await;
    let mut socket = accept(&listener, Arc::default(), None).await.unwrap();

    next_json(&mut socket).await;
    socket.send(Message::Text(welcome().to_string().into())).await.unwrap();
    next_event(&mut events).await;
    next_json(&mut socket).await;

    let send = |value: Value| Message::Text(value.to_string().into());

    socket.send(send(json!({ "type": "make_coffee", "id": "a" }))).await.unwrap();
    let reply = next_json(&mut socket).await;

    assert_eq!(reply["error"], json!({ "code": AgentErrorCode::UnsupportedRequest, "detail": "unsupported request type make_coffee" }));
    assert_eq!(reply["request"], Value::Null);

    socket.send(send(json!({ "type": "stop_server", "id": "b" }))).await.unwrap();
    let reply = next_json(&mut socket).await;

    assert_eq!(reply["error"]["code"], json!(AgentErrorCode::InvalidRequest));
    assert_eq!(reply["request"], json!(RequestType::StopServer));
    assert!(reply["error"]["detail"].as_str().unwrap().starts_with("allocation_id: Invalid input"), "{reply}");

    let status = json!({ "type": "get_server_status", "id": "c", "allocation_id": "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e" });

    socket.send(send(status.clone())).await.unwrap();
    socket.send(send(status)).await.unwrap();
    let reply = next_json(&mut socket).await;

    assert_eq!(
        reply,
        json!({ "type": "reply", "reply_to": "c", "request": "get_server_status", "ok": false, "error": { "code": "failed", "detail": "no such slot" } })
    );
    assert_eq!(next_event(&mut events).await, Event::Handled("c".to_owned()));
    assert!(tokio::time::timeout(Duration::from_millis(300), next_json(&mut socket)).await.is_err(), "the duplicate gets no reply");

    connection.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn a_missing_welcome_redials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_connection, mut events) = start(&listener).await;
    let mut socket = accept(&listener, Arc::default(), None).await.unwrap();

    next_json(&mut socket).await;

    match socket.next().await.unwrap().unwrap() {
        Message::Close(Some(frame)) => assert_eq!(frame.reason.as_str(), AGENT_WELCOME_TIMEOUT_REASON),
        other => panic!("expected a close, got {other:?}"),
    }

    drop(socket);
    assert!(matches!(next_event(&mut events).await, Event::Closed(..)));

    let mut again = accept(&listener, Arc::default(), None).await.unwrap();

    assert!(matches!(agent_message(&next_json(&mut again).await), Some(AgentToControllerMessage::Hello(_))));
}

#[tokio::test]
async fn an_unknown_token_stops_redialling() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_connection, mut events) = start(&listener).await;

    assert!(accept(&listener, Arc::default(), Some(StatusCode::UNAUTHORIZED)).await.is_none());

    match next_event(&mut events).await {
        Event::Failed(reason) => assert!(reason.contains("HTTP 401"), "{reason}"),
        other => panic!("expected a failure, got {other:?}"),
    }

    assert!(tokio::time::timeout(Duration::from_millis(400), listener.accept()).await.is_err(), "no second dial");
}

#[tokio::test]
async fn a_refused_hello_stops_redialling() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_connection, mut events) = start(&listener).await;
    let mut socket = accept(&listener, Arc::default(), None).await.unwrap();

    next_json(&mut socket).await;
    socket.send(Message::Text(hello_refused().into())).await.unwrap();

    match next_event(&mut events).await {
        Event::Failed(reason) => assert!(reason.contains("refused this node's hello (invalid_hello): no"), "{reason}"),
        other => panic!("expected a failure, got {other:?}"),
    }

    assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))), "the agent closes after the grace");
    assert!(tokio::time::timeout(Duration::from_millis(400), listener.accept()).await.is_err(), "no second dial");
}

#[tokio::test]
async fn takes_the_close_the_controller_sends_within_the_grace_after_refusing_the_hello_and_never_backs_off() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_connection, mut events, lines) = start_logged(&listener).await;
    let mut socket = accept(&listener, Arc::default(), None).await.unwrap();

    next_json(&mut socket).await;
    socket.send(Message::Text(hello_refused().into())).await.unwrap();
    assert!(matches!(next_event(&mut events).await, Event::Failed(_)));

    // The controller closes well inside the agent's grace, which then reports the controller's code and
    // reason.
    tokio::time::sleep(Duration::from_millis(20)).await;
    socket.close(Some(CloseFrame { code: close_code(close_codes::POLICY), reason: wire(CloseReason::HelloRefused).into() })).await.unwrap();
    assert_eq!(next_event(&mut events).await, Event::Closed(u16::from(close_code(close_codes::POLICY)), wire(CloseReason::HelloRefused)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!lines.lock().unwrap().iter().any(|line| line.contains("reconnecting in")), "{:?}", lines.lock().unwrap());
}

#[tokio::test]
async fn warns_of_a_second_agent_only_for_the_controller_s_node_connected_policy_close() {
    for (code, reason, warned) in [
        (close_code(close_codes::POLICY), wire(CloseReason::NodeConnected), true),
        (close_code(close_codes::POLICY), "too_many".to_owned(), false),
        (close_code(close_codes::NORMAL), wire(CloseReason::NodeConnected), false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (connection, mut events, lines) = start_logged(&listener).await;
        let mut socket = accept(&listener, Arc::default(), None).await.unwrap();

        next_json(&mut socket).await;
        socket.close(Some(CloseFrame { code, reason: reason.clone().into() })).await.unwrap();
        assert!(matches!(next_event(&mut events).await, Event::Closed(..)));
        // The close is judged after its hook runs.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(lines.lock().unwrap().iter().any(|line| line.contains("the node is already connected")), warned, "{code} {reason}");
        connection.shutdown(Signal::Sigterm).await;
    }
}

#[tokio::test]
async fn sends_its_first_ping_a_ping_interval_after_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connection, _events) = start(&listener).await;
    let mut socket = accept(&listener, Arc::default(), None).await.unwrap();
    // Every frame from the start, inside the welcome timeout, so none is skipped unseen.
    let pinged = tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            if let Some(Ok(Message::Ping(_))) = socket.next().await {
                return;
            }
        }
    })
    .await;

    assert!(pinged.is_err(), "a ping went out at once");
    connection.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn shutdown_sends_its_frame_and_closes_with_the_signal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connection, mut events) = start(&listener).await;
    let mut socket = accept(&listener, Arc::default(), None).await.unwrap();

    next_json(&mut socket).await;
    socket.send(Message::Text(welcome().to_string().into())).await.unwrap();
    next_event(&mut events).await;
    next_json(&mut socket).await;

    let server = tokio::spawn(async move {
        let frame = next_json(&mut socket).await;

        match socket.next().await.unwrap().unwrap() {
            Message::Close(Some(close)) => (frame, close.reason.to_string()),
            other => panic!("expected a close, got {other:?}"),
        }
    });

    connection.shutdown(Signal::Sigquit).await;

    let (frame, reason) = server.await.unwrap();

    assert_eq!(frame, json!(AgentToControllerMessage::Shutdown(AgentShutdown { signal: AgentShutdownSignal::Sigquit })));
    assert_eq!(reason, Signal::Sigquit.name());
    assert!(!connection.connected());
}
