//! The request handler behind a real connection to a controller peer: requests and their replies, a node
//! whose event spool will not open, and `set_config` moving the node or reporting what it changed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::agent_reporting::{ChangeReporter, HelloInput, build_hello, node_unavailable};
use ankor_agent::controller_connection::{BoxFuture, ControllerConnection, Defer, HandlerError, Hooks, Request, Signal, Timings};
use ankor_agent::event_spool::SPOOL_SCHEMA_VERSION;
use ankor_agent::gslt_keep_alive::{GsltSlotsPort, Release};
use ankor_agent::install::{MaintenanceGuard, MaintenanceGuardOptions};
use ankor_agent::logger::Logger;
use ankor_agent::request_handler::{RequestHandlerDeps, TokenChecks, handle};
use ankor_agent::spool_client::{SpoolClient, SpoolPort as _, open_spool};
use ankor_agent::testing::controller_peer::{ControllerPeer, PeerOptions};
use ankor_agent::testing::fixtures::{GSLT_A, agent_config, launch_config, ready_status};
use ankor_agent::testing::handler::make_handler;
use ankor_agent::testing::slots::{Reported, Slots, make_slots};
use ankor_agent::testing::start_connection;
use ankor_agent::testing::{agent_message, until, welcome, wire};
use ankor_protocol::constants::{DEFAULT_EVENTS_PATH, DEFAULT_SOCKET_PATH};
use ankor_protocol::helpers::agent_ws_url;
use ankor_protocol::types::{
    AgentErrorCode, AgentInfo, AgentSlotStatus, AgentToControllerMessage, Cs2State, EventBacklog, GsltState, HostInfo, MaintenanceOperation, NodeUnavailable,
    NodeUnavailableCode, SlotState,
};
use async_trait::async_trait;
use serde_json::{Value, json};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn allocation() -> String {
    format!("0199a0b4-5c00-7000-8000-{:012x}", NEXT.fetch_add(1, Ordering::SeqCst))
}

type OpenHook = Arc<dyn Fn(ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> + Send + Sync>;

/// What `main` wires: the handler answers requests, `on_open` says hello.
struct Wired {
    url: Arc<dyn Fn() -> String + Send + Sync>,
    deps: RequestHandlerDeps,
    on_open: Option<OpenHook>,
    on_welcome: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Hooks for Wired {
    fn url(&self) -> String {
        (self.url)()
    }

    fn on_open(&self, connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> {
        match &self.on_open {
            Some(hook) => hook(connection),
            None => Box::pin(async { Ok(()) }),
        }
    }

    fn on_welcome(&self, _connection: ControllerConnection) -> BoxFuture<'static, Result<(), HandlerError>> {
        if let Some(hook) = &self.on_welcome {
            hook();
        }

        Box::pin(async { Ok(()) })
    }

    fn handle(&self, request: Request, defer: Defer) -> BoxFuture<'static, Result<Value, HandlerError>> {
        let deps = self.deps.clone();

        Box::pin(async move { handle(&deps, request, defer).await })
    }
}

fn connect(hooks: Wired) -> ControllerConnection {
    let timings = Timings { min_backoff: Duration::from_millis(20), max_backoff: Duration::from_millis(40), ..Timings::default() };

    start_connection("test-token".to_owned(), Arc::new(hooks), timings, Logger::null())
}

/// The hello of the node in `base`, with its slots' statuses.
fn hello_of(base: &Slots, unavailable: Option<NodeUnavailable>) -> AgentToControllerMessage {
    let config = base.config.lock().unwrap().clone();

    build_hello(HelloInput {
        agent: AgentInfo { version: "0.1.0".to_owned(), image: "ankor-agent:latest".to_owned(), image_id: format!("sha256:{}", "a".repeat(64)) },
        host: HostInfo { cpu_count: 4, os_name: None, kernel: "6.12.48".to_owned() },
        public_ip: Some("203.0.113.7".to_owned()),
        country: Some("DK".to_owned()),
        cs2: Cs2State {
            build_id: None,
            latest_build_id: Some("19001".to_owned()),
            image: config.cs2_image.clone(),
            image_id: None,
            update: None,
            image_update: None,
        },
        config,
        slot_status: base.slots.statuses(),
        unavailable,
        demos: Vec::new(),
        server_exits: Vec::new(),
        event_backlog: EventBacklog { servers: Vec::new(), server_count: 0, event_count: 0, reason: None },
        agent_update: None,
    })
}

fn says_hello(base: &Slots, unavailable: Option<NodeUnavailable>) -> OpenHook {
    let hello = hello_of(base, unavailable);

    Arc::new(move |connection| {
        let hello = hello.clone();

        Box::pin(async move {
            connection.push(&hello).await;
            Ok(())
        })
    })
}

fn is_reply(id: &str) -> impl Fn(&Value) -> bool + '_ {
    move |message| matches!(agent_message(message), Some(AgentToControllerMessage::Reply(_))) && message["reply_to"] == id
}

