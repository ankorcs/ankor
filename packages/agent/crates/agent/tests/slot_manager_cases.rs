//! The slot manager case by case, against the shared fakes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::container_spec::{ContainerRole, GAME_CPU_WEIGHT, cpu_shares_for_weight, labels};
use ankor_agent::contract::constants::ankor_env;
use ankor_agent::contract::types::ServerStatus;
use ankor_agent::docker::AdoptedContainer;
use ankor_agent::docker_api::DockerError;
use ankor_agent::failure::AgentFailure;
use ankor_agent::gslt_keep_alive::GsltSlotsPort;
use ankor_agent::host_layout::BindMount;
use ankor_agent::install::build_update_spec;
use ankor_agent::logger::{Entry, Level, Logger};
use ankor_agent::slot_manager::{SlotError, SlotEvent};
use ankor_agent::testing::fixtures::{GSLT_A, GSLT_B, MATCH_ID, agent_config, launch_config, ready_status, server_launch_config};
use ankor_agent::testing::slots::{Slots, change_layout, launch_server, make_slots, make_slots_with};
use ankor_agent::testing::until;
use ankor_protocol::constants::{QUIT_TIMEOUT, SERVER_EXIT_DETAIL_MAX_CHARS, SLOT_RESERVATION, START_TIMEOUT, STOP_TIMEOUT};
use ankor_protocol::schemas::s_slot_status;
use ankor_protocol::types::{
    AgentErrorCode, ConfiguredSlot, Cs2UpdateMode, GetServerStatusReplyPlayers, GsltState, ServerExit, ServerExitReason, ServerLaunchConfig, Side, SlotState,
    SlotStatus, StartRefusal, StartServerReply,
};
use ankor_protocol::validate::safe_parse;
use futures_util::poll;
use serde_json::json;
use tokio::sync::oneshot;
use tokio::time::Instant;

const PLAYER_A: &str = "76561198000000001";
const PLAYER_B: &str = "76561198000000002";
const REPLACEMENT: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCC0003";
const PLUGIN_URL: &str = "ws://127.0.0.1:4500/ws/plugin?slot_id=a";
/// The relay key an adopted game's label carries.
const RELAY_KEY: &str = "0123456789abcdef";

/// A fresh allocation id for each launch.
fn new_allocation() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);

    format!("0199a0b4-5c00-7000-8000-{:012x}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn status(players: &[&str], config_error: Option<&str>, ready: bool) -> ServerStatus {
    ServerStatus {
        map_name: "de_mirage".to_owned(),
        r#match: None,
        players: players.iter().map(|id| GetServerStatusReplyPlayers { steam_id: (*id).to_owned(), name: (*id).to_owned(), side: Side::T }).collect(),
        config_error: config_error.map(str::to_owned),
        ready,
        request_id: None,
        server_steam_id: None,
    }
}

fn base() -> Slots {
    make_slots(agent_config(&json!({})))
}

fn slot(id: &str, port: i64, tv_port: i64, gslt: &str) -> ConfiguredSlot {
    ConfiguredSlot { id: id.to_owned(), port, tv_port, gslt: gslt.to_owned() }
}

fn exit(slot_id: &str, allocation_id: &str, reason: ServerExitReason, exit_code: Option<i64>, detail: Option<&str>) -> ServerExit {
    ServerExit { slot_id: slot_id.to_owned(), allocation_id: allocation_id.to_owned(), reason, exit_code, detail: detail.map(str::to_owned) }
}

/// A running game container labelled with its slot, its allocation and the token the slot has.
fn game(id: &str, name: &str, slot_id: Option<&str>, allocation_id: Option<&str>) -> AdoptedContainer {
    AdoptedContainer {
        gslt: Some(if slot_id == Some("b") { GSLT_B } else { GSLT_A }.to_owned()),
        id: id.to_owned(),
        name: name.to_owned(),
        slot_id: slot_id.map(str::to_owned),
        allocation_id: allocation_id.map(str::to_owned),
        match_id: None,
        idle_shutdown_seconds: None,
        relay_key: Some(RELAY_KEY.to_owned()),
        role: Some(ContainerRole::Game),
        running: true,
    }
}

fn code(error: &SlotError) -> Option<AgentErrorCode> {
    match error {
        SlotError::Failure(failure) | SlotError::Refused(failure) => Some(failure.code),
        _ => None,
    }
}

fn grace() -> i64 {
    i64::try_from(STOP_TIMEOUT.as_secs()).unwrap()
}

async fn start(base: &Slots, launch: ServerLaunchConfig, allocation_id: &str) -> Result<StartServerReply, SlotError> {
    launch_server(&base.slots, launch, allocation_id).await
}

fn reply(allocation_id: &str, slot_id: &str) -> StartServerReply {
    StartServerReply { allocation_id: allocation_id.to_owned(), slot_id: slot_id.to_owned() }
}

/// Lets spawned work run until it waits on something.
async fn settled() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn states(base: &Slots) -> Vec<SlotState> {
    base.reported().iter().map(|status| status.state).collect()
}

fn exits(base: &Slots) -> Vec<ServerExit> {
    base.exits().into_iter().map(|(exit, _)| exit).collect()
}

fn stopped(base: &Slots) -> Vec<(String, i64)> {
    base.docker.stopped.lock().unwrap().clone()
}

fn lock_config(base: &Slots) -> std::sync::MutexGuard<'_, ankor_protocol::config::AgentConfig> {
    base.config.lock().unwrap()
}

fn started_ids(base: &Slots) -> Vec<String> {
    base.docker.started.lock().unwrap().iter().map(|started| started.id.clone()).collect()
}

fn last_env(base: &Slots) -> Vec<String> {
    base.docker.specs().last().expect("a started container").env.clone()
}

/// A stop that records its call, waits for the test, and then makes the last container exit with
/// `code`, or leaves it running without one.
fn held_stop(base: &Slots, code: Option<i64>) -> oneshot::Sender<()> {
    let (answer, answered) = oneshot::channel::<()>();
    let docker = Arc::clone(&base.docker);

    base.docker.override_stop(move |_, _| async move {
        let _ = answered.await;

        if code.is_some() {
            docker.exit(docker.last(), code);
        }

        Ok(())
    });

    answer
}

/// A plugin URL that blocks its launch until the test releases it.
struct GatedUrl {
    calls: Arc<Mutex<Vec<(String, String)>>>,
    release: std::sync::mpsc::Sender<String>,
}

fn with_gated_url() -> (Slots, GatedUrl) {
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let (release, released) = std::sync::mpsc::channel::<String>();
    let released = Mutex::new(released);
    let recorded = Arc::clone(&calls);
    let base = make_slots_with(agent_config(&json!({})), move |deps| {
        deps.plugin_url = Arc::new(move |slot, allocation, _| {
            recorded.lock().unwrap().push((slot.to_owned(), allocation.to_owned()));
            // Blocks this worker until the test releases it; tokio first hands the worker's queued
            // tasks to others, or one of them (the test itself) could wait behind the block forever.
            tokio::task::block_in_place(|| released.lock().unwrap().recv().unwrap_or_default())
        });
    });

    (base, GatedUrl { calls, release })
}

mod transitions {
    use super::*;

    /// The agent runs on one thread, but its correctness may not depend on that: `check_games` picks
    /// the idle servers in one hold of the lock and stops them after calling out, so on more threads
    /// the server it picked can leave in between. The stop must leave the next launch on the slot be.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_idle_stop_stops_only_the_server_that_stayed_empty() {
        let (in_poll, polled) = tokio::sync::oneshot::channel::<()>();
        let in_poll = Arc::new(Mutex::new(Some(in_poll)));
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let wait = Arc::new(Mutex::new(wait));
        let base = make_slots_with(agent_config(&json!({})), {
            let (in_poll, wait) = (Arc::clone(&in_poll), Arc::clone(&wait));

            move |deps| {
                deps.poll_plugin = Arc::new(move |_| {
                    if let Some(signal) = in_poll.lock().unwrap().take() {
                        let _ = signal.send(());
                        let _ = wait.lock().unwrap().recv();
                    }
                });
            }
        });
        let (first, second) = (new_allocation(), new_allocation());

        launch_server(&base.slots, launch_config(&json!({ "idle_shutdown_seconds": 60 })), &first).await.unwrap();
        base.slots.plugin_online_changed(&first, true);
        base.slots.plugin_status(&first, &ready_status());
        assert_eq!(base.status("a").state, SlotState::Running);

        let slots = base.slots.clone();
        let later = tokio::time::Instant::now() + Duration::from_hours(1);
        let check = tokio::task::spawn_blocking(move || slots.check_games(later));

        polled.await.unwrap();
        // The first server exits while the check is between its pick and its stop, and the second
        // takes the slot.
        base.docker.exit(0, Some(0));
        launch_server(&base.slots, launch_config(&json!({})), &second).await.unwrap();
        go.send(()).unwrap();
        check.await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let second_container = base.docker.started.lock().unwrap()[1].id.clone();
        let stopped: Vec<String> = base.docker.stopped.lock().unwrap().iter().map(|(id, _)| id.clone()).collect();

        assert!(!stopped.contains(&second_container), "the second launch was stopped as idle: {stopped:?}; exits {:?}", base.exits());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn of_many_launches_of_one_allocation_at_once_one_starts_a_server() {
        let base = base();
        let allocation_id = new_allocation();
        let launches: Vec<_> = (0..50)
            .map(|_| {
                let (slots, allocation_id) = (base.slots.clone(), allocation_id.clone());

                tokio::spawn(async move { launch_server(&slots, launch_config(&json!({})), &allocation_id).await })
            })
            .collect();
        let mut started = 0;

        for launch in launches {
            match launch.await.unwrap() {
                Ok(_) => started += 1,
                Err(error) => assert!(matches!(code(&error), Some(AgentErrorCode::AllocationInUse | AgentErrorCode::AllocationReused)), "{error:?}"),
            }
        }

        assert_eq!(started, 1);
        assert_eq!(started_ids(&base).len(), 1);
    }

    #[tokio::test]
    async fn reports_the_exit_and_the_idle_status_before_the_stop_returns() {
        let reported: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&reported);
        let base = make_slots_with(agent_config(&json!({})), move |deps| {
            deps.report = Some(Arc::new(move |event| {
                let label = match event {
                    SlotEvent::ServerExited(_) => "exited".to_owned(),
                    SlotEvent::SlotState(status) => format!("{:?}", status.state),
                    SlotEvent::SlotIdle { allocation_id, .. } => format!("idle {}", allocation_id.is_some()),
                    _ => return,
                };

                seen.lock().unwrap().push(label);
            }));
        });
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        reported.lock().unwrap().clear();
        base.slots.stop_allocation(&allocation_id).await.unwrap();

        // Nothing ran between the change and the report: no task had to pass the events on.
        assert_eq!(*reported.lock().unwrap(), ["Stopping", "exited", "Idle", "idle true"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persists_the_relay_route_before_launching_and_passes_its_local_url() {
        for launch in [launch_config(&json!({})), server_launch_config(&json!({}))] {
            let (base, url) = with_gated_url();
            let allocation_id = new_allocation();
            let manager = base.slots.clone();
            let id = allocation_id.clone();
            let starting = tokio::spawn(async move { launch_server(&manager, launch, &id).await });

            until("the relay route", || !url.calls.lock().unwrap().is_empty()).await;
            assert_eq!(url.calls.lock().unwrap()[0], ("a".to_owned(), allocation_id.clone()));
            assert!(base.docker.started.lock().unwrap().is_empty());
            url.release.send(PLUGIN_URL.to_owned()).unwrap();
            starting.await.unwrap().unwrap();
            assert!(last_env(&base).contains(&format!("{}={PLUGIN_URL}", ankor_env::WS_URL)));
        }
    }

    #[tokio::test]
    async fn keeps_the_launchs_match_when_the_plugin_reports_none() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[PLAYER_A], None, true));

