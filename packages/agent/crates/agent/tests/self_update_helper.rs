//! The container swap, against a double of the Docker API client: the agent that asked for it is
//! gone by the time this runs, so the rollback is the only safety net.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::contract::constants::self_update_env;
use ankor_agent::docker_api::{
    ANSWER_WITHIN, ContainerInspect, ContainerState, DockerApiPort, DockerError, Filters, ImageInspect, ListedContainer, ListedImage, OutputStream,
    PendingExit, WaitCondition,
};
use ankor_agent::self_update_helper::{PREVIOUS_SUFFIX, SelfUpdateParams, SwapResult, params_from_env, run_helper, run_self_update};
use ankor_agent::testing::until;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::time::Instant;

const NAME: &str = "ankor-agent";
const IMAGE: &str = "ghcr.io/example/ankor-agent:v2";
const PREVIOUS: &str = "agent-container";
const REPLACEMENT: &str = "replacement";
const PARKED: &str = "ankor-agent.previous";
/// A failure message that the fake answers as a call that got no answer.
const NO_ANSWER: &str = "no answer";

#[derive(Clone, Debug, PartialEq)]
struct Call {
    container: String,
    action: String,
    detail: Option<String>,
}

#[derive(Default)]
struct FakeContainer {
    running: bool,
    /// Makes the replacement look like it died right after start.
    dies_immediately: bool,
    /// What the daemon reports after the restart policy brought a crashed container back.
    restart_count: i64,
}

type InspectHook = Arc<dyn Fn(&str) + Send + Sync>;

/// A daemon with the agent's container running and a replacement to create. A failure keyed
/// `id:action` fails every such call; `id:action:detail` only the one with that detail.
struct FakeDaemon {
    calls: Mutex<Vec<Call>>,
    /// Each failure's HTTP status, if it has one, and message.
    failures: Mutex<BTreeMap<String, (Option<u16>, String)>>,
    containers: Mutex<BTreeMap<String, FakeContainer>>,
    created: Mutex<Option<Value>>,
    /// Runs as each inspect begins.
    on_inspect: Mutex<Option<InspectHook>>,
    /// Makes every inspect report no state.
    no_state: Mutex<bool>,
    /// Every removal, with its force and volumes flags.
    removals: Mutex<Vec<(String, bool, bool)>>,
    /// A container an earlier swap left under the parked name, until it is removed.
    leftover: Mutex<bool>,
    /// Each container's name, by id, as creates and renames set it.
    names: Mutex<BTreeMap<String, String>>,
}

impl FakeDaemon {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            failures: Mutex::default(),
            containers: Mutex::new(BTreeMap::from([
                (PREVIOUS.to_owned(), FakeContainer { running: true, ..FakeContainer::default() }),
                (REPLACEMENT.to_owned(), FakeContainer { running: true, ..FakeContainer::default() }),
            ])),
            created: Mutex::default(),
            on_inspect: Mutex::default(),
            no_state: Mutex::default(),
            removals: Mutex::default(),
            leftover: Mutex::default(),
            names: Mutex::new(BTreeMap::from([(PREVIOUS.to_owned(), NAME.to_owned())])),
        })
    }

    /// The daemon as an earlier run of the helper left it: the old agent stopped, and parked under
    /// the other name when `parked`, with the replacement then holding the name; `None` when the
    /// old agent was removed.
    fn left_by_an_earlier_run(parked: Option<bool>) -> Arc<Self> {
        let daemon = Self::new();

        match parked {
            None => {
                daemon.containers.lock().unwrap().remove(PREVIOUS);
                daemon.names.lock().unwrap().remove(PREVIOUS);
                daemon.names.lock().unwrap().insert(REPLACEMENT.to_owned(), NAME.to_owned());
            }
            Some(parked) => {
                daemon.container(PREVIOUS, |container| container.running = false);

                if parked {
                    daemon.names.lock().unwrap().insert(PREVIOUS.to_owned(), PARKED.to_owned());
                    daemon.names.lock().unwrap().insert(REPLACEMENT.to_owned(), NAME.to_owned());
                } else {
                    daemon.containers.lock().unwrap().remove(REPLACEMENT);
                }
            }
        }

        daemon
    }

    fn fail(&self, key: &str, message: &str) {
        self.failures.lock().unwrap().insert(key.to_owned(), (None, message.to_owned()));
    }

    fn fail_with_status(&self, key: &str, status: u16, message: &str) {
        self.failures.lock().unwrap().insert(key.to_owned(), (Some(status), message.to_owned()));
    }

    fn container(&self, id: &str, change: impl FnOnce(&mut FakeContainer)) {
        change(self.containers.lock().unwrap().get_mut(id).unwrap());
    }

    fn running(&self, id: &str) -> bool {
        self.containers.lock().unwrap()[id].running
    }

    fn record(&self, container: &str, action: &str, detail: Option<&str>) -> Result<(), DockerError> {
        self.calls.lock().unwrap().push(Call { container: container.to_owned(), action: action.to_owned(), detail: detail.map(str::to_owned) });

        let failures = self.failures.lock().unwrap();
        let failure = failures.get(&format!("{container}:{action}")).or_else(|| failures.get(&format!("{container}:{action}:{}", detail.unwrap_or_default())));

        match failure {
            Some((None, message)) if message == NO_ANSWER => Err(DockerError::NoAnswer(ANSWER_WITHIN)),
            Some((Some(status), message)) => Err(DockerError::Api { status: *status, message: message.clone() }),
            Some((None, message)) => Err(DockerError::Http(message.clone())),
            None => Ok(()),
        }
    }

    fn actions(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|call| format!("{}:{}", call.container, call.action)).collect()
    }
}

