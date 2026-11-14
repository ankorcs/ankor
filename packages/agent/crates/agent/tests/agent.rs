//! The agent as `run_agent` wires it, on a fake Docker host against a controller peer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use ankor_agent::agent_update::{SwapFailure, SwapFailureOutcome};
use ankor_agent::contract::types::AgentToPluginMessage;
use ankor_agent::controller_connection::{Signal, close_code};
use ankor_agent::testing::agent_boot::{BootOptions, agent_image_id, boot, try_boot};
use ankor_agent::testing::{agent_message, until, welcome};
use ankor_protocol::constants::{close_codes, official_images};
use ankor_protocol::types::{
    AgentErrorCode, AgentToControllerMessage, AgentUpdate, AgentUpdateOutcome, Cs2ImageUpdateOutcome, Cs2UpdateOutcome, ImageUpdateStep, MaintenanceOperation,
    NodeUnavailableCode, ServerExit, ServerExitReason, SlotState,
};
use serde_json::{Value, json};

#[tokio::test]
async fn refuses_to_boot_beside_another_running_agent_container_before_it_touches_anything() {
    let data = std::sync::Arc::new(tempfile::tempdir().unwrap());
    let error = try_boot(BootOptions {
        data_dir: Some(std::sync::Arc::clone(&data)),
        prepare: Some(Box::new(|docker, _| *docker.other_agents.lock().unwrap() = vec!["ankor-agent-2".to_owned()])),
        ..BootOptions::default()
    })
    .await
    .err()
    .unwrap();

    assert!(error.0.to_string().contains("2 agent containers are running (ankor-agent, ankor-agent-2); a node runs one."), "{}", error.0);

    // No spool opened, no overlay extracted, no container started.
    let mut entries: Vec<String> = std::fs::read_dir(data.path()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();

    entries.sort();
    assert_eq!(entries, ["agent.yml"]);
    assert!(error.1.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn refuses_to_boot_outside_the_labelled_agent_container_which_says_what_it_runs() {
    let error = try_boot(BootOptions { prepare: Some(Box::new(|docker, _| *docker.agent_container.lock().unwrap() = None)), ..BootOptions::default() })
        .await
        .err()
        .unwrap();

    assert!(error.0.to_string().contains("the agent runs as the container node-setup.sh creates"), "{}", error.0);
}

#[tokio::test]
async fn says_hello_with_what_the_host_runs_and_tells_the_controller_the_newest_public_build_in_it_or_right_after() {
    let booted = boot(BootOptions::default()).await;

    until("the hello", || booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len() == 1).await;

    let hello = &booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0];

    assert_eq!(hello["agent"]["image"], "ankor-agent:test");
    assert_eq!(hello["agent"]["image_id"], agent_image_id());
    assert_eq!((hello["public_ip"].clone(), hello["country"].clone()), (json!("203.0.113.7"), json!("DK")));
    assert_eq!((hello["slot_status"][0]["slot_id"].clone(), hello["slot_status"][0]["state"].clone()), (json!("a"), json!(SlotState::Idle)));
    assert_eq!((hello["cs2"]["build_id"].clone(), hello["cs2"]["image"].clone()), (json!("19000"), json!("ankor-cs2:test")));
    assert_eq!(hello["unavailable"], Value::Null);
    assert_eq!(hello["host"]["cpu_count"], json!(std::thread::available_parallelism().unwrap().get()));
    assert_eq!(hello["host"]["kernel"], json!(nix::sys::utsname::uname().unwrap().release().to_string_lossy()));

    // Steam may answer before the hello is built; otherwise the runtime report follows it.
    until("the newest public build", || {
        booted
            .frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))
            .iter()
            .chain(booted.frames(|message| matches!(message, AgentToControllerMessage::NodeStatus(_))).iter())
            .any(|frame| frame["cs2"]["latest_build_id"] == "19000")
    })
    .await;
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
    booted.peer.expect_valid_frames();
}

/// Shutdown tells the controller at once, answers a request that comes after with `shutting_down`, and closes
/// only once the launch it was running has replied.
#[tokio::test]
async fn shutdown_waits_for_a_launch_under_way_and_refuses_a_later_request() {
    let booted = boot(BootOptions::default()).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));

    *booted.docker.start_gate.lock().unwrap() = Some(Arc::clone(&gate));
    hello_seen(&booted).await;
    booted.peer.send_json(&json!({ "type": "start_server", "id": "launch", "allocation_id": allocation(), "launch": launch_config(&json!({})) }));
    until("the launch under way", || booted.peer.status("a").is_some_and(|status| status["state"] == json!(SlotState::Starting))).await;

    let steps = async {
        until("the shutdown frame", || booted.frames(|message| matches!(message, AgentToControllerMessage::Shutdown(_))).len() == 1).await;
        booted.peer.send_json(&json!({ "type": "reserve_slot", "id": "late", "slot_id": "b" }));
        until("the late request's refusal", || {
            booted.frames(|message| matches!(message, AgentToControllerMessage::Reply(_))).iter().any(|reply| reply["reply_to"] == "late")
        })
        .await;
        assert!(booted.peer.disconnects().is_empty(), "the socket stays open while the launch runs");
        gate.add_permits(1);
    };
    let _ = tokio::join!(booted.agent.shutdown(Signal::Sigterm), steps);

    until("the close", || booted.peer.disconnects().len() == 1).await;

    let replies = booted.frames(|message| matches!(message, AgentToControllerMessage::Reply(_)));
    let late = replies.iter().find(|reply| reply["reply_to"] == "late").unwrap();
    let launch = replies.iter().find(|reply| reply["reply_to"] == "launch").unwrap();

    assert_eq!(late["error"]["code"], json!(AgentErrorCode::ShuttingDown));
    assert_eq!(launch["ok"], true, "{launch}");
    assert_eq!(role_count(&booted, ContainerRole::Game), 1);
    booted.peer.expect_valid_frames();
}

#[tokio::test]
async fn closes_the_socket_with_the_signal_on_shutdown_once_however_often_it_is_asked() {
    let booted = boot(BootOptions::default()).await;

    until("the hello", || booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len() == 1).await;
    let _ = tokio::join!(booted.agent.shutdown(Signal::Sigterm), booted.agent.shutdown(Signal::Sigint));
    until("the close", || booted.peer.disconnects().len() == 1).await;
    assert_eq!(booted.peer.disconnects(), [Signal::Sigterm.name()]);
}

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::container_spec::{ContainerRole, labels};
use ankor_agent::testing::agent_boot::{Booted, GamePlugin};
use ankor_agent::testing::fixtures::launch_config;

static NEXT: AtomicU64 = AtomicU64::new(1);

fn allocation() -> String {
    format!("0199a0b4-5c00-7000-8000-{:012x}", NEXT.fetch_add(1, Ordering::SeqCst))
}

async fn hello_seen(booted: &Booted) {
    until("the hello", || booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len() == 1).await;
}

fn start_frame(allocation_id: &str) -> Value {
    json!({ "type": "start_server", "allocation_id": allocation_id, "launch": launch_config(&json!({})) })
}

fn role_count(booted: &Booted, role: ContainerRole) -> usize {
    booted.docker.specs().iter().filter(|spec| spec.labels.get(labels::ROLE).map(String::as_str) == Some(role.as_str())).count()
}

fn index_of(booted: &Booted, allocation_id: &str) -> usize {
    booted.docker.specs().iter().position(|spec| spec.labels.get(labels::ALLOCATION).map(String::as_str) == Some(allocation_id)).expect("a game container")
}