        let status = base.status("a");

        assert_eq!(status.match_id.as_deref(), Some(MATCH_ID));
        assert_eq!(status.state, SlotState::Running);
    }

    #[tokio::test]
    async fn walks_idle_starting_running_stopping_idle() {
        let base = base();

        assert_eq!(base.status("a").state, SlotState::Idle);

        let allocation_id = new_allocation();
        let started = start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();

        assert_eq!(started, reply(&allocation_id, "a"));
        assert_eq!(started_ids(&base), ["container0"]);
        assert_eq!(base.status("a").state, SlotState::Starting);
        assert_eq!(base.status("a").slot_id, "a");

        base.slots.plugin_status(&allocation_id, &status(&[], None, false));
        assert_eq!(base.status("a").state, SlotState::Starting);

        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        assert_eq!(base.status("a").state, SlotState::Running);

        base.slots.stop_allocation(&allocation_id).await.unwrap();
        assert_eq!(stopped(&base), [("container0".to_owned(), grace())]);
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.status("a").match_id, None);

        assert_eq!(states(&base), [SlotState::Starting, SlotState::Starting, SlotState::Running, SlotState::Stopping, SlotState::Idle]);
        assert!(base.reported().iter().all(|status| status.slot_id == "a"));
    }

    /// Once a stop was asked for, the server stops: a stop Docker refuses leaves the slot
    /// `stopping`, and the next check tries it again until the server is gone.
    #[tokio::test]
    async fn keeps_stopping_a_server_whose_stop_docker_refused_until_it_is_gone() {
        let base = base();
        let (running, starting) = (new_allocation(), new_allocation());

        start(&base, launch_config(&json!({})), &running).await.unwrap();
        base.slots.plugin_status(&running, &status(&[], None, true));
        start(&base, launch_config(&json!({})), &starting).await.unwrap();
        *base.docker.stop_error.lock().unwrap() = Some("Docker unavailable".to_owned());

        assert!(base.slots.stop_allocation(&running).await.is_err());
        assert!(base.slots.stop_allocation(&starting).await.is_err());
        assert_eq!((base.status("a").state, base.status("b").state), (SlotState::Stopping, SlotState::Stopping));

        // Still refused: the next check tries again, and the slots stay as they are.
        base.slots.check_games(Instant::now());
        until("the retries", || stopped(&base).len() == 4).await;
        settled().await;
        assert_eq!((base.status("a").state, base.status("b").state), (SlotState::Stopping, SlotState::Stopping));

        *base.docker.stop_error.lock().unwrap() = None;
        base.slots.check_games(Instant::now());
        until("both stopped", || base.status("a").state == SlotState::Idle && base.status("b").state == SlotState::Idle).await;

        let ended = exits(&base);

        assert_eq!(ended.len(), 2);
        assert!(ended.iter().all(|exit| exit.reason == ServerExitReason::Requested), "{ended:?}");
    }

    /// A stop that joins one under way waits for the same exit; Docker refusing one of them changes
    /// nothing, since the other was accepted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_stop_leaves_the_slot_to_a_concurrent_stop_docker_accepted() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));

        let (refuse, refused) = tokio::sync::oneshot::channel::<()>();
        let (accept, accepted) = tokio::sync::oneshot::channel::<()>();
        let (docker, container) = (Arc::clone(&base.docker), base.docker.last());

        base.docker.override_stop(move |_, _| async move {
            let _ = refused.await;
            Err(DockerError::Http("Docker unavailable".to_owned()))
        });
        base.docker.override_stop(move |_, _| async move {
            let _ = accepted.await;
            docker.exit(container, Some(0));
            Ok(())
        });

        let stop = |base: &Slots| {
            let (slots, allocation_id) = (base.slots.clone(), allocation_id.clone());
            tokio::spawn(async move { slots.stop_allocation(&allocation_id).await })
        };
        let first = stop(&base);
        until("the first stop", || stopped(&base).len() == 1).await;
        let second = stop(&base);
        until("the second stop", || stopped(&base).len() == 2).await;

        // The accepted stop is still waiting for Docker, so a check starts no retry.
        refuse.send(()).unwrap();
        assert!(first.await.unwrap().is_err());
        base.slots.check_games(Instant::now());
        settled().await;
        assert_eq!(stopped(&base).len(), 2);
        assert_eq!(base.status("a").state, SlotState::Stopping);

        accept.send(()).unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(states(&base), [SlotState::Starting, SlotState::Starting, SlotState::Running, SlotState::Stopping, SlotState::Idle]);
    }

    /// Docker's answer can come after the exit settled the slot; an error then is no failure, as
    /// the server is gone.
    #[tokio::test]
    async fn answers_a_stop_that_docker_errored_on_after_the_server_exited_as_done() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));

        let (docker, container) = (Arc::clone(&base.docker), base.docker.last());

        base.docker.override_stop(move |_, _| async move {
            docker.exit(container, Some(0));
            Err(DockerError::Http("the stop timed out".to_owned()))
        });
        base.slots.stop_allocation(&allocation_id).await.unwrap();
        assert_eq!(base.status("a").state, SlotState::Idle);
    }

    #[tokio::test]
    async fn prepares_the_per_slot_shadow_tree_and_launch_configuration() {
        let base = base();

        start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap();

        let calls = base.files.calls.lock().unwrap().clone();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].slot_id, "a");
        assert_eq!(
            calls[0].cfg_files.iter().map(|file| file.rel_path.as_str()).collect::<Vec<_>>(),
            [
                "game/csgo/cfg/ankor_launch.cfg",
                "game/csgo/addons/swiftlys2/configs/plugins/Ankor/config.jsonc",
                "game/csgo/ankor/match.json",
                "game/csgo/ankor/restore.txt",
            ]
        );
    }

    #[tokio::test]
    async fn mounts_the_addon_overlay_it_resolved_for_the_launch_and_labels_the_container_with_it() {
        let base = base();

        start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap();

        let spec = base.docker.specs().last().unwrap().clone();

        // Consulted per launch, as the image id may have moved since the agent booted.
        assert_eq!(*base.overlay.calls.lock().unwrap(), 1);
        assert!(spec.binds.contains(&BindMount {
            source: "/srv/ankor/overlay/sha256_image/addons".to_owned(),
            target: "/home/steam/cs2-dedicated/game/csgo/addons".to_owned(),
            read_only: true,
        }));
        assert!(spec.binds.contains(&BindMount {
            source: "/srv/ankor/overlay/sha256_image/cfg/ankor".to_owned(),
            target: "/home/steam/cs2-dedicated/game/csgo/cfg/ankor".to_owned(),
            read_only: true,
        }));
        assert_eq!(spec.labels["ankor.overlay"], "sha256:image");
    }

    #[tokio::test]
    async fn refuses_the_launch_when_the_overlay_cannot_be_produced_and_leaves_the_slot_idle() {
        let base = base();

        *base.overlay.error.lock().unwrap() = Some("ghcr.io/example/ankor-cs2:latest is not on this host".to_owned());

        let error = start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err();

        assert!(error.to_string().contains("not on this host"), "{error}");
        assert!(base.docker.started.lock().unwrap().is_empty());
        assert_eq!(base.status("a").state, SlotState::Idle);
    }

    #[tokio::test]
    async fn flips_to_idle_when_the_container_exits_on_its_own() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        assert_eq!(base.status("a").state, SlotState::Running);
        assert_eq!(base.overlay.held(), 1);

        base.docker.exit(base.docker.last(), Some(0));
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.overlay.held(), 0);
    }

    /// An exit the watch of an earlier container reports late belongs to a server that is gone, so
    /// the next server in the slot keeps running and the late exit is not logged.
    #[tokio::test]
    async fn leaves_the_next_server_alone_when_an_earlier_containers_exit_comes_again() {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let exits_logged = Arc::clone(&lines);
        let base = make_slots_with(agent_config(&json!({})), move |deps| {
            deps.log = Logger::new(
                "agent",
                Some(Arc::new(move |entry: Entry| {
                    if entry.message.contains("container exited") {
                        exits_logged.lock().unwrap().push(entry.message);
                    }
                })),
            );
        });
        let first = new_allocation();

        start(&base, launch_config(&json!({})), &first).await.unwrap();
        base.docker.exit(base.docker.last(), Some(0));
        assert_eq!(base.status("a").state, SlotState::Idle);

        let next = new_allocation();

        start(&base, launch_config(&json!({})), &next).await.unwrap();
        base.slots.plugin_status(&next, &status(&[], None, true));
        base.docker.exit(0, Some(0));
        assert_eq!((base.status("a").state, base.status("a").allocation_id), (SlotState::Running, Some(next)));
        assert_eq!(exits(&base).len(), 1);
        assert_eq!(lines.lock().unwrap().len(), 1);
    }

    /// The server's Steam ID goes out in the `slot_status` of the ready status that first carries it, and a
    /// later status that repeats it sends nothing new. A running server whose status gains it, as
    /// an adopted one does at its first poll, reports it then.
    #[tokio::test]
    async fn reports_the_servers_steam_id_once_its_status_first_carries_it() {
        const STEAM_ID: &str = "85568392936840254";
        let with_id = |ready| ServerStatus { server_steam_id: Some(STEAM_ID.to_owned()), ..status(&[], None, ready) };

        let base = base();
        let first = new_allocation();

        start(&base, launch_config(&json!({})), &first).await.unwrap();
        base.slots.plugin_status(&first, &with_id(true));

        let reported = base.reported();
        let last = reported.last().unwrap();

        assert_eq!((last.state, last.server_steam_id.as_deref()), (SlotState::Running, Some(STEAM_ID)));
        assert_eq!(reported.iter().filter(|status| status.server_steam_id.is_some()).count(), 1);

        base.slots.plugin_status(&first, &with_id(true));
        assert_eq!(base.reported().len(), reported.len());

        base.docker.exit(base.docker.last(), Some(0));
        assert_eq!(base.reported().last().unwrap().server_steam_id, None);

        let next = new_allocation();

        start(&base, launch_config(&json!({})), &next).await.unwrap();
        base.slots.plugin_status(&next, &status(&[], None, true));
        assert_eq!(base.reported().last().unwrap().server_steam_id, None);

        let before = base.reported().len();

        base.slots.plugin_status(&next, &with_id(true));
        assert_eq!(base.reported().len(), before + 1);
        assert_eq!(base.reported().last().unwrap().server_steam_id.as_deref(), Some(STEAM_ID));
    }

    /// Docker's error can come after the next server took the slot. It belongs to the stop of the
    /// server that exited, so the stop still answers as done.
    #[tokio::test]
    async fn answers_a_stop_that_docker_errored_on_as_done_once_the_next_server_holds_the_slot() {
        let base = base();
        let first = new_allocation();

        start(&base, launch_config(&json!({})), &first).await.unwrap();
        base.slots.plugin_status(&first, &status(&[], None, true));

        let (docker, container) = (Arc::clone(&base.docker), base.docker.last());
        let (answer, answered) = oneshot::channel::<()>();

        base.docker.override_stop(move |_, _| async move {
            docker.exit(container, Some(0));
            let _ = answered.await;
            Err(DockerError::Http("the stop timed out".to_owned()))
        });

        let stopping = {
            let (slots, first) = (base.slots.clone(), first.clone());

            tokio::spawn(async move { slots.stop_allocation(&first).await.map_err(|error| error.to_string()) })
        };

        until("the exit", || base.status("a").state == SlotState::Idle).await;

        let next = new_allocation();

        start(&base, launch_config(&json!({})), &next).await.unwrap();
        answer.send(()).unwrap();
        assert_eq!(stopping.await.unwrap(), Ok(()));
        assert_eq!(base.status("a").allocation_id, Some(next));
    }

    #[tokio::test]
    async fn tells_the_relay_and_the_demo_scan_once_when_a_stop_returns_before_docker_reports_the_exit() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.stop_allocation(&allocation_id).await.unwrap();
        // The exit Docker reports afterwards belongs to a container the slot has already let go of.
        base.docker.exit(base.docker.last(), Some(0));

        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.idled(), ["a"]);
    }

    #[tokio::test]
    async fn refuses_a_launch_with_shutting_down_once_shutdown_began_and_starts_nothing() {
        let base = base();

        base.stop.cancel();

        assert_eq!(code(&start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err()), Some(AgentErrorCode::ShuttingDown));
        assert!(base.docker.started.lock().unwrap().is_empty());
        assert_eq!(base.status("a").state, SlotState::Idle);
    }

    #[tokio::test]
    async fn takes_the_next_free_slot_and_refuses_with_no_free_slot_once_every_slot_is_in_use() {
        let base = base();

        assert_eq!(start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap().slot_id, "a");
        assert_eq!(start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap().slot_id, "b");
        assert_eq!(code(&start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err()), Some(AgentErrorCode::NoFreeSlot));
        assert_eq!(base.docker.started.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn passes_over_a_slot_whose_token_steam_refused_and_logs_each_game_in_with_its_own_slots_token() {
        let base = base();

        base.slots.set_gslt_state("a", GSLT_A, GsltState::Invalid);
        assert_eq!(start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap().slot_id, "b");
        assert!(last_env(&base).contains(&format!("SRCDS_TOKEN={GSLT_B}")));
        assert_eq!(code(&start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err()), Some(AgentErrorCode::NoFreeSlot));
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.status("a").gslt_state, GsltState::Invalid);

        let frames = base.reported().len();

        base.slots.set_gslt_state("a", GSLT_A, GsltState::Invalid);
        base.slots.set_gslt_state("zz", GSLT_A, GsltState::Ok);
        // A state about a token the slot no longer has, as a set_config swapped it during the check.
        base.slots.set_gslt_state("b", GSLT_A, GsltState::Invalid);
        assert_eq!(base.reported().len(), frames);
        assert_eq!(base.status("b").gslt_state, GsltState::Unchecked);
    }

    #[tokio::test]
    async fn a_logon_hold_dropped_unanswered_gives_the_slot_back() {
        let base = base();
        let hold = base.slots.hold_for_logon("a", GSLT_A).unwrap();

        assert!(base.status("a").reserved);
        drop(hold);
        assert!(!base.status("a").reserved);
    }

    #[tokio::test]
    async fn keeps_a_reserved_slot_from_launches_until_it_gets_a_new_token() {
        let base = base();

        base.slots.reserve("a").unwrap();
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert!(base.status("a").reserved);
        assert_eq!(start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap().slot_id, "b");
        assert_eq!(code(&start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err()), Some(AgentErrorCode::NoFreeSlot));

        change_layout(&base.slots, &[slot("a", 27015, 27020, REPLACEMENT), slot("b", 27016, 27021, GSLT_B)]).unwrap();

        assert!(!base.status("a").reserved);
        assert_eq!(base.status("a").gslt_state, GsltState::Unchecked);
        base.slots.hold_for_logon("a", GSLT_A).unwrap().answer(None);
        assert!(!base.status("a").reserved);
        assert_eq!(start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap().slot_id, "a");
        assert!(last_env(&base).contains(&format!("SRCDS_TOKEN={REPLACEMENT}")));
    }

    #[tokio::test]
    async fn keeps_a_slot_reserved_through_a_new_token_while_the_token_before_it_is_still_logging_on() {
        let base = base();
        let release = base.slots.hold_for_logon("a", GSLT_A).unwrap();
        change_layout(&base.slots, &[slot("a", 27015, 27020, REPLACEMENT), slot("b", 27016, 27021, GSLT_B)]).unwrap();

        assert!(base.status("a").reserved);
        assert_eq!(base.status("a").gslt_state, GsltState::Unchecked);
        release.answer(None);
        assert!(!base.status("a").reserved);
    }

    /// A token that `set_config` moves to another slot while it logs on keeps that slot from launches,
    /// as a server logging on with it meanwhile would be logged off.
    #[tokio::test]
    async fn keeps_a_launch_off_the_slot_a_token_moved_to_while_it_logs_on() {
        let base = base();
        let release = base.slots.hold_for_logon("a", GSLT_A).unwrap();
        change_layout(&base.slots, &[slot("a", 27015, 27020, REPLACEMENT), slot("b", 27016, 27021, GSLT_A)]).unwrap();

        assert_eq!(code(&start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err()), Some(AgentErrorCode::NoFreeSlot));
        release.answer(None);
        assert_eq!(start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap().slot_id, "a");
    }

    #[tokio::test]
    async fn keeps_a_reservation_through_an_edit_that_leaves_the_slots_token_alone() {
        let base = base();

        base.slots.reserve("a").unwrap();

        change_layout(&base.slots, &[slot("a", 27015, 27020, GSLT_A), slot("b", 27016, 27021, GSLT_B)]).unwrap();

        assert!(base.status("a").reserved);
    }

    #[tokio::test]
    async fn reports_nothing_while_it_applies_a_layout_then_announces_the_slots_it_gave_a_new_token() {
        let base = base();
        let before = base.reported().len();
        let applied = change_layout(&base.slots, &[slot("a", 27015, 27020, REPLACEMENT), slot("c", 27040, 27041, GSLT_B)]).unwrap();

        assert_eq!(applied, ["a", "c"]);
        assert_eq!(base.removed(), ["b"]);
        assert!(base.reported()[before..].is_empty());

        base.slots.announce(&applied);

        let announced: Vec<(String, GsltState, bool)> =
            base.reported()[before..].iter().map(|status| (status.slot_id.clone(), status.gslt_state, status.reserved)).collect();

        assert_eq!(announced, [("a".to_owned(), GsltState::Unchecked, false), ("c".to_owned(), GsltState::Unchecked, false)]);
    }

    #[tokio::test]
    async fn reports_nothing_for_a_slot_set_config_removed_while_its_token_was_logging_on() {
        let base = base();
        let release = base.slots.hold_for_logon("b", GSLT_B).unwrap();
        change_layout(&base.slots, &[slot("a", 27015, 27020, GSLT_A)]).unwrap();
        let before = base.reported().len();

        release.answer(None);
        assert!(base.reported()[before..].is_empty());
    }

    #[tokio::test]
    async fn keeps_a_removed_slots_logon_from_releasing_the_slot_that_took_its_id() {
        let base = base();
        let slot_a = slot("a", 27015, 27020, GSLT_A);
        let release = base.slots.hold_for_logon("b", GSLT_B).unwrap();
        change_layout(&base.slots, std::slice::from_ref(&slot_a)).unwrap();
        change_layout(&base.slots, &[slot_a, slot("b", 27016, 27021, GSLT_B)]).unwrap();
        let release_new = base.slots.hold_for_logon("b", GSLT_B).unwrap();
        let before = base.reported().len();

        release.answer(None);
        assert!(base.reported()[before..].is_empty());
        assert!(base.status("b").reserved);
        release_new.answer(None);
        assert!(!base.status("b").reserved);
        assert_eq!(base.status("b").gslt_state, GsltState::Unchecked);
    }

    #[tokio::test(start_paused = true)]
    async fn ends_a_reservation_no_new_token_followed_and_restarts_the_wait_when_asked_again() {
        let base = base();
        let almost = SLOT_RESERVATION.checked_sub(Duration::from_secs(1)).unwrap();

        base.slots.reserve("a").unwrap();
        settled().await;
        tokio::time::advance(almost).await;
        base.slots.reserve("a").unwrap();
        settled().await;
        tokio::time::advance(almost).await;
        settled().await;
        assert!(base.status("a").reserved);
        tokio::time::advance(Duration::from_secs(1)).await;
        settled().await;
        assert!(!base.status("a").reserved);
        // Reserving again changes nothing on the wire: one report each way.
        assert_eq!(base.reported().iter().filter(|status| status.slot_id == "a").map(|status| status.reserved).collect::<Vec<_>>(), [true, false]);
    }

    #[tokio::test]
    async fn settles_a_reservation_and_a_launch_that_arrive_together_on_different_slots_whichever_comes_first() {
        let base = base();
        let launched = new_allocation();
        let manager = base.slots.clone();
        let id = launched.clone();
        let launching = tokio::spawn(async move { launch_server(&manager, launch_config(&json!({})), &id).await });

        until("the launch picking slot a", || base.status("a").state == SlotState::Starting).await;
        assert_eq!(code(&base.slots.reserve("a").unwrap_err()), Some(AgentErrorCode::SlotHeld));
        base.slots.reserve("b").unwrap();
        assert_eq!(launching.await.unwrap().unwrap(), reply(&launched, "a"));
        assert_eq!(code(&start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err()), Some(AgentErrorCode::NoFreeSlot));
        assert_eq!(base.status("b").state, SlotState::Idle);
        assert!(base.status("b").reserved);
    }

    #[tokio::test]
    async fn refuses_to_reserve_a_slot_holding_a_server_or_one_the_node_does_not_have() {
        let base = base();

        start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap();

        let held = base.slots.reserve("a").unwrap_err();
        let unknown = base.slots.reserve("zz").unwrap_err();

        assert_eq!(code(&held), Some(AgentErrorCode::SlotHeld));
        assert!(held.to_string().contains("slot a"), "{held}");
        assert_eq!(code(&unknown), Some(AgentErrorCode::UnknownSlot));
        assert!(unknown.to_string().contains("zz"), "{unknown}");
        assert!(!base.status("a").reserved);

        // No logon goes out with a token a server runs with.
        assert!(base.slots.hold_for_logon("a", GSLT_A).is_none());
        assert!(base.slots.hold_for_logon("b", GSLT_A).is_none());

        // A logon on a slot holding a server holds nothing; its release still carries Steam's answer
        // for the token the slot was given while its server runs.
        change_layout(&base.slots, &[slot("a", 27015, 27020, REPLACEMENT), slot("b", 27016, 27021, GSLT_B)]).unwrap();
        let release = base.slots.hold_for_logon("a", REPLACEMENT).unwrap();

        assert!(!base.status("a").reserved);
        release.answer(None);
        assert_eq!(base.status("a").gslt_state, GsltState::Unchecked);
        base.slots.hold_for_logon("a", REPLACEMENT).unwrap().answer(Some(GsltState::Invalid));
        assert!(!base.status("a").reserved);
        assert_eq!(base.status("a").gslt_state, GsltState::Invalid);
        base.slots.hold_for_logon("zz", REPLACEMENT).unwrap().answer(Some(GsltState::Ok));

        // Steam's answer about a token the slot no longer has leaves the slot alone.
        base.slots.hold_for_logon("b", "DDDDDDDDDDDDDDDDDDDDDDDDDDDD0004").unwrap().answer(Some(GsltState::Invalid));
        assert_eq!(base.status("b").gslt_state, GsltState::Unchecked);
    }

    #[tokio::test]
    async fn gives_two_starts_that_arrive_together_different_slots() {
        let base = base();
        let allocations = [new_allocation(), new_allocation(), new_allocation()];
        let (first, second, third) = tokio::join!(
            start(&base, launch_config(&json!({})), &allocations[0]),
            start(&base, launch_config(&json!({})), &allocations[1]),
            start(&base, launch_config(&json!({})), &allocations[2]),
        );

        assert_eq!(code(&third.unwrap_err()), Some(AgentErrorCode::NoFreeSlot));
        assert_eq!([first.unwrap().slot_id, second.unwrap().slot_id], ["a", "b"]);
        assert_eq!(base.docker.started.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn answers_a_stop_once_the_exit_watch_reports_the_game_gone_with_its_exit_code() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();

        // The stop's answer carries no exit code; only the watch does, whenever its answer lands.
        let docker = Arc::clone(&base.docker);

        base.docker.override_stop(move |_, _| async move {
            tokio::spawn(async move { docker.exit(docker.last(), Some(137)) });
            Ok(())
        });
        base.slots.stop_allocation(&allocation_id).await.unwrap();
        settled().await;

        assert_eq!(exits(&base), [exit("a", &allocation_id, ServerExitReason::Requested, Some(137), None)]);
        assert_eq!(base.status("a").state, SlotState::Idle);
    }

    #[tokio::test]
    async fn frees_the_slot_of_a_launch_docker_failed_whatever_the_failure_for_the_next_launch() {
        let base = base();
        let first = new_allocation();
        let next = new_allocation();

        *base.docker.start_error.lock().unwrap() = Some("connection reset".to_owned());

        let error = start(&base, launch_config(&json!({})), &first).await.unwrap_err();

        assert!(error.to_string().contains("connection reset"), "{error}");
        assert_eq!(exits(&base), [exit("a", &first, ServerExitReason::LaunchFailed, None, Some("connection reset"))]);
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.status("a").allocation_id, None);
        assert_eq!(base.overlay.held(), 0);
        assert!(matches!(base.slots.stop_allocation(&first).await, Err(SlotError::Failure(failure)) if failure.code == AgentErrorCode::UnknownAllocation));
        *base.docker.start_error.lock().unwrap() = None;
        start(&base, launch_config(&json!({})), &next).await.unwrap();
        assert_eq!(base.status("a").allocation_id.as_deref(), Some(next.as_str()));
    }

    #[tokio::test]
    async fn returns_to_idle_when_docker_refused_the_start_and_removed_the_container() {
        let base = base();
        let failed = new_allocation();

        // What a bind onto a missing mountpoint looks like: runc refuses, the daemon removes.
        *base.docker.start_error.lock().unwrap() = Some("error mounting demos: no such file or directory".to_owned());

        let error = start(&base, launch_config(&json!({})), &failed).await.unwrap_err();

        assert!(error.to_string().contains("error mounting demos"), "{error}");
        // Docker's message says why the launch failed.
        assert_eq!(exits(&base), [exit("a", &failed, ServerExitReason::LaunchFailed, None, Some("error mounting demos: no such file or directory"))]);
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.status("a").allocation_id, None);
        assert!(!base.slots.any_busy());
        assert_eq!(base.overlay.held(), 0);
    }

    #[tokio::test]
    async fn stays_idle_when_the_container_exited_before_docker_reported_its_start_as_failed() {
        let base = base();

        *base.docker.exit_on_start.lock().unwrap() = Some((Vec::new(), Some(1)));
        *base.docker.fail_after_start.lock().unwrap() = Some("connection reset".to_owned());

        let error = start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err();

        assert!(error.to_string().contains("connection reset"), "{error}");
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.status("a").allocation_id, None);
        assert_eq!(base.idled(), ["a"]);

        // A held slot naming no allocation is a frame the controller refuses.
        let refused: Vec<SlotStatus> =
            base.reported().into_iter().filter(|status| safe_parse(s_slot_status, serde_json::to_value(status).unwrap()).is_err()).collect();

        assert_eq!(refused, []);
    }

    #[tokio::test]
    async fn reports_a_launch_whose_container_exited_while_docker_started_it_idle_once() {
        let base = base();
        let allocation_id = new_allocation();

        *base.docker.exit_on_start.lock().unwrap() = Some((Vec::new(), Some(1)));

        assert_eq!(start(&base, launch_config(&json!({})), &allocation_id).await.unwrap(), reply(&allocation_id, "a"));
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(states(&base), [SlotState::Starting, SlotState::Idle]);
        assert_eq!(exits(&base), [exit("a", &allocation_id, ServerExitReason::Crashed, Some(1), None)]);
    }

    #[tokio::test]
    async fn releases_the_overlay_when_preparing_launch_files_fails() {
        let base = base();

        *base.files.error.lock().unwrap() = Some("disk full".to_owned());

        let error = start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap_err();

        assert!(error.to_string().contains("disk full"), "{error}");
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.overlay.held(), 0);
    }

    #[tokio::test]
    async fn labels_allocations_and_matches_and_rejects_a_second_start_of_the_same_allocation() {
        let base = base();
        let allocation_id = new_allocation();
        let mut pending = std::pin::pin!(start(&base, launch_config(&json!({})), &allocation_id));

        assert!(poll!(pending.as_mut()).is_pending());

        // A second start while the first launches, and after it: the server is on its way, or the allocation is used up.
        let in_progress = start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err();

        assert_eq!(code(&in_progress), Some(AgentErrorCode::AllocationInUse));
        assert!(in_progress.to_string().contains("launch in progress"), "{in_progress}");
        pending.await.unwrap();

        let reused = start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err();

        assert_eq!(code(&reused), Some(AgentErrorCode::AllocationReused));
        assert!(reused.to_string().contains(&allocation_id), "{reused}");
        assert_eq!(base.docker.started.lock().unwrap().len(), 1);

        let spec_labels = base.docker.specs().last().unwrap().labels.clone();

        assert_eq!(spec_labels[labels::ALLOCATION], allocation_id);
        assert_eq!(spec_labels["ankor.match"], MATCH_ID);
        assert_eq!(base.status("a").allocation_id.as_deref(), Some(allocation_id.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ends_the_original_launch_after_its_current_step_when_stopped_after_rejecting_a_duplicate() {
        let (base, url) = with_gated_url();
        let allocation_id = new_allocation();
        let manager = base.slots.clone();
        let id = allocation_id.clone();
        let starting = tokio::spawn(async move { launch_server(&manager, launch_config(&json!({})), &id).await });

        until("the relay route", || !url.calls.lock().unwrap().is_empty()).await;
        assert_eq!(code(&start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err()), Some(AgentErrorCode::AllocationInUse));
        assert_eq!(url.calls.lock().unwrap().len(), 1);
        assert!(base.docker.started.lock().unwrap().is_empty());

        let mut stopping = std::pin::pin!(base.slots.stop_allocation(&allocation_id));

        assert!(poll!(stopping.as_mut()).is_pending());
        url.release.send(PLUGIN_URL.to_owned()).unwrap();

        let error = starting.await.unwrap().unwrap_err();

        assert!(error.to_string().contains("stopped before its server started"), "{error}");
        stopping.await.unwrap();

        // No later step ran: no overlay, no files, no container.
        assert_eq!(*base.overlay.calls.lock().unwrap(), 0);
        assert!(base.docker.started.lock().unwrap().is_empty());
        assert!(stopped(&base).is_empty());
        assert_eq!(states(&base), [SlotState::Starting, SlotState::Idle]);
        assert_eq!(exits(&base), [exit("a", &allocation_id, ServerExitReason::Requested, None, None)]);
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.status("a").allocation_id, None);
        assert_eq!(base.overlay.held(), 0);
    }

    #[tokio::test]
    async fn releases_the_overlay_a_launch_stopped_after_acquiring_it_and_starts_no_container() {
        let base = base();

        base.overlay.close();

        let allocation_id = new_allocation();
        let manager = base.slots.clone();
        let id = allocation_id.clone();
        let starting = tokio::spawn(async move { launch_server(&manager, launch_config(&json!({})), &id).await });

        until("the overlay acquire", || *base.overlay.calls.lock().unwrap() == 1).await;

        let mut stopping = std::pin::pin!(base.slots.stop_allocation(&allocation_id));

        assert!(poll!(stopping.as_mut()).is_pending());
        base.overlay.gate.add_permits(1);

        let error = starting.await.unwrap().unwrap_err();

        assert!(error.to_string().contains("stopped before its server started"), "{error}");
        stopping.await.unwrap();

        assert_eq!(base.overlay.held(), 0);
        assert!(base.files.calls.lock().unwrap().is_empty());
        assert!(base.docker.started.lock().unwrap().is_empty());
        assert_eq!(exits(&base), [exit("a", &allocation_id, ServerExitReason::Requested, None, None)]);
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert_eq!(base.status("a").allocation_id, None);
    }

    #[tokio::test]
    async fn a_check_during_a_launch_takes_no_stop_that_waits_for_it_as_failed() {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let warnings = Arc::clone(&lines);
        let base = make_slots_with(agent_config(&json!({})), move |deps| {
            deps.log = Logger::new(
                "agent",
                Some(Arc::new(move |entry: Entry| {
                    if entry.level == Level::Warn {
                        warnings.lock().unwrap().push(entry.message);
                    }
                })),
            );
        });

        base.overlay.close();

        let allocation_id = new_allocation();
        let manager = base.slots.clone();
        let id = allocation_id.clone();
        let starting = tokio::spawn(async move { launch_server(&manager, launch_config(&json!({})), &id).await });

        until("the overlay acquire", || *base.overlay.calls.lock().unwrap() == 1).await;

        let mut stopping = std::pin::pin!(base.slots.stop_allocation(&allocation_id));

        assert!(poll!(stopping.as_mut()).is_pending());
        base.slots.check_games(Instant::now());
        base.overlay.gate.add_permits(1);
        starting.await.unwrap().unwrap_err();
        stopping.await.unwrap();

        for _ in 0..20 {
            tokio::task::yield_now().await;
        }

        assert!(lines.lock().unwrap().iter().all(|line| !line.contains("stop failed")), "{:?}", lines.lock().unwrap());
    }

    #[tokio::test]
    async fn stops_a_container_that_started_while_the_stop_waited_with_the_stops_grace_and_no_start_timeout() {
        let base = base();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));

        *base.docker.start_gate.lock().unwrap() = Some(Arc::clone(&gate));

        let allocation_id = new_allocation();
        let manager = base.slots.clone();
        let id = allocation_id.clone();
        let starting = tokio::spawn(async move { launch_server(&manager, launch_config(&json!({})), &id).await });

        until("the container start", || *base.docker.start_calls.lock().unwrap() == 1).await;

        let manager = base.slots.clone();
        let id = allocation_id.clone();
        let stopping = tokio::spawn(async move { manager.stop_allocation(&id).await });

        settled().await;
        gate.add_permits(1);
        assert_eq!(starting.await.unwrap().unwrap(), reply(&allocation_id, "a"));
        until("the stop", || stopped(&base) == [("container0".to_owned(), grace())]).await;
        base.docker.exit(0, Some(0));
        stopping.await.unwrap().unwrap();

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (allocation_id.as_str(), ServerExitReason::Requested));
        assert_eq!(base.overlay.held(), 0);
    }

    #[tokio::test]
    async fn rejects_an_ended_allocation_without_stopping_its_replacement() {
        let base = base();
        let first = new_allocation();
        let next = new_allocation();

        start(&base, launch_config(&json!({})), &first).await.unwrap();
        base.docker.exit(base.docker.last(), Some(1));
        start(&base, launch_config(&json!({})), &next).await.unwrap();
        assert!(matches!(base.slots.stop_allocation(&first).await, Err(SlotError::Failure(failure)) if failure.code == AgentErrorCode::UnknownAllocation));
        assert!(stopped(&base).is_empty());
        base.docker.override_stop(|_, _| async { Err(DockerError::Http("Docker unavailable".to_owned())) });
        assert!(base.slots.stop_allocation(&next).await.unwrap_err().to_string().contains("Docker unavailable"));
        assert_eq!(base.status("a").state, SlotState::Stopping);
        assert_eq!(base.status("a").allocation_id.as_deref(), Some(next.as_str()));
    }

    #[tokio::test]
    async fn ignores_a_stale_exit_callback_from_a_previous_launch() {
        let base = base();

        start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap();

        let first = base.docker.last();

        base.docker.exit(first, Some(0));
        assert_eq!(base.status("a").state, SlotState::Idle);

        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        assert_eq!(base.status("a").state, SlotState::Running);

        // The first container's stream finally reports; it must not touch the new launch.
        base.docker.line(first, "stale");
        base.docker.exit(first, Some(1));
        assert_eq!(base.status("a").state, SlotState::Running);
        assert!(base.logs().is_empty());
    }

    #[tokio::test]
    async fn ignores_old_callbacks_after_removing_and_re_adding_the_slot() {
        for source in ["launch", "adoption"] {
            let config = agent_config(&json!({}));
            let base = make_slots(config.clone());
            let first = new_allocation();
            let next = new_allocation();

            if source == "launch" {
                start(&base, launch_config(&json!({})), &first).await.unwrap();
            } else {
                base.docker
                    .adoptable
                    .lock()
                    .unwrap()
                    .push(AdoptedContainer { match_id: Some(MATCH_ID.to_owned()), ..game("adopted", "ankor-a", Some("a"), Some(&first)) });
                base.slots.adopt().await.unwrap();
            }

            let old = if source == "launch" {
                base.docker.started.lock().unwrap().last().unwrap().hooks.clone()
            } else {
                base.docker.followed.lock().unwrap()[0].hooks.clone()
            };

            base.slots.stop_allocation(&first).await.unwrap();

            let without_a: Vec<ConfiguredSlot> = config.slots.iter().filter(|slot| slot.id != "a").cloned().collect();

            change_layout(&base.slots, &without_a).unwrap();
            change_layout(&base.slots, &config.slots).unwrap();
            start(&base, launch_config(&json!({})), &next).await.unwrap();

            (old.on_line)("Gameserver logged on to Steam".to_owned());
            assert_eq!(base.status("a").state, SlotState::Starting, "{source}");
            assert_eq!(base.status("a").allocation_id.as_deref(), Some(next.as_str()), "{source}");
            assert!(base.logs().is_empty(), "{source}");
            (old.on_exit)(Some(0));
            assert_eq!(base.status("a").state, SlotState::Starting, "{source}");
            assert_eq!(base.status("a").allocation_id.as_deref(), Some(next.as_str()), "{source}");
            assert_eq!(base.overlay.held(), 1, "{source}");
            assert_eq!(base.idled(), ["a"], "{source}");
        }
    }
}

