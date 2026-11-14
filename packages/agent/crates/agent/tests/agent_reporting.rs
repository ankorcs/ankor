//! What reaches the controller about the node: the hello, the reports that send only what the controller does
//! not hold, and the feeds from slots, plugins and the spool's kept exits.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ankor_agent::agent_reporting::{
    ChangeReporter, DemoReporter, ExitReporter, HelloInput, ListDemos, PluginSlots, SendFrame, SlotFeed, StatusReports, build_hello, newest_demos,
    node_unavailable, plugin_feed,
};
use ankor_agent::contract::types::{PluginToAgentMessage, ServerStatus};
use ankor_agent::docker_api::DockerError;
use ankor_agent::event_uploader::nothing_stuck;
use ankor_agent::install::{MaintenanceGuard, MaintenanceGuardOptions};
use ankor_agent::logger::{Entry, Logger};
use ankor_agent::node_log::{NodeLog, Transport};
use ankor_agent::slot_manager::SlotEvent;
use ankor_agent::spool_client::SpoolPort;
use ankor_agent::testing::fixtures::{GSLT_A, GSLT_B, agent_config, idle_statuses, launch_config, ready_status};
use ankor_agent::testing::slots::{Reported, Slots, launch_server, make_slots};
use ankor_agent::testing::{FakeAllocations, FakeDocker, until};
use ankor_protocol::config::AgentConfig;
use ankor_protocol::constants::{ENDED_SERVER_LOGS, MAX_REPORTED_DEMOS, MAX_SLOTS};
use ankor_protocol::schemas::s_agent_to_controller_message;
use ankor_protocol::types::{
    AgentDemoRemoved, AgentInfo, AgentNodeStatus, AgentServerExited, AgentSlotStatus, AgentToControllerMessage, Cs2State, DemoFile, DemoRemovedReason,
    GsltState, HostInfo, LogSource, LogSourceAgent, LogSourceServer, MaintenanceOperation, NodeUnavailable, NodeUnavailableCode, ServerExit, ServerExitReason,
    SlotState,
};
use ankor_protocol::validate::{format_issues, safe_parse};
use futures_util::FutureExt as _;
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// Lets spawned work run until it waits on something.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// The frame as the contract reads it back; fails on a frame the contract refuses.
fn parsed(frame: &AgentToControllerMessage) -> Value {
    safe_parse(s_agent_to_controller_message, serde_json::to_value(frame).unwrap())
        .unwrap_or_else(|issues| panic!("an invalid frame: {}", format_issues(&issues)))
}

/// A socket that takes frames only once welcomed, as the connection does.
#[derive(Clone, Default)]
struct GatedSocket {
    welcomed: Arc<AtomicBool>,
    sent: Arc<Mutex<Vec<AgentToControllerMessage>>>,
}

impl GatedSocket {
    fn welcome(&self, open: bool) {
        self.welcomed.store(open, Ordering::SeqCst);
    }

    fn sent(&self) -> Vec<AgentToControllerMessage> {
        self.sent.lock().unwrap().clone()
    }

    fn send<T: Send + 'static>(&self, wrap: fn(T) -> AgentToControllerMessage) -> SendFrame<T> {
        let socket = self.clone();

        Arc::new(move |frame| {
            let open = socket.welcomed.load(Ordering::SeqCst);

            if open {
                socket.sent.lock().unwrap().push(wrap(frame));
            }

            async move { open }.boxed()
        })
    }
}

/// Records every frame and reports it sent.
fn recorder<T: Send + 'static>(sent: &Arc<Mutex<Vec<AgentToControllerMessage>>>, wrap: fn(T) -> AgentToControllerMessage) -> SendFrame<T> {
    let sent = Arc::clone(sent);

    Arc::new(move |frame| {
        sent.lock().unwrap().push(wrap(frame));

        async { true }.boxed()
    })
}

/// Records every frame and holds its result until the test gives it.
#[allow(clippy::type_complexity, reason = "a test double")]
fn held<T: Send + 'static>(
    wrap: fn(T) -> AgentToControllerMessage,
) -> (SendFrame<T>, Arc<Mutex<Vec<AgentToControllerMessage>>>, Arc<Mutex<Vec<oneshot::Sender<bool>>>>) {
    let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
    let finishes: Arc<Mutex<Vec<oneshot::Sender<bool>>>> = Arc::default();
    let (record, waiting) = (Arc::clone(&sent), Arc::clone(&finishes));
    let send: SendFrame<T> = Arc::new(move |frame| {
        let (finish, result) = oneshot::channel();

        record.lock().unwrap().push(wrap(frame));
        waiting.lock().unwrap().push(finish);

        async move { result.await.unwrap_or(false) }.boxed()
    });

    (send, sent, finishes)
}

fn finish(finishes: &Arc<Mutex<Vec<oneshot::Sender<bool>>>>, index: usize, sent: bool) {
    let finish = std::mem::replace(&mut finishes.lock().unwrap()[index], oneshot::channel().0);

    finish.send(sent).unwrap();
}

fn slot_reports(send: SendFrame<AgentSlotStatus>) -> ChangeReporter<AgentSlotStatus> {
    ChangeReporter::new(|frame: &AgentSlotStatus| frame.slot_id.clone(), send)
}

fn node_reports(send: SendFrame<AgentNodeStatus>) -> ChangeReporter<AgentNodeStatus> {
    ChangeReporter::new(|_: &AgentNodeStatus| "node".to_owned(), send)
}

fn status(reserved: bool) -> AgentSlotStatus {
    AgentSlotStatus {
        slot_id: "a".to_owned(),
        state: SlotState::Idle,
        allocation_id: None,
        match_id: None,
        plugin_online: false,
        gslt_state: GsltState::Ok,
        reserved,
        server_steam_id: None,
    }
}

fn idle() -> AgentSlotStatus {
    status(false)
}

fn reserved() -> AgentSlotStatus {
    status(true)
}

fn cs2(build_id: &str) -> Cs2State {
    Cs2State {
        build_id: Some(build_id.to_owned()),
        latest_build_id: Some("19001".to_owned()),
        image: "ankor-cs2:latest".to_owned(),
        image_id: None,
        update: None,
        image_update: None,
    }
}

fn runtime() -> AgentNodeStatus {
    AgentNodeStatus { cs2: cs2("19000"), unavailable: None, agent_update: None }
}

fn updated() -> AgentNodeStatus {
    AgentNodeStatus { cs2: cs2("19001"), unavailable: None, agent_update: None }
}

fn exits_to(sent: &Arc<Mutex<Vec<AgentToControllerMessage>>>) -> ExitReporter {
    ExitReporter::new(recorder(sent, AgentToControllerMessage::ServerExited), None, Logger::null())
}

/// Feeds the manager's events, in order, into the feed.
struct Pump {
    feed: SlotFeed,
    seen: usize,
}

impl Pump {
    fn pump(&mut self, base: &Slots) {
        let events = base.events();

        for event in &events[self.seen..] {
            let event = match event.clone() {
                Reported::ServerExited(exit, _) => SlotEvent::ServerExited(exit),
                Reported::SlotState(status) => SlotEvent::SlotState(status),
                Reported::ContainerLog { allocation_id, line } => SlotEvent::ContainerLog { allocation_id, line },
                Reported::SlotIdle(slot_id) => SlotEvent::SlotIdle { slot_id, allocation_id: None },
                Reported::SlotRemoved(slot_id) => SlotEvent::SlotRemoved(slot_id),
                Reported::TokenUsed(token) => SlotEvent::TokenUsed(token),
            };

            self.feed.handle(&event);
        }

        self.seen = events.len();
    }
}

