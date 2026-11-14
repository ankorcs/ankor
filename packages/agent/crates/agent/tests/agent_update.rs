//! What `agent_update` reports: a swap under way while its helper runs, and once it exits, failed
//! when the result file records a failure and completed otherwise. Without a running helper, the
//! file alone says whether the last swap failed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ankor_agent::agent_update::{AgentUpdateState, SwapFailure, SwapFailureOutcome, write_swap_failure};
use ankor_agent::docker::{DockerPort, SelfUpdateHelper};
use ankor_agent::logger::Logger;
use ankor_agent::testing::FakeDocker;
use ankor_protocol::constants::AGENT_UPDATE_DETAIL_MAX_CHARS;
use ankor_protocol::types::{AgentUpdate, AgentUpdateOutcome, ImageUpdateStep};

const IMAGE: &str = "ghcr.io/example/ankor-agent:2";
const OWN_IMAGE: &str = "ghcr.io/example/ankor-agent:1";
const HELPER: &str = "ankor-self-update-0c0ffe";

struct Setup {
    docker: Arc<FakeDocker>,
    failure_path: PathBuf,
    state: AgentUpdateState,
    changes: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}

fn state_at(docker: &Arc<FakeDocker>, failure_path: PathBuf, changes: &Arc<AtomicUsize>) -> AgentUpdateState {
    let counted = Arc::clone(changes);

    AgentUpdateState::new(
        Arc::clone(docker) as Arc<dyn DockerPort>,
        failure_path,
        OWN_IMAGE.to_owned(),
        Logger::null(),
        Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }),
    )
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let docker = FakeDocker::new();
    let failure_path = dir.path().join("agent-update-failed.json");
    let changes = Arc::new(AtomicUsize::new(0));
    let state = state_at(&docker, failure_path.clone(), &changes);

    Setup { docker, failure_path, state, changes, _dir: dir }
}

fn helper(image: Option<&str>, running: bool) -> SelfUpdateHelper {
    SelfUpdateHelper { name: HELPER.to_owned(), image: image.map(str::to_owned), running }
}

#[allow(clippy::unnecessary_wraps, reason = "compared with the state's Option")]
fn update(step: Option<ImageUpdateStep>, outcome: Option<AgentUpdateOutcome>, detail: Option<&str>) -> Option<AgentUpdate> {
    Some(AgentUpdate { image: IMAGE.to_owned(), step, outcome, detail: detail.map(str::to_owned) })
}

fn failure(outcome: SwapFailureOutcome, detail: &str) -> SwapFailure {
    SwapFailure { image: IMAGE.to_owned(), outcome, detail: detail.to_owned() }
}

#[tokio::test]
async fn reports_no_swap_when_no_helper_runs_and_no_failure_is_recorded() {
    let setup = setup();

    setup.state.load().await.unwrap();
    assert_eq!(setup.state.get(), None);
}

#[tokio::test]
async fn reports_the_failure_a_swap_recorded_cutting_a_long_reason_whatever_helper_is_left() {
    let setup = setup();
    let limit = usize::try_from(AGENT_UPDATE_DETAIL_MAX_CHARS).unwrap();

    *setup.docker.self_update_helpers.lock().unwrap() = vec![helper(Some(IMAGE), false)];
    write_swap_failure(&setup.failure_path, &failure(SwapFailureOutcome::Failed, &"x".repeat(limit + 5))).unwrap();
    setup.state.load().await.unwrap();
    assert_eq!(setup.state.get(), update(None, Some(AgentUpdateOutcome::Failed), Some(&"x".repeat(limit))));
}

#[tokio::test]
async fn records_no_failure_for_an_unreadable_file() {
    let setup = setup();

    std::fs::write(&setup.failure_path, "{").unwrap();
    setup.state.load().await.unwrap();
    assert_eq!(setup.state.get(), None);
}

