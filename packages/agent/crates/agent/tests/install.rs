//! `update_cs2` and the maintenance guard: what an update runs, what refuses it, what holds the
//! host while it runs, and what `cs2.update` says about it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ankor_agent::config::ConfigStore;
use ankor_agent::container_spec::labels;
use ankor_agent::docker::{DockerPort, ImageSummary, SelfUpdateHelper, UpdateKind};
use ankor_agent::failure::OpError;
use ankor_agent::install::{
    Cs2UpdateState, InstallOptions, InstallProgress, MaintenanceGuard, MaintenanceGuardOptions, UPDATE_QUIET, build_update_spec, update_cs2,
};
use ankor_agent::logger::{Entry, Logger};
use ankor_agent::overlay::OverlayPort;
use ankor_agent::testing::fixtures::agent_config;
use ankor_agent::testing::{FakeDocker, FakeOverlay, until, wire};
use ankor_protocol::constants::{CS2_REINSTALL_MIN_FREE_GB, CS2_UPDATE_DETAIL_MAX_CHARS, CS2_UPDATE_MIN_FREE_GB};
use ankor_protocol::types::{AgentErrorCode, Cs2Update, Cs2UpdateMode, Cs2UpdateOutcome, MaintenanceOperation};
use serde_json::json;

const UPDATE_ID: &str = "00000000-0000-4000-8000-00000000a0a0";
/// What the labels of an update container a test puts on the host say, unless it says otherwise.
const LABELLED_ID: &str = "00000000-0000-4000-8000-00000000a0a1";
const OTHER_IMAGE: &str = "ghcr.io/example/other:latest";
const OTHER_ID: &str = "sha256:d9298a10d1b0735837dc4bd85dac641b0f3cef27a47e5d53a54f2f3f5b2fcffa";

fn labelled() -> UpdateKind {
    UpdateKind { id: LABELLED_ID.to_owned(), mode: Cs2UpdateMode::Update, automatic: false }
}

fn guard(docker: &Arc<FakeDocker>) -> MaintenanceGuard {
    MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(docker) as Arc<dyn DockerPort>))
}

fn shared(config: ankor_protocol::config::AgentConfig) -> ConfigStore {
    ConfigStore::new(std::path::PathBuf::from("/nonexistent/agent.yml"), config)
}

fn cs2_image(config: &ConfigStore) -> String {
    config.read().cs2_image
}

struct Setup {
    config: ConfigStore,
    docker: Arc<FakeDocker>,
    overlay: Arc<FakeOverlay>,
    progress: Arc<Mutex<Vec<InstallProgress>>>,
    /// One guard per setup, shared by every run from it, as one agent process has one.
    updates: MaintenanceGuard,
    /// How each update ended, as `node_status` reports it.
    state: Cs2UpdateState,
}

impl Setup {
    fn new() -> Self {
        let docker = FakeDocker::new();

        Self {
            config: shared(agent_config(&json!({}))),
            updates: guard(&docker),
            docker,
            overlay: FakeOverlay::new(true),
            progress: Arc::default(),
            state: Cs2UpdateState::default(),
        }
    }

    fn options(&self, mode: Cs2UpdateMode, busy: bool) -> InstallOptions {
        let progress = Arc::clone(&self.progress);

        InstallOptions {
            config: self.config.reader(),
            docker: Arc::clone(&self.docker) as Arc<dyn DockerPort>,
            on_progress: Arc::new(move |entry| progress.lock().unwrap().push(entry)),
            on_log: None,
            log: Logger::null(),
            any_busy: Arc::new(move || busy),
            updates: self.updates.clone(),
            mode,
            automatic: false,
            state: Some(self.state.clone()),
            overlay: Arc::clone(&self.overlay) as Arc<dyn OverlayPort>,
            short_id: Some(Arc::new(|| "ff00ff".to_owned())),
        }
    }

    /// Runs an update on its own task, as a request's deferred work does.
    fn run(&self, mode: Cs2UpdateMode) -> tokio::task::JoinHandle<Result<(), String>> {
        let options = self.options(mode, false);

        tokio::spawn(async move { update_cs2(options).await.map_err(|error| error.to_string()) })
    }

    fn messages(&self) -> Vec<String> {
        self.progress.lock().unwrap().iter().map(|entry| entry.message.clone()).collect()
    }
}

async fn until_started(docker: &FakeDocker, count: usize) {
    until(&format!("{count} container(s) to start"), || docker.started.lock().unwrap().len() >= count).await;
}

async fn until_followed(docker: &FakeDocker, count: usize) {
    until(&format!("{count} container(s) to be followed"), || docker.followed.lock().unwrap().len() >= count).await;
}

fn followed_exit(docker: &FakeDocker, index: usize, code: Option<i64>) {
    let hooks = docker.followed.lock().unwrap()[index].hooks.clone();

    (hooks.on_exit)(code);
}

fn last_env(docker: &FakeDocker) -> Vec<String> {
    docker.specs().last().unwrap().env.clone()
}

#[tokio::test]
async fn removes_the_cs2_images_earlier_updates_superseded_once_every_slot_is_idle_and_the_update_is_done() {
    let setup = Setup::new();
    let (cs2, agent) = {
        let config = setup.config.read();

        (config.cs2_image.clone(), config.agent_image.clone())
    };
    let ours = |id: &str, tags: &[&str], kind: &str| {
        (
            ImageSummary { id: id.to_owned(), repo_tags: tags.iter().map(|tag| (*tag).to_owned()).collect() },
            BTreeMap::from([("ankor.image".to_owned(), kind.to_owned())]),
        )
    };

    setup.docker.images.lock().unwrap().extend([(cs2.clone(), "sha256:cs2-new".to_owned()), (agent.clone(), "sha256:agent".to_owned())]);
    // A game held the old one through its update_cs2_image; the next update, with every slot idle, takes it.
    *setup.docker.image_list.lock().unwrap() =
        vec![ours("sha256:cs2-new", &[&cs2], "cs2"), ours("sha256:cs2-old", &[], "cs2"), ours("sha256:agent", &[&agent], "agent")];

    let pending = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;
    assert!(setup.docker.removed_images.lock().unwrap().is_empty());
    setup.docker.exit(0, Some(0));
    pending.await.unwrap().unwrap();
    assert_eq!(*setup.docker.removed_images.lock().unwrap(), ["sha256:cs2-old"]);
}