fn wire(status: ankor_protocol::types::SlotStatus) -> AgentSlotStatus {
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

fn config() -> AgentConfig {
    agent_config(&json!({}))
}

mod slot_and_runtime_reports {
    use super::*;

    #[tokio::test]
    async fn sends_nothing_after_the_welcome_when_nothing_changed_since_the_hello() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));
        let nodes = node_reports(socket.send(AgentToControllerMessage::NodeStatus));

        slots.helloed(&[idle()]);
        nodes.helloed(&[runtime()]);
        socket.welcome(true);
        slots.report(idle());
        nodes.report(runtime());
        settle().await;
        assert_eq!(socket.sent(), []);
    }

    #[tokio::test]
    async fn sends_a_slot_again_once_it_is_forgotten_though_it_matches_what_the_controller_last_had() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));

        socket.welcome(true);
        slots.helloed(&[idle()]);
        // Removed and added again under the same id: the controller dropped the slot with the layout.
        slots.forget("a");
        slots.report(idle());
        settle().await;
        assert_eq!(socket.sent(), [AgentToControllerMessage::SlotStatus(idle())]);
    }

    /// A server's Steam ID learned between the hello and the welcome is a change of its slot.
    #[tokio::test]
    async fn sends_a_steam_id_learned_before_the_welcome_once_after_it() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));
        let running = AgentSlotStatus {
            state: SlotState::Running,
            allocation_id: Some("5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e".to_owned()),
            plugin_online: true,
            ..idle()
        };
        let verified = AgentSlotStatus { server_steam_id: Some("85568392936840254".to_owned()), ..running.clone() };

        slots.helloed(std::slice::from_ref(&running));
        slots.report(verified.clone());
        settle().await;
        socket.welcome(true);
        // The welcome's catch-up reports every slot again; only one that differs from the hello goes out.
        slots.report(running.clone());
        slots.report(verified.clone());
        settle().await;
        assert_eq!(socket.sent(), [AgentToControllerMessage::SlotStatus(verified)]);
    }

    #[tokio::test]
    async fn sends_a_slot_change_held_back_before_the_welcome_once_after_it() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));

        slots.helloed(&[idle()]);
        slots.report(reserved());
        settle().await;
        socket.welcome(true);
        slots.report(reserved());
        settle().await;
        slots.report(reserved());
        settle().await;
        assert_eq!(socket.sent(), [AgentToControllerMessage::SlotStatus(reserved())]);
        assert_eq!(
            parsed(&socket.sent()[0]),
            json!({ "type": "slot_status", "slot_id": "a", "state": "idle", "allocation_id": null, "match_id": null, "plugin_online": false, "gslt_state": "ok", "reserved": true, "server_steam_id": null })
        );
    }

    /// The catch-up can run before any task the refused report left behind, as on a runtime with
    /// more threads, and still sends the frame.
    #[tokio::test]
    async fn sends_a_slot_change_refused_before_the_welcome_when_the_catch_up_comes_before_any_other_task() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));

        slots.helloed(&[idle()]);
        slots.report(reserved());
        socket.welcome(true);
        slots.report(reserved());
        settle().await;
        assert_eq!(socket.sent(), [AgentToControllerMessage::SlotStatus(reserved())]);
    }

    #[tokio::test]
    async fn sends_nothing_after_the_welcome_for_a_slot_that_changed_and_changed_back_before_it() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));

        slots.helloed(&[idle()]);
        slots.report(reserved());
        slots.report(idle());
        settle().await;
        socket.welcome(true);
        slots.report(idle());
        settle().await;
        assert_eq!(socket.sent(), []);
    }

    #[tokio::test]
    async fn sends_a_runtime_change_held_back_before_the_welcome_once_after_it() {
        let socket = GatedSocket::default();
        let nodes = node_reports(socket.send(AgentToControllerMessage::NodeStatus));

        nodes.helloed(&[runtime()]);
        nodes.report(updated());
        settle().await;
        socket.welcome(true);
        nodes.report(updated());
        settle().await;
        nodes.report(updated());
        settle().await;
        assert_eq!(socket.sent(), [AgentToControllerMessage::NodeStatus(updated())]);
        assert_eq!(
            parsed(&socket.sent()[0]),
            json!({ "type": "node_status", "cs2": { "build_id": "19001", "latest_build_id": "19001", "image": "ankor-cs2:latest", "image_id": null, "update": null, "image_update": null }, "unavailable": null, "agent_update": null })
        );
    }

    #[tokio::test]
    async fn takes_a_later_hello_as_what_the_controller_holds_over_a_frame_still_in_flight() {
        let (send, sent, finishes) = held(AgentToControllerMessage::SlotStatus);
        let slots = slot_reports(send);

        slots.helloed(&[idle()]);
        slots.report(reserved());
        slots.helloed(&[reserved()]);
        finish(&finishes, 0, false);
        settle().await;
        slots.report(reserved());
        settle().await;
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn counts_no_send_result_that_comes_back_after_a_newer_hello() {
        let (send, sent, finishes) = held(AgentToControllerMessage::SlotStatus);
        let slots = slot_reports(send);

        slots.helloed(&[idle()]);
        slots.report(reserved());
        slots.helloed(&[idle()]);
        finish(&finishes, 0, true);
        settle().await;
        slots.report(reserved());
        finish(&finishes, 1, false);
        settle().await;
        slots.report(reserved());
        settle().await;
        assert_eq!(sent.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn keeps_a_newer_frame_that_left_the_socket_when_an_older_one_in_flight_did_not() {
        let (send, sent, finishes) = held(AgentToControllerMessage::SlotStatus);
        let slots = slot_reports(send);

        slots.helloed(&[]);
        slots.report(reserved());
        slots.report(idle());
        settle().await;
        finish(&finishes, 0, false);
        finish(&finishes, 1, true);
        settle().await;
        slots.report(idle());
        settle().await;
        assert_eq!(sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sends_a_forgotten_slot_again_after_its_first_frame_never_left_the_socket() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));

        slots.helloed(&[idle()]);
        slots.forget("a");
        slots.report(idle());
        settle().await;
        socket.welcome(true);
        slots.report(idle());
        settle().await;
        assert_eq!(socket.sent(), [AgentToControllerMessage::SlotStatus(idle())]);
    }

    #[tokio::test]
    async fn falls_back_to_the_last_frame_the_controller_got_not_the_hello_when_a_change_never_leaves_the_socket() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));

        slots.helloed(&[idle()]);
        socket.welcome(true);
        slots.report(reserved());
        settle().await;
        // The connection drops before the next change gets out; the controller still holds `reserved`.
        socket.welcome(false);
        slots.report(idle());
        settle().await;
        socket.welcome(true);
        slots.report(idle());
        settle().await;

        let reserved: Vec<_> = socket
            .sent()
            .iter()
            .map(|frame| match frame {
                AgentToControllerMessage::SlotStatus(status) => status.reserved,
                _ => panic!("{frame:?}"),
            })
            .collect();

        assert_eq!(reserved, [true, false]);
    }

    #[tokio::test]
    async fn sends_a_slot_the_hello_did_not_name_once_its_first_frame_never_left_the_socket() {
        let socket = GatedSocket::default();
        let slots = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));

        slots.helloed(&[]);
        slots.report(reserved());
        settle().await;
        socket.welcome(true);
        slots.report(reserved());
        settle().await;
        assert_eq!(socket.sent(), [AgentToControllerMessage::SlotStatus(reserved())]);
    }

    #[tokio::test]
    async fn sends_a_removed_slot_again_when_it_comes_back_the_same() {
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let reports = slot_reports(recorder(&sent, AgentToControllerMessage::SlotStatus));
        let feed = SlotFeed::new(Arc::new(reports), ExitReporter::new(Arc::new(|_| async { true }.boxed()), None, Logger::null()), NodeLog::default());
        let idle_slot = ankor_protocol::types::SlotStatus {
            slot_id: "a".to_owned(),
            state: SlotState::Idle,
            allocation_id: None,
            match_id: None,
            plugin_online: false,
            gslt_state: GsltState::Ok,
            reserved: false,
            server_steam_id: None,
        };

        feed.handle(&SlotEvent::SlotState(idle_slot.clone()));
        settle().await;
        feed.handle(&SlotEvent::SlotRemoved("a".to_owned()));
        feed.handle(&SlotEvent::SlotState(idle_slot));
        settle().await;
        assert_eq!(*sent.lock().unwrap(), [AgentToControllerMessage::SlotStatus(idle()), AgentToControllerMessage::SlotStatus(idle())]);
    }

    #[tokio::test]
    async fn sends_one_starting_for_a_launch_though_the_slot_also_records_its_container() {
        let socket = GatedSocket::default();
        let reports = slot_reports(socket.send(AgentToControllerMessage::SlotStatus));
        let base = make_slots(config());
        let mut pump = Pump {
            feed: SlotFeed::new(Arc::new(reports.clone()), ExitReporter::new(Arc::new(|_| async { true }.boxed()), None, Logger::null()), NodeLog::default()),
            seen: 0,
        };
        let allocation_id = "22222222-2222-4222-8222-222222222222";

        reports.helloed(&base.slots.statuses().into_iter().map(wire).collect::<Vec<_>>());
        socket.welcome(true);
        launch_server(&base.slots, launch_config(&json!({})), allocation_id).await.unwrap();
        settle().await;
        pump.pump(&base);
        settle().await;
        assert_eq!(
            socket.sent(),
            [AgentToControllerMessage::SlotStatus(AgentSlotStatus {
                slot_id: "a".to_owned(),
                state: SlotState::Starting,
                allocation_id: Some(allocation_id.to_owned()),
                match_id: Some("0199a0b4-5c00-7000-8000-000000000001".to_owned()),
                plugin_online: false,
                gslt_state: GsltState::Unchecked,
                reserved: false,
                server_steam_id: None,
            })]
        );
    }
}

