//! The controller's requests through the handler: launch refusals and the reads they make, stops during a
//! launch, and the requests a running server's plugin answers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::controller_connection::HandlerError;
use ankor_agent::failure::AgentFailure;
use ankor_agent::install::{MaintenanceGuard, MaintenanceGuardOptions};
use ankor_agent::testing::fixtures::{agent_config, launch_config};
use ankor_agent::testing::handler::{Handler, TakenCommand, make_handler};
use ankor_agent::testing::slots::{Slots, launch_server, make_slots};
use ankor_agent::testing::wire;
use ankor_protocol::constants::SERVER_STATUS_TIMEOUT;
use ankor_protocol::types::{
    AgentErrorCode, AgentToControllerMessage, MaintenanceOperation, NodeUnavailable, NodeUnavailableCode, ServerExitReason, ServerState, SlotState,
    StartRefusal,
};
use serde_json::{Value, json};
use tokio::sync::watch;

const PLAYER_A: &str = "76561198000000001";

static NEXT: AtomicU64 = AtomicU64::new(1);

fn allocation() -> String {
    format!("0199a0b4-5c00-7000-8000-{:012x}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn start(allocation_id: &str) -> Value {
    json!({ "type": "start_server", "id": "s1", "allocation_id": allocation_id, "launch": launch_config(&json!({})) })
}

fn code_of(error: &HandlerError) -> AgentErrorCode {
    error.refusal().unwrap_or_else(|| panic!("no agent failure: {error}")).code
}

fn status_timeout() -> Duration {
    SERVER_STATUS_TIMEOUT
}

fn base() -> Slots {
    make_slots(agent_config(&json!({})))
}

fn guard(base: &Slots) -> MaintenanceGuard {
    MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(&base.docker) as Arc<dyn ankor_agent::docker::DockerPort>))
}

/// Reads a value a test sets.
fn shared<T: Clone + Send + 'static>(value: &Arc<Mutex<T>>) -> impl Fn() -> T + Send + Sync + 'static {
    let value = Arc::clone(value);

    move || value.lock().unwrap().clone()
}

fn reads_build(handler_build: &Arc<Mutex<Option<String>>>) -> ankor_agent::request_handler::BuildRead {
    let read = shared(handler_build);

    Arc::new(move || {
        let build = read();

        Box::pin(async move { build })
    })
}

mod start_server_refusals {
    use super::*;

    /// Refuses a launch with a start refusal, leaving every slot and container as it was.
    async fn refuse(handler: &Handler, base: &Slots, refused: &Mutex<Vec<String>>) {
        let states = base.reported().len();
        let started = base.docker.started.lock().unwrap().len();
        let error = handler.request(start(&allocation())).await.unwrap_err();
        let failure = error.refusal().expect("an agent failure");
        let code = serde_json::to_value(failure.code).unwrap();

        assert!(StartRefusal::ALL.iter().any(|refusal| serde_json::to_value(refusal).unwrap() == code), "{failure}");
        assert!(base.reported()[states..].iter().all(|status| status.state != SlotState::Starting));
        assert_eq!(base.docker.started.lock().unwrap().len(), started);
        refused.lock().unwrap().push(code.as_str().unwrap().to_owned());
    }