fn start(allocation_id: &str) -> Value {
    json!({ "type": "start_server", "id": "s1", "allocation_id": allocation_id, "launch": launch_config(&json!({})) })
}

fn code_of(error: &HandlerError) -> AgentErrorCode {
    error.refusal().unwrap_or_else(|| panic!("no agent failure: {error}")).code
}

mod request_handler {
    use super::*;

    #[tokio::test]
    async fn starts_and_stops_a_slot_through_the_handler() {
        let base = make_slots(agent_config(&json!({})));
        let handler = make_handler(&base, |deps| deps.config = deps.config.with_persist(Arc::new(|_, _| Ok(()))));
        let allocation_id = allocation();

        handler.request(start(&allocation_id)).await.unwrap();
        assert_eq!(base.status("a").allocation_id.as_deref(), Some(allocation_id.as_str()));
        assert_eq!(
            handler.request(json!({ "type": "stop_server", "id": "s2", "allocation_id": allocation_id })).await.unwrap(),
            json!({ "allocation_id": allocation_id })
        );
        assert_eq!(base.status("a").state, SlotState::Idle);
    }

    #[tokio::test]
    async fn rejects_a_stale_allocation_stop_without_stopping_the_current_container() {
        let base = make_slots(agent_config(&json!({})));
        let handler = make_handler(&base, |deps| deps.config = deps.config.with_persist(Arc::new(|_, _| Ok(()))));
        let allocation_id = allocation();

        handler.request(start(&allocation_id)).await.unwrap();
        base.slots.plugin_status(&allocation_id, &ready_status());

        let error = handler.request(json!({ "type": "stop_server", "id": "s2", "allocation_id": "dd154843-f0f1-4530-85df-48d21bb4bdad" })).await.unwrap_err();

        assert_eq!(code_of(&error), AgentErrorCode::UnknownAllocation);

        let status = base.status("a");

        assert_eq!((status.state, status.allocation_id.as_deref()), (SlotState::Running, Some(allocation_id.as_str())));
    }
}

mod a_node_whose_event_spool_will_not_open {
    use super::*;

    #[tokio::test]
    async fn refuses_a_launch_during_maintenance_with_maintenance_running_and_with_node_unavailable_after_it() {
        let base = make_slots(agent_config(&json!({})));
        let updates = MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(&base.docker) as Arc<dyn ankor_agent::docker::DockerPort>));
        let spool = NodeUnavailable { code: NodeUnavailableCode::EventStorage, operation: None, detail: "event spool schema mismatch".to_owned() };
        let (guard, reported) = (updates.clone(), spool.clone());
        let handler = make_handler(&base, |deps| {
            deps.config = deps.config.with_persist(Arc::new(|_, _| Ok(())));
            deps.updates = updates.clone();
            deps.unavailable = Arc::new(move || node_unavailable(guard.holder(), Some(reported.clone())));
        });
        let release = updates.claim(MaintenanceOperation::AgentUpdate).await.unwrap();

        assert_eq!(code_of(&handler.request(start(&allocation())).await.unwrap_err()), AgentErrorCode::MaintenanceRunning);
        release.release(None);

        let error = handler.request(start(&allocation())).await.unwrap_err();

        assert_eq!(code_of(&error), AgentErrorCode::NodeUnavailable);
        assert!(error.to_string().contains(&spool.detail), "{error}");
        assert!(base.docker.started.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn says_hello_as_unavailable_refuses_launches_and_still_answers_other_requests() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("streams.sqlite");

        rusqlite::Connection::open(&file).unwrap().execute_batch(&format!("PRAGMA user_version = {};", SPOOL_SCHEMA_VERSION + 1)).unwrap();

        let spool = SpoolClient::start(file, Logger::null());
        let unavailable = open_spool(&spool, &Logger::null()).await.unwrap();

        let _ = spool.close().await;
        assert_eq!(unavailable.code, NodeUnavailableCode::EventStorage);
        assert!(unavailable.detail.contains("event spool schema mismatch"));

        let base = make_slots(agent_config(&json!({})));
        let reported = unavailable.clone();
        let handler = make_handler(&base, |deps| {
            deps.config = deps.config.with_persist(Arc::new(|_, _| Ok(())));
            deps.unavailable = Arc::new(move || Some(reported.clone()));
        });
        let peer = ControllerPeer::start(PeerOptions::default()).await;
        let url = peer.url.clone();
        let _connection = connect(Wired {
            url: Arc::new(move || url.clone()),
            deps: handler.deps.clone(),
            on_open: Some(says_hello(&base, Some(unavailable))),
            on_welcome: None,
        });
        let hello = peer.wait_for("the hello", |message| matches!(agent_message(message), Some(AgentToControllerMessage::Hello(_)))).await;

        assert_eq!(hello["unavailable"]["code"], wire(NodeUnavailableCode::EventStorage));

        let refused = peer.request(start(&allocation())).await;

        assert_eq!((refused["ok"].clone(), refused["error"]["code"].clone()), (json!(false), json!(AgentErrorCode::NodeUnavailable)));
        assert!(base.docker.started.lock().unwrap().is_empty());
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(peer.request(json!({ "type": "delete_demo", "slot_id": "a", "file_name": "x.dem" })).await["ok"], true);
        peer.expect_valid_frames();
    }
}