mod a_stopping_slot {
    use super::*;

    const ALLOCATION_ID: &str = "33333333-3333-4333-8333-333333333333";
    const MATCH_ID: &str = "0199a0b4-5c00-7000-8000-000000000001";

    /// A running, linked game whose slot reports reach the controller through the reporter.
    async fn running_game() -> (Slots, Pump, Arc<Mutex<Vec<AgentToControllerMessage>>>) {
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let reports = slot_reports(recorder(&sent, AgentToControllerMessage::SlotStatus));
        let feed = SlotFeed::new(Arc::new(reports), exits_to(&sent), NodeLog::default());
        let base = make_slots(config());
        let mut pump = Pump { feed, seen: 0 };

        launch_server(&base.slots, launch_config(&json!({})), ALLOCATION_ID).await.unwrap();
        base.slots.plugin_online_changed(ALLOCATION_ID, true);
        base.slots.plugin_status(ALLOCATION_ID, &ready_status());
        settle().await;
        pump.pump(&base);
        settle().await;
        sent.lock().unwrap().clear();

        (base, pump, sent)
    }

    fn stopping(plugin_online: bool) -> AgentToControllerMessage {
        AgentToControllerMessage::SlotStatus(AgentSlotStatus {
            slot_id: "a".to_owned(),
            state: SlotState::Stopping,
            allocation_id: Some(ALLOCATION_ID.to_owned()),
            match_id: Some(MATCH_ID.to_owned()),
            plugin_online,
            gslt_state: GsltState::Ok,
            reserved: false,
            server_steam_id: None,
        })
    }

    #[tokio::test]
    async fn reports_the_stop_the_plugin_leaving_during_it_then_the_exit_and_the_idle_slot() {
        let (base, mut pump, sent) = running_game().await;
        let (slots, docker) = (base.slots.clone(), Arc::clone(&base.docker));

        base.docker.override_stop(move |_, _| async move {
            slots.plugin_online_changed(ALLOCATION_ID, false);
            docker.exit(docker.last(), Some(143));

            Ok(())
        });
        base.slots.stop_allocation(ALLOCATION_ID).await.unwrap();
        settle().await;
        pump.pump(&base);
        settle().await;

        let sent = sent.lock().unwrap().clone();

        assert_eq!(
            sent,
            [
                stopping(true),
                stopping(false),
                AgentToControllerMessage::ServerExited(AgentServerExited {
                    slot_id: "a".to_owned(),
                    allocation_id: ALLOCATION_ID.to_owned(),
                    reason: ServerExitReason::Requested,
                    exit_code: Some(143),
                    detail: None,
                }),
                AgentToControllerMessage::SlotStatus(AgentSlotStatus { gslt_state: GsltState::Ok, ..idle() }),
            ]
        );

        for frame in &sent {
            parsed(frame);
        }
    }

    #[tokio::test]
    async fn reports_the_plugin_leaving_during_a_failed_stop_and_polls_the_plugin_only_while_it_is_connected() {
        let (base, mut pump, sent) = running_game().await;
        let slots = base.slots.clone();

        base.docker.override_stop(move |_, _| async move {
            slots.plugin_online_changed(ALLOCATION_ID, false);

            Err(DockerError::Http("Docker unavailable".to_owned()))
        });
        assert!(base.slots.stop_allocation(ALLOCATION_ID).await.unwrap_err().to_string().contains("Docker unavailable"));
        settle().await;
        pump.pump(&base);
        settle().await;
        assert_eq!(*sent.lock().unwrap(), [stopping(true), stopping(false)]);

        let status = base.status("a");

        assert_eq!((status.state, status.plugin_online), (SlotState::Stopping, false));
        base.polled.lock().unwrap().clear();
        base.slots.check_games(tokio::time::Instant::now());
        assert!(base.polled.lock().unwrap().is_empty());
    }
}