#[async_trait]
impl DockerApiPort for FakeDaemon {
    async fn engine(&self) -> Result<(String, String), DockerError> {
        unimplemented!()
    }

    async fn create_container(&self, name: &str, spec: &Value) -> Result<String, DockerError> {
        *self.created.lock().unwrap() = Some(spec.clone());
        self.record(REPLACEMENT, "create", None)?;
        self.names.lock().unwrap().insert(REPLACEMENT.to_owned(), name.to_owned());

        Ok(REPLACEMENT.to_owned())
    }

    async fn start_container(&self, id: &str) -> Result<(), DockerError> {
        self.record(id, "start", None)?;
        self.container(id, |container| container.running = !container.dies_immediately);
        Ok(())
    }

    async fn stop_container(&self, id: &str, _timeout_seconds: i64) -> Result<(), DockerError> {
        self.record(id, "stop", None)?;
        self.container(id, |container| container.running = false);
        Ok(())
    }

    async fn remove_container(&self, id: &str, force: bool, volumes: bool) -> Result<(), DockerError> {
        self.removals.lock().unwrap().push((id.to_owned(), force, volumes));
        self.record(id, "remove", None)?;

        if id == PARKED {
            *self.leftover.lock().unwrap() = false;
        }

        Ok(())
    }

    async fn rename_container(&self, id: &str, name: &str) -> Result<(), DockerError> {
        self.record(id, "rename", Some(name))?;

        if name == PARKED && *self.leftover.lock().unwrap() {
            return Err(DockerError::Api { status: 409, message: format!("the name {PARKED} is already in use") });
        }

        self.names.lock().unwrap().insert(id.to_owned(), name.to_owned());
        Ok(())
    }

    async fn inspect_container(&self, id: &str) -> Result<ContainerInspect, DockerError> {
        let hook = self.on_inspect.lock().unwrap().clone();

        if let Some(hook) = hook {
            hook(id);
        }

        if let Some((_, message)) = self.failures.lock().unwrap().get(&format!("{id}:inspect")) {
            return Err(DockerError::Http(message.clone()));
        }

        // By id, or by the name a container holds, as the daemon looks one up.
        let names = self.names.lock().unwrap();
        let id = names.iter().find(|(_, name)| *name == id).map_or(id, |(found, _)| found.as_str());
        let containers = self.containers.lock().unwrap();
        let Some(container) = containers.get(id) else {
            return Err(DockerError::Api { status: 404, message: format!("No such container: {id}") });
        };

        Ok(ContainerInspect {
            id: id.to_owned(),
            name: format!("/{}", names.get(id).map_or(id, String::as_str)),
            image: "sha256:image".to_owned(),
            config: json!({ "Image": IMAGE }),
            host_config: None,
            state: (!*self.no_state.lock().unwrap()).then_some(ContainerState { running: container.running, exit_code: None }),
            restart_count: Some(container.restart_count),
        })
    }