    #[tokio::test]
    async fn sends_every_start_refusal_before_reserving_a_slot() {
        let base = base();
        let updates = guard(&base);
        let unavailable: Arc<Mutex<Option<NodeUnavailable>>> = Arc::default();
        let installed = Arc::new(Mutex::new(Some("19000".to_owned())));
        let handler = make_handler(&base, |deps| {
            deps.config = deps.config.with_persist(Arc::new(|_, _| Ok(())));
            deps.updates = updates.clone();
            deps.unavailable = Arc::new(shared(&unavailable));
            deps.installed_build = reads_build(&installed);
        });
        let refused: Arc<Mutex<Vec<String>>> = Arc::default();
        let release = updates.claim(MaintenanceOperation::AgentUpdate).await.unwrap();

        refuse(&handler, &base, &refused).await;
        release.release(None);
        *unavailable.lock().unwrap() =
            Some(NodeUnavailable { code: NodeUnavailableCode::EventStorage, operation: None, detail: "event spool schema mismatch".to_owned() });
        refuse(&handler, &base, &refused).await;
        *unavailable.lock().unwrap() = None;
        *installed.lock().unwrap() = None;
        refuse(&handler, &base, &refused).await;
        *installed.lock().unwrap() = Some("19000".to_owned());
        handler.public_build.set_latest(Some("19001"));
        refuse(&handler, &base, &refused).await;
        handler.public_build.set_latest(None);
        handler.request(start(&allocation())).await.unwrap();
        handler.request(start(&allocation())).await.unwrap();
        refuse(&handler, &base, &refused).await;
        base.stop.cancel();
        refuse(&handler, &base, &refused).await;

        let mut seen = refused.lock().unwrap().clone();
        let mut all: Vec<String> = StartRefusal::ALL.iter().map(wire).collect();

        seen.sort();
        all.sort();
        assert_eq!(seen, all);
    }
}

mod start_server_and_a_maintenance_claim {
    use super::*;

    #[tokio::test]
    async fn refuses_a_start_when_an_update_takes_the_guard_during_its_build_read_with_no_slot_taken() {
        let base = base();
        let updates = guard(&base);
        let (read, reading) = watch::channel(None::<String>);
        let handler = make_handler(&base, |deps| {
            deps.updates = updates.clone();
            deps.installed_build = Arc::new(move || {
                let mut reading = reading.clone();

                Box::pin(async move { reading.wait_for(Option::is_some).await.ok().and_then(|build| build.clone()) })
            });
        });
        handler.public_build.set_latest(Some("19000"));
        let handler = Arc::new(handler);
        let starting = tokio::spawn({
            let handler = Arc::clone(&handler);

            async move { handler.request(start(&allocation())).await.map_err(|error| code_of(&error)) }
        });

        tokio::task::yield_now().await;

        let release = updates.claim(MaintenanceOperation::Cs2Update).await.unwrap();

        read.send_replace(Some("19000".to_owned()));
        assert_eq!(starting.await.unwrap().unwrap_err(), AgentErrorCode::MaintenanceRunning);
        assert!(!base.slots.any_busy());
        release.release(None);
    }
}

mod stop_server_during_its_launch {
    use super::*;

    /// A handler whose build read waits for the test, so a stop can arrive before the slot is picked.
    fn held_build_read(installed: &str) -> (Slots, Arc<Handler>, watch::Sender<bool>) {
        let base = base();
        let (read, reading) = watch::channel(false);
        let installed = installed.to_owned();
        let handler = make_handler(&base, |deps| {
            deps.installed_build = Arc::new(move || {
                let (mut reading, installed) = (reading.clone(), installed.clone());

                Box::pin(async move {
                    let _ = reading.wait_for(|done| *done).await;
                    Some(installed)
                })
            });
        });
        handler.public_build.set_latest(Some("19000"));

        (base, Arc::new(handler), read)
    }

    fn send(handler: &Arc<Handler>, frame: Value) -> tokio::task::JoinHandle<Result<Value, String>> {
        let handler = Arc::clone(handler);

        tokio::spawn(async move { handler.request(frame).await.map_err(|error| format!("{error:?}|{error}")) })
    }