/// The rest of a `hello` input, so a test about `unavailable` carries only that.
fn hello_input(unavailable: Option<NodeUnavailable>) -> HelloInput {
    HelloInput {
        config: config(),
        agent: AgentInfo {
            version: "0.1.0".to_owned(),
            image: "ghcr.io/example/ankor-agent:latest".to_owned(),
            image_id: "sha256:3d46a8ba87dcba6788a16842cc8e4b60b52e546f08fd0541229bb00918984e0a".to_owned(),
        },
        host: HostInfo { cpu_count: 8, os_name: Some("Debian GNU/Linux 13 (trixie)".to_owned()), kernel: "6.12.48".to_owned() },
        public_ip: Some("203.0.113.7".to_owned()),
        country: Some("DK".to_owned()),
        slot_status: idle_statuses(&config()),
        cs2: cs2("19000"),
        unavailable,
        demos: Vec::new(),
        server_exits: Vec::new(),
        event_backlog: nothing_stuck(),
        agent_update: None,
    }
}

fn hello_of(frame: &AgentToControllerMessage) -> &ankor_protocol::types::AgentHello {
    match frame {
        AgentToControllerMessage::Hello(hello) => hello,
        _ => panic!("{frame:?}"),
    }
}

mod hello {
    use super::*;

    #[test]
    fn reports_the_resolved_public_address_and_the_slots() {
        let image_id = "sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let hello = build_hello(HelloInput { cs2: Cs2State { image_id: Some(image_id.to_owned()), ..cs2("19000") }, ..hello_input(None) });
        let value = serde_json::to_value(&hello).unwrap();

        assert!(matches!(hello, AgentToControllerMessage::Hello(_)));
        assert_eq!(
            value["agent"],
            json!({ "version": "0.1.0", "image": "ghcr.io/example/ankor-agent:latest", "image_id": "sha256:3d46a8ba87dcba6788a16842cc8e4b60b52e546f08fd0541229bb00918984e0a" })
        );
        assert_eq!(value["host"], json!({ "cpu_count": 8, "os_name": "Debian GNU/Linux 13 (trixie)", "kernel": "6.12.48" }));
        assert_eq!((&value["public_ip"], &value["country"]), (&json!("203.0.113.7"), &json!("DK")));
        assert_eq!(
            value["cs2"],
            json!({ "build_id": "19000", "latest_build_id": "19001", "image": "ankor-cs2:latest", "image_id": image_id, "update": null, "image_update": null })
        );
        // A slot's token leaves the node only as its hint.
        assert_eq!(
            value["config"]["slots"],
            json!([{ "id": "a", "port": 27015, "tv_port": 27020, "gslt_hint": "AAAAAA0001" }, { "id": "b", "port": 27016, "tv_port": 27021, "gslt_hint": "AAAAAA0002" }])
        );
        assert!(!value.to_string().contains(GSLT_A));
        assert!(!value.to_string().contains(GSLT_B));
        parsed(&hello);

        // No build id: the node reports no game files rather than an install the controller could dispatch
        // onto.
        let empty =
            build_hello(HelloInput { public_ip: Some("auto-resolved".to_owned()), cs2: Cs2State { build_id: None, ..cs2("19000") }, ..hello_input(None) });

        assert_eq!(hello_of(&empty).cs2.build_id, None);
    }

    #[tokio::test]
    async fn tells_the_controller_an_update_is_running_here() {
        let docker = FakeDocker::new();

        *docker.update_container.lock().unwrap() = Some(("ankor-update-aa11bb".to_owned(), None));

        let updates = MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn ankor_agent::docker::DockerPort>));

        updates.adopt_running().await.unwrap();

        let hello = build_hello(hello_input(node_unavailable(updates.holder(), None)));

        // The frame the controller restarts into: without this it comes back with an empty update map and its
        // watch dispatches a second `update_cs2` onto a host that is still downloading.
        assert_eq!(
            hello_of(&hello).unavailable,
            Some(NodeUnavailable {
                code: NodeUnavailableCode::Maintenance,
                operation: Some(MaintenanceOperation::Cs2Update),
                detail: "an update container a previous agent started".to_owned()
            })
        );
        parsed(&hello);
    }

    #[tokio::test]
    async fn reports_an_update_this_agent_is_running_then_falls_back_to_the_spools_reason() {
        let updates = MaintenanceGuard::new(MaintenanceGuardOptions::new(FakeDocker::new()));
        let spool = NodeUnavailable { code: NodeUnavailableCode::EventStorage, operation: None, detail: "event spool schema mismatch".to_owned() };
        let release = updates.claim(MaintenanceOperation::Cs2Update).await.unwrap();

        assert_eq!(
            node_unavailable(updates.holder(), Some(spool.clone())),
            Some(NodeUnavailable { code: NodeUnavailableCode::Maintenance, operation: Some(MaintenanceOperation::Cs2Update), detail: "CS2 update".to_owned() })
        );

        release.release(None);
        assert_eq!(node_unavailable(updates.holder(), Some(spool.clone())), Some(spool.clone()));
        assert_eq!(node_unavailable(updates.holder(), None), None);
        parsed(&build_hello(hello_input(node_unavailable(updates.holder(), Some(spool)))));
    }

    #[tokio::test]
    async fn names_the_operation_that_holds_the_guard() {
        let updates = MaintenanceGuard::new(MaintenanceGuardOptions::new(FakeDocker::new()));
        let mut seen = Vec::new();

        for operation in
            [MaintenanceOperation::Cs2Update, MaintenanceOperation::AgentUpdate, MaintenanceOperation::Cs2ImageUpdate, MaintenanceOperation::ImageCleanup]
        {
            let release = updates.claim(operation).await.unwrap();
            let hello = build_hello(hello_input(node_unavailable(updates.holder(), None)));

            parsed(&hello);
            seen.push(serde_json::to_value(&hello_of(&hello).unavailable).unwrap());
            release.release(None);
        }

        assert_eq!(
            seen,
            [
                json!(NodeUnavailable {
                    code: NodeUnavailableCode::Maintenance,
                    operation: Some(MaintenanceOperation::Cs2Update),
                    detail: "CS2 update".to_owned()
                }),
                json!(NodeUnavailable {
                    code: NodeUnavailableCode::Maintenance,
                    operation: Some(MaintenanceOperation::AgentUpdate),
                    detail: "agent update".to_owned()
                }),
                json!(NodeUnavailable {
                    code: NodeUnavailableCode::Maintenance,
                    operation: Some(MaintenanceOperation::Cs2ImageUpdate),
                    detail: "CS2 image update".to_owned()
                }),
                json!(NodeUnavailable {
                    code: NodeUnavailableCode::Maintenance,
                    operation: Some(MaintenanceOperation::ImageCleanup),
                    detail: "image cleanup".to_owned()
                }),
            ]
        );
    }
}

mod the_hellos_demo_upload {
    use super::*;

    #[test]
    fn names_the_upload_header_and_carries_its_value_only_as_a_hash() {
        let secret = "Bearer s3cr3t-demo-key";
        let config = agent_config(
            &json!({ "demo_upload": { "url": "https://demos.example.com/put", "header": { "name": "Authorization", "value": secret }, "limit_mbit": 40 } }),
        );
        let hello = build_hello(HelloInput { config, ..hello_input(None) });
        let value = serde_json::to_value(&hello).unwrap();

        // The value's SHA-256, worked out elsewhere.
        assert_eq!(
            value["config"]["demo_upload"],
            json!({
                "url": "https://demos.example.com/put",
                "header": { "name": "Authorization", "value_sha256": "bb9f0795e1849eb6834db6334c99ac12a23638bd09da3bc21bca4ce6f57b2bd8" },
                "limit_mbit": 40,
            })
        );
        assert!(!value.to_string().contains(secret));
        assert_eq!(parsed(&hello), value);
    }