mod plugin_facts {
    use super::*;

    #[tokio::test]
    async fn reports_the_plugin_linking_and_leaving_and_forgets_both_once_the_slot_is_idle() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_online_changed(&allocation_id, true);

        let last = base.reported().last().unwrap().clone();

        assert_eq!((last.allocation_id.as_deref(), last.plugin_online), (Some(allocation_id.as_str()), true));

        let linked = base.reported().len();

        base.slots.plugin_online_changed(&allocation_id, true);
        assert_eq!(base.reported().len(), linked);
        base.slots.plugin_online_changed(&allocation_id, false);

        let last = base.reported().last().unwrap().clone();

        assert_eq!((last.allocation_id.as_deref(), last.plugin_online), (Some(allocation_id.as_str()), false));
        base.slots.plugin_online_changed(&allocation_id, true);
        base.slots.stop_allocation(&allocation_id).await.unwrap();
        assert_eq!(base.status("a").state, SlotState::Idle);
        assert!(!base.status("a").plugin_online);

        // A link report for a game that ended changes nothing.
        let frames = base.reported().len();

        base.slots.plugin_online_changed(&allocation_id, true);
        assert_eq!(base.reported().len(), frames);
    }

    #[tokio::test]
    async fn stays_starting_until_the_first_ready_status_then_runs_and_reports_nothing_for_players_coming_and_going() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();

        let frames = base.reported().len();

        base.slots.plugin_status(&allocation_id, &status(&[], None, false));
        assert_eq!(base.reported().len(), frames);
        assert_eq!(base.status("a").state, SlotState::Starting);
        base.slots.plugin_status(&allocation_id, &status(&[PLAYER_A], None, true));
        assert_eq!(base.reported()[frames..].iter().map(|status| status.state).collect::<Vec<_>>(), [SlotState::Running]);
        base.slots.plugin_status(&allocation_id, &status(&[PLAYER_A, PLAYER_B], None, true));
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.plugin_status(&allocation_id, &status(&[PLAYER_B], None, false));
        base.slots.plugin_status(&new_allocation(), &status(&[], Some("map_name is empty"), true));
        assert_eq!(base.reported().len(), frames + 1);
        assert!(serde_json::to_value(base.reported().last().unwrap()).unwrap().get("player_count").is_none());
    }

    #[tokio::test]
    async fn reports_the_token_a_server_got_onto_steam_with_on_its_first_ready_and_only_then() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, false));
        assert!(base.tokens_used().is_empty());
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.plugin_status(&allocation_id, &status(&[PLAYER_A], None, true));
        assert_eq!(base.tokens_used(), [GSLT_A]);
    }

    #[tokio::test]
    async fn marks_the_slots_token_accepted_on_the_first_ready_unless_the_slot_has_another_token_by_then() {
        let base = base();
        let first = new_allocation();

        start(&base, launch_config(&json!({})), &first).await.unwrap();
        base.slots.plugin_status(&first, &status(&[], None, false));
        assert_eq!(base.status("a").gslt_state, GsltState::Unchecked);
        base.slots.plugin_status(&first, &status(&[], None, true));
        assert_eq!((base.status("a").state, base.status("a").gslt_state), (SlotState::Running, GsltState::Ok));

        let replaced = super::base();
        let second = new_allocation();

        start(&replaced, launch_config(&json!({})), &second).await.unwrap();

        change_layout(&replaced.slots, &[slot("a", 27015, 27020, REPLACEMENT), slot("b", 27016, 27021, GSLT_B)]).unwrap();

        replaced.slots.plugin_status(&second, &status(&[], None, true));
        assert_eq!((replaced.status("a").state, replaced.status("a").gslt_state), (SlotState::Running, GsltState::Unchecked));
    }

    #[tokio::test(start_paused = true)]
    async fn starts_the_idle_clock_only_once_the_plugin_reports_ready() {
        let base = base();
        let allocation_id = new_allocation();
        let launched_at = Instant::now();

        start(&base, server_launch_config(&json!({ "idle_shutdown_seconds": 60 })), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, false));
        base.slots.check_games(launched_at + Duration::from_hours(1));
        settled().await;
        assert!(stopped(&base).is_empty());
        assert_eq!(base.status("a").state, SlotState::Starting);

        // Only the clock is at issue here, so the start timeout does not run out on the way.
        base.stop.cancel();
        tokio::time::advance(Duration::from_hours(1)).await;
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.check_games(launched_at + Duration::from_secs(3659));
        settled().await;
        assert!(stopped(&base).is_empty());
        base.slots.check_games(launched_at + Duration::from_mins(61));
        until("the idle stop", || base.status("a").state == SlotState::Idle).await;
    }

    #[tokio::test]
    async fn stops_a_game_whose_plugin_refused_its_config_and_says_so_on_its_exit() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], Some("map_name is empty"), false));
        base.slots.plugin_status(&allocation_id, &status(&[], Some("map_name is empty"), true));
        until("the stop", || base.status("a").state == SlotState::Idle).await;

        assert_eq!(stopped(&base), [("container0".to_owned(), grace())]);
        assert!(!base.reported().iter().any(|status| status.state == SlotState::Running));
        // The plugin's reason goes with the exit, so the controller learns what to fix in the config it sent.
        assert_eq!(
            base.exits(),
            [(exit("a", &allocation_id, ServerExitReason::ConfigRejected, Some(143), Some("map_name is empty")), Some(SlotState::Stopping))]
        );

        // A reason longer than the contract allows is cut to it.
        let long = new_allocation();
        let max = usize::try_from(SERVER_EXIT_DETAIL_MAX_CHARS).unwrap();

        start(&base, launch_config(&json!({})), &long).await.unwrap();
        base.slots.plugin_status(&long, &status(&[], Some(&"x".repeat(max + 10)), true));
        until("the second exit", || base.exits().len() == 2).await;
        assert_eq!(exits(&base)[1].detail, Some("x".repeat(max)));

        let last = base.reported().last().unwrap().clone();

        assert_eq!((last.state, last.allocation_id), (SlotState::Idle, None));
    }

    #[tokio::test]
    async fn tries_a_stop_of_its_own_again_on_the_next_check_after_docker_refused_it_under_the_same_reason() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.docker.override_stop(|_, _| async { Err(DockerError::Http("Docker unavailable".to_owned())) });
        base.slots.plugin_status(&allocation_id, &status(&[], Some("map_name is empty"), true));
        until("the first stop", || stopped(&base).len() == 1).await;
        settled().await;
        assert_eq!(base.status("a").state, SlotState::Stopping);

        base.slots.check_games(Instant::now());
        until("the second stop", || base.status("a").state == SlotState::Idle).await;
        assert_eq!(stopped(&base).len(), 2);

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!(
            (ended[0].allocation_id.as_str(), ended[0].reason, ended[0].detail.as_deref()),
            (allocation_id.as_str(), ServerExitReason::ConfigRejected, Some("map_name is empty"))
        );
    }

    /// The first reason stays, so the exit names why the server was first stopped.
    #[tokio::test]
    async fn keeps_the_first_reason_when_a_stop_of_its_own_docker_refused_is_asked_for_again() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.docker.override_stop(|_, _| async { Err(DockerError::Http("Docker unavailable".to_owned())) });
        base.slots.plugin_status(&allocation_id, &status(&[], Some("map_name is empty"), true));
        until("the first stop", || stopped(&base).len() == 1).await;
        settled().await;

        base.slots.plugin_status(&allocation_id, &status(&[], Some("team1 is empty"), true));
        until("the second stop", || base.status("a").state == SlotState::Idle).await;

        let ended = exits(&base);

        assert_eq!((ended[0].reason, ended[0].detail.as_deref()), (ServerExitReason::ConfigRejected, Some("map_name is empty")));
    }

    /// A stop of its own already under way takes the next request for one, so Docker is asked once.
    #[tokio::test]
    async fn asks_docker_once_while_a_stop_of_its_own_is_under_way() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));

        let answer = held_stop(&base, Some(0));

        base.slots.plugin_status(&allocation_id, &status(&[], Some("map_name is empty"), true));
        until("the first stop", || stopped(&base).len() == 1).await;
        base.slots.plugin_status(&allocation_id, &status(&[], Some("map_name is empty"), true));
        settled().await;
        assert_eq!(stopped(&base).len(), 1);
        answer.send(()).unwrap();
        until("the exit", || base.status("a").state == SlotState::Idle).await;
    }

    #[tokio::test]
    async fn names_the_reason_of_a_stop_of_its_own_that_docker_refused() {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let warnings = Arc::clone(&lines);
        let base = make_slots_with(agent_config(&json!({})), move |deps| {
            deps.log = Logger::new(
                "agent",
                Some(Arc::new(move |entry: Entry| {
                    if entry.level == Level::Warn {
                        warnings.lock().unwrap().push(entry.message);
                    }
                })),
            );
        });
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.docker.override_stop(|_, _| async { Err(DockerError::Http("Docker unavailable".to_owned())) });
        base.slots.plugin_status(&allocation_id, &status(&[], Some("map_name is empty"), true));
        until("the warning", || lines.lock().unwrap().iter().any(|line| line.contains("config_rejected stop failed"))).await;
        assert!(
            lines.lock().unwrap().contains(&"[agent] slot a: config_rejected stop failed: Docker unavailable; trying again on the next check".to_owned()),
            "{:?}",
            lines.lock().unwrap()
        );
        assert_eq!(lines.lock().unwrap().iter().filter(|line| line.contains("stop failed")).count(), 1, "logged once: {:?}", lines.lock().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn stops_a_game_whose_plugin_never_reports_ready_after_the_start_timeout_and_says_so_on_its_exit() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_online_changed(&allocation_id, true);
        base.slots.plugin_status(&allocation_id, &status(&[], None, false));
        settled().await;
        tokio::time::advance(START_TIMEOUT.checked_sub(Duration::from_millis(1)).unwrap()).await;
        settled().await;
        assert_eq!(base.status("a").state, SlotState::Starting);
        tokio::time::advance(Duration::from_millis(1)).await;
        until("the start timeout", || base.status("a").state == SlotState::Idle).await;

        assert_eq!(stopped(&base), [("container0".to_owned(), 0)]);

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (allocation_id.as_str(), ServerExitReason::StartTimeout));
    }

    #[tokio::test(start_paused = true)]
    async fn neither_stops_a_starting_game_nor_ends_a_reservation_once_shutdown_cancels_its_timers() {
        let base = base();

        start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap();
        base.slots.reserve("b").unwrap();
        settled().await;
        base.stop.cancel();
        tokio::time::advance(START_TIMEOUT.max(SLOT_RESERVATION)).await;
        settled().await;

        // The servers outlive the agent and are adopted by the next one.
        assert!(stopped(&base).is_empty());
        assert!(base.exits().is_empty());
        assert_eq!(base.status("a").state, SlotState::Starting);
        assert!(base.status("b").reserved);
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_stop_a_timed_out_game_again_when_a_game_check_runs_during_its_stop() {
        let base = base();
        let allocation_id = new_allocation();
        let answer = held_stop(&base, Some(143));

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        settled().await;
        tokio::time::advance(START_TIMEOUT).await;
        until("the timed-out stop", || stopped(&base).len() == 1).await;
        assert_eq!(base.status("a").state, SlotState::Stopping);

        base.slots.check_games(Instant::now());
        settled().await;
        answer.send(()).unwrap();
        until("the exit", || base.status("a").state == SlotState::Idle).await;

        assert_eq!(stopped(&base), [("container0".to_owned(), 0)]);

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (allocation_id.as_str(), ServerExitReason::StartTimeout));
    }

    #[tokio::test]
    async fn stops_the_next_game_on_a_slot_whose_last_game_exited_before_docker_answered_its_stop() {
        let base = base();
        let first = new_allocation();
        let second = new_allocation();

        start(&base, launch_config(&json!({})), &first).await.unwrap();
        base.slots.plugin_status(&first, &status(&[], None, true));

        let answer = held_stop(&base, None);

        base.slots.plugin_status(&first, &status(&[], Some("map_name is empty"), true));
        until("the first stop", || stopped(&base).len() == 1).await;
        // The follow stream reports the exit before the stop request returns.
        base.docker.exit(base.docker.last(), Some(0));
        assert_eq!(base.status("a").state, SlotState::Idle);

        start(&base, launch_config(&json!({})), &second).await.unwrap();
        base.slots.plugin_status(&second, &status(&[], Some("map_name is empty"), true));
        settled().await;
        answer.send(()).unwrap();
        settled().await;
        base.slots.check_games(Instant::now());

        until("both stops", || stopped(&base).into_iter().map(|(id, _)| id).collect::<Vec<_>>() == ["container0", "container1"]).await;
    }

    #[tokio::test]
    async fn keeps_a_stop_of_its_own_in_flight_for_the_next_game_when_the_last_games_stop_answers_late() {
        let base = base();
        let first = new_allocation();
        let second = new_allocation();
        let answer_first = held_stop(&base, None);
        let answer_second = held_stop(&base, Some(143));

        start(&base, launch_config(&json!({})), &first).await.unwrap();
        base.slots.plugin_status(&first, &status(&[], Some("map_name is empty"), true));
        until("the first stop", || stopped(&base).len() == 1).await;
        base.docker.exit(base.docker.last(), Some(0));
        start(&base, launch_config(&json!({})), &second).await.unwrap();
        base.slots.plugin_status(&second, &status(&[], Some("map_name is empty"), true));
        until("the second stop", || stopped(&base).len() == 2).await;
        answer_first.send(()).unwrap();
        settled().await;
        base.slots.check_games(Instant::now());
        settled().await;
        assert_eq!(stopped(&base).into_iter().map(|(id, _)| id).collect::<Vec<_>>(), ["container0", "container1"]);
        answer_second.send(()).unwrap();
        until("the second exit", || base.status("a").state == SlotState::Idle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn leaves_a_quitting_game_running_once_shutdown_has_begun() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.plugin_quitting(&allocation_id);
        settled().await;
        base.stop.cancel();
        tokio::time::advance(QUIT_TIMEOUT).await;
        settled().await;

        // The next agent adopts it.
        assert!(stopped(&base).is_empty());
        assert_eq!(base.status("a").state, SlotState::Running);
    }

    #[tokio::test(start_paused = true)]
    async fn stops_a_game_still_running_after_its_plugin_said_it_was_quitting_and_says_so_on_its_exit() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.plugin_quitting(&allocation_id);
        settled().await;
        tokio::time::advance(QUIT_TIMEOUT.checked_sub(Duration::from_millis(1)).unwrap()).await;
        settled().await;
        assert!(stopped(&base).is_empty());
        tokio::time::advance(Duration::from_millis(1)).await;
        until("the quit timeout", || base.status("a").state == SlotState::Idle).await;

        assert_eq!(stopped(&base), [("container0".to_owned(), grace())]);

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (allocation_id.as_str(), ServerExitReason::QuitTimeout));
    }

    #[tokio::test(start_paused = true)]
    async fn leaves_the_slot_alone_when_the_quitting_game_exited_even_with_the_next_game_on_it() {
        let base = base();
        let quitting = new_allocation();

        start(&base, launch_config(&json!({})), &quitting).await.unwrap();
        base.slots.plugin_quitting(&quitting);
        settled().await;
        base.docker.exit(base.docker.last(), Some(0));

        let next = new_allocation();

        start(&base, launch_config(&json!({})), &next).await.unwrap();
        tokio::time::advance(QUIT_TIMEOUT).await;
        settled().await;

        assert!(stopped(&base).is_empty());
        assert_eq!(base.status("a").allocation_id.as_deref(), Some(next.as_str()));
        assert_eq!(exits(&base), [exit("a", &quitting, ServerExitReason::CleanExit, Some(0), None)]);
    }

    #[tokio::test]
    async fn stops_the_game_the_controller_names_on_a_later_slot_and_leaves_the_one_on_the_first_slot_running() {
        let base = base();
        let first = new_allocation();
        let second = new_allocation();

        start(&base, launch_config(&json!({})), &first).await.unwrap();
        start(&base, launch_config(&json!({})), &second).await.unwrap();
        assert_eq!(base.status("b").allocation_id.as_deref(), Some(second.as_str()));
        base.slots.stop_allocation(&second).await.unwrap();

        assert_eq!(stopped(&base), [("container1".to_owned(), grace())]);
        assert_eq!((base.status("b").state, base.status("b").allocation_id), (SlotState::Idle, None));
        assert_eq!((base.status("a").state, base.status("a").allocation_id), (SlotState::Starting, Some(first)));
    }

    #[tokio::test]
    async fn says_why_a_game_ended_the_controllers_stop_or_its_own_exit_by_its_code_or_lost_without_one() {
        let base = base();
        let requested = new_allocation();

        start(&base, launch_config(&json!({})), &requested).await.unwrap();
        base.slots.stop_allocation(&requested).await.unwrap();
        assert_eq!(exits(&base).last(), Some(&exit("a", &requested, ServerExitReason::Requested, Some(143), None)));

        for (code, reason) in [(Some(0), ServerExitReason::CleanExit), (Some(139), ServerExitReason::Crashed), (None, ServerExitReason::Lost)] {
            let allocation_id = new_allocation();

            start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
            base.docker.exit(base.docker.last(), code);

            let last = exits(&base).last().unwrap().clone();

            assert_eq!((last.allocation_id, last.reason, last.exit_code), (allocation_id, reason, code));
        }

        assert_eq!(exits(&base).len(), 4);
    }

    #[tokio::test]
    async fn calls_the_controllers_stop_requested_whatever_code_the_stop_makes_the_game_exit_with() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();

        let docker = Arc::clone(&base.docker);

        base.docker.override_stop(move |_, _| async move {
            docker.exit(docker.last(), Some(143));
            Ok(())
        });
        base.slots.stop_allocation(&allocation_id).await.unwrap();
        assert_eq!(exits(&base), [exit("a", &allocation_id, ServerExitReason::Requested, Some(143), None)]);
    }

    #[tokio::test(start_paused = true)]
    async fn polls_a_linked_server_without_a_match_and_stops_it_once_it_stayed_empty_for_its_launchs_idle_shutdown() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, server_launch_config(&json!({ "idle_shutdown_seconds": 180 })), &allocation_id).await.unwrap();
        assert_eq!(base.docker.specs().last().unwrap().labels["ankor.idle-shutdown"], "180");

        let ready_at = Instant::now();
        let at = |seconds: u64| ready_at + Duration::from_secs(seconds);

        base.slots.check_games(ready_at);
        assert!(base.polled.lock().unwrap().is_empty());
        base.slots.plugin_online_changed(&allocation_id, true);
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.check_games(at(179));
        assert_eq!(*base.polled.lock().unwrap(), std::slice::from_ref(&allocation_id));
        settled().await;
        assert!(stopped(&base).is_empty());

        // Someone on the server starts the clock over once they leave.
        tokio::time::advance(Duration::from_secs(100)).await;
        base.slots.plugin_status(&allocation_id, &status(&[PLAYER_A], None, true));
        base.slots.check_games(at(300));
        settled().await;
        assert!(stopped(&base).is_empty());
        tokio::time::advance(Duration::from_secs(50)).await;
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.check_games(at(300));
        settled().await;
        assert!(stopped(&base).is_empty());

        base.slots.check_games(at(330));
        until("the idle stop", || base.status("a").state == SlotState::Idle).await;
        assert_eq!(stopped(&base), [("container0".to_owned(), grace())]);

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (allocation_id.as_str(), ServerExitReason::IdleEmpty));
    }

    #[tokio::test]
    async fn polls_and_stops_an_empty_match_by_its_idle_shutdown_too() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({ "idle_shutdown_seconds": 60 })), &allocation_id).await.unwrap();
        base.slots.plugin_online_changed(&allocation_id, true);
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.check_games(Instant::now() + Duration::from_secs(61));
        until("the idle stop", || base.status("a").state == SlotState::Idle).await;
        assert_eq!(*base.polled.lock().unwrap(), std::slice::from_ref(&allocation_id));

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (allocation_id.as_str(), ServerExitReason::IdleEmpty));
    }

    #[tokio::test]
    async fn never_stops_a_server_that_is_not_ready_yet_or_a_match_without_an_idle_shutdown_for_being_empty() {
        let base = base();
        let server = new_allocation();
        let r#match = new_allocation();

        start(&base, server_launch_config(&json!({ "idle_shutdown_seconds": 1 })), &server).await.unwrap();
        base.slots.plugin_status(&server, &status(&[], None, false));
        start(&base, launch_config(&json!({})), &r#match).await.unwrap();
        assert!(!base.docker.specs().last().unwrap().labels.contains_key("ankor.idle-shutdown"));
        base.slots.plugin_status(&r#match, &status(&[], None, true));
        base.slots.check_games(Instant::now() + Duration::from_hours(1));
        settled().await;
        assert!(stopped(&base).is_empty());
    }

    #[tokio::test]
    async fn leaves_an_empty_server_that_the_controller_is_stopping_to_that_stop_however_long_its_idle_shutdown_ran_out() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, server_launch_config(&json!({ "idle_shutdown_seconds": 60 })), &allocation_id).await.unwrap();
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));

        let answer = held_stop(&base, Some(143));
        let manager = base.slots.clone();
        let id = allocation_id.clone();
        let stopping = tokio::spawn(async move { manager.stop_allocation(&id).await });

        until("the stop", || stopped(&base).len() == 1).await;
        base.slots.check_games(Instant::now() + Duration::from_hours(1));
        settled().await;
        answer.send(()).unwrap();
        stopping.await.unwrap().unwrap();
        assert_eq!(stopped(&base), [("container0".to_owned(), grace())]);

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (allocation_id.as_str(), ServerExitReason::Requested));
    }

    #[tokio::test]
    async fn stops_an_adopted_server_by_the_idle_shutdown_on_its_label() {
        let base = base();
        let allocation_id = new_allocation();

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { idle_shutdown_seconds: Some(60), ..game("adopted", "ankor-a-server-x", Some("a"), Some(&allocation_id)) });
        base.slots.adopt().await.unwrap();
        assert!(!base.status("a").plugin_online);
        base.slots.plugin_status(&allocation_id, &status(&[], None, true));
        base.slots.check_games(Instant::now() + Duration::from_mins(1));
        until("the idle stop", || base.status("a").state == SlotState::Idle).await;
        assert_eq!(stopped(&base), [("adopted".to_owned(), grace())]);

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (allocation_id.as_str(), ServerExitReason::IdleEmpty));
    }
}