fn node_statuses(booted: &Booted) -> Vec<Value> {
    booted.frames(|message| matches!(message, AgentToControllerMessage::NodeStatus(_)))
}

fn check_build(booted: &Booted, latest: &str) {
    latest.clone_into(&mut booted.steam.0.lock().unwrap());
    booted.peer.send_json(&json!({ "type": "check_cs2_build" }));
}

#[tokio::test]
async fn logs_why_the_controller_closed_the_socket() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;
    booted.peer.close(4000, "going away");
    until("the disconnect logged", || booted.lines.lock().unwrap().iter().any(|line| line == "[agent:ws] disconnected (4000 going away)")).await;
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn runs_a_console_command_from_the_controller_on_its_server_through_the_servers_plugin() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();

    hello_seen(&booted).await;
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["data"]["slot_id"], "a");

    let mut game = GamePlugin::connect(&booted.docker, &allocation_id).await;

    until("the plugin link", || booted.peer.status("a").is_some_and(|status| status["plugin_online"] == true)).await;

    let peer = &booted.peer;
    let reply = peer.request(json!({ "type": "console_command", "allocation_id": allocation_id, "command": "mp_restartgame 1" }));
    let answer = async {
        until("the command at the plugin", || {
            game.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_))).iter().any(|message| message["command"] == "mp_restartgame 1")
        })
        .await;

        let request_id = game.received_where(|message| matches!(message, AgentToPluginMessage::Exec(_)))[0]["request_id"].clone();

        game.send(&json!({ "type": "command_result", "request_id": request_id, "error": null })).await;
    };
    let (reply, ()) = tokio::join!(reply, answer);

    assert_eq!((reply["ok"].clone(), reply["data"].clone()), (json!(true), json!({ "allocation_id": allocation_id })));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
    booted.peer.expect_valid_frames();
}

#[tokio::test]
async fn holds_a_cs2_update_while_a_game_runs_and_updates_once_its_slot_is_idle() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();

    hello_seen(&booted).await;
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);
    check_build(&booted, "19001");
    until("the newer public build", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["latest_build_id"] == "19001")).await;
    assert_eq!(role_count(&booted, ContainerRole::Update), 0);

    booted.docker.exit(index_of(&booted, &allocation_id), Some(0));
    until("the game's exit", || booted.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_))).len() == 1).await;
    until("the build after the update", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["build_id"] == "19001")).await;
    until("the update completed", || {
        node_statuses(&booted).last().is_some_and(|frame| frame["cs2"]["update"]["outcome"] == json!(Cs2UpdateOutcome::Completed))
    })
    .await;
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
    booted.peer.expect_valid_frames();
}

#[tokio::test]
async fn keeps_its_build_and_takes_launches_behind_the_public_one_with_auto_update_off() {
    let booted = boot(BootOptions { auto_update: Some(false), ..BootOptions::default() }).await;

    hello_seen(&booted).await;
    check_build(&booted, "19001");
    until("the newer public build", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["latest_build_id"] == "19001")).await;
    assert_eq!(booted.peer.request(start_frame(&allocation())).await["ok"], true);

    let last = node_statuses(&booted).last().cloned().unwrap();

    assert_eq!((last["cs2"]["build_id"].clone(), last["cs2"]["latest_build_id"].clone()), (json!("19000"), json!("19001")));
    assert_eq!(role_count(&booted, ContainerRole::Update), 0);
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn streams_a_watched_log_only_once_the_controller_welcomes_the_hello_and_replays_it_then() {
    let booted = boot(BootOptions { welcome: Some(false), ..BootOptions::default() }).await;

    until("the hello", || booted.peer.hellos() == 1).await;
    booted.peer.send_json(&json!({ "type": "watch_logs", "source": { "kind": "agent" }, "replay_id": "11111111-1111-4111-8111-111111111111" }));
    // Past the log flush: nothing of the agent's log goes out before the welcome.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(booted.frames(|message| matches!(message, AgentToControllerMessage::LogBatch(_))).is_empty());

    booted.peer.send_json(&welcome());
    until("the replayed log", || {
        booted.frames(|message| matches!(message, AgentToControllerMessage::LogBatch(_))).iter().any(|frame| {
            frame["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line["line"].as_str().is_some_and(|text| text.contains("running as ankor-agent:test"))))
        })
    })
    .await;
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn reports_a_cs2_update_that_fails_keeps_its_build_and_takes_launches_guard_back() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;
    // As the image's update mode ends when steamcmd gives up: the version file stays as it was.
    booted
        .docker
        .helper_runs
        .lock()
        .unwrap()
        .insert(ContainerRole::Update, Arc::new(|_| (vec!["steamcmd failed after 3 attempts (last exit 8)".to_owned()], Some(1))));
    check_build(&booted, "19001");
    until("the update's outcome", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["update"]["outcome"] == json!(Cs2UpdateOutcome::Failed))).await;
    assert!(node_statuses(&booted).iter().any(|frame| frame["unavailable"]["code"] == json!(NodeUnavailableCode::Maintenance)));
    until("the guard released", || node_statuses(&booted).last().is_some_and(|frame| frame["unavailable"].is_null())).await;

    let last = node_statuses(&booted).last().cloned().unwrap();

    assert_eq!((last["cs2"]["build_id"].clone(), last["cs2"]["latest_build_id"].clone()), (json!("19000"), json!("19001")));
    assert_eq!(role_count(&booted, ContainerRole::Update), 1);
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
    booted.peer.expect_valid_frames();
}

