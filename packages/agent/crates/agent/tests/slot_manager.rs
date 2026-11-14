//! The slot manager against a fake Docker and overlay.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::Arc;
use std::time::Duration;

use ankor_agent::container_spec::{ContainerRole, labels};
use ankor_agent::contract::types::ServerStatus;
use ankor_agent::docker::{AdoptedContainer, DockerPort};
use ankor_agent::logger::Logger;
use ankor_agent::overlay::OverlayPort;
use ankor_agent::slot_files::SlotFiles;
use ankor_agent::slot_manager::{SlotError, SlotEvent, SlotManager, SlotManagerDeps};
use ankor_agent::testing::slots::{change_layout, launch_server};
use ankor_agent::testing::{FakeDocker, FakeOverlay};
use ankor_protocol::config::AgentConfig;
use ankor_protocol::schemas::s_server_launch_config;
use ankor_protocol::types::{AgentErrorCode, ConfiguredSlot, ControllerFields, ServerExitReason, ServerLaunchConfig, SlotState};
use ankor_protocol::validate::safe_parse;
use serde_json::json;
use tokio::sync::mpsc;

const A: &str = "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e";
const B: &str = "7c2d3e4f-1a2b-4c5d-8e9f-0a1b2c3d4e5f";

struct Setup {
    manager: SlotManager,
    docker: Arc<FakeDocker>,
    overlay: Arc<FakeOverlay>,
    events: mpsc::UnboundedReceiver<SlotEvent>,
    _dir: tempfile::TempDir,
}

fn config(slots: usize) -> AgentConfig {
    AgentConfig {
        token: "t".repeat(64),
        controller: ControllerFields {
            host: "ankor.sh".to_owned(),
            tls: true,
            socket_path: "/ws/agent".to_owned(),
            events_path: "/api/node/events".to_owned(),
        },
        pinned_ip: None,
        cs2_image: "ghcr.io/ankorcs/cs2:1".to_owned(),
        agent_image: "ghcr.io/ankorcs/agent:1".to_owned(),
        memory_mb: None,
        auto_update: true,
        official_images_only: true,
        slots: (0..slots)
            .map(|index| ConfiguredSlot {
                id: format!("s{}", index + 1),
                port: 27015 + i64::try_from(index).unwrap() * 10,
                tv_port: 27020 + i64::try_from(index).unwrap() * 10,
                gslt: format!("TOKEN{index}"),
            })
            .collect(),
        demo_upload: None,
    }
}

fn setup(slots: usize, open_overlay: bool) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let docker = FakeDocker::new();
    let overlay = FakeOverlay::new(open_overlay);
    let config = config(slots);
    let (manager, events) = SlotManager::new(SlotManagerDeps {
        config: Arc::new(move || config.clone()),
        docker: Arc::clone(&docker) as Arc<dyn DockerPort>,
        files: Arc::new(SlotFiles::new(dir.path())),
        overlay: Arc::clone(&overlay) as Arc<dyn OverlayPort>,
        plugin_url: Arc::new(|slot, _, key| format!("ws://127.0.0.1:4500/ws/plugin?slot_id={slot}&key={key}")),
        allocations: None,
        poll_plugin: Arc::new(|_| {}),
        report: None,
        log: Logger::null(),
        stop: tokio_util::sync::CancellationToken::new(),
    });

    Setup { manager, docker, overlay, events, _dir: dir }
}

fn launch() -> ServerLaunchConfig {
    serde_json::from_value(
        safe_parse(s_server_launch_config, json!({ "max_players": 10, "map_name": "de_inferno", "idle_shutdown_seconds": 600, "exec": [], "commands": [] }))
            .unwrap(),
    )
    .unwrap()
}

fn ready(ready: bool, config_error: Option<&str>) -> ServerStatus {
    ServerStatus {
        map_name: "de_inferno".to_owned(),
        r#match: None,
        players: Vec::new(),
        config_error: config_error.map(str::to_owned),
        ready,
        request_id: None,
        server_steam_id: None,
    }
}

async fn next_exit(events: &mut mpsc::UnboundedReceiver<SlotEvent>) -> ankor_protocol::types::ServerExit {
    loop {
        if let SlotEvent::ServerExited(exit) = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.expect("an event in time").expect("events") {
            return exit;
        }
    }
}