mod running_allocation {
    use super::*;

    #[tokio::test]
    async fn names_the_allocation_holding_a_slot_from_its_launch_until_its_game_ends_and_none_for_a_slot_the_node_lacks() {
        let base = base();
        let allocation_id = new_allocation();

        assert_eq!(base.slots.running_allocation("a"), None);
        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        assert_eq!(base.slots.running_allocation("a"), Some(allocation_id.clone()));
        assert_eq!(base.slots.running_allocation("b"), None);
        base.slots.stop_allocation(&allocation_id).await.unwrap();
        assert_eq!(base.slots.running_allocation("a"), None);
        assert_eq!(base.slots.running_allocation("zz"), None);
    }
}

mod log_handling {
    use super::*;

    #[tokio::test]
    async fn forwards_every_line_the_server_writes_in_order() {
        let base = base();

        start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap();

        let last = base.docker.last();

        base.docker.line(last, "Gameserver logged on to Steam");
        base.docker.line(last, "random chatter");
        base.docker.line(last, "[Ankor] going live");

        // The controller keeps the whole log per slot; what an operator needs from a crashed server is rarely
        // a line anyone predicted.
        assert_eq!(
            base.logs().into_iter().map(|(_, line)| line).collect::<Vec<_>>(),
            ["Gameserver logged on to Steam", "random chatter", "[Ankor] going live"]
        );
    }
}