/// A node moved to another controller while it runs: the controller edits its address, the agent persists it,
/// replies `ok` on the socket it is about to lose, and comes up on the new one. Two real servers,
/// because "it reconnected" only means something against the second.
mod set_config_moves_the_connection {
    use super::*;

    #[tokio::test]
    async fn replies_ok_persists_the_new_address_and_connects_to_it() {
        for (socket_path, events_path) in [(DEFAULT_SOCKET_PATH, DEFAULT_EVENTS_PATH), ("/ankor/agent", "/ankor/events")] {
            let first = ControllerPeer::start(PeerOptions::default()).await;
            let second = ControllerPeer::start(PeerOptions { socket_path: socket_path.to_owned(), ..PeerOptions::default() }).await;
            let host_of = |peer: &ControllerPeer| format!("127.0.0.1:{}", peer.port);
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("agent.yml");

            std::fs::write(
                &file,
                format!(
                    "token: 0123456789abcdef0123456789abcdef\ncontroller:\n  host: {}\n  tls: false\ncs2_image: ghcr.io/example/ankor-cs2:latest\nagent_image: ghcr.io/example/ankor-agent:latest\nslots:\n  - id: a\n    port: 27015\n    tv_port: 27020\n",
                    host_of(&first)
                ),
            )
            .unwrap();

            let base = make_slots(agent_config(&json!({ "controller": { "host": host_of(&first), "tls": false } })));
            let connection: Arc<Mutex<Option<ControllerConnection>>> = Arc::default();
            let redialled = Arc::clone(&connection);
            let path = file.clone();
            // What main wires: the reply is still owed on the socket this redial replaces.
            let handler = make_handler(&base, move |deps| {
                deps.config = deps.config.at(path);
                deps.redial = Arc::new(move || {
                    if let Some(connection) = redialled.lock().unwrap().as_ref() {
                        connection.redial();
                    }
                });
            });
            let config = Arc::clone(&base.config);
            let url = Arc::new(move || {
                let controller = config.lock().unwrap().controller.clone();

                agent_ws_url(&controller.host, controller.tls, &controller.socket_path)
            });

            *connection.lock().unwrap() = Some(connect(Wired { url, deps: handler.deps.clone(), on_open: None, on_welcome: None }));
            first.wait_for_connection().await;
            first.send_json(&json!({ "type": "set_config", "id": "r1", "config": {
                "pinned_ip": null, "slots": [{ "id": "a", "port": 27015, "tv_port": 27020 }],
                "controller": { "host": host_of(&second), "tls": false, "socket_path": socket_path, "events_path": events_path }, "demo_upload": null,
            } }));

            // The reply goes out before the socket is replaced: the controller must not see the request fail.
            let reply = first.wait_for("the reply to r1", is_reply("r1")).await;

            assert_eq!(reply["ok"], true);
            assert_eq!(
                (
                    reply["data"]["controller"]["host"].clone(),
                    reply["data"]["controller"]["socket_path"].clone(),
                    reply["data"]["controller"]["events_path"].clone()
                ),
                (json!(host_of(&second)), json!(socket_path), json!(events_path))
            );
            second.wait_for_connection().await;
            until("the connection to the new address", || second.accepted() == 1).await;

            // Persisted, so a restart dials the new address too.
            let persisted: Value = yaml_serde::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let expected = if socket_path == DEFAULT_SOCKET_PATH {
                json!({ "host": host_of(&second), "tls": false })
            } else {
                json!({ "host": host_of(&second), "tls": false, "socket_path": socket_path, "events_path": events_path })
            };

            assert_eq!(persisted["controller"], expected);

            let replaced = connection.lock().unwrap().take();

            if let Some(replaced) = replaced {
                replaced.shutdown(Signal::Sigterm).await;
            }
        }
    }
}