    async fn list_containers(&self, _all: bool, _filters: Option<&Filters>) -> Result<Vec<ListedContainer>, DockerError> {
        unimplemented!()
    }

    async fn wait_container(&self, _id: &str, _condition: WaitCondition) -> Result<PendingExit, DockerError> {
        unimplemented!()
    }

    async fn attach_container(&self, _id: &str) -> Result<OutputStream, DockerError> {
        unimplemented!()
    }

    async fn inspect_image(&self, _reference: &str) -> Result<ImageInspect, DockerError> {
        unimplemented!()
    }

    async fn list_images(&self, _filters: Option<&Filters>) -> Result<Vec<ListedImage>, DockerError> {
        unimplemented!()
    }

    async fn remove_image(&self, _reference: &str) -> Result<(), DockerError> {
        unimplemented!()
    }

    async fn registry_digest(&self, _reference: &str) -> Result<String, DockerError> {
        unimplemented!()
    }

    async fn pull_image(&self, _reference: &str) -> Result<(), DockerError> {
        unimplemented!()
    }

    async fn copy_from_container(&self, _id: &str, _path: &str, _target: &Path) -> Result<(), DockerError> {
        unimplemented!()
    }
}

fn params() -> SelfUpdateParams {
    params_proving(Duration::from_millis(20))
}

fn params_proving(period: Duration) -> SelfUpdateParams {
    SelfUpdateParams { previous_id: PREVIOUS.to_owned(), spec: json!({ "name": NAME, "Image": IMAGE }), prove_running: Some(period) }
}

/// Records when each inspect began, from now.
fn record_looks(daemon: &FakeDaemon) -> Arc<Mutex<Vec<Duration>>> {
    let looked: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let looks = Arc::clone(&looked);
    let started = Instant::now();

    *daemon.on_inspect.lock().unwrap() = Some(Arc::new(move |_| looks.lock().unwrap().push(started.elapsed())));

    looked
}

fn seconds(millis: &[u64]) -> Vec<Duration> {
    millis.iter().map(|&millis| Duration::from_millis(millis)).collect()
}

mod the_self_update_swap {
    use super::*;

    #[tokio::test]
    async fn stops_renames_replaces_proves_it_stays_up_and_only_then_discards_the_old_container() {
        let daemon = FakeDaemon::new();
        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert_eq!(outcome, SwapResult { ok: true, message: format!("{NAME} now runs {IMAGE}") });
        assert_eq!(daemon.actions(), ["agent-container:stop", "agent-container:rename", "replacement:create", "replacement:start", "agent-container:remove"]);
        assert_eq!(daemon.calls.lock().unwrap()[1].detail.as_deref(), Some(format!("{NAME}{PREVIOUS_SUFFIX}").as_str()));

        let created = daemon.created.lock().unwrap().clone().unwrap();

        assert_eq!((created["name"].as_str(), created["Image"].as_str()), (Some(NAME), Some(IMAGE)));
    }

    #[tokio::test]
    async fn keeps_a_proven_swap_when_the_old_container_will_not_go_and_says_so() {
        let daemon = FakeDaemon::new();

        daemon.fail("agent-container:remove", "device or resource busy");

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(outcome.ok);
        assert!(outcome.message.contains(&format!("{NAME}{PREVIOUS_SUFFIX} could not be removed")));
        // The replacement stays; nothing is renamed back or restarted.
        assert_eq!(daemon.actions()[daemon.actions().len() - 2..], ["replacement:start", "agent-container:remove"]);
    }

    #[tokio::test]
    async fn puts_the_old_container_back_under_its_own_name_when_the_new_one_will_not_stay_up() {
        let daemon = FakeDaemon::new();

        daemon.container(REPLACEMENT, |container| container.dies_immediately = true);

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert_eq!(
            outcome,
            SwapResult {
                ok: false,
                message: format!("{NAME} was not replaced with {IMAGE}: the replacement container exited on its own (the previous container was restarted)"),
            }
        );
        assert_eq!(
            daemon.actions(),
            [
                "agent-container:stop",
                "agent-container:rename",
                "replacement:create",
                "replacement:start",
                "replacement:remove",
                "agent-container:rename",
                "agent-container:start"
            ]
        );
        // The name is free again, and the agent that asked for the update is the one running.
        let calls = daemon.calls.lock().unwrap().clone();

        assert_eq!(calls[calls.len() - 2].detail.as_deref(), Some(NAME));
        assert!(daemon.running(PREVIOUS));
    }