mod recovery {
    use super::*;

    #[tokio::test]
    async fn restores_the_slot_relay_binding_when_adopting_a_running_allocation() {
        let calls: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        let recorded = Arc::clone(&calls);
        let base = make_slots_with(agent_config(&json!({})), move |deps| {
            deps.plugin_url = Arc::new(move |slot, allocation, key| {
                recorded.lock().unwrap().push((slot.to_owned(), allocation.to_owned(), key.to_owned()));
                PLUGIN_URL.to_owned()
            });
        });

        base.docker.adoptable.lock().unwrap().push(AdoptedContainer {
            match_id: Some(MATCH_ID.to_owned()),
            ..game("running", "ankor-a", Some("a"), Some("00000000-0000-4000-8000-000000000001"))
        });
        base.slots.adopt().await.unwrap();
        // The key from the game's label, so its plugin's URL still holds.
        assert_eq!(*calls.lock().unwrap(), [("a".to_owned(), "00000000-0000-4000-8000-000000000001".to_owned(), RELAY_KEY.to_owned())]);
    }

    /// A `set_config` gave slot a a new token while its server ran on the old one, then the agent
    /// restarted. The adopted server keeps the token on its label, so that token cannot go to
    /// another slot while it runs.
    #[tokio::test]
    async fn adopts_a_server_on_the_token_it_runs_on_not_the_one_its_slot_has_since() {
        let base = make_slots(agent_config(&json!({
            "slots": [
                { "id": "a", "port": 27015, "tv_port": 27020, "gslt": REPLACEMENT },
                { "id": "b", "port": 27016, "tv_port": 27021, "gslt": GSLT_B },
            ],
        })));

        base.docker.adoptable.lock().unwrap().push(game("running", "ankor-a", Some("a"), Some(&new_allocation())));
        base.slots.adopt().await.unwrap();
        assert_eq!(base.status("a").state, SlotState::Starting);

        let moved = change_layout(&base.slots, &[slot("a", 27015, 27020, REPLACEMENT), slot("b", 27016, 27021, GSLT_A)]).unwrap_err();

        assert_eq!(code(&moved), Some(AgentErrorCode::SlotHeld));
    }