    #[tokio::test]
    async fn waits_for_a_start_still_reading_the_build_to_pick_its_slot_then_ends_the_launch_before_its_server_starts() {
        let (base, handler, read) = held_build_read("19000");
        let allocation_id = allocation();
        let starting = send(&handler, start(&allocation_id));

        tokio::task::yield_now().await;

        let stopping = send(&handler, json!({ "type": "stop_server", "id": "s2", "allocation_id": allocation_id }));

        tokio::task::yield_now().await;
        read.send_replace(true);
        assert!(starting.await.unwrap().unwrap_err().contains("stopped before its server started"));
        assert_eq!(stopping.await.unwrap().unwrap(), json!({ "allocation_id": allocation_id }));
        assert!(base.docker.started.lock().unwrap().is_empty());

        let exits = base.exits();

        assert_eq!(exits.len(), 1);
        assert_eq!((exits[0].0.allocation_id.as_str(), exits[0].0.reason), (allocation_id.as_str(), ServerExitReason::Requested));
        assert_eq!(base.status("a").state, SlotState::Idle);
    }

    #[tokio::test]
    async fn sees_auto_update_turned_on_during_the_build_read() {
        let base = make_slots(agent_config(&json!({ "auto_update": false })));
        let (read, reading) = watch::channel(false);
        let handler = Arc::new(make_handler(&base, |deps| {
            deps.installed_build = Arc::new(move || {
                let mut reading = reading.clone();

                Box::pin(async move {
                    let _ = reading.wait_for(|done| *done).await;
                    Some("18999".to_owned())
                })
            });
        }));
        handler.public_build.set_latest(Some("19000"));
        let starting = send(&handler, start(&allocation()));

        tokio::task::yield_now().await;
        base.config.lock().unwrap().auto_update = true;
        read.send_replace(true);
        assert!(starting.await.unwrap().unwrap_err().contains("Cs2Outdated"));
    }

    #[tokio::test]
    async fn answers_unknown_allocation_once_a_start_it_waited_for_is_refused() {
        let (base, handler, read) = held_build_read("18999");
        let allocation_id = allocation();
        let starting = send(&handler, start(&allocation_id));

        tokio::task::yield_now().await;

        let stopping = send(&handler, json!({ "type": "stop_server", "id": "s2", "allocation_id": allocation_id }));

        tokio::task::yield_now().await;
        read.send_replace(true);
        assert!(starting.await.unwrap().unwrap_err().contains("Cs2Outdated"));
        assert!(stopping.await.unwrap().unwrap_err().contains("UnknownAllocation"));
        assert!(base.docker.started.lock().unwrap().is_empty());
    }
}

mod start_server_on_an_outdated_build {
    use super::*;

    #[tokio::test]
    async fn refuses_while_a_newer_public_build_is_out_and_launches_once_the_node_is_on_it() {
        let base = base();
        let installed = Arc::new(Mutex::new(Some("19000".to_owned())));
        let handler = make_handler(&base, |deps| {
            deps.installed_build = reads_build(&installed);
        });
        handler.public_build.set_latest(Some("19001"));
        let error = handler.request(start(&allocation())).await.unwrap_err();

        assert_eq!(code_of(&error), AgentErrorCode::Cs2Outdated);
        assert!(regex::Regex::new("19000.*19001").unwrap().is_match(&error.to_string()), "{error}");
        assert!(!base.slots.any_busy());

        *installed.lock().unwrap() = Some("19001".to_owned());
        assert!(handler.request(start(&allocation())).await.unwrap().get("slot_id").is_some());
    }

    #[tokio::test]
    async fn launches_behind_the_public_build_with_auto_update_off() {
        let base = make_slots(agent_config(&json!({ "auto_update": false })));
        let handler = make_handler(&base, |_| {});

        handler.public_build.set_latest(Some("19001"));
        assert!(handler.request(start(&allocation())).await.unwrap().get("slot_id").is_some());
    }

    #[tokio::test]
    async fn launches_while_the_newest_public_build_is_unknown() {
        let base = base();
        let handler = make_handler(&base, |_| {});

        assert!(handler.request(start(&allocation())).await.unwrap().get("slot_id").is_some());
    }
}

mod start_server_on_a_node_without_cs2 {
    use super::*;