mod set_config_answers_with_the_settings {
    use super::*;

    const GSLT_C: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCC0003";

    /// The token check: holds the slot it was told about until the test lets go, as a logon does.
    struct Holding {
        slots: ankor_agent::slot_manager::SlotManager,
        hold: bool,
        held: Arc<Mutex<Option<Release>>>,
    }

    #[async_trait]
    impl TokenChecks for Holding {
        async fn changed(&self, slot_ids: &[String]) {
            if let (true, Some(slot_id)) = (self.hold, slot_ids.first()) {
                *self.held.lock().unwrap() = self.slots.hold_for_logon(slot_id, GSLT_C);
            }
        }
    }

    struct Setup {
        peer: ControllerPeer,
        base: Arc<Slots>,
        held: Arc<Mutex<Option<Release>>>,
        _connection: ControllerConnection,
        _dir: tempfile::TempDir,
    }

    /// What main wires: slot changes go through a change reporter that the hello sets, the hello is
    /// `on_open`, and a welcome reports each slot's current status.
    async fn wired(hold: bool) -> Setup {
        let peer = ControllerPeer::start(PeerOptions { welcome: false, ..PeerOptions::default() }).await;
        let base = Arc::new(make_slots(agent_config(&json!({}))));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.yml");

        std::fs::write(
            &file,
            "token: 0123456789abcdef0123456789abcdef\ncontroller:\n  host: cs.example.com\n  tls: true\ncs2_image: ghcr.io/example/ankor-cs2:latest\nagent_image: ghcr.io/example/ankor-agent:latest\n",
        )
        .unwrap();

        let held: Arc<Mutex<Option<Release>>> = Arc::default();
        let tokens = Arc::new(Holding { slots: base.slots.clone(), hold, held: Arc::clone(&held) });
        let path = file.clone();
        let handler = make_handler(&base, move |deps| {
            deps.config = deps.config.at(path);
            deps.gslts = tokens;
        });
        let connection_slot: Arc<Mutex<Option<ControllerConnection>>> = Arc::default();
        let sending = Arc::clone(&connection_slot);
        let reports = ChangeReporter::<AgentSlotStatus>::new(
            |frame: &AgentSlotStatus| frame.slot_id.clone(),
            Arc::new(move |frame: AgentSlotStatus| {
                let connection = sending.lock().unwrap().clone();

                Box::pin(async move {
                    match connection {
                        Some(connection) => connection.push(&AgentToControllerMessage::SlotStatus(frame)).await,
                        None => false,
                    }
                })
            }),
        );
        let status_frame = |status: ankor_protocol::types::SlotStatus| AgentSlotStatus {
            slot_id: status.slot_id,
            state: status.state,
            allocation_id: status.allocation_id,
            match_id: status.match_id,
            plugin_online: status.plugin_online,
            gslt_state: status.gslt_state,
            reserved: status.reserved,
            server_steam_id: status.server_steam_id.clone(),
        };

        // The slot manager's events, fed to the reporter as main feeds them.
        {
            let (base, reports) = (Arc::clone(&base), reports.clone());

            tokio::spawn(async move {
                let mut seen = 0;

                loop {
                    let events = base.events();

                    for event in events.iter().skip(seen) {
                        match event {
                            Reported::SlotState(status) => reports.report(status_frame(status.clone())),
                            Reported::SlotRemoved(slot_id) => reports.forget(slot_id),
                            _ => {}
                        }
                    }

                    seen = events.len();
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            });
        }

        let (open_base, open_reports, welcome_base, welcome_reports) = (Arc::clone(&base), reports.clone(), Arc::clone(&base), reports.clone());
        let url = peer.url.clone();
        let connection = connect(Wired {
            url: Arc::new(move || url.clone()),
            deps: handler.deps.clone(),
            on_open: Some(Arc::new(move |connection| {
                let (base, reports) = (Arc::clone(&open_base), open_reports.clone());

                Box::pin(async move {
                    let statuses: Vec<AgentSlotStatus> = base.slots.statuses().into_iter().map(status_frame).collect();

                    reports.helloed(&statuses);
                    connection.push(&hello_of(&base, None)).await;
                    Ok(())
                })
            })),
            on_welcome: Some(Arc::new(move || {
                for status in welcome_base.slots.statuses() {
                    welcome_reports.report(status_frame(status));
                }
            })),
        });

        *connection_slot.lock().unwrap() = Some(connection.clone());
        until("the first hello", || peer.hellos() == 1).await;
        peer.send_json(&welcome());
        // A request answered after the welcome: the catch-up has gone out by then.
        assert_eq!(peer.request(json!({ "type": "delete_demo", "slot_id": "a", "file_name": "x.dem" })).await["ok"], true);

        Setup { peer, base, held, _connection: connection, _dir: dir }
    }

    #[tokio::test]
    async fn replies_with_the_applied_settings_then_reports_the_slot_it_gave_a_new_token_and_says_no_second_hello() {
        let setup = wired(true).await;
        let peer = &setup.peer;

        // A checked token, so the new one's `unchecked` is news to the controller.
        setup.base.slots.set_gslt_state("a", GSLT_A, GsltState::Ok);
        until("the checked token", || {
            peer.received()
                .iter()
                .any(|frame| matches!(agent_message(frame), Some(AgentToControllerMessage::SlotStatus(status)) if status.gslt_state == GsltState::Ok))
        })
        .await;

        let before = peer.received().len();
        let gslt_b = setup.base.config.lock().unwrap().slots[1].gslt.clone();

        peer.send_json(&json!({ "type": "set_config", "id": "edit", "config": {
            "slots": [{ "id": "a", "port": 27015, "tv_port": 27020, "gslt": GSLT_C }, { "id": "b", "port": 27016, "tv_port": 27021 }],
        } }));

        let reply = peer.wait_for("the set_config reply", is_reply("edit")).await;

        assert_eq!(reply["ok"], true);
        assert_eq!(
            reply["data"]["slots"],
            json!([
                { "id": "a", "port": 27015, "tv_port": 27020, "gslt_hint": &GSLT_C[GSLT_C.len() - 10..] },
                { "id": "b", "port": 27016, "tv_port": 27021, "gslt_hint": &gslt_b[gslt_b.len() - 10..] },
            ])
        );
        until("the new token reported and reserved", || peer.received().len() >= before + 3).await;

        // The reply first, so the controller knows the layout; then the slot, then the token check's hold.
        let after: Vec<Value> = peer.received()[before..].to_vec();

        assert_eq!(after[0], reply);
        assert_eq!(
            (after[1]["slot_id"].clone(), after[1]["gslt_state"].clone(), after[1]["reserved"].clone()),
            (json!("a"), json!(GsltState::Unchecked), json!(false))
        );
        assert_eq!(
            (after[2]["slot_id"].clone(), after[2]["gslt_state"].clone(), after[2]["reserved"].clone()),
            (json!("a"), json!(GsltState::Unchecked), json!(true))
        );

        let release = setup.held.lock().unwrap().take().unwrap();

        release.answer(None);
        until("the hold released", || peer.received().len() == before + 4).await;

        let last = peer.received().last().cloned().unwrap();

        assert!(matches!(agent_message(&last), Some(AgentToControllerMessage::SlotStatus(status)) if status.slot_id == "a" && !status.reserved));
        assert_eq!(peer.hellos(), 1);
        peer.expect_valid_frames();
    }

    #[tokio::test]
    async fn answers_an_edit_that_changes_nothing_with_the_same_settings_and_reports_nothing_after_it() {
        let setup = wired(false).await;
        let peer = &setup.peer;
        let before = peer.received().len();

        peer.send_json(&json!({ "type": "set_config", "id": "same", "config": {} }));

        let reply = peer.wait_for("the set_config reply", is_reply("same")).await;

        assert_eq!(reply["ok"], true);
        assert_eq!(reply["data"]["controller"], serde_json::to_value(setup.base.config.lock().unwrap().controller.clone()).unwrap());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(peer.received()[before..].iter().map(agent_message).collect::<Vec<_>>().as_slice(), [Some(AgentToControllerMessage::Reply(_))]));
        assert_eq!(peer.hellos(), 1);
    }
}