#[tokio::test]
async fn reports_a_swap_still_proving_as_under_way_then_how_it_ended_once_the_helper_exits() {
    for recorded in [None, Some("rolled back")] {
        let setup = setup();

        *setup.docker.self_update_helpers.lock().unwrap() = vec![helper(Some(IMAGE), true)];
        setup.state.load().await.unwrap();
        assert_eq!(setup.state.get(), update(Some(ImageUpdateStep::Switching), None, None));
        assert_eq!(setup.docker.followed.lock().unwrap().iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(), [HELPER]);

        if let Some(detail) = recorded {
            write_swap_failure(&setup.failure_path, &failure(SwapFailureOutcome::Failed, detail)).unwrap();
        }

        let on_exit = setup.docker.followed.lock().unwrap()[0].hooks.on_exit.clone();

        on_exit(Some(i64::from(recorded.is_some())));
        // The helper's file is read off the exit watch's call.
        ankor_agent::testing::until("the swap's end reported", || setup.changes.load(Ordering::SeqCst) == 1).await;

        let ended = match recorded {
            None => update(None, Some(AgentUpdateOutcome::Completed), None),
            Some(detail) => update(None, Some(AgentUpdateOutcome::Failed), Some(detail)),
        };

        assert_eq!(setup.state.get(), ended);
    }
}

/// A swap this agent started whose helper fails without stopping it: this agent keeps running, and
/// only the helper's exit tells it the swap failed.
#[tokio::test]
async fn reports_how_a_swap_this_agent_started_ended_once_its_helper_exits() {
    let setup = setup();

    setup.state.started(IMAGE, ImageUpdateStep::Switching);
    setup.state.follow_helper(HELPER, IMAGE).await;
    write_swap_failure(&setup.failure_path, &failure(SwapFailureOutcome::Failed, "could not stop the old agent")).unwrap();

    let on_exit = setup.docker.followed.lock().unwrap()[0].hooks.on_exit.clone();

    on_exit(Some(1));
    ankor_agent::testing::until("the swap's end reported", || {
        setup.state.get() == update(None, Some(AgentUpdateOutcome::Failed), Some("could not stop the old agent"))
    })
    .await;
}

/// A helper that fails before it records a result, as a build that cannot start, is a failed swap
/// on record, so the image auto-update does not retry it; one that exits cleanly completed.
#[tokio::test]
async fn records_a_helper_that_exits_non_zero_without_a_result_as_failed() {
    let setup = setup();
    let detail = "the helper exited with 1 before recording a result";

    setup.state.started(IMAGE, ImageUpdateStep::Switching);
    setup.state.follow_helper(HELPER, IMAGE).await;

    let on_exit = setup.docker.followed.lock().unwrap()[0].hooks.on_exit.clone();

    on_exit(Some(1));
    ankor_agent::testing::until("the failure recorded", || setup.state.get() == update(None, Some(AgentUpdateOutcome::Failed), Some(detail))).await;

    let next = state_at(&setup.docker, setup.failure_path.clone(), &Arc::default());

    next.load().await.unwrap();
    assert_eq!(next.get(), setup.state.get());

    let clean = self::setup();

    clean.state.started(IMAGE, ImageUpdateStep::Switching);
    clean.state.follow_helper(HELPER, IMAGE).await;

    let on_exit = clean.docker.followed.lock().unwrap()[0].hooks.on_exit.clone();

    on_exit(Some(0));
    ankor_agent::testing::until("the swap completed", || clean.state.get() == update(None, Some(AgentUpdateOutcome::Completed), None)).await;
}

#[tokio::test]
async fn names_this_agents_own_image_for_a_running_helper_whose_spec_cannot_be_read() {
    let setup = setup();

    *setup.docker.self_update_helpers.lock().unwrap() = vec![helper(None, true)];
    setup.state.load().await.unwrap();

    let state = setup.state.get().unwrap();

    assert_eq!((state.image.as_str(), state.outcome), (OWN_IMAGE, None));
}

#[tokio::test]
async fn records_a_swap_whose_helper_would_not_start_as_failed_for_this_agent_and_the_next() {
    let setup = setup();
    let detail = "the self-update helper did not start: no space left on device";

    setup.state.record_failure(IMAGE, SwapFailureOutcome::Failed, detail);
    assert_eq!(setup.state.get(), update(None, Some(AgentUpdateOutcome::Failed), Some(detail)));
    assert_eq!(setup.changes.load(Ordering::SeqCst), 1);

    let next = state_at(&setup.docker, setup.failure_path.clone(), &Arc::default());

    next.load().await.unwrap();
    assert_eq!(next.get(), setup.state.get());
}