#[tokio::test]
async fn reports_a_cs2_update_that_completes_with_the_build_it_installed() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;
    check_build(&booted, "19001");
    until("the update's outcome", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["update"]["outcome"] == json!(Cs2UpdateOutcome::Completed))).await;

    // No report pairs the outcome with the build the update replaced.
    let completed: Vec<Value> =
        node_statuses(&booted).into_iter().filter(|frame| frame["cs2"]["update"]["outcome"] == json!(Cs2UpdateOutcome::Completed)).collect();

    assert!(completed.iter().any(|frame| frame["cs2"]["build_id"] == "19001"));
    assert!(completed.iter().all(|frame| frame["cs2"]["build_id"] == "19001"));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn streams_the_hosts_resources_to_the_controller_once_it_watches_them() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;
    booted.peer.send_json(&json!({ "type": "watch_resources", "replay": false }));
    // The first tick is taken a few seconds after boot.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    while !booted
        .frames(|message| matches!(message, AgentToControllerMessage::ResourceBatch(_)))
        .iter()
        .any(|frame| frame["ticks"].as_array().is_some_and(|ticks| !ticks.is_empty()))
    {
        assert!(std::time::Instant::now() < deadline, "no resource tick");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

/// A demo receiver that records each request's path.
async fn demo_receiver() -> (u16, Arc<Mutex<Vec<String>>>) {
    let received: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = Arc::clone(&received);
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, _body: axum::body::Bytes| {
        let recorded = Arc::clone(&recorded);

        async move {
            recorded.lock().unwrap().push(uri.path().to_owned());
            ""
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, received)
}

#[tokio::test]
async fn leaves_the_demo_a_running_game_records_alone_and_uploads_it_once_the_game_exits() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();
    let (port, received) = demo_receiver().await;

    hello_seen(&booted).await;
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);

    // GOTV is still writing it: no ready marker beside it.
    let demos = booted.data_dir.path().join("slots/a/game/csgo/ankor/demos").join(&allocation_id);
    let demo = demos.join("20260922-183000_match-1_de_mirage.dem");

    std::fs::create_dir_all(&demos).unwrap();
    std::fs::write(&demo, "gotv-bytes").unwrap();
    // Turning uploads on scans the demo folders immediately.
    let upload =
        json!({ "type": "set_config", "config": { "demo_upload": { "url": format!("http://127.0.0.1:{port}/demos"), "header": null, "limit_mbit": null } } });

    assert_eq!(booted.peer.request(upload).await["ok"], true);
    // The scan runs after the reply; an upload to a local receiver would be over well within this.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(received.lock().unwrap().is_empty());
    assert!(demo.exists());

    booted.docker.exit(index_of(&booted, &allocation_id), Some(0));
    until("the demo uploaded and removed", || !demo.exists()).await;
    assert_eq!(*received.lock().unwrap(), ["/demos"]);
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn moves_to_the_controller_address_set_config_names_says_hello_there_and_keeps_it_in_agent_yml() {
    let booted = boot(BootOptions::default()).await;
    let next = ankor_agent::testing::controller_peer::ControllerPeer::start(ankor_agent::testing::controller_peer::PeerOptions {
        token: Some(ankor_agent::testing::agent_boot::TOKEN.to_owned()),
        ..Default::default()
    })
    .await;

    hello_seen(&booted).await;

    let host = format!("127.0.0.1:{}", next.port);
    let edit = json!({ "type": "set_config", "config": { "controller": { "host": host, "tls": false, "socket_path": "/ws/agent", "events_path": "/api/node/events" } } });

    assert_eq!(booted.peer.request(edit).await["ok"], true);
    until("the hello at the new address", || next.hellos() == 1).await;
    assert!(std::fs::read_to_string(&booted.config_path).unwrap().contains(&host));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn refuses_an_image_outside_its_official_repository_while_official_images_only_is_on() {
    let images = (Some(format!("{}:0.6", official_images::CS2)), Some(format!("{}:0.8", official_images::AGENT)));
    let booted = boot(BootOptions { official_images_only: Some(true), images, ..BootOptions::default() }).await;

    hello_seen(&booted).await;

    // Another registry or owner, a look-alike name, and each official image sent as the other kind.
    for (kind, image) in [
        ("update_agent", "ghcr.io/example/ankor-agent:0.8".to_owned()),
        ("update_agent", format!("{}-x:0.8", official_images::AGENT)),
        ("update_agent", format!("{}:0.6", official_images::CS2)),
        ("update_cs2_image", "ankor-cs2:next".to_owned()),
        ("update_cs2_image", format!("{}:0.8", official_images::AGENT)),
    ] {
        let refused = booted.peer.request(json!({ "type": kind, "image": image, "pull": false })).await;

        assert_eq!((refused["ok"].clone(), refused["error"]["code"].clone()), (json!(false), json!(AgentErrorCode::ImageNotAllowed)), "{kind} {image}");
    }

    // An official image goes ahead, by tag or by digest.
    let digest = format!("{}@sha256:{}", official_images::CS2, "b".repeat(64));

    assert_eq!(booted.peer.request(json!({ "type": "update_cs2_image", "image": digest, "pull": false })).await["ok"], true);
    until("the new image", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["image"] == json!(digest))).await;
    assert!(!std::fs::read_to_string(&booted.config_path).unwrap().contains("ankor-cs2:next"));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn sets_the_memory_cap_of_later_servers_and_removes_it_with_null() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;

    let set = booted.peer.request(json!({ "type": "set_config", "config": { "memory_mb": 6144 } })).await;

    let file = || yaml_serde::from_str::<Value>(&std::fs::read_to_string(&booted.config_path).unwrap()).unwrap();

    assert_eq!((set["ok"].clone(), set["data"]["memory_mb"].clone()), (json!(true), json!(6144)));
    // A number, as agent.yml takes it, not the string "6144".
    assert_eq!(file()["memory_mb"], json!(6144));

    let removed = booted.peer.request(json!({ "type": "set_config", "config": { "memory_mb": null } })).await;

    assert_eq!((removed["ok"].clone(), removed["data"]["memory_mb"].clone()), (json!(true), Value::Null));
    assert!(file().get("memory_mb").is_none());
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn reports_a_new_cs2_image_in_a_node_status_without_a_new_hello() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;
    assert_eq!(booted.peer.request(json!({ "type": "update_cs2_image", "image": "ankor-cs2:next", "pull": false })).await["ok"], true);
    until("the new image", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["image"] == "ankor-cs2:next")).await;
    // Past where a hello after the reply would have gone out.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len(), 1);
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn ends_on_the_image_updates_release_when_the_guards_first_report_reads_slowly() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;
    // Only the report of the guard being taken still reads the old image; it takes its time.
    booted.docker.image_id_delays.lock().unwrap().insert("ankor-cs2:test".to_owned(), Duration::from_millis(300));
    assert_eq!(booted.peer.request(json!({ "type": "update_cs2_image", "image": "ankor-cs2:next", "pull": false })).await["ok"], true);
    until("the new image", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["image"] == "ankor-cs2:next")).await;
    // Past the slow read: nothing older goes out after the new image.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let last = node_statuses(&booted).last().cloned().unwrap();

    assert_eq!((last["cs2"]["image"].clone(), last["unavailable"].clone()), (json!("ankor-cs2:next"), Value::Null));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn tells_the_controller_in_the_hello_of_a_game_it_stopped_at_boot_because_its_slot_left_agent_yml() {
    let allocation_id = allocation();
    let orphan = allocation_id.clone();
    let booted = boot(BootOptions {
        prepare: Some(Box::new(move |docker, _| {
            *docker.adoptable.lock().unwrap() = vec![ankor_agent::docker::AdoptedContainer {
                id: "orphan".to_owned(),
                name: "ankor-gone-match".to_owned(),
                slot_id: Some("gone".to_owned()),
                allocation_id: Some(orphan),
                match_id: None,
                idle_shutdown_seconds: Some(180),
                role: Some(ankor_agent::container_spec::ContainerRole::Game),
                running: true,
                relay_key: None,
                gslt: None,
            }];
        })),
        ..BootOptions::default()
    })
    .await;

    hello_seen(&booted).await;
    assert_eq!(booted.docker.stopped.lock().unwrap().iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["orphan"]);
    assert_eq!(
        booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0]["server_exits"],
        json!([ServerExit {
            slot_id: "gone".to_owned(),
            allocation_id: allocation_id.clone(),
            reason: ServerExitReason::SlotRemoved,
            exit_code: Some(143),
            detail: None
        }])
    );
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

use ankor_agent::docker::{ImageDefaults, ImageSummary, SelfUpdateHelper};
use ankor_agent::image_auto_update::VERSION_LABEL;
use ankor_agent::registry_tags::{Method, RegistryAnswer, RegistryFetch};
use ankor_agent::testing::fixtures::MATCH_ID;
use ankor_agent::testing::wire;
use ankor_protocol::types::CloseReason;
use std::collections::BTreeMap;

/// A log event from a plugin, as its server's event stream numbers it.
fn event_frame(seq: i64) -> Value {
    json!({ "type": "event", "seq": seq, "event": { "type": "log", "match_id": MATCH_ID, "tick": 640, "timestamp": jiff::Timestamp::now().as_millisecond(), "level": "info", "message": format!("event-{seq}") } })
}

fn no_backlog() -> Value {
    json!({ "servers": [], "server_count": 0, "event_count": 0, "reason": null })
}