    #[tokio::test]
    async fn rolls_back_a_replacement_that_crashed_and_was_restarted_between_two_polls() {
        let daemon = FakeDaemon::new();

        daemon.container(REPLACEMENT, |container| container.restart_count = 1);

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert!(outcome.message.contains("restarted 1 time(s)"));
        assert_eq!(daemon.actions()[daemon.actions().len() - 3..], ["replacement:remove", "agent-container:rename", "agent-container:start"]);
    }

    #[tokio::test]
    async fn rolls_back_a_replacement_the_daemon_refused_to_create() {
        let daemon = FakeDaemon::new();

        daemon.fail("replacement:create", "no such image");

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert!(outcome.message.contains("no such image"));
        assert_eq!(
            daemon.actions(),
            ["agent-container:stop", "agent-container:rename", "replacement:create", "agent-container:rename", "agent-container:start"]
        );
    }

    #[tokio::test]
    async fn removes_a_container_an_earlier_swap_left_under_the_parked_name_and_swaps() {
        let daemon = FakeDaemon::new();

        *daemon.leftover.lock().unwrap() = true;

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(
            daemon.actions(),
            [
                "agent-container:stop",
                "agent-container:rename",
                &format!("{PARKED}:remove"),
                "agent-container:rename",
                "replacement:create",
                "replacement:start",
                "agent-container:remove"
            ]
        );
    }

    /// A create that got no answer may still make the container, which would keep the old one from
    /// getting its name back.
    #[tokio::test]
    async fn removes_by_name_a_replacement_whose_create_got_no_answer_before_restoring_the_old_container() {
        let daemon = FakeDaemon::new();

        daemon.fail("replacement:create", NO_ANSWER);

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert_eq!(
            daemon.actions(),
            [
                "agent-container:stop",
                "agent-container:rename",
                "replacement:create",
                &format!("{NAME}:remove"),
                "agent-container:rename",
                "agent-container:start"
            ]
        );
        assert!(daemon.running(PREVIOUS));
    }

    /// A 404 for the removal by name means the create never made the container, so the result
    /// notes only another failure.
    #[tokio::test]
    async fn notes_a_failed_removal_by_name_unless_no_replacement_was_made() {
        let note = "a replacement the daemon may have created could not be removed";
        let never_made = FakeDaemon::new();

        never_made.fail("replacement:create", NO_ANSWER);
        never_made.fail_with_status(&format!("{NAME}:remove"), 404, "no such container");

        let outcome = run_self_update(never_made.as_ref(), &params()).await;

        assert!(!outcome.message.contains(note), "{}", outcome.message);

        let stuck = FakeDaemon::new();

        stuck.fail("replacement:create", NO_ANSWER);
        stuck.fail_with_status(&format!("{NAME}:remove"), 500, "removal already in progress");

        let outcome = run_self_update(stuck.as_ref(), &params()).await;

        assert!(outcome.message.contains(&format!("{note}: ")), "{}", outcome.message);
    }

    /// Docker answers 304 to a start of a container that runs, as one whose stop got no answer
    /// and never happened does.
    #[tokio::test]
    async fn takes_a_304_to_the_restart_as_the_old_container_running() {
        let daemon = FakeDaemon::new();

        daemon.fail("agent-container:stop", NO_ANSWER);
        daemon.fail_with_status("agent-container:start", 304, "HTTP 304");

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert!(outcome.message.contains("the previous container was restarted"), "{}", outcome.message);
    }

    /// A stop that got no answer may still stop the old container later, which the restart policy
    /// would not undo.
    #[tokio::test]
    async fn starts_the_old_container_again_when_its_stop_got_no_answer() {
        let daemon = FakeDaemon::new();

        daemon.fail("agent-container:stop", NO_ANSWER);

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert!(outcome.message.contains("the previous container was restarted"), "{}", outcome.message);
        assert_eq!(daemon.actions(), ["agent-container:stop", "agent-container:start"]);
    }