#[tokio::test]
async fn runs_the_update_in_the_image_agent_yml_names() {
    let setup = Setup::new();

    *setup.docker.default_image_id.lock().unwrap() = Some("sha256:26ef923651df6c49386cdf2573249158f11bc83ee2c26eba9a86a4a1726d3414".to_owned());

    let pending = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;
    // The image is on the host, so the update runs on the one that is there.
    assert!(setup.docker.pulled.lock().unwrap().is_empty());
    setup.docker.exit(0, Some(0));
    pending.await.unwrap().unwrap();

    // Every progress frame names the update `cs2.update` reports.
    let id = setup.state.get().unwrap().id;
    let progress = setup.progress.lock().unwrap().clone();

    assert_eq!(setup.state.get().unwrap().outcome, Some(Cs2UpdateOutcome::Completed));
    // The image sweep is the last step, under the guard.
    assert_eq!(
        progress[progress.len() - 2..],
        [
            InstallProgress { update_id: id.clone(), percent: Some(100.0), message: "install/update complete".to_owned() },
            InstallProgress { update_id: id.clone(), percent: None, message: "removing images an update superseded".to_owned() }
        ]
    );
    assert!(progress.iter().all(|entry| entry.update_id == id));
}

#[tokio::test]
async fn pulls_the_game_image_when_the_host_does_not_have_it() {
    let setup = Setup::new();
    let image = cs2_image(&setup.config);

    setup.docker.images.lock().unwrap().insert(OTHER_IMAGE.to_owned(), OTHER_ID.to_owned());
    setup.docker.pull_ids.lock().unwrap().insert(image.clone(), "sha256:5ca8a34eac37884841c6925cc3ce17323a6ed90543d759706c97f2582af8fb03".to_owned());

    let pending = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;
    assert_eq!(*setup.docker.pulled.lock().unwrap(), std::slice::from_ref(&image));
    assert!(setup.messages().contains(&format!("pulling {image}")));
    setup.docker.exit(0, Some(0));
    pending.await.unwrap().unwrap();
}