/// A stop still running when shutdown begins finishes within the request wait, and the exit it
/// causes is kept in the spool until the controller has it: the spool closes only after the requests.
#[tokio::test]
async fn keeps_the_exit_of_a_stop_that_runs_into_shutdown() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();

    hello_seen(&booted).await;
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);

    let (docker, index) = (Arc::clone(&booted.docker), index_of(&booted, &allocation_id));
    let stopping = Arc::new(tokio::sync::Notify::new());
    let begun = Arc::clone(&stopping);

    booted.docker.override_stop(move |_, _| async move {
        begun.notify_one();
        tokio::time::sleep(Duration::from_millis(500)).await;
        docker.exit(index, Some(0));
        Ok(())
    });
    booted.peer.send_json(&json!({ "type": "stop_server", "id": "stop", "allocation_id": allocation_id }));
    stopping.notified().await;
    booted.agent.shutdown(Signal::Sigterm).await.unwrap();

    assert_eq!(booted.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_))).len(), 1);
    assert!(!booted.lines.lock().unwrap().iter().any(|line| line.contains("event spool is closing")), "{:?}", booted.lines.lock().unwrap());
}

#[tokio::test]
async fn tells_the_controller_of_a_game_that_ended_before_it_welcomed_the_hello_after_an_agent_restart_too() {
    let first = boot(BootOptions { welcome: Some(false), ..BootOptions::default() }).await;
    let allocation_id = allocation();

    until("the hello", || first.peer.hellos() == 1).await;
    assert_eq!(first.peer.request(start_frame(&allocation_id)).await["ok"], true);
    first.docker.exit(index_of(&first, &allocation_id), Some(0));
    // Not welcomed, so the exit waits; the agent stops before the controller hears of it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(first.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_))).is_empty());
    let _ = first.agent.shutdown(Signal::Sigterm).await;

    let second = boot(BootOptions { data_dir: Some(Arc::clone(&first.data_dir)), ..BootOptions::default() }).await;

    until("the hello after the restart", || second.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len() == 1).await;
    assert_eq!(
        second.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0]["server_exits"],
        json!([ServerExit {
            slot_id: "a".to_owned(),
            allocation_id: allocation_id.clone(),
            reason: ServerExitReason::CleanExit,
            exit_code: Some(0),
            detail: None
        }])
    );
    let _ = second.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn says_in_its_first_hello_how_the_last_agent_swap_went_and_when_one_still_proving_ends() {
    let image = "ankor-agent:next";
    let failed = boot(BootOptions {
        prepare: Some(Box::new(move |docker, data_dir| {
            *docker.self_update_helpers.lock().unwrap() =
                vec![SelfUpdateHelper { name: "ankor-self-update-0c0ffe".to_owned(), image: Some(image.to_owned()), running: false }];
            std::fs::write(
                data_dir.join("agent-update-failed.json"),
                json!(SwapFailure {
                    image: image.to_owned(),
                    outcome: SwapFailureOutcome::Failed,
                    detail: "the replacement container exited on its own".to_owned()
                })
                .to_string(),
            )
            .unwrap();
        })),
        ..BootOptions::default()
    })
    .await;

    hello_seen(&failed).await;
    assert_eq!(
        failed.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0]["agent_update"],
        json!(AgentUpdate {
            image: image.to_owned(),
            step: None,
            outcome: Some(AgentUpdateOutcome::Failed),
            detail: Some("the replacement container exited on its own".to_owned())
        })
    );
    let _ = failed.agent.shutdown(Signal::Sigterm).await;

    let booted = boot(BootOptions {
        prepare: Some(Box::new(move |docker, _| {
            *docker.self_update_helpers.lock().unwrap() =
                vec![SelfUpdateHelper { name: "ankor-self-update-beef00".to_owned(), image: Some(image.to_owned()), running: true }];
            // The image the swap replaced, which only its proving helper still pins.
            docker.image_list.lock().unwrap().push((
                ImageSummary { id: "sha256:agent-old".to_owned(), repo_tags: vec!["ankor-agent:old".to_owned()] },
                std::collections::BTreeMap::from([("ankor.image".to_owned(), "agent".to_owned())]),
            ));
        })),
        ..BootOptions::default()
    })
    .await;

    hello_seen(&booted).await;

    let hello = booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0].clone();

    assert_eq!(hello["agent_update"], json!(AgentUpdate { image: image.to_owned(), step: Some(ImageUpdateStep::Switching), outcome: None, detail: None }));
    // The swap holds the maintenance guard until its helper is done, so it refuses another one.
    assert_eq!(
        (hello["unavailable"]["code"].clone(), hello["unavailable"]["operation"].clone()),
        (json!(NodeUnavailableCode::Maintenance), json!(MaintenanceOperation::AgentUpdate))
    );

    let refused = booted.peer.request(json!({ "type": "update_agent", "image": image, "pull": false })).await;

    assert_eq!((refused["ok"].clone(), refused["error"]["code"].clone()), (json!(false), json!(AgentErrorCode::MaintenanceRunning)));

    // The swap holds: its helper leaves no failure behind.
    *booted.docker.self_update_helpers.lock().unwrap() =
        vec![SelfUpdateHelper { name: "ankor-self-update-beef00".to_owned(), image: Some(image.to_owned()), running: false }];

    let exits: Vec<_> =
        booted.docker.followed.lock().unwrap().iter().filter(|entry| entry.id == "ankor-self-update-beef00").map(|entry| entry.hooks.on_exit.clone()).collect();

    for on_exit in exits {
        on_exit(Some(0));
    }

    until("the swap reported done and the guard free", || {
        node_statuses(&booted)
            .last()
            .is_some_and(|frame| frame["agent_update"]["outcome"] == json!(AgentUpdateOutcome::Completed) && frame["unavailable"].is_null())
    })
    .await;
    // The boot's sweep waited for the swap, and removes what it left behind.
    until("the replaced agent image removed", || booted.docker.removed_images.lock().unwrap().contains(&"ankor-agent:old".to_owned())).await;
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn tells_the_controller_a_game_crashed_with_its_exit_code() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();

    hello_seen(&booted).await;
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);
    booted.docker.exit(index_of(&booted, &allocation_id), Some(139));
    until("the exit", || booted.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_))).len() == 1).await;

    let exit = booted.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_)))[0].clone();

    assert_eq!(
        (exit["allocation_id"].clone(), exit["reason"].clone(), exit["exit_code"].clone(), exit["detail"].clone()),
        (json!(allocation_id), json!(ServerExitReason::Crashed), json!(139), Value::Null)
    );
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn stops_uploading_events_once_the_controller_deletes_the_node_and_keeps_them_spooled() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();

    booted.peer.accept_uploads();
    hello_seen(&booted).await;
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);

    let mut game = GamePlugin::connect(&booted.docker, &allocation_id).await;
    let uploaded = |seq: i64| {
        booted.peer.uploads().iter().any(|(_, batch)| batch["frames"].as_array().is_some_and(|frames| frames.iter().any(|frame| frame["seq"] == seq)))
    };

    game.send(&event_frame(0)).await;
    until("the first event at the controller", || uploaded(0)).await;
    booted.peer.close(u16::from(close_code(close_codes::POLICY)), &wire(CloseReason::AgentDeleted));
    until("the close", || booted.peer.disconnects().len() == 1).await;
    game.send(&event_frame(1)).await;
    until("the second event spooled", || {
        game.received_where(|message| matches!(message, AgentToPluginMessage::Ack(_))).iter().any(|ack| ack["to_seq"].as_i64().is_some_and(|seq| seq >= 1))
    })
    .await;
    // Past two of the uploader's one-second ticks.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(!uploaded(1));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn sends_no_hello_while_docker_cannot_say_whether_the_cs2_image_is_there() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;

    let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let fails = Arc::clone(&failing);

    *booted.docker.on_image_id.lock().unwrap() = Some(Arc::new(move |_| fails.load(Ordering::SeqCst).then(|| "Docker connection reset".to_owned())));
    booted.peer.terminate();
    until("a socket dropped for its failed hello", || booted.peer.accepted() >= 3).await;
    assert_eq!(booted.peer.hellos(), 1);

    failing.store(false, Ordering::SeqCst);
    until("the hello once Docker answers", || booted.peer.hellos() == 2).await;
    assert!(booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[1]["cs2"]["image_id"].is_string());
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