    #[tokio::test]
    async fn restarts_the_old_container_when_the_rename_fails_because_a_docker_stop_defeats_unless_stopped() {
        let daemon = FakeDaemon::new();

        daemon.fail("agent-container:rename", "name already in use by ankor-agent.previous");

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert_eq!(
            outcome,
            SwapResult {
                ok: false,
                message: format!("{NAME} was not replaced with {IMAGE}: name already in use by ankor-agent.previous (the previous container was restarted)"),
            }
        );
        assert_eq!(daemon.actions(), ["agent-container:stop", "agent-container:rename", "agent-container:start"]);
        assert!(daemon.running(PREVIOUS));
    }

    #[tokio::test]
    async fn bails_out_when_the_container_it_would_replace_is_already_gone() {
        let daemon = FakeDaemon::new();

        daemon.fail_with_status("agent-container:stop", 404, "no such container");

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert!(outcome.message.contains("no such container"));
        assert!(outcome.message.contains("could not be stopped"));
        // The state the swap found is unknown, so it touches nothing.
        assert_eq!(daemon.actions(), ["agent-container:stop"]);
    }

    #[tokio::test]
    async fn says_so_when_even_the_rollback_failed_because_nothing_else_will() {
        let daemon = FakeDaemon::new();

        daemon.fail("replacement:start", "exec format error");
        daemon.fail("agent-container:start", "port already allocated");

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert!(outcome.message.contains("exec format error"));
        assert!(outcome.message.contains("the previous container is stopped"));
    }

    #[tokio::test]
    async fn still_restarts_the_old_container_when_the_rollback_cannot_clear_the_name_and_says_what_is_left() {
        let daemon = FakeDaemon::new();

        daemon.container(REPLACEMENT, |container| container.dies_immediately = true);
        daemon.fail("replacement:remove", "removal already in progress");
        daemon.fail(&format!("agent-container:rename:{NAME}"), "name in use");

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert_eq!(
            outcome.message,
            format!(
                "{NAME} was not replaced with {IMAGE}: the replacement container exited on its own (the previous container was restarted; the replacement container could not be removed: removal already in progress; the previous container keeps the name {NAME}{PREVIOUS_SUFFIX}: name in use)"
            )
        );
        assert!(daemon.running(PREVIOUS));
    }
}

mod one_agent_at_a_time {
    use super::*;

    // A new agent refuses to boot while another agent container runs, so a swap or a rollback that
    // started one before the other was stopped or removed would leave the node without either.
    #[tokio::test]
    async fn never_has_two_agent_containers_running_during_any_swap() {
        type Setup = fn(&FakeDaemon);

        let cases: [(&str, Setup); 6] = [
            ("a swap that holds", |_| {}),
            ("a replacement that dies immediately", |daemon| daemon.container(REPLACEMENT, |container| container.dies_immediately = true)),
            ("a replacement that crashed and was restarted", |daemon| daemon.container(REPLACEMENT, |container| container.restart_count = 1)),
            ("a replacement the daemon will not create", |daemon| daemon.fail("replacement:create", "no such image")),
            ("a replacement the daemon will not start", |daemon| daemon.fail("replacement:start", "port is already allocated")),
            ("a rename that fails", |daemon| daemon.fail("agent-container:rename", "name already in use")),
        ];

        for (case, setup) in cases {
            let daemon = FakeDaemon::new();

            setup(&daemon);
            run_self_update(daemon.as_ref(), &params()).await;

            // Replay the daemon's calls: the old agent runs at the start, nothing else does.
            let mut running = vec![PREVIOUS.to_owned()];

            for call in daemon.calls.lock().unwrap().iter() {
                match call.action.as_str() {
                    "stop" | "remove" => running.retain(|container| *container != call.container),
                    "start" => {
                        assert!(running.is_empty(), "{} started beside another agent during {case}", call.container);
                        running.push(call.container.clone());
                    }
                    _ => {}
                }
            }
        }
    }
}

mod the_self_update_swap_closer_up {
    use super::*;