    #[test]
    fn reports_no_header_when_the_upload_has_none() {
        let config = agent_config(&json!({ "demo_upload": { "url": "https://demos.example.com/put" } }));
        let hello = build_hello(HelloInput { config, ..hello_input(None) });

        assert_eq!(
            serde_json::to_value(&hello).unwrap()["config"]["demo_upload"],
            json!({ "url": "https://demos.example.com/put", "header": null, "limit_mbit": null })
        );
    }
}

mod demo_reports {
    use super::*;

    fn demo(slot_id: &str, file_name: &str, size_bytes: i64, modified_at: i64) -> DemoFile {
        DemoFile {
            slot_id: slot_id.to_owned(),
            file_name: file_name.to_owned(),
            size_bytes,
            modified_at,
            recording: false,
            uploading: false,
            upload_error: None,
        }
    }

    fn held_demo() -> DemoFile {
        demo("a", "held.dem", 4096, 1_700_000_000_000)
    }

    fn fresh() -> DemoFile {
        demo("b", "fresh.dem", 2048, 1_700_000_100_000)
    }

    fn numbered(index: i64) -> DemoFile {
        demo("a", &format!("demo-{index}.dem"), 1, 1_700_000_000_000 + index)
    }

    fn frame(demo: &DemoFile) -> AgentToControllerMessage {
        AgentToControllerMessage::Demo(serde_json::from_value(serde_json::to_value(demo).unwrap()).unwrap())
    }

    fn removed(slot_id: &str, file_name: &str, reason: DemoRemovedReason) -> AgentToControllerMessage {
        AgentToControllerMessage::DemoRemoved(AgentDemoRemoved { slot_id: slot_id.to_owned(), file_name: file_name.to_owned(), reason })
    }

    /// A reporter sending into `sent` and saying each frame left, and a listing of `listed`.
    fn reporter(listed: &Arc<Mutex<Vec<DemoFile>>>, sent: &Arc<Mutex<Vec<AgentToControllerMessage>>>) -> (DemoReporter, ListDemos) {
        let listed = Arc::clone(listed);
        let list: ListDemos = Arc::new(move || {
            let demos = listed.lock().unwrap().clone();

            async move { Ok(demos) }.boxed()
        });

        (DemoReporter::new(recorder(sent, |frame| frame)), list)
    }