/// Fails every image lookup while the returned flag is set.
fn failing_image_lookups(booted: &Booted) -> Arc<std::sync::atomic::AtomicBool> {
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let fails = Arc::clone(&failing);

    *booted.docker.on_image_id.lock().unwrap() = Some(Arc::new(move |_| fails.load(Ordering::SeqCst).then(|| "Docker connection reset".to_owned())));

    failing
}

fn failed_reports(booted: &Booted) -> usize {
    booted.lines.lock().unwrap().iter().filter(|line| line.contains("could not report")).count()
}

#[tokio::test(start_paused = true)]
async fn reads_a_node_report_again_until_docker_answers() {
    let booted = boot(BootOptions { auto_update: Some(false), ..BootOptions::default() }).await;

    hello_seen(&booted).await;

    let failing = failing_image_lookups(&booted);
    let reported = node_statuses(&booted).len();

    check_build(&booted, "19001");
    until("two failed reads", || failed_reports(&booted) >= 2).await;
    assert_eq!(node_statuses(&booted).len(), reported);

    failing.store(false, Ordering::SeqCst);
    until("the report once Docker answers", || node_statuses(&booted).len() > reported).await;
    assert_eq!(node_statuses(&booted).last().unwrap()["cs2"]["latest_build_id"], "19001");

    // The read that went out ends the retries: past two more intervals, nothing else is read or sent.
    let failed = failed_reports(&booted);

    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!((node_statuses(&booted).len(), failed_reports(&booted)), (reported + 1, failed));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

/// A newer change reads the node again, so the older report stops retrying.
#[tokio::test(start_paused = true)]
async fn retries_only_the_newest_node_report_once_a_newer_change_follows_a_failed_one() {
    let booted = boot(BootOptions { auto_update: Some(false), ..BootOptions::default() }).await;

    hello_seen(&booted).await;

    let _failing = failing_image_lookups(&booted);

    check_build(&booted, "19001");
    until("the first failed read", || failed_reports(&booted) >= 1).await;
    check_build(&booted, "19002");
    until("the newer failed read", || failed_reports(&booted) >= 2).await;

    let failed = failed_reports(&booted);

    // Past two more intervals, only the newer report has read again, once in each.
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!(failed_reports(&booted), failed + 2);
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn lists_the_stuck_event_backlog_in_every_hello_and_none_once_the_uploads_work_again() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();

    hello_seen(&booted).await;
    assert_eq!(booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0]["event_backlog"], no_backlog());
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);

    let mut game = GamePlugin::connect(&booted.docker, &allocation_id).await;

    // The controller refuses the upload: the server's event is stuck on the node.
    game.send(&event_frame(0)).await;
    until("the backlog report", || !booted.frames(|message| matches!(message, AgentToControllerMessage::EventBacklog(_))).is_empty()).await;
    booted.peer.terminate();
    until("the hello after the reconnect", || booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len() == 2).await;
    assert_eq!(
        booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[1]["event_backlog"],
        json!({ "servers": [{ "allocation_id": allocation_id, "pending": 1 }], "server_count": 1, "event_count": 1, "reason": "event backend returned HTTP 503" })
    );

    // Once the event is uploaded, the next hello lists nothing, whatever became of the frame that cleared it.
    booted.peer.accept_uploads();
    until("the backlog cleared", || {
        booted.frames(|message| matches!(message, AgentToControllerMessage::EventBacklog(_))).iter().any(|frame| frame["server_count"] == 0)
    })
    .await;
    booted.peer.terminate();
    until("the third hello", || booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len() == 3).await;
    assert_eq!(booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[2]["event_backlog"], no_backlog());
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

/// Maintenance leaves the spool readable, so a hello during an update still counts the backlog.
#[tokio::test]
async fn lists_the_stuck_event_backlog_in_a_hello_during_maintenance() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();

    hello_seen(&booted).await;
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);

    let mut game = GamePlugin::connect(&booted.docker, &allocation_id).await;

    game.send(&event_frame(0)).await;
    until("the backlog report", || !booted.frames(|message| matches!(message, AgentToControllerMessage::EventBacklog(_))).is_empty()).await;
    booted.docker.exit(index_of(&booted, &allocation_id), Some(0));
    until("the exit", || !booted.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_))).is_empty()).await;
    // The update container runs until the test ends, so the maintenance holds.
    booted.docker.helper_runs.lock().unwrap().remove(&ContainerRole::Update);
    booted.peer.send_json(&json!({ "type": "update_cs2", "id": "u1", "mode": "update" }));
    until("the maintenance", || node_statuses(&booted).iter().any(|frame| frame["unavailable"]["code"] == json!(NodeUnavailableCode::Maintenance))).await;
    booted.peer.terminate();
    until("the hello after the reconnect", || booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len() == 2).await;

    let hello = booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[1].clone();

    assert_eq!(hello["unavailable"]["code"], json!(NodeUnavailableCode::Maintenance));
    assert_eq!((hello["event_backlog"]["server_count"].clone(), hello["event_backlog"]["event_count"].clone()), (json!(1), json!(1)));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn sends_the_event_backlog_a_failed_upload_found_before_the_controller_welcomed_the_hello_once_it_does() {
    let booted = boot(BootOptions { welcome: Some(false), ..BootOptions::default() }).await;
    let allocation_id = allocation();

    until("the hello", || booted.peer.hellos() == 1).await;
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);

    let mut game = GamePlugin::connect(&booted.docker, &allocation_id).await;

    // The upload fails while the hello waits: its backlog report cannot go out yet.
    game.send(&event_frame(0)).await;
    until("the failed upload", || !booted.peer.uploads().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(booted.frames(|message| matches!(message, AgentToControllerMessage::EventBacklog(_))).is_empty());

    booted.peer.send_json(&welcome());
    until("the backlog after the welcome", || !booted.frames(|message| matches!(message, AgentToControllerMessage::EventBacklog(_))).is_empty()).await;

    let backlog = booted.frames(|message| matches!(message, AgentToControllerMessage::EventBacklog(_)))[0].clone();

    assert_eq!(
        (backlog["server_count"].clone(), backlog["event_count"].clone(), backlog["reason"].clone()),
        (json!(1), json!(1), json!("event backend returned HTTP 503"))
    );
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn reads_the_installed_build_again_after_a_read_that_found_none_and_keeps_one_it_found() {
    let booted = boot(BootOptions { welcome: Some(false), uninstalled: true, ..BootOptions::default() }).await;
    let record = on_volume(booted.volume.path(), &installed_json(), ankor_agent::host_layout::INSTALL_DIR);

    until("the hello", || booted.peer.hellos() == 1).await;
    assert_eq!(booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0]["cs2"]["build_id"], Value::Null);

    write_installed(&record, "19000");
    booted.peer.send_json(&welcome());
    until("the build read again", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["build_id"] == "19000")).await;

    // The read that found the build is kept, so the next hello does not read the files again.
    std::fs::remove_file(&record).unwrap();
    booted.peer.terminate();
    until("the hello on the next socket", || booted.peer.hellos() == 2).await;
    assert_eq!(booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[1]["cs2"]["build_id"], "19000");
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn tells_the_controller_of_a_game_that_ended_while_its_new_hello_waited_for_the_welcome_once_welcomed() {
    let booted = boot(BootOptions { welcome: Some(false), ..BootOptions::default() }).await;
    let allocation_id = allocation();

    until("the hello", || booted.peer.hellos() == 1).await;
    booted.peer.send_json(&welcome());
    assert_eq!(booted.peer.request(start_frame(&allocation_id)).await["ok"], true);
    booted.peer.terminate();
    until("the hello on the next socket", || booted.peer.hellos() == 2).await;
    assert_eq!(booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[1]["slot_status"][0]["allocation_id"], json!(allocation_id));

    let since = booted.peer.received().len();
    let after = || booted.peer.received()[since..].to_vec();

    booted.docker.exit(index_of(&booted, &allocation_id), Some(0));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        after().iter().all(|frame| !matches!(agent_message(frame), Some(AgentToControllerMessage::ServerExited(_) | AgentToControllerMessage::SlotStatus(_))))
    );

    booted.peer.send_json(&welcome());
    until("the exit after the welcome", || {
        after().iter().any(|frame| matches!(agent_message(frame), Some(AgentToControllerMessage::ServerExited(exit)) if exit.allocation_id == allocation_id))
    })
    .await;
    until("the idle slot after the welcome", || {
        after().iter().any(|frame| matches!(agent_message(frame), Some(AgentToControllerMessage::SlotStatus(status)) if status.state == SlotState::Idle))
    })
    .await;
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