    #[tokio::test]
    async fn takes_an_old_container_that_is_already_stopped_304_as_stopped_and_restarts_it_on_a_rollback() {
        let daemon = FakeDaemon::new();

        daemon.fail_with_status("agent-container:stop", 304, "HTTP 304");
        assert!(run_self_update(daemon.as_ref(), &params()).await.ok);

        let failed = FakeDaemon::new();

        failed.fail_with_status("agent-container:stop", 304, "HTTP 304");
        failed.fail("replacement:create", "no such image");
        assert!(!run_self_update(failed.as_ref(), &params()).await.ok);
        assert_eq!(failed.actions().last().unwrap(), "agent-container:start");
    }

    #[tokio::test]
    async fn gives_up_on_an_old_container_the_daemon_will_not_stop_for_any_other_reason_and_leaves_it_running() {
        let daemon = FakeDaemon::new();

        daemon.fail_with_status("agent-container:stop", 500, "HTTP 500");

        let outcome = run_self_update(daemon.as_ref(), &params()).await;

        assert!(!outcome.ok);
        assert_eq!(daemon.actions(), ["agent-container:stop"]);
    }

    #[tokio::test(start_paused = true)]
    async fn looks_at_the_replacement_once_a_second_through_the_proof() {
        let daemon = FakeDaemon::new();
        let looked = record_looks(&daemon);

        assert!(run_self_update(daemon.as_ref(), &params_proving(Duration::from_secs(5))).await.ok);
        assert_eq!(*looked.lock().unwrap(), seconds(&[1_000, 2_000, 3_000, 4_000, 5_000]));
    }

    #[tokio::test(start_paused = true)]
    async fn takes_its_last_look_at_the_end_of_the_proof() {
        let daemon = FakeDaemon::new();
        let looked = record_looks(&daemon);

        assert!(run_self_update(daemon.as_ref(), &params_proving(Duration::from_millis(2_500))).await.ok);
        assert_eq!(*looked.lock().unwrap(), seconds(&[1_000, 2_000, 2_500]));
    }

    #[tokio::test]
    async fn takes_a_replacement_the_daemon_reports_no_state_for_as_exited() {
        let daemon = FakeDaemon::new();

        *daemon.no_state.lock().unwrap() = true;
        assert!(run_self_update(daemon.as_ref(), &params()).await.message.contains("the replacement container exited on its own"));
    }

    #[tokio::test(start_paused = true)]
    async fn rolls_back_a_replacement_that_exits_during_the_proof_at_the_next_look_not_at_its_end() {
        let daemon = FakeDaemon::new();
        let looked = record_looks(&daemon);
        let swapping = Arc::clone(&daemon);
        let swap = tokio::spawn(async move { run_self_update(swapping.as_ref(), &params_proving(Duration::from_secs(10))).await });
        let started = Instant::now();

        tokio::time::sleep(Duration::from_millis(1_500)).await;
        daemon.container(REPLACEMENT, |container| container.running = false);
        tokio::time::sleep(Duration::from_millis(500)).await;
        until("the rollback", || daemon.actions().last().map(String::as_str) == Some("agent-container:start")).await;
        assert!(started.elapsed() < Duration::from_millis(2_100));
        assert_eq!(*looked.lock().unwrap(), seconds(&[1_000, 2_000]));
        assert!(!swap.await.unwrap().ok);
    }

    #[tokio::test]
    async fn removes_containers_by_force_and_never_with_their_volumes() {
        let swapped = FakeDaemon::new();
        let rolled_back = FakeDaemon::new();

        rolled_back.container(REPLACEMENT, |container| container.dies_immediately = true);
        run_self_update(swapped.as_ref(), &params()).await;
        run_self_update(rolled_back.as_ref(), &params()).await;

        let removals: Vec<_> = swapped.removals.lock().unwrap().iter().chain(rolled_back.removals.lock().unwrap().iter()).cloned().collect();

        assert_eq!(removals, [(PREVIOUS.to_owned(), true, false), (REPLACEMENT.to_owned(), true, false)]);
    }
}

mod the_helper_environment {
    use super::*;