#[tokio::test]
async fn fails_the_install_when_the_missing_game_image_cannot_be_pulled() {
    let setup = Setup::new();
    let message = format!("pulling {} failed: manifest unknown", cs2_image(&setup.config));

    setup.docker.images.lock().unwrap().insert(OTHER_IMAGE.to_owned(), OTHER_ID.to_owned());
    *setup.docker.pull_error.lock().unwrap() = Some("manifest unknown".to_owned());

    // cs2.update says so, because the request has had its reply by then.
    assert_eq!(setup.run(Cs2UpdateMode::Update).await.unwrap().unwrap_err(), message);

    let state = setup.state.get().unwrap();

    assert_eq!((state.outcome, state.detail), (Some(Cs2UpdateOutcome::PullFailed), Some(message)));
    assert!(setup.docker.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn does_not_pull_or_install_when_image_inspection_fails() {
    let setup = Setup::new();

    *setup.docker.on_image_id.lock().unwrap() = Some(Arc::new(|_| Some("Docker connection reset".to_owned())));

    let error = update_cs2(setup.options(Cs2UpdateMode::Update, false)).await.unwrap_err();

    assert!(matches!(error, ankor_agent::failure::OpError::Failed(_)));
    assert_eq!(error.to_string(), "Docker connection reset");
    assert!(setup.docker.pulled.lock().unwrap().is_empty());
    assert!(setup.docker.started.lock().unwrap().is_empty());

    let state = setup.state.get().unwrap();

    assert_eq!((state.outcome, state.detail.as_deref()), (Some(Cs2UpdateOutcome::Failed), Some("Docker connection reset")));
    assert!(!setup.updates.running());
}

#[tokio::test]
async fn extracts_the_addon_overlay_before_the_update_container_starts() {
    let setup = Setup::new();
    let pending = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;

    // The overlay belongs to the image, so a new image means a new extraction, and it happens here
    // rather than at the first launch after the update.
    assert_eq!(*setup.overlay.calls.lock().unwrap(), 1);
    assert!(setup.messages().contains(&"extracting the addon overlay".to_owned()));
    assert!(setup.messages().contains(&"addon overlay ready for sha256:image".to_owned()));
    setup.docker.exit(0, Some(0));
    pending.await.unwrap().unwrap();
}

#[tokio::test]
async fn fails_the_request_when_the_overlay_cannot_be_extracted_without_touching_the_volume() {
    let setup = Setup::new();

    *setup.overlay.error.lock().unwrap() = Some("no space left on device".to_owned());

    let error = setup.run(Cs2UpdateMode::Update).await.unwrap().unwrap_err();

    assert!(error.contains("extracting the addon overlay failed: no space left on device"));

    let state = setup.state.get().unwrap();

    assert_eq!(
        (state.outcome, state.detail.as_deref()),
        (Some(Cs2UpdateOutcome::OverlayFailed), Some("extracting the addon overlay failed: no space left on device"))
    );
    // A game update whose plugin never arrived is the half-update this ordering avoids.
    assert!(setup.docker.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn passes_the_request_s_mode_into_the_update_container() {
    // Per request, because verifying re-hashes all 67 GB and a reinstall downloads it again.
    let plain = Setup::new();
    let plain_run = plain.run(Cs2UpdateMode::Update);

    until_started(&plain.docker, 1).await;

    let env = last_env(&plain.docker);

    for entry in ["CS2_VALIDATE=0".to_owned(), "CS2_REINSTALL=0".to_owned(), format!("CS2_MIN_FREE_GB={CS2_UPDATE_MIN_FREE_GB}")] {
        assert!(env.contains(&entry), "{entry}");
    }

    assert!(plain.messages().contains(&"installing/updating CS2 with steamcmd".to_owned()));
    plain.docker.exit(0, Some(0));
    plain_run.await.unwrap().unwrap();

    let verifying = Setup::new();
    let verifying_run = verifying.run(Cs2UpdateMode::Verify);

    until_started(&verifying.docker, 1).await;

    let env = last_env(&verifying.docker);

    for entry in ["CS2_VALIDATE=1".to_owned(), "CS2_REINSTALL=0".to_owned(), format!("CS2_MIN_FREE_GB={CS2_UPDATE_MIN_FREE_GB}")] {
        assert!(env.contains(&entry), "{entry}");
    }

    // And the operator can see it in the progress the node reports.
    assert!(verifying.messages().contains(&"installing/updating CS2 with steamcmd, verifying every game file".to_owned()));
    verifying.docker.exit(0, Some(0));
    verifying_run.await.unwrap().unwrap();
}

#[tokio::test]
async fn wipes_and_re_downloads_in_reinstall_mode_which_never_also_verifies() {
    let reinstalling = Setup::new();
    let pending = reinstalling.run(Cs2UpdateMode::Reinstall);

    until_started(&reinstalling.docker, 1).await;

    // steamcmd's `validate` on an empty directory only re-hashes what it just wrote.
    let env = last_env(&reinstalling.docker);

    for entry in ["CS2_REINSTALL=1".to_owned(), "CS2_VALIDATE=0".to_owned(), format!("CS2_MIN_FREE_GB={CS2_REINSTALL_MIN_FREE_GB}")] {
        assert!(env.contains(&entry), "{entry}");
    }

    assert!(reinstalling.messages().contains(&"reinstalling CS2 with steamcmd, downloading the whole game from scratch".to_owned()));
    reinstalling.docker.exit(0, Some(0));
    pending.await.unwrap().unwrap();
}

#[tokio::test]
async fn refuses_with_slots_held_while_a_slot_is_not_idle() {
    let setup = Setup::new();
    let error = update_cs2(setup.options(Cs2UpdateMode::Update, true)).await.unwrap_err();

    assert!(matches!(error, OpError::Refused(failure) if failure.code == AgentErrorCode::SlotsHeld));
    assert_eq!(*setup.docker.update_container_checks.lock().unwrap(), 0);
    assert!(setup.docker.pulled.lock().unwrap().is_empty());
    assert!(setup.docker.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn refuses_with_slots_held_when_a_slot_takes_a_game_while_the_guard_is_being_claimed() {
    let setup = Setup::new();
    // Idle when asked first; a launch lands while the guard checks Docker for an update container.
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&asked);
    let mut options = setup.options(Cs2UpdateMode::Update, false);

    options.state = None;
    options.short_id = None;
    options.any_busy = Arc::new(move || counted.fetch_add(1, Ordering::SeqCst) > 0);

    assert!(matches!(update_cs2(options).await, Err(OpError::Refused(failure)) if failure.code == AgentErrorCode::SlotsHeld));
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    assert!(setup.docker.started.lock().unwrap().is_empty());
    assert!(!setup.updates.running());
}

#[tokio::test]
async fn refuses_a_second_request_while_the_first_update_is_still_running_in_this_process() {
    let setup = Setup::new();
    let first = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;

    // The controller's request timed out after 60 s while steamcmd was still downloading, and the controller
    // sent a second `update_cs2`. The in-memory reservation answers that before Docker is asked.
    assert!(setup.run(Cs2UpdateMode::Update).await.unwrap().unwrap_err().contains("is already running"));
    assert_eq!(setup.docker.started.lock().unwrap().len(), 1);

    setup.docker.exit(0, Some(0));
    first.await.unwrap().unwrap();

    // And the host can be taken again as soon as the first one is done.
    let second = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 2).await;
    setup.docker.exit(1, Some(0));
    second.await.unwrap().unwrap();
}

#[tokio::test]
async fn refuses_when_docker_reports_an_update_container_this_process_did_not_start() {
    let setup = Setup::new();

    // What an agent restarted mid-install comes back to: nothing in memory, a live steamcmd on the host.
    *setup.docker.update_container.lock().unwrap() = Some(("ankor-update-aa11bb".to_owned(), Some(labelled())));

    assert!(setup.run(Cs2UpdateMode::Update).await.unwrap().unwrap_err().contains("is already running"));
    assert!(setup.updates.running());
    assert!(setup.docker.pulled.lock().unwrap().is_empty());
    assert!(setup.docker.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn frees_the_guard_at_once_after_a_failed_start() {
    let setup = Setup::new();

    *setup.docker.start_error.lock().unwrap() = Some("start refused".to_owned());

    assert!(setup.run(Cs2UpdateMode::Update).await.unwrap().unwrap_err().contains("start refused"));
    assert!(!setup.updates.running());
    assert!(setup.docker.followed.lock().unwrap().is_empty());
}

/// A quiet update container that could not be removed may still be writing the game files, so the
/// guard keeps launches out until Docker reports it gone.
#[tokio::test(start_paused = true)]
async fn holds_the_guard_until_a_quiet_update_container_it_could_not_remove_exits() {
    let setup = Setup::new();

    *setup.docker.remove_error.lock().unwrap() = Some("daemon busy".to_owned());

    let running = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;
    tokio::time::sleep(UPDATE_QUIET).await;
    assert!(running.await.unwrap().unwrap_err().contains("printed nothing"));
    assert!(setup.updates.running());
    until_followed(&setup.docker, 1).await;
    assert_eq!(setup.docker.followed.lock().unwrap()[0].id, "container0");

    let hooks = setup.docker.followed.lock().unwrap()[0].hooks.clone();

    (hooks.on_exit)(None);
    until("the guard freed once the container is gone", || !setup.updates.running()).await;
}

#[tokio::test]
async fn refuses_from_boot_until_the_adopted_update_container_exits() {
    let docker = FakeDocker::new();
    let progress: Arc<Mutex<Vec<InstallProgress>>> = Arc::default();
    let reports = Arc::clone(&progress);
    let mut options = MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn DockerPort>);

    options.on_progress = Arc::new(move |report| reports.lock().unwrap().push(report));
    *docker.update_container.lock().unwrap() = Some(("ankor-update-aa11bb".to_owned(), Some(labelled())));

    let guard = MaintenanceGuard::new(options);

    guard.adopt_running().await.unwrap();
    assert!(guard.running());
    assert!(
        matches!(guard.claim(MaintenanceOperation::Cs2Update).await.err(), Some(ankor_agent::failure::OpError::Refused(failure)) if failure.code == ankor_protocol::types::AgentErrorCode::MaintenanceRunning)
    );

    // The inherited container's log is followed like one this agent started, so the controller keeps its
    // progress line across an agent restart.
    until_followed(&docker, 1).await;
    assert_eq!(docker.followed.lock().unwrap().iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(), ["ankor-update-aa11bb"]);
    // Its end counts once Docker removed it, so the sweep the freed guard starts cannot list and adopt it again.
    assert_eq!(*docker.followed_until.lock().unwrap(), [("ankor-update-aa11bb".to_owned(), ankor_agent::docker_api::WaitCondition::Removed)]);

    let hooks = docker.followed.lock().unwrap()[0].hooks.clone();

    (hooks.on_line)(" Update state (0x61) downloading, progress: 42.00 (30 / 73)".to_owned());

    let reported = progress.lock().unwrap().clone();

    assert_eq!(reported.len(), 1);
    assert_eq!((reported[0].update_id.as_str(), reported[0].percent), (LABELLED_ID, Some(42.0)));
    assert!(reported[0].message.contains("downloading"));

    // steamcmd finished and the container is gone: its exit ends the refusal, rather than it
    // lasting until someone restarts the agent again.
    *docker.update_container.lock().unwrap() = None;
    (hooks.on_exit)(Some(0));
    until("the exit to release the guard", || !guard.running()).await;

    let release = guard.claim(MaintenanceOperation::Cs2Update).await.unwrap();

    release.release(None);
    assert!(!guard.running());
}

#[tokio::test]
async fn ignores_a_repeated_exit_callback_once_the_guard_is_claimed_again() {
    let docker = FakeDocker::new();
    let guard = guard(&docker);

    *docker.update_container.lock().unwrap() = Some(("ankor-update-old".to_owned(), Some(labelled())));
    guard.adopt_running().await.unwrap();
    until_followed(&docker, 1).await;
    *docker.update_container.lock().unwrap() = None;
    followed_exit(&docker, 0, Some(0));
    until("the exit to release the guard", || !guard.running()).await;

    let release = guard.claim(MaintenanceOperation::Cs2Update).await.unwrap();

    followed_exit(&docker, 0, Some(0));
    tokio::task::yield_now().await;
    assert!(guard.running());
    release.release(None);
}

#[tokio::test]
async fn ignores_a_repeated_exit_callback_once_another_update_container_holds_the_guard() {
    let docker = FakeDocker::new();
    let guard = guard(&docker);

    *docker.update_container.lock().unwrap() = Some(("ankor-update-old".to_owned(), Some(labelled())));
    guard.adopt_running().await.unwrap();
    until_followed(&docker, 1).await;
    followed_exit(&docker, 0, Some(0));
    *docker.update_container.lock().unwrap() = Some(("ankor-update-new".to_owned(), Some(labelled())));
    guard.adopt_running().await.unwrap();
    followed_exit(&docker, 0, Some(0));
    assert_eq!(guard.holder().unwrap().operation, MaintenanceOperation::Cs2Update);
}

#[tokio::test]
async fn removes_the_update_container_once_it_has_exited() {
    let setup = Setup::new();
    let pending = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;

    let id = setup.docker.started.lock().unwrap()[0].id.clone();

    setup.docker.exit(0, Some(0));
    pending.await.unwrap().unwrap();
    assert_eq!(*setup.docker.removed.lock().unwrap(), [id]);
}

#[tokio::test]
async fn reports_a_non_zero_exit_as_an_error() {
    let setup = Setup::new();
    let pending = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;
    setup.docker.exit(0, Some(3));
    assert_eq!(pending.await.unwrap().unwrap_err(), "install/update container exited with 3");

    let state = setup.state.get().unwrap();

    assert_eq!((state.outcome, state.detail.as_deref()), (Some(Cs2UpdateOutcome::Failed), Some("install/update container exited with 3")));
}

#[tokio::test]
async fn reports_an_exit_docker_could_not_observe_as_an_error() {
    let setup = Setup::new();
    let pending = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;
    setup.docker.exit(0, None);
    assert_eq!(pending.await.unwrap().unwrap_err(), "install/update container exited with no observed status");

    let state = setup.state.get().unwrap();

    assert_eq!((state.outcome, state.detail.as_deref()), (Some(Cs2UpdateOutcome::Failed), Some("install/update container exited with no observed status")));
}

#[tokio::test]
async fn reports_a_failure_without_a_message_as_the_update_failing() {
    let setup = Setup::new();

    *setup.docker.on_image_id.lock().unwrap() = Some(Arc::new(|_| Some(String::new())));
    assert!(setup.run(Cs2UpdateMode::Update).await.unwrap().is_err());

    let state = setup.state.get().unwrap();

    assert_eq!((state.outcome, state.detail.as_deref()), (Some(Cs2UpdateOutcome::Failed), Some("the update failed")));
}

#[tokio::test]
async fn fails_an_update_that_reports_to_no_state_with_the_update_s_error() {
    let setup = Setup::new();
    let mut options = setup.options(Cs2UpdateMode::Update, false);

    options.state = None;
    options.short_id = None;

    let updating = tokio::spawn(async move { update_cs2(options).await.map_err(|error| error.to_string()) });

    until_started(&setup.docker, 1).await;
    setup.docker.exit(0, Some(3));
    assert_eq!(updating.await.unwrap().unwrap_err(), "install/update container exited with 3");
}

#[tokio::test]
async fn frees_the_guard_once_however_often_a_claim_is_released_leaving_a_later_claim_held() {
    let guard = guard(&FakeDocker::new());
    let first = guard.claim(MaintenanceOperation::Cs2Update).await.unwrap();

    first.release(None);

    let second = guard.claim(MaintenanceOperation::AgentUpdate).await.unwrap();

    first.release(None);
    assert_eq!(guard.holder().unwrap().operation, MaintenanceOperation::AgentUpdate);
    second.release(None);
    assert!(!guard.running());
}

#[tokio::test]
async fn refuses_a_claim_while_this_agent_or_an_adopted_container_holds_the_guard() {
    let docker = FakeDocker::new();
    let guard = guard(&docker);
    let release = guard.claim(MaintenanceOperation::Cs2Update).await.unwrap();

    assert!(
        matches!(guard.claim(MaintenanceOperation::AgentUpdate).await.err(), Some(ankor_agent::failure::OpError::Refused(failure)) if failure.code == ankor_protocol::types::AgentErrorCode::MaintenanceRunning)
    );
    release.release(None);
    *docker.update_container.lock().unwrap() = Some(("ankor-update-aa11bb".to_owned(), Some(labelled())));
    guard.adopt_running().await.unwrap();
    assert!(
        matches!(guard.claim(MaintenanceOperation::AgentUpdate).await.err(), Some(ankor_agent::failure::OpError::Refused(failure)) if failure.code == ankor_protocol::types::AgentErrorCode::MaintenanceRunning)
    );
}

fn watched_guard(docker: &Arc<FakeDocker>) -> (MaintenanceGuard, tokio::sync::watch::Receiver<bool>) {
    let guard = MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(docker) as Arc<dyn DockerPort>));
    let running = guard.subscribe();

    (guard, running)
}

/// Whether the guard changed since the last look, and whether it is held now.
fn look(running: &mut tokio::sync::watch::Receiver<bool>) -> (bool, bool) {
    let changed = running.has_changed().unwrap();

    (changed, *running.borrow_and_update())
}

#[tokio::test]
async fn publishes_a_claim_and_its_release_and_a_claim_docker_could_not_check() {
    let docker = FakeDocker::new();
    let (guard, mut running) = watched_guard(&docker);
    let release = guard.claim(MaintenanceOperation::Cs2Update).await.unwrap();

    assert_eq!(look(&mut running), (true, true));
    release.release(None);
    assert_eq!(look(&mut running), (true, false));
    *docker.update_container_error.lock().unwrap() = Some("Docker unavailable".to_owned());
    assert_eq!(guard.claim(MaintenanceOperation::Cs2Update).await.err().unwrap().to_string(), "Docker unavailable");
    // Held while Docker was asked, then free again.
    assert_eq!(look(&mut running), (true, false));
}

#[tokio::test]
async fn holds_the_guard_for_an_agent_swap_s_helper_until_it_exits_unless_another_container_took_the_guard() {
    let docker = FakeDocker::new();
    let (guard, mut running) = watched_guard(&docker);

    *docker.self_update_helpers.lock().unwrap() = vec![SelfUpdateHelper { name: "ankor-agent-swap".to_owned(), image: None, running: true }];
    guard.adopt_running().await.unwrap();
    assert_eq!(guard.holder().unwrap().operation, MaintenanceOperation::AgentUpdate);
    assert_eq!(look(&mut running), (true, true));
    until_followed(&docker, 1).await;
    followed_exit(&docker, 0, Some(0));
    assert!(!guard.running());
    assert_eq!(look(&mut running), (true, false));

    guard.adopt_running().await.unwrap();
    docker.self_update_helpers.lock().unwrap().clear();
    *docker.update_container.lock().unwrap() = Some(("ankor-update-aa11bb".to_owned(), Some(labelled())));
    guard.adopt_running().await.unwrap();
    until_followed(&docker, 3).await;
    followed_exit(&docker, 1, Some(0));
    assert_eq!(guard.holder().unwrap().operation, MaintenanceOperation::Cs2Update);
}

#[tokio::test]
async fn ignores_an_exit_docker_reports_after_shutdown() {
    let docker = FakeDocker::new();
    let progress = Arc::new(AtomicUsize::new(0));
    let reports = Arc::clone(&progress);
    let mut options = MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn DockerPort>);

    options.on_progress = Arc::new(move |_| {
        reports.fetch_add(1, Ordering::SeqCst);
    });
    *docker.update_container.lock().unwrap() = Some(("ankor-update-aa11bb".to_owned(), Some(labelled())));

    let stop = tokio_util::sync::CancellationToken::new();

    options.stop = stop.clone();

    let guard = MaintenanceGuard::new(options);
    let mut running = guard.subscribe();

    guard.adopt_running().await.unwrap();
    assert_eq!(look(&mut running), (true, true));
    stop.cancel();
    until_followed(&docker, 1).await;
    followed_exit(&docker, 0, Some(0));
    assert_eq!(look(&mut running), (false, true));
    assert_eq!(progress.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn forwards_every_line_the_update_container_prints() {
    let setup = Setup::new();
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let forwarded = Arc::clone(&lines);
    let mut options = setup.options(Cs2UpdateMode::Update, false);

    options.state = None;
    options.short_id = None;
    options.on_log = Some(Arc::new(move |line| forwarded.lock().unwrap().push(line)));

    let updating = tokio::spawn(update_cs2(options));

    until_started(&setup.docker, 1).await;
    setup.docker.line(0, "[entrypoint] update-only mode");
    setup.docker.line(0, " Update state (0x61) downloading, progress: 5.80 (1 / 2)");
    setup.docker.exit(0, Some(0));
    updating.await.unwrap().unwrap();
    assert_eq!(*lines.lock().unwrap(), ["[entrypoint] update-only mode", " Update state (0x61) downloading, progress: 5.80 (1 / 2)"]);
}

#[tokio::test]
async fn tells_the_agent_to_report_its_install_again_once_the_update_is_done() {
    let setup = Setup::new();
    let mut running = setup.updates.subscribe();
    let updating = setup.run(Cs2UpdateMode::Update);

    until_started(&setup.docker, 1).await;
    assert_eq!(look(&mut running), (true, true));
    setup.docker.exit(0, Some(0));
    updating.await.unwrap().unwrap();
    // The controller's hello for this node still says the old build and free space until a fresh one lands.
    assert_eq!(look(&mut running), (true, false));
}

#[test]
fn runs_from_started_to_one_end_cutting_a_long_reason_and_reports_each_change() {
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);
    let started: Arc<Mutex<Vec<String>>> = Arc::default();
    let starts = Arc::clone(&started);
    let state = Cs2UpdateState::new(
        Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }),
        Arc::new(|| {}),
        Arc::new(move |id| starts.lock().unwrap().push(id.to_owned())),
    );

    assert_eq!(state.get(), None);
    state.ended(Cs2UpdateOutcome::Failed, Some("x"));
    assert_eq!(state.get(), None);
    state.started(UPDATE_ID, Cs2UpdateMode::Verify, true);
    assert_eq!(*started.lock().unwrap(), [UPDATE_ID]);
    assert_eq!(state.get(), Some(Cs2Update { id: UPDATE_ID.to_owned(), mode: Cs2UpdateMode::Verify, automatic: true, outcome: None, detail: None }));

    let max = usize::try_from(CS2_UPDATE_DETAIL_MAX_CHARS).unwrap();

    state.ended(Cs2UpdateOutcome::Failed, Some(&"x".repeat(max + 5)));
    assert_eq!(
        state.get(),
        Some(Cs2Update {
            id: UPDATE_ID.to_owned(),
            mode: Cs2UpdateMode::Verify,
            automatic: true,
            outcome: Some(Cs2UpdateOutcome::Failed),
            detail: Some("x".repeat(max))
        })
    );
    // A second word on the same end changes nothing.
    state.ended(Cs2UpdateOutcome::Completed, None);
    assert_eq!(state.get().unwrap().outcome, Some(Cs2UpdateOutcome::Failed));
    // Only the start reports; the guard's release reports the end with the free node.
    assert_eq!(changes.load(Ordering::SeqCst), 1);
}

#[test]
fn labels_the_update_container_with_its_id_its_mode_and_whether_the_agent_started_it_itself() {
    let config = agent_config(&json!({}));
    let reinstall = build_update_spec(&config, "ff00ff", UPDATE_ID, Cs2UpdateMode::Reinstall, true).labels;
    let update = build_update_spec(&config, "ff00ff", UPDATE_ID, Cs2UpdateMode::Update, false).labels;

    assert_eq!(
        (reinstall[labels::UPDATE_ID].clone(), reinstall[labels::UPDATE_MODE].clone(), reinstall[labels::UPDATE_AUTOMATIC].clone()),
        (UPDATE_ID.to_owned(), wire(Cs2UpdateMode::Reinstall), true.to_string())
    );
    assert_eq!((update[labels::UPDATE_MODE].clone(), update[labels::UPDATE_AUTOMATIC].clone()), (wire(Cs2UpdateMode::Update), false.to_string()));
}

#[tokio::test]
async fn reports_an_update_this_agent_runs_as_running_then_how_it_ended() {
    let setup = Setup::new();
    let run = |automatic: bool| {
        let mut options = setup.options(Cs2UpdateMode::Update, false);

        options.automatic = automatic;
        tokio::spawn(async move { update_cs2(options).await.map_err(|error| error.to_string()) })
    };
    let pending = run(true);

    until_started(&setup.docker, 1).await;

    let id = setup.state.get().unwrap().id;

    assert_eq!(setup.state.get(), Some(Cs2Update { id: id.clone(), mode: Cs2UpdateMode::Update, automatic: true, outcome: None, detail: None }));
    assert_eq!(setup.docker.specs()[0].labels[labels::UPDATE_ID], id);
    setup.docker.exit(0, Some(0));
    pending.await.unwrap().unwrap();
    assert_eq!(
        setup.state.get(),
        Some(Cs2Update { id: id.clone(), mode: Cs2UpdateMode::Update, automatic: true, outcome: Some(Cs2UpdateOutcome::Completed), detail: None })
    );

    *setup.docker.pull_error.lock().unwrap() = Some("manifest unknown".to_owned());
    setup.docker.images.lock().unwrap().insert(OTHER_IMAGE.to_owned(), OTHER_ID.to_owned());
    assert!(run(false).await.unwrap().unwrap_err().contains("manifest unknown"));

    // A new update, with an id of its own.
    let state = setup.state.get().unwrap();

    assert_ne!(state.id, id);
    assert_eq!(
        state,
        Cs2Update {
            id: state.id.clone(),
            mode: Cs2UpdateMode::Update,
            automatic: false,
            outcome: Some(Cs2UpdateOutcome::PullFailed),
            detail: Some(format!("pulling {} failed: manifest unknown", cs2_image(&setup.config))),
        }
    );
}

#[tokio::test]
async fn leaves_it_as_it_was_when_busy_slots_refuse_the_update_which_is_no_attempt() {
    let setup = Setup::new();
    let mut options = setup.options(Cs2UpdateMode::Update, true);

    options.automatic = true;
    options.short_id = None;
    assert!(matches!(update_cs2(options).await, Err(OpError::Refused(failure)) if failure.code == AgentErrorCode::SlotsHeld));
    assert_eq!(setup.state.get(), None);
}

fn reporting_guard(docker: &Arc<FakeDocker>, state: &Cs2UpdateState, progress: &Arc<Mutex<Vec<InstallProgress>>>) -> MaintenanceGuard {
    let reports = Arc::clone(progress);
    let mut options = MaintenanceGuardOptions::new(Arc::clone(docker) as Arc<dyn DockerPort>);

    options.state = Some(state.clone());
    options.on_progress = Arc::new(move |report| reports.lock().unwrap().push(report));

    MaintenanceGuard::new(options)
}

#[tokio::test]
async fn reports_an_update_container_found_at_start_by_its_labels_and_its_exit_as_its_outcome() {
    for (code, outcome, detail) in [(0, Cs2UpdateOutcome::Completed, None), (3, Cs2UpdateOutcome::Failed, Some("the update container exited with 3"))] {
        let docker = FakeDocker::new();
        let state = Cs2UpdateState::default();
        let guard = reporting_guard(&docker, &state, &Arc::default());

        *docker.update_container.lock().unwrap() =
            Some(("ankor-update-aa11bb".to_owned(), Some(UpdateKind { id: UPDATE_ID.to_owned(), mode: Cs2UpdateMode::Verify, automatic: true })));
        guard.adopt_running().await.unwrap();
        assert_eq!(state.get(), Some(Cs2Update { id: UPDATE_ID.to_owned(), mode: Cs2UpdateMode::Verify, automatic: true, outcome: None, detail: None }));

        *docker.update_container.lock().unwrap() = None;
        until_followed(&docker, 1).await;
        followed_exit(&docker, 0, Some(code));
        until("the exit to release the guard", || !guard.running()).await;
        assert_eq!(
            state.get(),
            Some(Cs2Update {
                id: UPDATE_ID.to_owned(),
                mode: Cs2UpdateMode::Verify,
                automatic: true,
                outcome: Some(outcome),
                detail: detail.map(str::to_owned)
            }),
            "exit {code}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn removes_an_inherited_update_container_that_stays_quiet_and_reports_why() {
    let docker = FakeDocker::new();
    let state = Cs2UpdateState::default();
    let guard = reporting_guard(&docker, &state, &Arc::default());

    *docker.update_container.lock().unwrap() =
        Some(("ankor-update-aa11bb".to_owned(), Some(UpdateKind { id: UPDATE_ID.to_owned(), mode: Cs2UpdateMode::Update, automatic: false })));
    guard.adopt_running().await.unwrap();
    *docker.update_container.lock().unwrap() = None;
    until_followed(&docker, 1).await;

    let line = docker.followed.lock().unwrap()[0].hooks.on_line.clone();

    // A line every 20 minutes keeps it running past the quiet limit.
    for _ in 0..3 {
        tokio::time::sleep(std::time::Duration::from_mins(20)).await;
        line("Update state (0x61) downloading, progress: 5.80 (1 / 2)".to_owned());
    }
    assert!(docker.removed.lock().unwrap().is_empty());

    tokio::time::sleep(UPDATE_QUIET).await;
    until("the quiet container removed", || !docker.removed.lock().unwrap().is_empty()).await;
    assert_eq!(*docker.removed.lock().unwrap(), ["ankor-update-aa11bb"]);

    followed_exit(&docker, 0, Some(137));
    until("the exit to release the guard", || !guard.running()).await;
    assert_eq!(state.get().unwrap().outcome, Some(Cs2UpdateOutcome::Failed));
    assert_eq!(state.get().unwrap().detail.as_deref(), Some(format!("ankor-update-aa11bb printed nothing for {} s", UPDATE_QUIET.as_secs()).as_str()));
}

#[tokio::test(start_paused = true)]
async fn leaves_an_inherited_update_container_alone_once_it_has_exited() {
    let docker = FakeDocker::new();
    let state = Cs2UpdateState::default();
    let guard = reporting_guard(&docker, &state, &Arc::default());

    *docker.update_container.lock().unwrap() =
        Some(("ankor-update-aa11bb".to_owned(), Some(UpdateKind { id: UPDATE_ID.to_owned(), mode: Cs2UpdateMode::Update, automatic: false })));
    guard.adopt_running().await.unwrap();
    *docker.update_container.lock().unwrap() = None;
    until_followed(&docker, 1).await;
    followed_exit(&docker, 0, Some(0));
    until("the exit to release the guard", || !guard.running()).await;

    tokio::time::sleep(UPDATE_QUIET * 2).await;
    assert!(docker.removed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn calls_an_update_container_s_end_unobserved_when_the_guard_missed_its_exit() {
    let docker = FakeDocker::new();
    let state = Cs2UpdateState::default();
    let guard = reporting_guard(&docker, &state, &Arc::default());

    *docker.update_container.lock().unwrap() =
        Some(("ankor-update-aa11bb".to_owned(), Some(UpdateKind { id: UPDATE_ID.to_owned(), mode: Cs2UpdateMode::Update, automatic: false })));
    guard.adopt_running().await.unwrap();
    // Gone before Docker could report an exit code.
    *docker.update_container.lock().unwrap() = None;
    until_followed(&docker, 1).await;
    followed_exit(&docker, 0, None);
    until("the exit to release the guard", || !guard.running()).await;
    assert_eq!(state.get().unwrap().outcome, Some(Cs2UpdateOutcome::Unobserved));
}

/// An inherited update removes the images an update superseded before it frees the guard, as an
/// update of this agent's own does, so no launch or claim sees the node free mid-sweep.
#[tokio::test]
async fn sweeps_after_an_inherited_update_before_it_frees_the_guard() {
    let docker = FakeDocker::new();
    let state = Cs2UpdateState::default();
    let progress: Arc<Mutex<Vec<InstallProgress>>> = Arc::default();
    let config = shared(agent_config(&json!({})));
    let (cs2, agent) = {
        let config = config.read();

        (config.cs2_image.clone(), config.agent_image.clone())
    };
    let ours = |id: &str, tags: &[&str], kind: &str| {
        (
            ImageSummary { id: id.to_owned(), repo_tags: tags.iter().map(|tag| (*tag).to_owned()).collect() },
            BTreeMap::from([("ankor.image".to_owned(), kind.to_owned())]),
        )
    };
    let reports = Arc::clone(&progress);
    let mut options = MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn DockerPort>);

    options.state = Some(state.clone());
    options.config = Some(config.reader());
    options.on_progress = Arc::new(move |report| reports.lock().unwrap().push(report));

    let guard = MaintenanceGuard::new(options);
    let held_while_sweeping = Arc::new(AtomicBool::new(false));
    let (watched, seen) = (guard.clone(), Arc::clone(&held_while_sweeping));

    docker.images.lock().unwrap().extend([(cs2.clone(), "sha256:cs2-new".to_owned()), (agent.clone(), "sha256:agent".to_owned())]);
    *docker.image_list.lock().unwrap() =
        vec![ours("sha256:cs2-new", &[&cs2], "cs2"), ours("sha256:cs2-old", &[], "cs2"), ours("sha256:agent", &[&agent], "agent")];
    *docker.on_images_in_use.lock().unwrap() = Some(Arc::new(move || seen.store(watched.running(), Ordering::SeqCst)));
    *docker.update_container.lock().unwrap() =
        Some(("ankor-update-aa11bb".to_owned(), Some(UpdateKind { id: UPDATE_ID.to_owned(), mode: Cs2UpdateMode::Update, automatic: false })));
    guard.adopt_running().await.unwrap();
    *docker.update_container.lock().unwrap() = None;
    until_followed(&docker, 1).await;
    followed_exit(&docker, 0, Some(0));
    until("the exit to release the guard", || !guard.running()).await;

    assert_eq!(*docker.removed_images.lock().unwrap(), ["sha256:cs2-old"]);
    assert!(held_while_sweeping.load(Ordering::SeqCst), "the sweep ran with the guard held");
    assert_eq!(progress.lock().unwrap().last().unwrap().message, "removing images an update superseded");
    assert_eq!(state.get().unwrap().outcome, Some(Cs2UpdateOutcome::Completed));
}

/// An update whose container could not be removed keeps the guard. Its end still reaches the node
/// report through the guard's watch, though the guard stays held.
#[tokio::test]
async fn reports_an_update_whose_container_keeps_the_guard() {
    let docker = FakeDocker::new();
    let guard = guard(&docker);
    let release = guard.claim(MaintenanceOperation::Cs2Update).await.unwrap();
    let mut running = guard.subscribe();

    running.borrow_and_update();
    release.release(Some("ankor-update-stuck".to_owned()));
    assert!(running.has_changed().unwrap(), "the handover is reported");
    assert!(guard.running());
}

#[tokio::test]
async fn holds_the_guard_for_an_update_container_missing_its_labels_and_says_so_instead_of_reporting_it() {
    let docker = FakeDocker::new();
    let state = Cs2UpdateState::default();
    let progress: Arc<Mutex<Vec<InstallProgress>>> = Arc::default();
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let logged = Arc::clone(&lines);
    let reports = Arc::clone(&progress);
    let mut options = MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn DockerPort>);

    options.state = Some(state.clone());
    options.on_progress = Arc::new(move |report| reports.lock().unwrap().push(report));
    options.log = Logger::new(
        "update",
        Some(Arc::new(move |entry: Entry| {
            let level = entry.level.name();

            logged.lock().unwrap().push(format!("{level} {}", entry.message));
        })),
    );

    let guard = MaintenanceGuard::new(options);

    *docker.update_container.lock().unwrap() = Some(("ankor-update-cc33dd".to_owned(), None));
    guard.adopt_running().await.unwrap();
    assert!(guard.running());
    assert_eq!(state.get(), None);
    assert!(
        lines
            .lock()
            .unwrap()
            .contains(&"error [update] the update container ankor-update-cc33dd is missing required labels; its update is not reported".to_owned())
    );

    // Nothing names an update to tie its progress to, so its lines go to the log alone.
    until_followed(&docker, 1).await;

    let hooks = docker.followed.lock().unwrap()[0].hooks.clone();

    (hooks.on_line)(" Update state (0x61) downloading, progress: 42.00 (30 / 73)".to_owned());
    assert!(progress.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_claim_dropped_while_docker_answers_frees_the_guard() {
    let docker = FakeDocker::new();

    *docker.update_container_delay.lock().unwrap() = Some(std::time::Duration::from_mins(1));

    let guard = guard(&docker);

    assert!(tokio::time::timeout(std::time::Duration::from_millis(20), guard.claim(MaintenanceOperation::Cs2Update)).await.is_err());
    assert!(!guard.running());
}

#[tokio::test]
async fn a_claim_dropped_unreleased_frees_the_guard() {
    let docker = FakeDocker::new();
    let guard = guard(&docker);
    let release = guard.claim(MaintenanceOperation::Cs2Update).await.unwrap();

    assert!(guard.running());
    drop(release);
    assert!(!guard.running());
}
