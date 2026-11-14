//! Connects to a controller as a node with no slots, waits for `welcome`, then shuts down.
//!
//! ```sh
//! ANKOR_NODE_TOKEN=… cargo run -p ankor-agent --example live_hello
//! ```
//!
//! `ANKOR_CONTROLLER_HOST` overrides the host (`ankor.sh`).

#![allow(clippy::expect_used, clippy::panic, reason = "a manual check that stops at its first failure")]

use std::sync::Arc;
use std::time::Duration;

use ankor_agent::controller_connection::{BoxFuture, ControllerConnection, Defer, HandlerError, Hooks, Request, Signal, Timings};
use ankor_agent::logger::Logger;
use ankor_protocol::constants::{DEFAULT_EVENTS_PATH, DEFAULT_SOCKET_PATH, NODE_TOKEN_VARIABLE};
use ankor_protocol::helpers::{agent_ws_url, wire_name};
use ankor_protocol::schemas::s_agent_to_controller_message;
use ankor_protocol::types::{AgentHello, AgentInfo, AgentToControllerMessage, ControllerAddress, Cs2State, EventBacklog, HostInfo, NodeSettings};
use ankor_protocol::validate::{format_issues, safe_parse};
use serde_json::Value;
use tokio::sync::Notify;

struct Live {
    host: String,
    welcomed: Arc<Notify>,
}

fn hello(host: &str) -> AgentToControllerMessage {
    AgentToControllerMessage::Hello(AgentHello {
        agent: AgentInfo {
            version: ankor_agent::AGENT_VERSION.to_owned(),
            image: "ghcr.io/ankorcs/agent:0.3".to_owned(),
            image_id: format!("sha256:{}", "a".repeat(64)),
        },
        host: HostInfo {
            cpu_count: i64::try_from(std::thread::available_parallelism().map_or(1, usize::from)).unwrap_or(1),
            os_name: Some("Rust live test".to_owned()),
            kernel: "test".to_owned(),
        },
        public_ip: None,
        country: None,
        config: NodeSettings {
            slots: Vec::new(),
            pinned_ip: None,
            controller: ControllerAddress {
                host: host.to_owned(),
                tls: true,
                socket_path: DEFAULT_SOCKET_PATH.to_owned(),
                events_path: DEFAULT_EVENTS_PATH.to_owned(),
            },
            auto_update: false,
            demo_upload: None,
            memory_mb: None,
        },
        slot_status: Vec::new(),
        cs2: Cs2State { build_id: None, latest_build_id: None, image: "ghcr.io/ankorcs/cs2:0".to_owned(), image_id: None, update: None, image_update: None },
        unavailable: None,
        demos: Vec::new(),
        server_exits: Vec::new(),
        event_backlog: EventBacklog { servers: Vec::new(), server_count: 0, event_count: 0, reason: None },
        agent_update: None,
    })
}

impl Hooks for Live {
    fn url(&self) -> String {
        agent_ws_url(&self.host, true, DEFAULT_SOCKET_PATH)
    }

    fn on_open(&self, connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> {
        let frame = hello(&self.host);

        Box::pin(async move {
            let value = serde_json::to_value(&frame).map_err(|error| error.to_string())?;

            safe_parse(s_agent_to_controller_message, value).map_err(|issues| format!("hello does not match the protocol: {}", format_issues(&issues)))?;

            if connection.push(&frame).await { Ok(()) } else { Err("hello did not go out".into()) }
        })
    }

    fn on_welcome(&self, _connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> {
        let welcomed = Arc::clone(&self.welcomed);

        Box::pin(async move {
            welcomed.notify_one();
            Ok(())
        })
    }

    fn on_close(&self, code: u16, reason: &str) {
        println!("closed: {code} {reason}");
    }

    fn on_failed(&self, reason: &str) {
        println!("failed: {reason}");
    }

    fn handle(&self, request: Request, _defer: Defer) -> BoxFuture<'static, Result<Value, HandlerError>> {
        Box::pin(async move { Err(format!("{} is not handled by this test", wire_name(request.request_type)).into()) })
    }
}

#[tokio::main]
async fn main() {
    let token = std::env::var(NODE_TOKEN_VARIABLE).unwrap_or_else(|_| panic!("set {NODE_TOKEN_VARIABLE}"));
    let host = std::env::var("ANKOR_CONTROLLER_HOST").unwrap_or_else(|_| "ankor.sh".to_owned());
    let welcomed = Arc::new(Notify::new());
    let hooks = Arc::new(Live { host, welcomed: Arc::clone(&welcomed) });
    let (connection, driver) = ControllerConnection::new(token, Timings::default(), Logger::new("live", None));

    driver.start(hooks);

    match tokio::time::timeout(Duration::from_secs(20), welcomed.notified()).await {
        Ok(()) => println!("welcomed: {}", connection.welcomed()),
        Err(_) => println!("no welcome within 20s"),
    }

    tokio::time::sleep(Duration::from_secs(2)).await;
    connection.shutdown(Signal::Sigterm).await;
    println!("shut down; connected: {}", connection.connected());
}