    #[test]
    fn carries_the_swap_and_refuses_to_run_on_half_of_it() {
        let env = BTreeMap::from([
            (self_update_env::PREVIOUS_ID.to_owned(), PREVIOUS.to_owned()),
            (self_update_env::SPEC.to_owned(), json!({ "name": NAME, "Image": IMAGE }).to_string()),
        ]);
        let params = params_from_env(&env).unwrap();

        assert_eq!(params.previous_id, PREVIOUS);
        assert_eq!(params.spec, json!({ "name": NAME, "Image": IMAGE }));

        let mut without_spec = env.clone();

        without_spec.remove(self_update_env::SPEC);
        assert!(params_from_env(&without_spec).unwrap_err().to_string().contains(self_update_env::SPEC));

        let mut empty_previous = env;

        empty_previous.insert(self_update_env::PREVIOUS_ID.to_owned(), String::new());
        assert_eq!(params_from_env(&empty_previous).unwrap_err().to_string(), format!("{} is not set", self_update_env::PREVIOUS_ID));
    }
}

mod the_helper_run {
    use super::*;

    fn helper_env(failure_path: &Path) -> BTreeMap<String, String> {
        BTreeMap::from([
            (self_update_env::PREVIOUS_ID.to_owned(), PREVIOUS.to_owned()),
            (self_update_env::SPEC.to_owned(), json!({ "name": NAME, "Image": IMAGE }).to_string()),
            (self_update_env::FAILURE.to_owned(), failure_path.display().to_string()),
        ])
    }

    /// Runs the helper, returning its exit code and what it wrote to stderr.
    async fn run(env: &BTreeMap<String, String>, daemon: &FakeDaemon, prove: Duration) -> (i32, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_helper(env, daemon, Some(prove), &mut out, &mut err).await.unwrap();

        (code, String::from_utf8(err).unwrap())
    }

    #[tokio::test]
    async fn removes_the_failure_an_earlier_swap_recorded_once_its_own_swap_holds_and_exits_0() {
        let dir = tempfile::tempdir().unwrap();
        let failure_path = dir.path().join("agent-update-failed.json");

        std::fs::write(&failure_path, json!({ "image": "ghcr.io/example/ankor-agent:0", "detail": "rolled back" }).to_string()).unwrap();
        assert_eq!(run(&helper_env(&failure_path), &FakeDaemon::new(), Duration::from_millis(20)).await.0, 0);
        assert!(!failure_path.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn proves_the_replacement_for_as_long_as_it_is_told_and_has_no_earlier_failure_to_remove() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = FakeDaemon::new();
        let looked = record_looks(&daemon);
        let (code, errors) = run(&helper_env(&dir.path().join("agent-update-failed.json")), &daemon, Duration::from_millis(20)).await;

        assert_eq!(code, 0);
        // One look at the old agent, to tell where the swap stands, and one proof poll.
        assert_eq!(looked.lock().unwrap().len(), 2);
        assert_eq!(errors, "");
    }

    #[tokio::test]
    async fn leaves_a_rolled_back_swap_as_failed_with_its_reason_and_exits_0() {
        let dir = tempfile::tempdir().unwrap();
        let failure_path = dir.path().join("agent-update-failed.json");
        let daemon = FakeDaemon::new();

        daemon.container(REPLACEMENT, |container| container.dies_immediately = true);
        assert_eq!(run(&helper_env(&failure_path), &daemon, Duration::from_millis(20)).await.0, 0);

        let written: Value = serde_json::from_str(&std::fs::read_to_string(&failure_path).unwrap()).unwrap();
        let keys: Vec<&String> = written.as_object().unwrap().keys().collect();

        assert_eq!(keys, ["image", "outcome", "detail"]);
        assert_eq!((written["image"].as_str(), written["outcome"].as_str()), (Some(IMAGE), Some("failed")));
        assert!(written["detail"].as_str().unwrap().contains("exited on its own"));
    }

    #[tokio::test]
    async fn says_so_when_a_swap_that_held_cannot_remove_the_earlier_failure() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the failure file would be cannot be removed as a file.
        let failure_path = dir.path().join("agent-update-failed.json");

        std::fs::create_dir(&failure_path).unwrap();

        let (code, errors) = run(&helper_env(&failure_path), &FakeDaemon::new(), Duration::from_millis(20)).await;

        assert_eq!(code, 0);
        assert!(errors.contains(&format!("could not record the result in {}", failure_path.display())), "{errors}");
    }

    #[tokio::test]
    async fn keeps_its_exit_code_when_the_result_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let failure_path = dir.path().join("missing").join("agent-update-failed.json");
        let daemon = FakeDaemon::new();