    #[tokio::test]
    async fn sends_each_demo_that_differs_from_what_the_controller_holds_and_each_that_left() {
        let listed = Arc::new(Mutex::new(vec![held_demo(), fresh()]));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let (reporter, list) = reporter(&listed, &sent);

        reporter.helloed(&[held_demo()]);
        reporter.report(&list).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), [frame(&fresh())]);

        *listed.lock().unwrap() = vec![DemoFile { uploading: true, ..held_demo() }];
        reporter.report(&list).await.unwrap();
        assert_eq!(sent.lock().unwrap()[1..], [frame(&DemoFile { uploading: true, ..held_demo() }), removed("b", "fresh.dem", DemoRemovedReason::Deleted)]);

        // Nothing changed: nothing to say.
        reporter.report(&list).await.unwrap();
        assert_eq!(sent.lock().unwrap().len(), 3);

        for frame in sent.lock().unwrap().iter() {
            parsed(frame);
        }
    }

    #[tokio::test]
    async fn reports_a_demo_removed_only_once_it_leaves_the_node_not_when_newer_ones_crowd_it_out_of_the_hello() {
        let reported = i64::try_from(MAX_REPORTED_DEMOS).unwrap();
        let listed = Arc::new(Mutex::new((0..reported).map(numbered).collect::<Vec<_>>()));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let (reporter, list) = reporter(&listed, &sent);

        reporter.helloed(&newest_demos(&listed.lock().unwrap()));
        // A new demo: the oldest the controller holds is no longer among the newest, but it is still on the
        // node.
        listed.lock().unwrap().push(numbered(reported));
        reporter.report(&list).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), [frame(&numbered(reported))]);

        // The controller still holds it, so it still hears of it.
        for entry in listed.lock().unwrap().iter_mut().filter(|entry| entry.file_name == "demo-0.dem") {
            entry.uploading = true;
        }

        reporter.report(&list).await.unwrap();
        assert_eq!(sent.lock().unwrap()[1..], [frame(&DemoFile { uploading: true, ..numbered(0) })]);

        // Uploaded: now it has left, and the reporter heard why.
        reporter.uploaded("a", "demo-0.dem");
        listed.lock().unwrap().retain(|entry| entry.file_name != "demo-0.dem");
        reporter.report(&list).await.unwrap();
        assert_eq!(sent.lock().unwrap()[2..], [removed("a", "demo-0.dem", DemoRemovedReason::Uploaded)]);
    }

    #[tokio::test]
    async fn keeps_why_a_demo_was_removed_across_a_hello_whether_or_not_the_hello_lists_it() {
        let listed: Arc<Mutex<Vec<DemoFile>>> = Arc::default();
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let (reporter, list) = reporter(&listed, &sent);

        reporter.helloed(&[held_demo()]);
        reporter.uploaded("a", "held.dem");
        reporter.helloed(&[held_demo()]);
        reporter.report(&list).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), [removed("a", "held.dem", DemoRemovedReason::Uploaded)]);

        // A hello whose listing missed the demo, as one that timed out on a slow disk does, while the
        // file is still there: a report lists it again, then it goes. The target took it.
        reporter.helloed(&[held_demo()]);
        reporter.uploaded("a", "held.dem");
        reporter.helloed(&[]);
        *listed.lock().unwrap() = vec![held_demo()];
        reporter.report(&list).await.unwrap();
        listed.lock().unwrap().clear();
        reporter.report(&list).await.unwrap();
        assert_eq!(sent.lock().unwrap()[1..], [frame(&held_demo()), removed("a", "held.dem", DemoRemovedReason::Uploaded)]);
    }

    #[tokio::test]
    async fn forgets_an_uploaded_demo_once_its_removal_went_out_and_keeps_one_the_target_took_before_any_report_listed_it() {
        let listed: Arc<Mutex<Vec<DemoFile>>> = Arc::default();
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let (reporter, list) = reporter(&listed, &sent);

        reporter.helloed(&[held_demo()]);
        reporter.uploaded("a", "held.dem");
        reporter.report(&list).await.unwrap();
        *listed.lock().unwrap() = vec![held_demo()];
        reporter.report(&list).await.unwrap();
        listed.lock().unwrap().clear();
        reporter.report(&list).await.unwrap();

        let kinds: Vec<Option<DemoRemovedReason>> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|frame| match frame {
                AgentToControllerMessage::DemoRemoved(removed) => Some(removed.reason),
                _ => None,
            })
            .collect();

        assert_eq!(kinds, [Some(DemoRemovedReason::Uploaded), None, Some(DemoRemovedReason::Deleted)]);

        // The target took it before any report listed it; a report that read the disk before its removal
        // tells the controller of it, and the next one of its removal.
        reporter.uploaded("b", "fresh.dem");
        *listed.lock().unwrap() = vec![fresh()];
        reporter.report(&list).await.unwrap();
        listed.lock().unwrap().clear();
        reporter.report(&list).await.unwrap();
        assert_eq!(sent.lock().unwrap()[3..], [frame(&fresh()), removed("b", "fresh.dem", DemoRemovedReason::Uploaded)]);
    }

    #[tokio::test]
    async fn leaves_a_demo_older_than_the_newest_unreported_until_a_hello_lists_it() {
        let reported = i64::try_from(MAX_REPORTED_DEMOS).unwrap();
        let listed = Arc::new(Mutex::new((0..=reported).map(numbered).collect::<Vec<_>>()));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let (reporter, list) = reporter(&listed, &sent);
        let newest = newest_demos(&listed.lock().unwrap());

        assert_eq!(newest.len(), usize::try_from(MAX_REPORTED_DEMOS).unwrap());
        assert!(!newest.iter().any(|entry| entry.file_name == "demo-0.dem"));
        reporter.helloed(&newest);
        reporter.report(&list).await.unwrap();
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sends_a_change_again_when_the_frame_never_left_the_socket() {
        let open = Arc::new(AtomicBool::new(false));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let (record, gate) = (Arc::clone(&sent), Arc::clone(&open));
        let list: ListDemos = Arc::new(|| async { Ok(vec![fresh()]) }.boxed());
        let reporter = DemoReporter::new(Arc::new(move |message| {
            record.lock().unwrap().push(message);

            let open = gate.load(Ordering::SeqCst);

            async move { open }.boxed()
        }));

        reporter.helloed(&[]);
        reporter.report(&list).await.unwrap();
        open.store(true, Ordering::SeqCst);
        reporter.report(&list).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), [frame(&fresh()), frame(&fresh())]);
    }

    #[tokio::test]
    async fn keeps_why_a_demo_was_removed_when_it_left_before_the_send_of_its_frame_resolved() {
        let listed = Arc::new(Mutex::new(vec![fresh()]));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (record, held) = (Arc::clone(&sent), Arc::clone(&gate));
        let listing = Arc::clone(&listed);
        let list: ListDemos = Arc::new(move || {
            let demos = listing.lock().unwrap().clone();

            async move { Ok(demos) }.boxed()
        });
        let reporter = DemoReporter::new(Arc::new(move |message| {
            record.lock().unwrap().push(message);

            let held = Arc::clone(&held);

            async move {
                held.acquire().await.is_ok_and(|permit| {
                    permit.forget();
                    true
                })
            }
            .boxed()
        }));

        reporter.helloed(&[]);

        let reporting = tokio::spawn({
            let (reporter, list) = (reporter.clone(), Arc::clone(&list));

            async move { reporter.report(&list).await }
        });

        // The demo uploads while its frame is still on its way out.
        until("the demo frame", || sent.lock().unwrap().len() == 1).await;
        reporter.uploaded("b", "fresh.dem");
        listed.lock().unwrap().clear();
        gate.add_permits(2);
        reporting.await.unwrap().unwrap();
        reporter.report(&list).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), [frame(&fresh()), removed("b", "fresh.dem", DemoRemovedReason::Uploaded)]);
    }

    /// The target takes a demo right after a report read it from the disk, before that report has told
    /// the controller of it; the removal that follows is still `uploaded`.
    #[tokio::test]
    async fn keeps_why_a_demo_was_removed_when_the_target_took_it_while_a_report_lists_it_for_the_first_time() {
        let listed = Arc::new(Mutex::new(vec![fresh()]));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let (record, listing) = (Arc::clone(&sent), Arc::clone(&listed));
        let reporter = DemoReporter::new(Arc::new(move |message| {
            record.lock().unwrap().push(message);

            async { true }.boxed()
        }));
        let telling = reporter.clone();
        let list: ListDemos = Arc::new(move || {
            let demos = listing.lock().unwrap().clone();

            if !demos.is_empty() {
                telling.uploaded("b", "fresh.dem");
            }

            async move { Ok(demos) }.boxed()
        });

        reporter.helloed(&[]);
        reporter.report(&list).await.unwrap();
        listed.lock().unwrap().clear();
        reporter.report(&list).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), [frame(&fresh()), removed("b", "fresh.dem", DemoRemovedReason::Uploaded)]);
    }

    #[tokio::test]
    async fn keeps_why_a_demo_the_controller_holds_was_removed_until_its_removal_leaves_the_socket() {
        let open = Arc::new(AtomicBool::new(false));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let (record, gate) = (Arc::clone(&sent), Arc::clone(&open));
        let list: ListDemos = Arc::new(|| async { Ok(Vec::new()) }.boxed());
        let reporter = DemoReporter::new(Arc::new(move |message| {
            record.lock().unwrap().push(message);

            let open = gate.load(Ordering::SeqCst);

            async move { open }.boxed()
        }));

        reporter.helloed(&[held_demo()]);
        reporter.uploaded("a", "held.dem");
        // The removal never leaves the socket; the next report says it again, with its reason.
        reporter.report(&list).await.unwrap();
        open.store(true, Ordering::SeqCst);
        reporter.report(&list).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), [removed("a", "held.dem", DemoRemovedReason::Uploaded), removed("a", "held.dem", DemoRemovedReason::Uploaded)]);
    }

    #[tokio::test]
    async fn keeps_reporting_after_a_listing_fails() {
        let fail = Arc::new(AtomicBool::new(true));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let failing = Arc::clone(&fail);
        let list: ListDemos = Arc::new(move || {
            let fail = failing.load(Ordering::SeqCst);

            async move { if fail { Err(std::io::Error::other("EIO")) } else { Ok(vec![fresh()]) } }.boxed()
        });
        let reporter = DemoReporter::new(recorder(&sent, |frame| frame));

        assert_eq!(reporter.report(&list).await.unwrap_err().to_string(), "EIO");
        fail.store(false, Ordering::SeqCst);
        reporter.report(&list).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), [frame(&fresh())]);
    }
}

fn game(index: usize) -> String {
    format!("00000000-0000-4000-8000-{index:012}")
}

fn exit_of(slot_id: &str, allocation_id: &str, reason: ServerExitReason, exit_code: Option<i64>) -> AgentServerExited {
    AgentServerExited { slot_id: slot_id.to_owned(), allocation_id: allocation_id.to_owned(), reason, exit_code, detail: None }
}

fn kept(exit: &AgentServerExited) -> ServerExit {
    ServerExit {
        slot_id: exit.slot_id.clone(),
        allocation_id: exit.allocation_id.clone(),
        reason: exit.reason,
        exit_code: exit.exit_code,
        detail: exit.detail.clone(),
    }
}

/// Sends exits only while `open`, recording those it sent.
fn gated_exits(open: &Arc<AtomicBool>, sent: &Arc<Mutex<Vec<AgentToControllerMessage>>>, store: Option<Arc<dyn SpoolPort>>, log: Logger) -> ExitReporter {
    let (open, sent) = (Arc::clone(open), Arc::clone(sent));

    ExitReporter::new(
        Arc::new(move |exit| {
            let open = open.load(Ordering::SeqCst);

            if open {
                sent.lock().unwrap().push(AgentToControllerMessage::ServerExited(exit));
            }

            async move { open }.boxed()
        }),
        store,
        log,
    )
}