#[test]
fn reports_a_failure_it_could_not_write() {
    let setup = setup();
    let changes = Arc::new(AtomicUsize::new(0));

    std::fs::write(&setup.failure_path, "").unwrap();

    let state = state_at(&setup.docker, setup.failure_path.join("agent-update-failed.json"), &changes);

    state.record_failure(IMAGE, SwapFailureOutcome::Failed, "rolled back");
    assert_eq!(state.get(), update(None, Some(AgentUpdateOutcome::Failed), Some("rolled back")));
    assert_eq!(changes.load(Ordering::SeqCst), 1);
}

#[test]
fn follows_an_accepted_update_from_its_pull_to_its_swap_and_reports_each_step() {
    let setup = setup();

    setup.state.started(IMAGE, ImageUpdateStep::Pulling);
    assert_eq!(setup.state.get(), update(Some(ImageUpdateStep::Pulling), None, None));
    setup.state.switching();
    assert_eq!(setup.state.get(), update(Some(ImageUpdateStep::Switching), None, None));
    assert_eq!(setup.changes.load(Ordering::SeqCst), 2);
}

#[test]
fn reports_an_update_that_found_the_image_already_running_as_unchanged() {
    let setup = setup();

    setup.state.started(IMAGE, ImageUpdateStep::Pulling);
    setup.state.unchanged(IMAGE);
    assert_eq!(setup.state.get(), update(None, Some(AgentUpdateOutcome::Unchanged), None));
    assert_eq!(setup.changes.load(Ordering::SeqCst), 2);
}

#[test]
fn takes_no_switch_without_an_update_under_way_nor_after_one_ended() {
    let setup = setup();

    setup.state.switching();
    assert_eq!(setup.state.get(), None);
    setup.state.record_failure(IMAGE, SwapFailureOutcome::PullFailed, "manifest unknown");

    let before = setup.changes.load(Ordering::SeqCst);

    setup.state.switching();

    let state = setup.state.get().unwrap();

    assert_eq!((state.step, state.outcome), (None, Some(AgentUpdateOutcome::PullFailed)));
    assert_eq!(setup.changes.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn keeps_a_failed_pull_apart_from_a_failed_swap_for_this_agent_and_the_next() {
    let setup = setup();
    let detail = format!("pulling {IMAGE} failed: manifest unknown");

    setup.state.record_failure(IMAGE, SwapFailureOutcome::PullFailed, &detail);
    assert_eq!(setup.state.get(), update(None, Some(AgentUpdateOutcome::PullFailed), Some(&detail)));

    let next = state_at(&setup.docker, setup.failure_path.clone(), &Arc::default());

    next.load().await.unwrap();
    assert_eq!(next.get(), setup.state.get());
}

#[tokio::test]
async fn reports_each_step_the_same_through_the_report_the_swap_is_given() {
    use ankor_agent::self_update::AgentUpdateReport;

    let setup = setup();
    let report: &dyn AgentUpdateReport = &setup.state;

    report.started(IMAGE, ImageUpdateStep::Pulling);
    report.switching();
    assert_eq!(setup.state.get(), update(Some(ImageUpdateStep::Switching), None, None));
    report.unchanged(IMAGE);
    assert_eq!(setup.state.get(), update(None, Some(AgentUpdateOutcome::Unchanged), None));
    report.started(IMAGE, ImageUpdateStep::Pulling);
    report.record_failure(IMAGE, SwapFailureOutcome::PullFailed, "manifest unknown");
    assert_eq!(setup.state.get(), update(None, Some(AgentUpdateOutcome::PullFailed), Some("manifest unknown")));
    report.follow_helper(HELPER, IMAGE).await;
    assert_eq!(setup.docker.followed.lock().unwrap().iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(), [HELPER]);
}