    #[tokio::test]
    async fn adopts_a_running_server_container_knows_its_match_from_the_label_and_re_attaches_to_its_log() {
        let base = base();
        let survivor = "00000000-0000-4000-8000-000000000002";

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { match_id: Some(MATCH_ID.to_owned()), ..game("container-survivor", "ankor-a-match-ab12cd", Some("a"), Some(survivor)) });
        base.slots.adopt().await.unwrap();

        let adopted = base.status("a");

        assert_eq!((adopted.state, adopted.match_id.as_deref(), adopted.allocation_id.as_deref()), (SlotState::Starting, Some(MATCH_ID), Some(survivor)));
        assert_eq!(base.docker.followed.lock().unwrap().iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(), ["container-survivor"]);

        let hooks = base.docker.followed.lock().unwrap()[0].hooks.clone();

        (hooks.on_line)("x".to_owned());
        assert_eq!(base.logs(), [(survivor.to_owned(), "x".to_owned())]);
        base.slots.plugin_status(survivor, &status(&[PLAYER_A], None, true));
        assert_eq!(states(&base), [SlotState::Starting, SlotState::Running]);
        // Back on Steam after the agent's restart, with the slot's token as far as the agent knows.
        assert_eq!(base.tokens_used(), [GSLT_A]);