/// A registry that serves `tags` and answers a manifest `HEAD` with `digest`, as ghcr.io does once
/// authorized. It records each `HEAD`.
struct Registry {
    tags: BTreeMap<String, Vec<String>>,
    digest: String,
    heads: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl RegistryFetch for Registry {
    async fn fetch(&self, url: &str, method: Method, _headers: &[(&str, String)]) -> Result<RegistryAnswer, ankor_agent::registry_tags::RegistryError> {
        let path = url::Url::parse(url).unwrap().path().to_owned();

        if method == Method::Head && path.contains("/manifests/") {
            self.heads.lock().unwrap().push(url.to_owned());

            return Ok(RegistryAnswer { status: 200, headers: BTreeMap::from([("docker-content-digest".to_owned(), self.digest.clone())]), body: Vec::new() });
        }

        let repository = path.strip_prefix("/v2/").and_then(|rest| rest.strip_suffix("/tags/list")).unwrap_or_default();
        let tags = self.tags.get(repository).cloned().unwrap_or_default();

        Ok(RegistryAnswer { status: 200, headers: BTreeMap::new(), body: json!({ "tags": tags }).to_string().into_bytes() })
    }
}

fn registry(tags: &[(&str, &[&str])], digest: &str) -> Arc<Registry> {
    Arc::new(Registry {
        tags: tags.iter().map(|(repository, tags)| ((*repository).to_owned(), tags.iter().map(|tag| (*tag).to_owned()).collect())).collect(),
        digest: digest.to_owned(),
        heads: Mutex::default(),
    })
}

fn labelled(version: &str) -> ImageDefaults {
    ImageDefaults { env: Vec::new(), labels: BTreeMap::from([(VERSION_LABEL.to_owned(), version.to_owned())]) }
}

#[tokio::test]
async fn updates_its_cs2_image_to_a_newer_release_of_its_major_when_it_starts_as_update_cs2_image_would() {
    let image = "ghcr.io/ankorcs/ankor-cs2:1";
    let (current, next) = (format!("sha256:{}", "1".repeat(64)), format!("sha256:{}", "2".repeat(64)));
    let (current_id, next_id) = (current.clone(), next.clone());
    let booted = boot(BootOptions {
        images: (Some(image.to_owned()), None),
        registry: Some(registry(&[("ankorcs/ankor-cs2", &["1", "1.0.0", "1.1.0", "2.0.0"])], "sha256:current")),
        prepare: Some(Box::new(move |docker, _| {
            docker.images.lock().unwrap().insert(image.to_owned(), current_id.clone());
            docker.images.lock().unwrap().insert("ankor-agent:test".to_owned(), agent_image_id());
            docker.pull_ids.lock().unwrap().insert(image.to_owned(), next_id);
            docker.image_defaults_by_id.lock().unwrap().insert(current_id, labelled("1.0.0"));
        })),
        ..BootOptions::default()
    })
    .await;

    until("the image update", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["image_update"]["outcome"] == json!(Cs2ImageUpdateOutcome::Completed)))
        .await;
    assert_eq!(*booted.docker.pulled.lock().unwrap(), [image]);

    let cs2 = node_statuses(&booted).last().unwrap()["cs2"].clone();