    #[tokio::test]
    async fn refuses_before_reserving_a_slot_whether_or_not_the_newest_public_build_is_known_and_launches_once_cs2_is_installed() {
        for latest in [None, Some("19001")] {
            let base = base();
            let installed: Arc<Mutex<Option<String>>> = Arc::default();
            let handler = make_handler(&base, |deps| {
                deps.installed_build = reads_build(&installed);
            });
            handler.public_build.set_latest(latest);
            let error = handler.request(start(&allocation())).await.unwrap_err();

            assert_eq!(code_of(&error), AgentErrorCode::Cs2NotInstalled);
            assert!(!error.to_string().trim().is_empty());
            assert!(!base.slots.any_busy());
            assert!(base.docker.started.lock().unwrap().is_empty());

            *installed.lock().unwrap() = Some(latest.unwrap_or("19000").to_owned());
            assert!(handler.request(start(&allocation())).await.unwrap().get("slot_id").is_some());
        }
    }

    #[tokio::test]
    async fn refuses_with_maintenance_running_while_the_first_install_holds_the_guard() {
        let base = base();
        let updates = guard(&base);
        let release = updates.claim(MaintenanceOperation::Cs2Update).await.unwrap();
        let handler = make_handler(&base, |deps| {
            deps.updates = updates.clone();
            deps.installed_build = Arc::new(|| Box::pin(async { None }));
        });

        assert_eq!(code_of(&handler.request(start(&allocation())).await.unwrap_err()), AgentErrorCode::MaintenanceRunning);
        release.release(None);
    }