mod a_slots_reports {
    use super::*;

    struct Pushed(Arc<Mutex<Vec<AgentToControllerMessage>>>);

    impl StatusReports for Pushed {
        fn report(&self, frame: AgentSlotStatus) {
            self.0.lock().unwrap().push(AgentToControllerMessage::SlotStatus(frame));
        }

        fn forget(&self, _: &str) {}
    }

    struct Frames(Mutex<Vec<AgentToControllerMessage>>);

    impl Transport for Frames {
        fn connected(&self) -> bool {
            true
        }

        fn send(&self, frame: AgentToControllerMessage) {
            self.0.lock().unwrap().push(frame);
        }
    }

    #[tokio::test]
    async fn go_to_the_controller_and_a_games_output_is_kept_until_enough_later_games_ended() {
        let pushed: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let frames = Arc::new(Frames(Mutex::default()));
        let node_log = NodeLog::default();
        let feed = SlotFeed::new(Arc::new(Pushed(Arc::clone(&pushed))), exits_to(&pushed), node_log.clone());
        let ended = usize::try_from(ENDED_SERVER_LOGS).unwrap();
        let idle_slot = ankor_protocol::types::SlotStatus {
            slot_id: "a".to_owned(),
            state: SlotState::Idle,
            allocation_id: None,
            match_id: None,
            plugin_online: false,
            gslt_state: GsltState::Ok,
            reserved: false,
            server_steam_id: None,
        };

        node_log.attach(Arc::clone(&frames) as Arc<dyn Transport>);

        for index in 0..=ended {
            feed.handle(&SlotEvent::ContainerLog { allocation_id: game(index), line: format!("game {index}") });
            feed.handle(&SlotEvent::ServerExited(kept(&exit_of("a", &game(index), ServerExitReason::CleanExit, Some(0)))));
            feed.handle(&SlotEvent::SlotState(idle_slot.clone()));
        }

        node_log.watch(&LogSource::Agent(LogSourceAgent {}), None);

        let pushed = pushed.lock().unwrap().clone();

        assert_eq!(pushed.len(), 2 * (ended + 1));
        assert_eq!(
            pushed[pushed.len() - 2..],
            [
                AgentToControllerMessage::ServerExited(exit_of("a", &game(ended), ServerExitReason::CleanExit, Some(0))),
                AgentToControllerMessage::SlotStatus(idle())
            ]
        );

        let sources: Vec<LogSource> = frames
            .0
            .lock()
            .unwrap()
            .iter()
            .flat_map(|frame| match frame {
                AgentToControllerMessage::LogIndex(index) => index.sources[1..].iter().map(|entry| entry.source.clone()).collect(),
                _ => Vec::new(),
            })
            .collect();

        assert_eq!(sources, (1..=ended).map(|index| LogSource::Server(LogSourceServer { allocation_id: game(index) })).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn keep_an_exit_that_did_not_leave_the_socket_for_the_next_welcome_and_drop_it_once_it_did() {
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let open = Arc::new(AtomicBool::new(false));
        let exits = gated_exits(&open, &sent, None, Logger::null());
        let exit = exit_of("a", &game(1), ServerExitReason::IdleEmpty, None);

        exits.exited(exit.clone());
        settle().await;
        assert!(sent.lock().unwrap().is_empty());

        open.store(true, Ordering::SeqCst);
        exits.welcomed();
        until("the exit", || *sent.lock().unwrap() == [AgentToControllerMessage::ServerExited(exit.clone())]).await;
        // The send's result lands after the frame left.
        settle().await;
        exits.welcomed();
        settle().await;
        assert_eq!(*sent.lock().unwrap(), [AgentToControllerMessage::ServerExited(exit)]);
    }

    #[tokio::test]
    async fn put_an_exit_held_back_before_the_welcome_in_the_hello_and_send_only_later_ones_after_it() {
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let open = Arc::new(AtomicBool::new(false));
        let exits = gated_exits(&open, &sent, None, Logger::null());
        let before = exit_of("a", &game(1), ServerExitReason::IdleEmpty, None);
        let after = exit_of("b", &game(2), ServerExitReason::StartTimeout, None);

        exits.exited(before.clone());
        assert_eq!(exits.helloed(), [kept(&before)]);
        // A hello that the controller never welcomed carries it again.
        assert_eq!(exits.helloed(), [kept(&before)]);
        exits.exited(after.clone());
        settle().await;

        open.store(true, Ordering::SeqCst);
        exits.welcomed();
        until("the later exit", || *sent.lock().unwrap() == [AgentToControllerMessage::ServerExited(after.clone())]).await;
        // The send's result lands after the frame left.
        settle().await;
        assert_eq!(exits.helloed(), []);
    }
}

mod exits_kept_in_the_spool {
    use super::*;

    fn calls(store: &FakeAllocations) -> Vec<String> {
        store.exit_calls.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn keeps_each_exit_until_it_leaves_the_socket_or_until_the_hello_that_carried_it_is_welcomed() {
        let store = Arc::new(FakeAllocations::default());
        let open = Arc::new(AtomicBool::new(false));
        let exits = gated_exits(&open, &Arc::default(), Some(Arc::clone(&store) as Arc<dyn SpoolPort>), Logger::null());
        let first = exit_of("a", &game(1), ServerExitReason::IdleEmpty, None);
        let second = exit_of("b", &game(2), ServerExitReason::CleanExit, Some(0));

        exits.exited(first);
        until("the kept exit", || calls(&store) == [format!("record {}", game(1))]).await;
        // The hello carries it; its welcome means the controller has it.
        let _ = exits.helloed();
        open.store(true, Ordering::SeqCst);
        exits.welcomed();
        until("the dropped exit", || calls(&store) == [format!("record {}", game(1)), format!("forget {}", game(1))]).await;

        exits.exited(second);
        until("the second exit", || calls(&store)[2..] == [format!("record {}", game(2)), format!("forget {}", game(2))]).await;
    }

    #[tokio::test]
    async fn puts_the_exits_a_previous_run_kept_in_the_first_hello() {
        let exits = ExitReporter::new(Arc::new(|_| async { false }.boxed()), Some(Arc::new(FakeAllocations::default()) as Arc<dyn SpoolPort>), Logger::null());
        let restored = ServerExit { slot_id: "a".to_owned(), allocation_id: game(1), reason: ServerExitReason::StartTimeout, exit_code: None, detail: None };

        exits.restore(std::slice::from_ref(&restored));
        assert_eq!(exits.helloed(), [restored]);
    }

    #[tokio::test]
    async fn puts_as_many_kept_exits_in_the_hello_as_there_are_slots_and_sends_the_rest_after_the_welcome() {
        let store = Arc::new(FakeAllocations::default());
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let open = Arc::new(AtomicBool::new(false));
        let exits = gated_exits(&open, &sent, Some(Arc::clone(&store) as Arc<dyn SpoolPort>), Logger::null());
        let slots = usize::try_from(MAX_SLOTS).unwrap();
        let restored: Vec<ServerExit> = (0..=slots)
            .map(|index| ServerExit {
                slot_id: "a".to_owned(),
                allocation_id: game(index),
                reason: ServerExitReason::StartTimeout,
                exit_code: None,
                detail: None,
            })
            .collect();

        exits.restore(&restored);
        assert_eq!(exits.helloed(), restored[..slots]);
        open.store(true, Ordering::SeqCst);
        exits.welcomed();

        let last = &restored[slots];
        let expected = AgentToControllerMessage::ServerExited(exit_of("a", &last.allocation_id, ServerExitReason::StartTimeout, None));

        until("the exit left out of the hello", || *sent.lock().unwrap() == [expected.clone()]).await;
    }

    #[tokio::test]
    async fn drops_nothing_from_the_spool_on_a_welcome_for_a_hello_that_carried_no_exits() {
        let store = Arc::new(FakeAllocations::default());
        let exits = ExitReporter::new(Arc::new(|_| async { true }.boxed()), Some(Arc::clone(&store) as Arc<dyn SpoolPort>), Logger::null());

        let _ = exits.helloed();
        exits.welcomed();
        settle().await;
        assert!(calls(&store).is_empty());
    }

    #[tokio::test]
    async fn still_reports_an_exit_the_spool_could_not_keep_and_says_so() {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&lines);
        let log = Logger::new("agent", Some(Arc::new(move |entry: Entry| seen.lock().unwrap().push(entry.message.trim_start_matches("[agent] ").to_owned()))));
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let exit = exit_of("a", &game(1), ServerExitReason::CleanExit, Some(0));
        let store = Arc::new(FakeAllocations::default());

        *store.exit_error.lock().unwrap() = Some("disk I/O error".to_owned());

        let exits = ExitReporter::new(recorder(&sent, AgentToControllerMessage::ServerExited), Some(store as Arc<dyn SpoolPort>), log);

        exits.exited(exit.clone());
        until("the warning", || *lines.lock().unwrap() == [format!("could not keep the exit of {}: disk I/O error", game(1))]).await;
        assert_eq!(*sent.lock().unwrap(), [AgentToControllerMessage::ServerExited(exit)]);
    }
}

mod a_plugins_reports {
    use super::*;

    #[derive(Default)]
    struct FakeSlots {
        online: Mutex<Vec<(String, bool)>>,
        statuses: Mutex<Vec<(String, ServerStatus)>>,
        quitting: Mutex<Vec<String>>,
    }

    impl PluginSlots for FakeSlots {
        fn plugin_online_changed(&self, allocation_id: &str, online: bool) {
            self.online.lock().unwrap().push((allocation_id.to_owned(), online));
        }

        fn plugin_status(&self, allocation_id: &str, status: &ServerStatus) {
            self.statuses.lock().unwrap().push((allocation_id.to_owned(), status.clone()));
        }

        fn plugin_quitting(&self, allocation_id: &str) {
            self.quitting.lock().unwrap().push(allocation_id.to_owned());
        }
    }

    const ALLOCATION_ID: &str = "11111111-1111-4111-8111-111111111111";

    fn message(value: Value) -> PluginToAgentMessage {
        serde_json::from_value(value).unwrap()
    }

    fn feed(slots: &Arc<FakeSlots>, sent: &Arc<Mutex<Vec<AgentToControllerMessage>>>) -> ankor_agent::plugin_relay::RelayHooks {
        let (slots, sent) = (Arc::clone(slots), Arc::clone(sent));

        plugin_feed(slots as Arc<dyn PluginSlots>, Arc::new(move |frame| sent.lock().unwrap().push(frame)))
    }

    #[test]
    fn reach_its_slot_the_link_a_status_and_quitting() {
        let slots = Arc::new(FakeSlots::default());
        let feed = feed(&slots, &Arc::default());
        let status = ready_status();

        (feed.on_plugin_online)(ALLOCATION_ID, true);
        (feed.on_plugin_message)(ALLOCATION_ID, PluginToAgentMessage::ServerStatus(status.clone()));
        (feed.on_plugin_message)(ALLOCATION_ID, message(json!({ "type": "quitting" })));

        assert_eq!(*slots.online.lock().unwrap(), [(ALLOCATION_ID.to_owned(), true)]);
        assert_eq!(*slots.statuses.lock().unwrap(), [(ALLOCATION_ID.to_owned(), status)]);
        assert_eq!(*slots.quitting.lock().unwrap(), [ALLOCATION_ID]);
    }

    #[test]
    fn send_their_matchs_phase_and_score_on_to_the_controller_with_the_server_that_sent_them() {
        let slots = Arc::new(FakeSlots::default());
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let feed = feed(&slots, &sent);

        (feed.on_plugin_message)(ALLOCATION_ID, message(json!({ "type": "phase_change", "phase": "live" })));
        (feed.on_plugin_message)(ALLOCATION_ID, message(json!({ "type": "score", "team1_score": 1, "team2_score": 0 })));

        let sent: Vec<Value> = sent.lock().unwrap().iter().map(|frame| serde_json::to_value(frame).unwrap()).collect();

        assert_eq!(
            sent,
            [
                json!({ "type": "phase_change", "allocation_id": ALLOCATION_ID, "phase": "live" }),
                json!({ "type": "score", "allocation_id": ALLOCATION_ID, "team1_score": 1, "team2_score": 0 }),
            ]
        );
        assert!(slots.statuses.lock().unwrap().is_empty());
    }

    #[test]
    fn send_a_backup_on_to_the_controller_with_the_complete_round_backup_event() {
        let slots = Arc::new(FakeSlots::default());
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let feed = feed(&slots, &sent);
        let backup = json!({
            "type": "round_backup",
            "timestamp": 1_758_564_000_000_i64,
            // The type holds a tick as a float, so it writes `128000.0`.
            "tick": 128_000.0,
            "team1_side": "ct",
            "rules": { "max_rounds": 24, "overtime": true, "overtime_max_rounds": 6, "friendly_fire": false },
            "file": "backup_round12.txt contents",
            "round": 13,
            "team1_score": 7,
            "team2_score": 5,
            "roster": {
                "team1": { "name": "Alpha", "players": ["76561198000000001"] },
                "team2": { "name": "Bravo", "players": ["76561198000000002"] },
                "spectators": []
            }
        });

        (feed.on_plugin_message)(ALLOCATION_ID, message(json!({ "type": "backup", "backup": backup })));

        let sent: Vec<Value> = sent.lock().unwrap().iter().map(|frame| serde_json::to_value(frame).unwrap()).collect();
        let expected = json!({ "type": "backup", "allocation_id": ALLOCATION_ID, "backup": backup });

        // The event goes on unchanged, its own `type` included, as the contract requires.
        assert_eq!(sent, std::slice::from_ref(&expected));
        assert!(ankor_protocol::validate::safe_parse(ankor_protocol::schemas::s_agent_to_controller_message, expected).is_ok());
    }

    #[test]
    fn leave_a_command_result_to_the_request_that_waits_for_it() {
        let slots = Arc::new(FakeSlots::default());
        let feed = feed(&slots, &Arc::default());

        (feed.on_plugin_message)(ALLOCATION_ID, message(json!({ "type": "command_result", "request_id": "r1", "error": null })));

        assert!(slots.statuses.lock().unwrap().is_empty());
        assert!(slots.quitting.lock().unwrap().is_empty());
    }
}