        // An adopted slot is not free: a start takes the other one, and the exit is observed.
        assert_eq!(start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap().slot_id, "b");
        (hooks.on_exit)(Some(0));
        assert_eq!(base.status("a").state, SlotState::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn stops_an_adopted_game_whose_plugin_does_not_report_ready_within_the_start_timeout_from_adoption() {
        let base = base();
        let silent = new_allocation();
        let reporting = new_allocation();

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .extend([game("silent", "ankor-a", Some("a"), Some(&silent)), game("reporting", "ankor-b", Some("b"), Some(&reporting))]);
        base.slots.adopt().await.unwrap();
        base.slots.plugin_status(&reporting, &status(&[PLAYER_A], None, true));
        settled().await;
        tokio::time::advance(START_TIMEOUT).await;
        until("the start timeout", || base.status("a").state == SlotState::Idle).await;

        assert_eq!(base.status("b").state, SlotState::Running);
        assert_eq!(stopped(&base), [("silent".to_owned(), 0)]);

        let ended = exits(&base);

        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].allocation_id.as_str(), ended[0].reason), (silent.as_str(), ServerExitReason::StartTimeout));
    }

    #[tokio::test]
    async fn removes_exited_containers_and_leaves_slots_idle() {
        let base = base();

        base.docker.adoptable.lock().unwrap().push(AdoptedContainer { running: false, ..game("container-dead", "ankor-b-server-000000", Some("b"), None) });
        base.slots.adopt().await.unwrap();

        assert_eq!(*base.docker.removed.lock().unwrap(), ["container-dead"]);
        assert_eq!(base.status("b").state, SlotState::Idle);
    }

    #[tokio::test]
    async fn stops_a_running_container_whose_slot_is_not_in_the_config_with_time_to_close_its_demo() {
        let base = base();

        base.docker.adoptable.lock().unwrap().extend([
            game("container-orphan", "ankor-z-match-000000", Some("z"), None),
            game("container-no-allocation", "ankor-b", Some("b"), None),
            game("container-no-slot", "ankor-x", None, Some(&new_allocation())),
        ]);
        base.slots.adopt().await.unwrap();

        assert_eq!(stopped(&base), ["container-orphan", "container-no-allocation", "container-no-slot"].map(|id| (id.to_owned(), grace())));
        // Without both labels there is no server to report.
        assert!(base.exits().is_empty());
        assert!(base.docker.followed.lock().unwrap().is_empty());
        assert!(!base.slots.any_busy());
    }

    #[tokio::test]
    async fn reports_the_server_a_removed_slot_ran_as_slot_removed_with_the_code_it_exited_with() {
        let base = base();
        let allocation_id = new_allocation();

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("container-orphan", "ankor-z-match-000000", Some("z"), Some(&allocation_id)) });
        base.slots.adopt().await.unwrap();

        assert_eq!(stopped(&base), [("container-orphan".to_owned(), grace())]);
        assert_eq!(base.exits(), [(exit("z", &allocation_id, ServerExitReason::SlotRemoved, Some(143), None), None)]);
        assert!(!base.slots.any_busy());
    }

    #[tokio::test]
    async fn reports_a_removed_slots_server_with_its_exit_code_when_its_output_cannot_be_attached() {
        let base = base();
        let allocation_id = new_allocation();

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("container-orphan", "ankor-z-match-000000", Some("z"), Some(&allocation_id)) });
        *base.docker.follow_error.lock().unwrap() = Some("no such container".to_owned());
        base.slots.adopt().await.unwrap();

        assert_eq!(exits(&base), [exit("z", &allocation_id, ServerExitReason::SlotRemoved, Some(143), None)]);
    }

    #[tokio::test]
    async fn leaves_one_shot_helper_containers_alone() {
        let base = base();

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { role: Some(ContainerRole::Update), ..game("container-update", "ankor-update-ab12cd", None, None) });
        base.slots.adopt().await.unwrap();

        assert!(stopped(&base).is_empty());
        assert!(base.docker.followed.lock().unwrap().is_empty());
        assert!(!base.slots.any_busy());
    }

    /// The agent labels every game it launches with its relay key; a game without one is not its own.
    #[tokio::test]
    async fn stops_a_running_game_without_its_relay_key_as_untracked() {
        let base = base();

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { relay_key: None, ..game("keyless", "ankor-keyless", Some("a"), Some(&new_allocation())) });
        base.slots.adopt().await.unwrap();

        assert_eq!(stopped(&base), [("keyless".to_owned(), i64::try_from(STOP_TIMEOUT.as_secs()).unwrap())]);
        assert_eq!(base.status("a").state, SlotState::Idle);
    }

    #[tokio::test]
    async fn refuses_recovery_when_an_untracked_running_game_cannot_be_stopped() {
        let base = base();

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("orphan", "ankor-orphan", Some("removed"), Some(&new_allocation())) });
        *base.docker.stop_error.lock().unwrap() = Some("Docker unavailable".to_owned());

        let error = base.slots.adopt().await.unwrap_err();

        assert!(error.to_string().contains("could not stop untracked container ankor-orphan: Docker unavailable"), "{error}");
        assert!(base.exits().is_empty());
    }

    #[tokio::test]
    async fn refuses_recovery_when_running_containers_share_an_allocation() {
        let base = base();
        let allocation_id = new_allocation();

        base.docker.adoptable.lock().unwrap().extend([
            AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("first", "ankor-first", Some("a"), Some(&allocation_id)) },
            AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("second", "ankor-second", Some("b"), Some(&allocation_id)) },
        ]);

        let error = base.slots.adopt().await.unwrap_err();

        assert!(error.to_string().contains("cannot adopt ankor-second: allocation"), "{error}");
        assert_eq!(base.docker.followed.lock().unwrap().iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(), ["first"]);
        assert!(stopped(&base).is_empty());
    }

    /// An orphan of a start Docker carried out after its launch gave up stops, rather than keeping
    /// the agent from booting.
    #[tokio::test]
    async fn stops_a_second_running_container_that_claims_an_adopted_slot_and_boots() {
        let base = base();
        let (first, second) = (new_allocation(), new_allocation());

        base.docker.adoptable.lock().unwrap().extend([
            AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("first", "ankor-first", Some("a"), Some(&first)) },
            AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("second", "ankor-second", Some("a"), Some(&second)) },
        ]);

        base.slots.adopt().await.unwrap();

        assert_eq!(stopped(&base).into_iter().map(|(id, _)| id).collect::<Vec<_>>(), ["second"]);
        assert!(base.slots.holds(&first));
        assert!(!base.slots.holds(&second));
        // The orphan leaves the relay entry of the adopted server, whose plugin would otherwise be refused.
        assert_eq!(base.relay.lock().unwrap().get("a"), Some(&first));
    }
}