    /// The disk is asked before the slot lock, and admission reads that answer.
    #[tokio::test]
    async fn asks_the_disk_before_admitting_a_launch_and_refuses_on_what_it_answers() {
        let base = base();
        let short = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler = make_handler(&base, |deps| {
            let (asked, read) = (Arc::clone(&short), Arc::clone(&short));

            deps.check_disk = Arc::new(move || {
                asked.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            deps.unavailable = Arc::new(move || {
                read.load(Ordering::SeqCst).then(|| NodeUnavailable {
                    code: NodeUnavailableCode::DiskSpace,
                    operation: None,
                    detail: "less than 1024 MiB free under /srv/ankor".to_owned(),
                })
            });
        });

        assert_eq!(code_of(&handler.request(start(&allocation())).await.unwrap_err()), AgentErrorCode::NodeUnavailable);
    }

    /// The last disk figure can be up to a check old; a launch is judged on the disk's answer to
    /// it, so space freed since is taken at once.
    #[tokio::test]
    async fn takes_a_launch_once_the_disk_has_room_again_although_the_last_figure_was_short() {
        let base = base();
        let short = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let handler = make_handler(&base, |deps| {
            let (freed, read) = (Arc::clone(&short), Arc::clone(&short));

            deps.check_disk = Arc::new(move || {
                freed.store(false, Ordering::SeqCst);
                Box::pin(async {})
            });
            deps.unavailable = Arc::new(move || {
                read.load(Ordering::SeqCst).then(|| NodeUnavailable {
                    code: NodeUnavailableCode::DiskSpace,
                    operation: None,
                    detail: "less than 1024 MiB free under /srv/ankor".to_owned(),
                })
            });
        });

        handler.request(start(&allocation())).await.unwrap();
    }
}

mod get_server_status {
    use super::*;

    #[tokio::test]
    async fn asks_the_plugin_of_a_game_the_node_runs_and_replies_with_its_status() {
        let base = base();
        let handler = make_handler(&base, |_| {});
        let allocation_id = allocation();
        let answer: ServerState = serde_json::from_value(json!({
            "map_name": "de_mirage", "match": null, "config_error": null, "ready": true, "server_steam_id": "85568392936840254",
            "players": [{ "steam_id": PLAYER_A, "name": PLAYER_A, "side": "t" }],
        }))
        .unwrap();

        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        *handler.plugins.answer.lock().unwrap() = Some(answer.clone());

        let mut expected = json!({ "allocation_id": allocation_id });

        expected.as_object_mut().unwrap().extend(serde_json::to_value(&answer).unwrap().as_object().unwrap().clone());
        assert_eq!(handler.request(json!({ "type": "get_server_status", "id": "r1", "allocation_id": allocation_id })).await.unwrap(), expected);
        assert_eq!(*handler.plugins.asked.lock().unwrap(), [(allocation_id, status_timeout())]);
    }

    #[tokio::test]
    async fn refuses_a_game_the_node_does_not_run_without_asking_any_plugin() {
        let base = base();
        let handler = make_handler(&base, |_| {});
        let error = handler.request(json!({ "type": "get_server_status", "id": "r1", "allocation_id": allocation() })).await.unwrap_err();

        assert_eq!(code_of(&error), AgentErrorCode::UnknownAllocation);
        assert!(handler.plugins.asked.lock().unwrap().is_empty());
    }
}

mod console_command {
    use super::*;

    #[tokio::test]
    async fn hands_the_command_to_the_plugin_of_a_game_the_node_runs_and_replies_with_its_allocation() {
        let base = base();
        let handler = make_handler(&base, |_| {});
        let allocation_id = allocation();

        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        assert_eq!(
            handler.request(json!({ "type": "console_command", "id": "r1", "allocation_id": allocation_id, "command": "status" })).await.unwrap(),
            json!({ "allocation_id": allocation_id })
        );
        assert_eq!(*handler.plugins.commands.lock().unwrap(), [(allocation_id, TakenCommand::Exec("status".to_owned()))]);
    }

    #[tokio::test]
    async fn refuses_a_game_the_node_does_not_run_and_one_whose_plugin_is_not_connected() {
        let base = base();
        let handler = make_handler(&base, |_| {});
        let allocation_id = allocation();
        let command = json!({ "type": "console_command", "id": "r1", "allocation_id": allocation_id, "command": "status" });

        assert_eq!(code_of(&handler.request(command.clone()).await.unwrap_err()), AgentErrorCode::UnknownAllocation);
        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        *handler.plugins.online.lock().unwrap() = false;
        assert_eq!(code_of(&handler.request(command).await.unwrap_err()), AgentErrorCode::PluginOffline);
        assert!(handler.plugins.commands.lock().unwrap().is_empty());
    }
}

mod restore_round {
    use super::*;

    /// The backup the engine wrote as round 14 of the game in slot `a` started.
    fn write_backup(data_dir: &std::path::Path) {
        let dir = data_dir.join("slots/a/game/csgo/ankor/backups");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ankor_round13.txt"), "\"SaveFile\"\n{\n}\n").unwrap();
    }

    fn setup() -> (Slots, Handler, tempfile::TempDir, String) {
        let base = base();
        let data_dir = tempfile::tempdir().unwrap();
        let path = data_dir.path().to_owned();
        let handler = make_handler(&base, |deps| deps.data_dir = path);

        (base, handler, data_dir, allocation())
    }

    #[tokio::test]
    async fn hands_the_round_to_the_plugin_once_the_node_has_its_backup_and_replies_with_both() {
        let (base, handler, data_dir, allocation_id) = setup();

        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        write_backup(data_dir.path());
        assert_eq!(
            handler.request(json!({ "type": "restore_round", "id": "r1", "allocation_id": allocation_id, "round": 14 })).await.unwrap(),
            json!({ "allocation_id": allocation_id, "round": 14 })
        );
        assert_eq!(*handler.plugins.commands.lock().unwrap(), [(allocation_id, TakenCommand::RestoreRound(14))]);
    }

    #[tokio::test]
    async fn refuses_a_round_the_node_has_no_backup_of_naming_the_file_without_troubling_the_plugin() {
        let (base, handler, data_dir, allocation_id) = setup();

        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        write_backup(data_dir.path());

        let error = handler.request(json!({ "type": "restore_round", "id": "r1", "allocation_id": allocation_id, "round": 15 })).await.unwrap_err();

        assert_eq!(code_of(&error), AgentErrorCode::NoBackup);
        assert_eq!(error.to_string(), "no backup of round 15: ankor_round14.txt is not on the node");
        assert!(handler.plugins.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_a_game_the_node_does_not_run_and_one_whose_plugin_is_not_connected() {
        let (base, handler, data_dir, allocation_id) = setup();
        let restore = json!({ "type": "restore_round", "id": "r1", "allocation_id": allocation_id, "round": 14 });

        assert_eq!(code_of(&handler.request(restore.clone()).await.unwrap_err()), AgentErrorCode::UnknownAllocation);
        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        write_backup(data_dir.path());
        *handler.plugins.online.lock().unwrap() = false;
        assert_eq!(code_of(&handler.request(restore).await.unwrap_err()), AgentErrorCode::PluginOffline);
        assert!(handler.plugins.commands.lock().unwrap().is_empty());
    }
}

mod start_match {
    use super::*;

    #[tokio::test]
    async fn replies_once_the_plugin_starts_the_match_waiting_no_longer_than_for_a_status() {
        let base = base();
        let handler = make_handler(&base, |_| {});
        let allocation_id = allocation();

        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        assert_eq!(
            handler.request(json!({ "type": "start_match", "id": "r1", "allocation_id": allocation_id })).await.unwrap(),
            json!({ "allocation_id": allocation_id })
        );
        assert_eq!(*handler.plugins.commands.lock().unwrap(), [(allocation_id.clone(), TakenCommand::StartMatch)]);
        assert_eq!(*handler.plugins.asked.lock().unwrap(), [(allocation_id, status_timeout())]);
    }

    #[tokio::test]
    async fn passes_the_plugins_refusal_on() {
        let base = base();
        let handler = make_handler(&base, |_| {});
        let allocation_id = allocation();

        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        *handler.plugins.error.lock().unwrap() = Some(AgentFailure::code(AgentErrorCode::TeamEmpty));
        assert_eq!(
            code_of(&handler.request(json!({ "type": "start_match", "id": "r1", "allocation_id": allocation_id })).await.unwrap_err()),
            AgentErrorCode::TeamEmpty
        );
    }

    #[tokio::test]
    async fn refuses_a_game_the_node_does_not_run_and_one_whose_plugin_is_not_connected() {
        let base = base();
        let handler = make_handler(&base, |_| {});
        let allocation_id = allocation();
        let request = json!({ "type": "start_match", "id": "r1", "allocation_id": allocation_id });

        assert_eq!(code_of(&handler.request(request.clone()).await.unwrap_err()), AgentErrorCode::UnknownAllocation);
        launch_server(&base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        *handler.plugins.online.lock().unwrap() = false;
        assert_eq!(code_of(&handler.request(request).await.unwrap_err()), AgentErrorCode::PluginOffline);
        assert!(handler.plugins.commands.lock().unwrap().is_empty());
    }
}

mod update_cs2_over_the_request_handler {
    use super::*;

    #[tokio::test]
    async fn replies_immediately_with_the_updates_id_then_extracts_and_runs_the_game_update() {
        let base = base();
        let handler = make_handler(&base, |_| {});
        // The reply comes before any work, and names the update its progress frames carry.
        let reply = handler.request(json!({ "type": "update_cs2", "id": "u1", "mode": "update" })).await.unwrap();
        let update_id = reply["update_id"].as_str().unwrap().to_owned();

        assert!(regex::Regex::new("^[0-9a-f-]{36}$").unwrap().is_match(&update_id), "{reply}");
        ankor_agent::testing::until("the update container", || base.docker.started.lock().unwrap().len() == 1).await;
        base.docker.exit(0, Some(0));
        ankor_agent::testing::until("the update to end", || !handler.updates.running()).await;

        assert_eq!(*base.overlay.calls.lock().unwrap(), 1);
        assert_eq!(base.docker.started.lock().unwrap().len(), 1);

        // Progress reaches the controller as `cs2_update_progress`, the extraction included, each naming the
        // update.
        let progress: Vec<Value> = handler
            .pushed
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| matches!(frame, AgentToControllerMessage::Cs2UpdateProgress(_)))
            .map(|frame| serde_json::to_value(frame).unwrap())
            .collect();

        assert!(progress.len() > 1);
        assert!(progress.iter().all(|frame| frame["update_id"] == update_id.as_str()));
    }
}

#[tokio::test]
async fn excludes_game_and_gslt_launches_while_an_update_owns_the_host() {
    let base = base();
    let handler = make_handler(&base, |_| {});

    base.overlay.close();
    handler.request(json!({ "type": "update_cs2", "id": "update", "mode": "update" })).await.unwrap();
    ankor_agent::testing::until("the overlay extraction", || *base.overlay.calls.lock().unwrap() == 1).await;
    assert_eq!(code_of(&handler.request(start(&allocation())).await.unwrap_err()), AgentErrorCode::MaintenanceRunning);
    assert!(base.docker.started.lock().unwrap().is_empty());
    base.overlay.gate.add_permits(1 << 20);
    ankor_agent::testing::until("the update container", || base.docker.started.lock().unwrap().len() == 1).await;
    base.docker.exit(0, Some(0));
    ankor_agent::testing::until("the update to end", || !handler.updates.running()).await;
    assert!(handler.request(start(&allocation())).await.unwrap().get("slot_id").is_some());
}

#[tokio::test]
async fn refuses_updates_while_a_cs2_game_is_preparing_and_permits_concurrent_games() {
    let base = base();
    let handler = Arc::new(make_handler(&base, |_| {}));

    base.overlay.close();

    let first = tokio::spawn({
        let handler = Arc::clone(&handler);

        async move { handler.request(start(&allocation())).await.map_err(|error| error.to_string()) }
    });

    // Preparing: the launch has read the installed build and taken its slot.
    ankor_agent::testing::until("the slot taken", || base.slots.any_busy()).await;
    assert_eq!(code_of(&handler.request(json!({ "type": "update_cs2", "id": "u", "mode": "update" })).await.unwrap_err()), AgentErrorCode::SlotsHeld);

    let second = tokio::spawn({
        let handler = Arc::clone(&handler);

        async move { handler.request(start(&allocation())).await.map_err(|error| error.to_string()) }
    });

    base.overlay.gate.add_permits(1 << 20);
    assert!(second.await.unwrap().unwrap().get("slot_id").is_some());
    first.await.unwrap().unwrap();
}

mod the_demo_requests_the_controller_sends {
    use super::*;

    const DEMO: &str = "20260922-183000_match-1_de_mirage.dem";

    #[tokio::test]
    async fn answers_delete_demo_with_the_file_it_removed() {
        let handler = make_handler(&base(), |_| {});

        assert_eq!(
            handler.request(json!({ "type": "delete_demo", "id": "r2", "slot_id": "a", "file_name": DEMO })).await.unwrap(),
            json!({ "slot_id": "a", "file_name": DEMO })
        );
        assert_eq!(*handler.demos.removed.lock().unwrap(), [("a".to_owned(), DEMO.to_owned())]);
    }

    #[tokio::test]
    async fn turns_a_refused_delete_into_a_failed_reply() {
        let handler = make_handler(&base(), |_| {});

        *handler.demos.remove_error.lock().unwrap() = Some("unknown slot c".to_owned());
        assert!(
            handler
                .request(json!({ "type": "delete_demo", "id": "r1", "slot_id": "c", "file_name": DEMO }))
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown slot c")
        );
    }
}