#[tokio::test]
async fn a_server_starts_runs_and_exits() {
    let mut setup = setup(1, true);
    let reply = launch_server(&setup.manager, launch(), A).await.unwrap();

    assert_eq!(reply.slot_id, "s1");
    assert_eq!(setup.manager.statuses()[0].state, SlotState::Starting);

    let spec = setup.docker.specs()[0].clone();

    assert_eq!(spec.labels[labels::ALLOCATION], A);
    assert!(spec.env.contains(&"SRCDS_TOKEN=TOKEN0".to_owned()));
    assert!(spec.name.starts_with("ankor-s1-game-"));

    setup.manager.plugin_status(A, &ready(true, None));
    assert_eq!(setup.manager.statuses()[0].state, SlotState::Running);

    setup.docker.exit(0, Some(0));

    let exit = next_exit(&mut setup.events).await;

    assert_eq!(exit.reason, ServerExitReason::CleanExit);
    assert_eq!(setup.manager.statuses()[0].state, SlotState::Idle);
    assert_eq!(setup.manager.statuses()[0].allocation_id, None);
}

#[tokio::test]
async fn a_full_node_refuses_and_an_allocation_holds_one_slot() {
    let setup = setup(1, true);

    launch_server(&setup.manager, launch(), A).await.unwrap();

    match launch_server(&setup.manager, launch(), B).await.unwrap_err() {
        SlotError::Refused(failure) => assert_eq!(failure.code, AgentErrorCode::NoFreeSlot),
        other => panic!("{other:?}"),
    }

    match launch_server(&setup.manager, launch(), A).await.unwrap_err() {
        SlotError::Failure(failure) => assert_eq!(failure.code, AgentErrorCode::AllocationInUse),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_stop_mid_launch_starts_nothing() {
    let mut setup = setup(1, false);
    let manager = setup.manager.clone();
    let launching = tokio::spawn(async move { launch_server(&manager, launch(), A).await });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let manager = setup.manager.clone();
    let stopping = tokio::spawn(async move { manager.stop_allocation(A).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    setup.overlay.gate.add_permits(1);

    assert!(matches!(launching.await.unwrap(), Err(SlotError::Stopped(_))));
    stopping.await.unwrap().unwrap();
    assert_eq!(next_exit(&mut setup.events).await.reason, ServerExitReason::Requested);
    assert!(setup.docker.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_requested_stop_and_a_refused_config() {
    let mut setup = setup(2, true);

    launch_server(&setup.manager, launch(), A).await.unwrap();
    launch_server(&setup.manager, launch(), B).await.unwrap();
    setup.manager.stop_allocation(A).await.unwrap();
    assert_eq!(next_exit(&mut setup.events).await.reason, ServerExitReason::Requested);

    setup.manager.plugin_status(B, &ready(false, Some("bad roster")));

    let exit = next_exit(&mut setup.events).await;

    assert_eq!(exit.reason, ServerExitReason::ConfigRejected);
    assert_eq!(exit.detail.as_deref(), Some("bad roster"));
    assert!(matches!(setup.manager.stop_allocation(A).await, Err(SlotError::Failure(failure)) if failure.code == AgentErrorCode::UnknownAllocation));
}

#[tokio::test]
async fn layouts_and_reservations() {
    let setup = setup(2, true);

    launch_server(&setup.manager, launch(), A).await.unwrap();

    let mut moved = config(2).slots;

    moved[0].port = 28000;
    assert!(change_layout(&setup.manager, &moved).unwrap_err().to_string().contains("held slots would change: s1"));

    let mut retokened = config(2).slots;

    retokened[1].gslt = "NEWTOKEN".to_owned();
    assert_eq!(change_layout(&setup.manager, &retokened).unwrap(), ["s2"]);

    setup.manager.reserve("s2").unwrap();
    assert!(setup.manager.statuses()[1].reserved);
    assert!(matches!(launch_server(&setup.manager, launch(), B).await, Err(SlotError::Refused(_))));
    assert!(setup.manager.reserve("s1").is_err());
}

#[tokio::test]
async fn running_servers_are_adopted() {
    let setup = setup(1, true);

    setup.docker.adoptable.lock().unwrap().push(AdoptedContainer {
        id: "running".to_owned(),
        name: "ankor-s1-game-abc".to_owned(),
        slot_id: Some("s1".to_owned()),
        allocation_id: Some(A.to_owned()),
        match_id: None,
        idle_shutdown_seconds: Some(600),
        role: Some(ContainerRole::Game),
        running: true,
        relay_key: Some("0123456789abcdef".to_owned()),
        gslt: Some("TOKEN0".to_owned()),
    });
    setup.manager.adopt().await.unwrap();

    let status = &setup.manager.statuses()[0];

    assert_eq!(status.state, SlotState::Starting);
    assert_eq!(status.allocation_id.as_deref(), Some(A));
    assert!(setup.manager.holds(A));
}