        daemon.container(REPLACEMENT, |container| container.dies_immediately = true);

        let (code, errors) = run(&helper_env(&failure_path), &daemon, Duration::from_millis(20)).await;

        assert_eq!(code, 1);
        assert!(errors.contains(&format!("could not record the result in {}", failure_path.display())));
    }

    fn written(failure_path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(failure_path).unwrap()).unwrap()
    }

    /// A rerun finds the old agent parked and the replacement holding the name, so it removes the
    /// replacement, gives the old agent its name back and starts it. The build is not blamed.
    #[tokio::test]
    async fn rolls_back_the_swap_an_earlier_run_left_parked_and_lets_the_update_try_again() {
        let dir = tempfile::tempdir().unwrap();
        let failure_path = dir.path().join("agent-update-failed.json");
        let daemon = FakeDaemon::left_by_an_earlier_run(Some(true));

        assert_eq!(run(&helper_env(&failure_path), &daemon, Duration::from_millis(20)).await.0, 0);
        assert_eq!(daemon.actions(), [format!("{NAME}:remove"), "agent-container:rename".to_owned(), "agent-container:start".to_owned()]);
        assert_eq!(daemon.calls.lock().unwrap()[1].detail.as_deref(), Some(NAME));
        assert!(daemon.running(PREVIOUS));

        let written = written(&failure_path);

        assert_eq!(written["outcome"], "not_started");
        assert!(written["detail"].as_str().unwrap().contains("an earlier run of the helper stopped partway"), "{written}");
    }

    /// Stopped but still under its name, the old agent is only started again.
    #[tokio::test]
    async fn starts_the_old_agent_an_earlier_run_stopped_before_parking_it() {
        let dir = tempfile::tempdir().unwrap();
        let failure_path = dir.path().join("agent-update-failed.json");
        let daemon = FakeDaemon::left_by_an_earlier_run(Some(false));

        assert_eq!(run(&helper_env(&failure_path), &daemon, Duration::from_millis(20)).await.0, 0);
        assert_eq!(daemon.actions(), ["agent-container:start"]);
        assert!(daemon.running(PREVIOUS));
        assert_eq!(written(&failure_path)["outcome"], "not_started");
    }

    /// The old agent is removed only once the replacement held, so a rerun that finds it gone and
    /// the replacement running reports the swap as done.
    #[tokio::test]
    async fn reports_a_swap_an_earlier_run_finished_as_done() {
        let dir = tempfile::tempdir().unwrap();
        let failure_path = dir.path().join("agent-update-failed.json");

        std::fs::write(&failure_path, json!({ "image": "ghcr.io/example/ankor-agent:0", "detail": "rolled back" }).to_string()).unwrap();

        let daemon = FakeDaemon::left_by_an_earlier_run(None);

        assert_eq!(run(&helper_env(&failure_path), &daemon, Duration::from_millis(20)).await.0, 0);
        assert!(daemon.actions().is_empty(), "{:?}", daemon.actions());
        assert!(!failure_path.exists());

        daemon.container(REPLACEMENT, |container| container.running = false);
        assert_eq!(run(&helper_env(&failure_path), &daemon, Duration::from_millis(20)).await.0, 0);
        assert!(written(&failure_path)["detail"].as_str().unwrap().contains("the previous container is gone and no replacement runs"));
    }

    /// A rerun that cannot read the old agent records nothing and exits with an error, so Docker
    /// runs it again instead of leaving the old agent stopped.
    #[tokio::test]
    async fn exits_with_an_error_and_records_nothing_when_it_cannot_tell_where_the_swap_stands() {
        let dir = tempfile::tempdir().unwrap();
        let failure_path = dir.path().join("agent-update-failed.json");
        let daemon = FakeDaemon::left_by_an_earlier_run(Some(true));

        daemon.fail("agent-container:inspect", "Docker is restarting");

        let (code, errors) = run(&helper_env(&failure_path), &daemon, Duration::from_millis(20)).await;

        assert_eq!(code, 1);
        assert!(errors.contains("could not tell where the swap stands: Docker is restarting"), "{errors}");
        assert!(daemon.actions().is_empty(), "{:?}", daemon.actions());
        assert!(!failure_path.exists());
    }
}