    assert_eq!((cs2["image"].clone(), cs2["image_id"].clone()), (json!(image), json!(next)));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn skips_the_build_a_rolled_back_swap_failed_with_and_takes_a_rebuild_of_its_release() {
    // A swap that never ran the build, as when its helper did not start, does not count against it.
    for (outcome, digest, pulls) in [
        (SwapFailureOutcome::Failed, "sha256:failed", Vec::new()),
        (SwapFailureOutcome::Failed, "sha256:rebuilt", vec!["ghcr.io/ankorcs/ankor-agent:1"]),
        (SwapFailureOutcome::NotStarted, "sha256:failed", vec!["ghcr.io/ankorcs/ankor-agent:1"]),
    ] {
        let image = "ghcr.io/ankorcs/ankor-agent:1";
        let registry = registry(&[("ankorcs/ankor-agent", &["1.0.0", "1.1.0"])], digest);
        let booted = boot(BootOptions {
            images: (None, Some(image.to_owned())),
            registry: Some(Arc::clone(&registry) as Arc<dyn RegistryFetch>),
            prepare: Some(Box::new(move |docker, data_dir| {
                std::fs::write(
                    data_dir.join("agent-update-failed.json"),
                    json!(SwapFailure { image: image.to_owned(), outcome, detail: "the replacement exited".to_owned() }).to_string(),
                )
                .unwrap();

                let default = docker.default_image_id.lock().unwrap().clone().unwrap_or_default();

                docker.images.lock().unwrap().insert("ankor-cs2:test".to_owned(), default);
                // The swap's pull moved the local tag to the build that failed; this agent still runs 1.0.0.
                docker.images.lock().unwrap().insert(image.to_owned(), "sha256:agent-1.1.0".to_owned());
                docker.image_defaults_by_id.lock().unwrap().insert(agent_image_id(), labelled("1.0.0"));
                docker.image_defaults_by_id.lock().unwrap().insert("sha256:agent-1.1.0".to_owned(), labelled("1.1.0"));
                docker.repo_digests_by_id.lock().unwrap().insert("sha256:agent-1.1.0".to_owned(), vec!["ghcr.io/ankorcs/ankor-agent@sha256:failed".to_owned()]);
            })),
            ..BootOptions::default()
        })
        .await;

        // Only a build that ran is compared with the registry's digest.
        if outcome == SwapFailureOutcome::Failed {
            until("the digest read", || !registry.heads.lock().unwrap().is_empty()).await;
        }

        until("the pulls", || booted.docker.pulled.lock().unwrap().len() == pulls.len()).await;
        // Past where a swap that should have been skipped would have pulled.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*booted.docker.pulled.lock().unwrap(), pulls, "{digest}");
        let _ = booted.agent.shutdown(Signal::Sigterm).await;
    }
}

#[tokio::test]
async fn tries_a_failed_image_pull_again_at_the_next_registry_read_not_when_the_failure_releases_the_guard() {
    let (cs2_image, agent_image) = ("ghcr.io/ankorcs/ankor-cs2:1", "ghcr.io/ankorcs/ankor-agent:1");
    let current = format!("sha256:{}", "1".repeat(64));
    let booted = boot(BootOptions {
        images: (Some(cs2_image.to_owned()), Some(agent_image.to_owned())),
        registry: Some(registry(&[("ankorcs/ankor-cs2", &["1.0.0", "1.1.0"]), ("ankorcs/ankor-agent", &["1.0.0", "1.1.0"])], "sha256:current")),
        prepare: Some(Box::new(move |docker, _| {
            docker.images.lock().unwrap().insert(cs2_image.to_owned(), current.clone());
            docker.images.lock().unwrap().insert(agent_image.to_owned(), agent_image_id());
            docker.image_defaults_by_id.lock().unwrap().insert(current, labelled("1.0.0"));
            docker.image_defaults_by_id.lock().unwrap().insert(agent_image_id(), labelled("1.0.0"));
            // A pull takes a while before it fails, as a real one does, so the other image's update meets its guard.
            *docker.pull_delay.lock().unwrap() = Some(Duration::from_millis(50));
            *docker.pull_error.lock().unwrap() = Some("no space left on device".to_owned());
        })),
        ..BootOptions::default()
    })
    .await;

    // Both updates are due; each pull fails once, and nothing tries again before the next interval.
    until("both pulls", || booted.docker.pull_attempts.lock().unwrap().len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*booted.docker.pull_attempts.lock().unwrap(), [cs2_image, agent_image]);
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn skips_the_build_a_swap_failed_with_after_agent_yml_has_moved_back_to_the_running_image() {
    let (running, failed) = ("ghcr.io/ankorcs/ankor-agent:1.0.0", "ghcr.io/ankorcs/ankor-agent:1.1.0");
    let registry = registry(&[("ankorcs/ankor-agent", &["1.0.0", "1.1.0"])], "sha256:failed");
    let booted = boot(BootOptions {
        images: (None, Some(running.to_owned())),
        registry: Some(Arc::clone(&registry) as Arc<dyn RegistryFetch>),
        prepare: Some(Box::new(move |docker, data_dir| {
            // A manual update_agent to the running image set agent.yml back; the failure file names the failed tag.
            std::fs::write(
                data_dir.join("agent-update-failed.json"),
                json!(SwapFailure { image: failed.to_owned(), outcome: SwapFailureOutcome::Failed, detail: "the replacement exited".to_owned() }).to_string(),
            )
            .unwrap();

            let default = docker.default_image_id.lock().unwrap().clone().unwrap_or_default();

            docker.images.lock().unwrap().insert("ankor-cs2:test".to_owned(), default);
            docker.images.lock().unwrap().insert(running.to_owned(), agent_image_id());
            docker.images.lock().unwrap().insert(failed.to_owned(), "sha256:agent-1.1.0".to_owned());
            docker.image_defaults_by_id.lock().unwrap().insert(agent_image_id(), labelled("1.0.0"));
            docker.image_defaults_by_id.lock().unwrap().insert("sha256:agent-1.1.0".to_owned(), labelled("1.1.0"));
            docker.repo_digests_by_id.lock().unwrap().insert("sha256:agent-1.1.0".to_owned(), vec!["ghcr.io/ankorcs/ankor-agent@sha256:failed".to_owned()]);
        })),
        ..BootOptions::default()
    })
    .await;

    until("the digest read", || !registry.heads.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(booted.docker.pulled.lock().unwrap().is_empty());
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

use ankor_agent::docker::UpdateKind;
use ankor_agent::testing::agent_boot::{installed_json, on_volume, write_installed};
use ankor_protocol::types::{Cs2Update, Cs2UpdateMode};

fn previous_update(kind: Option<UpdateKind>) -> ankor_agent::testing::agent_boot::Prepare {
    Box::new(move |docker, _| *docker.update_container.lock().unwrap() = Some(("ankor-update-previous".to_owned(), kind)))
}

fn default_kind() -> UpdateKind {
    UpdateKind { id: "00000000-0000-4000-8000-00000000a0a1".to_owned(), mode: Cs2UpdateMode::Update, automatic: false }
}

/// The previous agent's update ends with `code`.
fn previous_update_exits(booted: &Booted, code: i64) {
    *booted.docker.update_container.lock().unwrap() = None;

    let exits: Vec<_> =
        booted.docker.followed.lock().unwrap().iter().filter(|entry| entry.id == "ankor-update-previous").map(|entry| entry.hooks.on_exit.clone()).collect();

    for on_exit in exits {
        on_exit(Some(code));
    }
}

#[tokio::test]
async fn names_a_running_update_before_a_spool_it_cannot_open_and_reads_the_build_again_once_the_update_ends() {
    let booted = boot(BootOptions {
        prepare: Some(Box::new(|docker, data_dir| {
            std::fs::create_dir_all(data_dir.join("events")).unwrap();
            rusqlite::Connection::open(data_dir.join("events/streams.sqlite"))
                .unwrap()
                .execute_batch(&format!("PRAGMA user_version = {};", ankor_agent::event_spool::SPOOL_SCHEMA_VERSION + 1))
                .unwrap();
            previous_update(Some(default_kind()))(docker, data_dir);
        })),
        ..BootOptions::default()
    })
    .await;

    hello_seen(&booted).await;

    let unavailable = booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0]["unavailable"].clone();

    assert_eq!(
        (unavailable["code"].clone(), unavailable["operation"].clone()),
        (json!(NodeUnavailableCode::Maintenance), json!(MaintenanceOperation::Cs2Update))
    );

    // The previous agent's update records its build and exits.
    write_installed(&on_volume(booted.volume.path(), &installed_json(), ankor_agent::host_layout::INSTALL_DIR), "19001");
    previous_update_exits(&booted, 0);
    until("the build after the update", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["build_id"] == "19001")).await;
    assert_eq!(node_statuses(&booted).last().unwrap()["unavailable"]["code"], json!(NodeUnavailableCode::EventStorage));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn says_in_its_first_hello_which_update_runs_when_it_starts_beside_one_and_how_it_ended() {
    let kind = UpdateKind { id: "00000000-0000-4000-8000-00000000a0a2".to_owned(), mode: Cs2UpdateMode::Verify, automatic: true };
    let booted = boot(BootOptions { prepare: Some(previous_update(Some(kind))), ..BootOptions::default() }).await;

    hello_seen(&booted).await;

    let hello = booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))[0].clone();

    assert_eq!(
        (hello["unavailable"]["code"].clone(), hello["unavailable"]["operation"].clone()),
        (json!(NodeUnavailableCode::Maintenance), json!(MaintenanceOperation::Cs2Update))
    );
    assert_eq!(
        (hello["cs2"]["update"]["mode"].clone(), hello["cs2"]["update"]["automatic"].clone(), hello["cs2"]["update"]["outcome"].clone()),
        (json!(Cs2UpdateMode::Verify), json!(true), Value::Null)
    );

    write_installed(&on_volume(booted.volume.path(), &installed_json(), ankor_agent::host_layout::INSTALL_DIR), "19001");
    previous_update_exits(&booted, 0);
    until("the guard released", || node_statuses(&booted).last().is_some_and(|frame| frame["unavailable"].is_null())).await;

    let cs2 = node_statuses(&booted).last().unwrap()["cs2"].clone();

    assert_eq!(cs2["build_id"], "19001");
    assert_eq!(
        (cs2["update"]["mode"].clone(), cs2["update"]["automatic"].clone(), cs2["update"]["outcome"].clone(), cs2["update"]["detail"].clone()),
        (json!(Cs2UpdateMode::Verify), json!(true), json!(Cs2UpdateOutcome::Completed), Value::Null)
    );
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn updates_cs2_itself_once_an_update_it_did_not_start_exits_without_installing_the_newest_build() {
    let booted = boot(BootOptions { prepare: Some(previous_update(Some(default_kind()))), ..BootOptions::default() }).await;

    hello_seen(&booted).await;
    // Steam names a newer build while the previous agent's update still holds the guard.
    check_build(&booted, "19001");
    until("the newest public build", || {
        booted
            .frames(|message| matches!(message, AgentToControllerMessage::Hello(_)))
            .iter()
            .chain(node_statuses(&booted).iter())
            .any(|frame| frame["cs2"]["latest_build_id"] == "19001")
    })
    .await;
    assert_eq!(role_count(&booted, ContainerRole::Update), 0);

    // That update ends and leaves the installed build as it was.
    previous_update_exits(&booted, 1);
    until("the build after its own update", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["build_id"] == "19001")).await;
    assert_eq!(role_count(&booted, ContainerRole::Update), 1);

    // Its own run, labelled and reported as one it started itself.
    let spec =
        booted.docker.specs().into_iter().find(|spec| spec.labels.get(labels::ROLE).map(String::as_str) == Some(ContainerRole::Update.as_str())).unwrap();

    assert_eq!((spec.labels[labels::UPDATE_MODE].clone(), spec.labels[labels::UPDATE_AUTOMATIC].as_str()), (wire(Cs2UpdateMode::Update), "true"));
    until("its update reported done", || {
        node_statuses(&booted).last().is_some_and(|frame| frame["cs2"]["update"]["outcome"] == json!(Cs2UpdateOutcome::Completed))
    })
    .await;
    assert_eq!(
        node_statuses(&booted).last().unwrap()["cs2"]["update"],
        json!(Cs2Update {
            id: spec.labels[labels::UPDATE_ID].clone(),
            mode: Cs2UpdateMode::Update,
            automatic: true,
            outcome: Some(Cs2UpdateOutcome::Completed),
            detail: None
        })
    );
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

// The setup runs on the real clock: paused time skips ahead while a test waits on real I/O, such as
// the first upload, and could reach the next scan before the second demo is written. Time pauses
// once the uploader has gone idle, at most a few seconds after boot, so the next scan is 30 seconds
// after boot and the 20 seconds below end before it.
#[tokio::test]
async fn uploads_a_demo_left_on_an_idle_slot_on_its_30_second_scan_alone() {
    let booted = boot(BootOptions::default()).await;
    let (port, received) = demo_receiver().await;

    hello_seen(&booted).await;

    let slot_demos = booted.data_dir.path().join("slots/a/game/csgo/ankor/demos");
    let earlier = slot_demos.join("22222222-2222-4222-8222-222222222222");

    // A demo already there when uploads turn on: its upload shows the scan that turning them on
    // starts has run, so the next demo can only be found by a later scan.
    std::fs::create_dir_all(&earlier).unwrap();
    std::fs::write(earlier.join("20260922-180000_match-0_de_dust2.dem"), "earlier-bytes").unwrap();

    let upload =
        json!({ "type": "set_config", "config": { "demo_upload": { "url": format!("http://127.0.0.1:{port}/demos"), "header": null, "limit_mbit": null } } });

    assert_eq!(booted.peer.request(upload).await["ok"], true);
    until("the earlier demo's upload", || received.lock().unwrap().len() == 1).await;
    // After an upload the uploader lists the folders again and uploads whatever waits, so a demo
    // written before that listing would go up without a scan. The listing follows the deletion of
    // the uploaded file directly, and is a local directory read.
    until("the earlier demo's removal", || !earlier.join("20260922-180000_match-0_de_dust2.dem").exists()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::time::pause();

    let demos = slot_demos.join("11111111-1111-4111-8111-111111111111");

    std::fs::create_dir_all(&demos).unwrap();
    std::fs::write(demos.join("20260922-183000_match-1_de_mirage.dem"), "gotv-bytes").unwrap();
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert_eq!(received.lock().unwrap().len(), 1);
    until("the upload", || received.lock().unwrap().len() == 2).await;
    assert_eq!(*received.lock().unwrap(), ["/demos", "/demos"]);
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test(start_paused = true)]
async fn stops_a_game_that_stays_empty_past_its_idle_shutdown_on_its_30_second_check_alone() {
    let booted = boot(BootOptions::default()).await;
    let allocation_id = allocation();

    hello_seen(&booted).await;

    let start = json!({ "type": "start_server", "allocation_id": allocation_id, "launch": launch_config(&json!({ "idle_shutdown_seconds": 60 })) });

    assert_eq!(booted.peer.request(start).await["ok"], true);

    let mut game = GamePlugin::connect(&booted.docker, &allocation_id).await;

    game.send(
        &json!({ "type": "server_status", "request_id": null, "map_name": "de_mirage", "match": null, "players": [], "config_error": null, "ready": true, "server_steam_id": null }),
    )
    .await;
    until("the game running", || booted.peer.status("a").is_some_and(|status| status["state"] == json!(SlotState::Running))).await;

    // Empty from the start: past its 60 s, the next check stops it.
    tokio::time::sleep(Duration::from_secs(55)).await;
    assert!(booted.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_))).is_empty());
    until("the idle stop", || !booted.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_))).is_empty()).await;

    let exit = booted.frames(|message| matches!(message, AgentToControllerMessage::ServerExited(_)))[0].clone();

    assert_eq!((exit["allocation_id"].clone(), exit["reason"].clone()), (json!(allocation_id), json!(ServerExitReason::IdleEmpty)));
    let _ = booted.agent.shutdown(Signal::Sigterm).await;
}

#[tokio::test]
async fn starts_no_cs2_update_once_shutdown_has_begun_when_the_maintenance_that_held_it_back_ends() {
    let booted = boot(BootOptions::default()).await;

    hello_seen(&booted).await;
    // The image switch holds the guard through its slow pull, so the newer build waits for it.
    *booted.docker.pull_delay.lock().unwrap() = Some(Duration::from_millis(800));
    assert_eq!(booted.peer.request(json!({ "type": "update_cs2_image", "image": "ankor-cs2:next", "pull": true })).await["ok"], true);
    check_build(&booted, "19001");
    until("the newer public build", || node_statuses(&booted).iter().any(|frame| frame["cs2"]["latest_build_id"] == "19001")).await;
    assert_eq!(role_count(&booted, ContainerRole::Update), 0);

    let _ = booted.agent.shutdown(Signal::Sigterm).await;
    until("the pull", || booted.docker.pulled.lock().unwrap().iter().any(|image| image == "ankor-cs2:next")).await;
    // Past the switch's end, which releases the guard.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(role_count(&booted, ContainerRole::Update), 0);
}