mod game_priority {
    use super::*;

    #[tokio::test]
    async fn puts_cpu_shares_on_the_game_container_and_nothing_on_the_update_container() {
        let base = base();
        let config = agent_config(&json!({}));

        start(&base, launch_config(&json!({})), &new_allocation()).await.unwrap();

        // The game asks for ten times the baseline; the one-shot update container asks for nothing
        // and therefore runs at the baseline, which is how the game wins under contention.
        assert_eq!(base.docker.specs().last().unwrap().cpu_shares, Some(cpu_shares_for_weight(GAME_CPU_WEIGHT)));
        assert_eq!(build_update_spec(&config, "ff00ff", "00000000-0000-4000-8000-00000000a0a0", Cs2UpdateMode::Update, false).cpu_shares, None);
    }
}

mod allocations_launched_before {
    use super::*;

    fn one_slot() -> Slots {
        make_slots(agent_config(&json!({ "slots": [{ "id": "a", "port": 27015, "tv_port": 27020, "gslt": GSLT_A }] })))
    }

    #[tokio::test]
    async fn refuses_a_reused_allocation_before_admitting_it_or_picking_a_slot() {
        let base = base();
        let allocation_id = new_allocation();
        let admitted = Arc::new(Mutex::new(false));
        let admit = Arc::clone(&admitted);

        base.allocations.recorded.lock().unwrap().push(allocation_id.clone());

        let error = base
            .slots
            .start_with(launch_config(&json!({})), &allocation_id, async {}, move |()| {
                *admit.lock().unwrap() = true;
                Ok(())
            })
            .await
            .unwrap_err();

        assert_eq!(code(&error), Some(AgentErrorCode::AllocationReused));
        assert!(error.to_string().contains(&allocation_id), "{error}");
        assert!(!*admitted.lock().unwrap());
        assert!(base.docker.started.lock().unwrap().is_empty());
        assert_eq!(base.slots.statuses().iter().map(|status| status.state).collect::<Vec<_>>(), [SlotState::Idle, SlotState::Idle]);
        assert_eq!(*base.allocations.recorded.lock().unwrap(), [allocation_id]);
    }

    #[tokio::test]
    async fn records_an_allocation_once_its_launch_starts_a_server() {
        let base = base();
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();
        assert_eq!(*base.allocations.recorded.lock().unwrap(), [allocation_id]);
    }

    #[tokio::test]
    async fn records_an_allocation_whose_launch_docker_failed_so_it_never_launches_again() {
        let base = base();
        let allocation_id = new_allocation();

        *base.docker.start_error.lock().unwrap() = Some("no such image".to_owned());
        assert!(start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err().to_string().contains("no such image"));
        assert_eq!(*base.allocations.recorded.lock().unwrap(), std::slice::from_ref(&allocation_id));
        *base.docker.start_error.lock().unwrap() = None;
        assert_eq!(code(&start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err()), Some(AgentErrorCode::AllocationReused));
        assert!(base.docker.started.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn leaves_an_allocation_refused_with_no_free_slot_or_a_refusal_of_its_own_free_to_launch_later() {
        for own_refusal in [false, true] {
            let base = one_slot();
            let running = new_allocation();
            let allocation_id = new_allocation();

            start(&base, launch_config(&json!({})), &running).await.unwrap();

            if own_refusal {
                let error = base
                    .slots
                    .start_with(launch_config(&json!({})), &allocation_id, async {}, |()| {
                        Err(SlotError::Refused(AgentFailure::start_refused(StartRefusal::MaintenanceRunning, None)))
                    })
                    .await
                    .unwrap_err();

                assert!(matches!(&error, SlotError::Refused(failure) if failure.code == AgentErrorCode::MaintenanceRunning), "{error:?}");
            } else {
                assert_eq!(code(&start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err()), Some(AgentErrorCode::NoFreeSlot));
            }

            assert_eq!(*base.allocations.recorded.lock().unwrap(), std::slice::from_ref(&running));

            let docker = Arc::clone(&base.docker);

            base.docker.override_stop(move |_, _| async move {
                tokio::spawn(async move { docker.exit(docker.last(), Some(0)) });
                Ok(())
            });
            base.slots.stop_allocation(&running).await.unwrap();
            assert_eq!(start(&base, launch_config(&json!({})), &allocation_id).await.unwrap(), reply(&allocation_id, "a"));
            assert_eq!(*base.allocations.recorded.lock().unwrap(), [running, allocation_id]);
        }
    }

    /// An agent that dies mid-launch leaves the allocation used, whatever became of its server.
    #[tokio::test]
    async fn records_an_allocation_once_its_launch_holds_a_slot_before_docker_is_asked_for_anything() {
        let base = base();
        let allocation_id = new_allocation();

        base.overlay.close();

        let mut first = std::pin::pin!(start(&base, launch_config(&json!({})), &allocation_id));

        // The launch waits at the overlay, its first step, for as long as the test likes.
        for _ in 0..100 {
            assert!(poll!(first.as_mut()).is_pending());
            tokio::task::yield_now().await;
        }

        assert_ne!(base.slots.statuses()[0].state, SlotState::Idle);

        assert_eq!(*base.allocations.recorded.lock().unwrap(), std::slice::from_ref(&allocation_id));
        assert!(base.docker.started.lock().unwrap().is_empty());
        assert_eq!(code(&start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err()), Some(AgentErrorCode::AllocationInUse));
        base.overlay.gate.add_permits(1);
        first.await.unwrap();
        assert_eq!(*base.allocations.recorded.lock().unwrap(), std::slice::from_ref(&allocation_id));
    }

    /// An image switch while a launch prepares neither changes the launch's image nor lets the sweep
    /// take it, as the launch holds it from its pick until its server is gone.
    #[tokio::test]
    async fn a_launch_holds_the_image_it_picked_its_slot_with_through_an_image_switch() {
        let base = base();
        let allocation_id = new_allocation();
        let picked = lock_config(&base).cs2_image.clone();

        base.overlay.close();

        let mut first = std::pin::pin!(start(&base, launch_config(&json!({})), &allocation_id));

        for _ in 0..100 {
            assert!(poll!(first.as_mut()).is_pending());
            tokio::task::yield_now().await;
        }

        assert_eq!(base.slots.held_images(), std::slice::from_ref(&picked));
        lock_config(&base).cs2_image = "ghcr.io/example/cs2:switched".to_owned();
        base.overlay.gate.add_permits(1);
        first.await.unwrap();

        // The container comes from the image id the overlay resolved for the picked reference.
        let created = base.docker.specs().last().unwrap().image.clone();

        assert_eq!(*base.overlay.images.lock().unwrap(), std::slice::from_ref(&picked));
        assert_eq!(base.slots.held_images(), [picked, created]);
    }

    #[tokio::test]
    async fn refuses_a_second_start_of_a_running_allocation_by_its_slot_while_the_spool_is_unavailable() {
        let base = make_slots_with(agent_config(&json!({})), |deps| deps.allocations = None);
        let allocation_id = new_allocation();

        start(&base, launch_config(&json!({})), &allocation_id).await.unwrap();

        let error = start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err();

        assert_eq!(code(&error), Some(AgentErrorCode::AllocationInUse));
        assert!(error.to_string().contains("already holds a slot"), "{error}");
    }

    #[tokio::test]
    async fn refuses_a_launch_whose_allocation_cannot_be_recorded_before_docker() {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let warnings = Arc::clone(&lines);
        let base = make_slots_with(agent_config(&json!({})), move |deps| {
            deps.log = Logger::new(
                "agent",
                Some(Arc::new(move |entry: Entry| {
                    if entry.level == Level::Warn {
                        warnings.lock().unwrap().push(entry.message);
                    }
                })),
            );
        });

        *base.allocations.record_error.lock().unwrap() = Some("disk I/O error".to_owned());

        let allocation_id = new_allocation();

        let error = start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err();

        assert!(matches!(&error, SlotError::Spool(_)), "{error:?}");
        assert!(started_ids(&base).is_empty());
        assert!(base.slots.statuses().iter().all(|status| status.state == SlotState::Idle));
        // Returned to the request, which logs it once.
        assert!(error.to_string().contains("disk I/O error"), "{error}");
        assert!(lines.lock().unwrap().is_empty(), "{:?}", lines.lock().unwrap());
    }

    #[tokio::test]
    async fn fails_a_launch_whose_spool_cannot_say_if_the_allocation_ran_and_counts_it_used() {
        let base = base();

        *base.allocations.used_error.lock().unwrap() = Some("event spool is busy; retry without discarding events".to_owned());

        let allocation_id = new_allocation();
        let error = start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err();

        assert!(matches!(&error, SlotError::Spool(_)), "{error:?}");
        assert_eq!(error.to_string(), "event spool is busy; retry without discarding events");
        assert!(started_ids(&base).is_empty());
        until("the allocation recorded", || base.allocations.recorded.lock().unwrap().contains(&allocation_id)).await;
    }

    #[tokio::test]
    async fn records_the_allocation_of_a_server_it_adopts() {
        let base = base();
        let allocation_id = new_allocation();

        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("adopted", "ankor-a-server-x", Some("a"), Some(&allocation_id)) });
        base.slots.adopt().await.unwrap();
        settled().await;
        assert_eq!(*base.allocations.recorded.lock().unwrap(), std::slice::from_ref(&allocation_id));
        assert_eq!(code(&start(&base, launch_config(&json!({})), &allocation_id).await.unwrap_err()), Some(AgentErrorCode::AllocationReused));
    }

    /// Adoption waits for no answer, so the failure is logged where it happens.
    #[tokio::test]
    async fn logs_an_allocation_it_adopts_but_cannot_record() {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let warnings = Arc::clone(&lines);
        let base = make_slots_with(agent_config(&json!({})), move |deps| {
            deps.log = Logger::new(
                "agent",
                Some(Arc::new(move |entry: Entry| {
                    if entry.level == Level::Warn {
                        warnings.lock().unwrap().push(entry.message);
                    }
                })),
            );
        });
        let allocation_id = new_allocation();

        *base.allocations.record_error.lock().unwrap() = Some("disk I/O error".to_owned());
        base.docker
            .adoptable
            .lock()
            .unwrap()
            .push(AdoptedContainer { idle_shutdown_seconds: Some(180), ..game("adopted", "ankor-a-server-x", Some("a"), Some(&allocation_id)) });
        base.slots.adopt().await.unwrap();
        until("the warning", || !lines.lock().unwrap().is_empty()).await;
        assert!(lines.lock().unwrap().contains(&format!("[agent] could not record allocation {allocation_id} as launched: disk I/O error")));
    }
}
