//! `update_agent` and `update_cs2_image`: what moves the node to another tag, what they refuse,
//! and the image sweep that removes what an update superseded.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ankor_agent::agent_update::SwapFailureOutcome;
use ankor_agent::config::{ConfigStore, SharedConfig};
use ankor_agent::contract::constants::{SELF_UPDATE_COMMAND, self_update_env};
use ankor_agent::docker::{DockerPort, ImageDefaults, ImageSummary, InspectedContainer, SelfUpdateHelper, UpdateKind};
use ankor_agent::failure::OpError;
use ankor_agent::install::{InstallOptions, MaintenanceGuard, MaintenanceGuardOptions, MaintenanceHolder, update_cs2};
use ankor_agent::logger::{Entry, Logger};
use ankor_agent::overlay::OverlayPort;
use ankor_agent::self_update::{
    AgentUpdateDeps, AgentUpdateReport, Cs2ImageUpdateDeps, Cs2ImageUpdateState, ImageUpdateReport, prune_superseded_images, update_agent, update_cs2_image,
};
use ankor_agent::testing::fixtures::agent_config;
use ankor_agent::testing::{FakeDocker, FakeOverlay, until, wire};
use ankor_protocol::constants::CS2_UPDATE_DETAIL_MAX_CHARS;
use ankor_protocol::types::{
    AgentErrorCode, Cs2ImageUpdate, Cs2ImageUpdateOutcome, Cs2UpdateMode, ImageUpdateStep, MaintenanceOperation, UpdateAgentReply, UpdateAgentReplyStep,
    UpdateCs2ImageReply,
};
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

const CS2: &str = "ghcr.io/example/ankor-cs2";
const AGENT: &str = "ghcr.io/example/ankor-agent";
/// A path nothing writes: a swap's result is left to its helper.
const DATA_DIR: &str = "/nonexistent/ankor-data";

fn original() -> String {
    format!(
        "# agent.yml for an Ankor node.
token: 0123456789abcdef0123456789abcdef
controller:
  host: cs.example.com
  tls: true
cs2_image: {CS2}:v1
agent_image: {AGENT}:v1
slots:
  # the only slot on this box
  - id: a
    port: 27015
    tv_port: 27020
    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001
"
    )
}

fn inspected(image_id: &str) -> InspectedContainer {
    InspectedContainer {
        id: "agent-container".to_owned(),
        name: "ankor-agent".to_owned(),
        image_id: image_id.to_owned(),
        image: format!("{AGENT}:v1"),
        config: json!({
            // As the daemon reports it: the image's ENV and LABEL merged with what `docker run` added.
            "Env": ["PATH=/usr/local/bin:/usr/bin", "NODE_ENV=production", "TZ=Europe/Copenhagen"],
            "Labels": { "ankor.image": "agent", "ankor.node": "basement-box", "ankor.role": "agent" },
            "Entrypoint": ["node", "/app/build/main.js"],
            "Cmd": null,
            "User": "",
            "WorkingDir": "/app",
        }),
        host_config: json!({
            "Binds": ["/var/run/docker.sock:/var/run/docker.sock", "/srv/ankor:/srv/ankor"],
            "NetworkMode": "host",
            "RestartPolicy": { "Name": "unless-stopped" },
            // As an agent that node-setup.sh started before the log options had it.
            "LogConfig": { "Type": "local", "Config": {} },
            "CpuShares": 2598,
        }),
    }
}

/// What the requests reported of agent updates and cs2 image switches, in order.
#[derive(Default)]
struct Reports {
    agent: Mutex<Vec<String>>,
    /// Swaps that failed, as recorded.
    failed: Mutex<Vec<(String, SwapFailureOutcome, String)>>,
    image: Mutex<Vec<String>>,
    /// The helpers a swap asked to have followed.
    followed: Mutex<Vec<String>>,
}

#[async_trait]
impl AgentUpdateReport for Reports {
    fn started(&self, image: &str, step: ImageUpdateStep) {
        self.agent.lock().unwrap().push(format!("{} {image}", wire(step)));
    }

    fn switching(&self) {
        self.agent.lock().unwrap().push(wire(ImageUpdateStep::Switching));
    }

    fn unchanged(&self, image: &str) {
        self.agent.lock().unwrap().push(format!("{} {image}", wire(UpdateAgentReplyStep::Unchanged)));
    }

    fn record_failure(&self, image: &str, outcome: SwapFailureOutcome, detail: &str) {
        self.agent.lock().unwrap().push(format!("{} {image}", wire(outcome)));
        self.failed.lock().unwrap().push((image.to_owned(), outcome, detail.to_owned()));
    }

    async fn follow_helper(&self, helper: &str, _image: &str) {
        self.followed.lock().unwrap().push(helper.to_owned());
    }
}

impl ImageUpdateReport for Reports {
    fn started(&self, image: &str, step: ImageUpdateStep) {
        self.image.lock().unwrap().push(format!("{} {image}", wire(step)));
    }

    fn switching(&self) {
        self.image.lock().unwrap().push(wire(ImageUpdateStep::Switching));
    }

    fn ended(&self, outcome: Cs2ImageUpdateOutcome, detail: Option<&str>) {
        self.image.lock().unwrap().push(detail.map_or_else(|| wire(outcome), |detail| format!("{}: {detail}", wire(outcome))));
    }
}

/// Work owed once a reply has left; here it waits for the test to run it.
#[derive(Clone, Default)]
struct Owed {
    queue: Arc<Mutex<VecDeque<BoxFuture<'static, ()>>>>,
    scheduled: Arc<AtomicUsize>,
}

impl Owed {
    fn schedule(&self) -> ankor_agent::self_update::Schedule {
        let owed = self.clone();

        Arc::new(move |work| {
            owed.scheduled.fetch_add(1, Ordering::SeqCst);
            owed.queue.lock().unwrap().push_back(work);
        })
    }

    fn count(&self) -> usize {
        self.scheduled.load(Ordering::SeqCst)
    }

    /// Runs the oldest work owed.
    async fn run_next(&self) {
        let work = self.queue.lock().unwrap().pop_front();

        if let Some(work) = work {
            work.await;
        }
    }

    /// Runs the work owed since the last call, in order.
    async fn run_owed(&self) {
        while !self.queue.lock().unwrap().is_empty() {
            self.run_next().await;
        }
    }
}

struct Setup {
    config: SharedConfig,
    store: ConfigStore,
    docker: Arc<FakeDocker>,
    overlay: Arc<FakeOverlay>,
    updates: MaintenanceGuard,
    reports: Arc<Reports>,
    owed: Owed,
    /// The images launches hold, as the slot manager reports them.
    held: Arc<Mutex<Vec<String>>>,
    file: PathBuf,
    data_dir: PathBuf,
    _dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.yml");
        let docker = FakeDocker::new();

        std::fs::write(&file, original()).unwrap();
        // The agent mounts agent.yml where this process reads it; the test's copy lives in a temp dir.
        *docker.agent_container.lock().unwrap() = Some(inspected("sha256:agent-v1"));
        docker.image_defaults_by_id.lock().unwrap().insert(
            "sha256:agent-v1".to_owned(),
            ImageDefaults {
                env: vec!["PATH=/usr/local/bin:/usr/bin".to_owned(), "NODE_ENV=production".to_owned()],
                labels: BTreeMap::from([("ankor.image".to_owned(), "agent".to_owned())]),
            },
        );
        docker.images.lock().unwrap().extend([(format!("{CS2}:v1"), "sha256:cs2-v1".to_owned()), (format!("{AGENT}:v1"), "sha256:agent-v1".to_owned())]);

        let config: SharedConfig = Arc::new(Mutex::new(agent_config(&json!({ "cs2_image": format!("{CS2}:v1"), "agent_image": format!("{AGENT}:v1") }))));

        Self {
            store: ConfigStore::over(file.clone(), Arc::clone(&config)),
            config,
            updates: MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn DockerPort>)),
            docker,
            overlay: FakeOverlay::new(true),
            reports: Arc::default(),
            owed: Owed::default(),
            held: Arc::default(),
            file,
            data_dir: PathBuf::from(DATA_DIR),
            _dir: dir,
        }
    }

    fn agent_deps(&self) -> AgentUpdateDeps {
        AgentUpdateDeps {
            config: self.store.clone(),
            docker: Arc::clone(&self.docker) as Arc<dyn DockerPort>,
            data_dir: self.data_dir.clone(),
            agent_update: Arc::clone(&self.reports) as Arc<dyn AgentUpdateReport>,
            updates: self.updates.clone(),
            log: Logger::null(),
            schedule: self.owed.schedule(),
        }
    }

    fn image_deps(&self) -> Cs2ImageUpdateDeps {
        Cs2ImageUpdateDeps {
            config: self.store.clone(),
            docker: Arc::clone(&self.docker) as Arc<dyn DockerPort>,
            overlay: Arc::clone(&self.overlay) as Arc<dyn OverlayPort>,
            updates: self.updates.clone(),
            log: Logger::null(),
            state: Arc::clone(&self.reports) as Arc<dyn ImageUpdateReport>,
            schedule: self.owed.schedule(),
            held_images: {
                let held = Arc::clone(&self.held);

                Arc::new(move || held.lock().unwrap().clone())
            },
        }
    }

    async fn agent(&self, image: &str, pull: bool) -> Result<UpdateAgentReply, OpError> {
        update_agent(image, pull, &self.agent_deps()).await
    }

    async fn cs2(&self, image: &str, pull: bool) -> Result<UpdateCs2ImageReply, OpError> {
        update_cs2_image(image, pull, &self.image_deps()).await
    }

    fn yaml(&self) -> String {
        std::fs::read_to_string(&self.file).unwrap()
    }

    fn agent_updates(&self) -> Vec<String> {
        self.reports.agent.lock().unwrap().clone()
    }

    fn image_updates(&self) -> Vec<String> {
        self.reports.image.lock().unwrap().clone()
    }

    fn failed_swaps(&self) -> Vec<(String, SwapFailureOutcome, String)> {
        self.reports.failed.lock().unwrap().clone()
    }

    fn detached(&self) -> Vec<(String, Value)> {
        self.docker.detached.lock().unwrap().clone()
    }

    fn pulled(&self) -> Vec<String> {
        self.docker.pulled.lock().unwrap().clone()
    }

    /// The pair the tag `v2` resolves to on the host once it is pulled.
    fn pulls_to(&self, cs2_id: &str, agent_id: &str) {
        let mut pull_ids = self.docker.pull_ids.lock().unwrap();

        pull_ids.insert(format!("{CS2}:v2"), cs2_id.to_owned());
        pull_ids.insert(format!("{AGENT}:v2"), agent_id.to_owned());
        drop(pull_ids);
        // An agent image carries its label, which the swap checks before it starts the helper.
        self.docker
            .image_defaults_by_id
            .lock()
            .unwrap()
            .insert(agent_id.to_owned(), ImageDefaults { env: Vec::new(), labels: BTreeMap::from([("ankor.image".to_owned(), "agent".to_owned())]) });
    }
}

fn code(error: &OpError) -> Option<AgentErrorCode> {
    error.refusal().map(|failure| failure.code)
}

fn agent_reply(image: &str, step: UpdateAgentReplyStep) -> UpdateAgentReply {
    UpdateAgentReply { image: image.to_owned(), step }
}

fn image_reply(image: &str, step: ImageUpdateStep) -> UpdateCs2ImageReply {
    UpdateCs2ImageReply { image: image.to_owned(), step }
}

#[tokio::test]
async fn replies_immediately_that_it_pulls_then_pulls_and_swaps_the_agent_without_preparing_the_game_image() {
    let setup = Setup::new();
    let image = format!("{AGENT}:v2");

    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v2");
    assert_eq!(setup.agent(&image, true).await.unwrap(), agent_reply(&image, UpdateAgentReplyStep::Pulling));
    // Nothing waits for the pull, which runs once the reply has left.
    assert!(setup.pulled().is_empty());

    setup.owed.run_owed().await;
    assert_eq!(setup.pulled(), std::slice::from_ref(&image));
    assert_eq!(setup.config.lock().unwrap().cs2_image, format!("{CS2}:v1"));
    assert!(setup.yaml().contains(&format!("cs2_image: {CS2}:v1")));
    assert!(setup.yaml().contains(&format!("agent_image: {AGENT}:v2")));
    assert_eq!(*setup.overlay.calls.lock().unwrap(), 0);
    assert_eq!(setup.detached().len(), 1);
    assert_eq!(setup.agent_updates(), [format!("{} {image}", wire(ImageUpdateStep::Pulling)), wire(ImageUpdateStep::Switching)]);
    // The helper's exit says how the swap ended, if this agent is still there to hear it.
    assert_eq!(*setup.reports.followed.lock().unwrap(), ["helper-1"]);
}

/// An image that is no agent's is refused once pulled, before anything changes: no helper
/// starts, agent.yml keeps the running image, and the guard is free again.
#[tokio::test]
async fn refuses_to_swap_to_an_image_that_is_not_an_agent_image() {
    let setup = Setup::new();
    let image = format!("{AGENT}:v2");

    setup.docker.pull_ids.lock().unwrap().insert(image.clone(), "sha256:not-an-agent".to_owned());
    setup.agent(&image, true).await.unwrap();
    setup.owed.run_owed().await;

    assert!(setup.detached().is_empty());
    assert!(setup.yaml().contains(&format!("agent_image: {AGENT}:v1")));
    assert!(!setup.updates.running());

    let failed = setup.failed_swaps();

    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].1, SwapFailureOutcome::NotStarted);
    assert!(failed[0].2.contains("is not an Ankor agent image"), "{:?}", failed[0].2);
}

#[tokio::test]
async fn replies_unchanged_without_pulling_when_the_registry_still_names_the_running_image() {
    let setup = Setup::new();
    let image = format!("{AGENT}:v2");

    setup.docker.registry.lock().unwrap().insert(image.clone(), "sha256:agent-v1".to_owned());
    assert_eq!(setup.agent(&image, true).await.unwrap(), agent_reply(&image, UpdateAgentReplyStep::Unchanged));
    assert!(setup.pulled().is_empty());
    assert_eq!(setup.owed.count(), 0);
    assert!(!setup.updates.running());
    assert_eq!(setup.agent_updates(), [format!("unchanged {image}")]);
    assert!(setup.yaml().contains(&format!("agent_image: {image}")));
}

#[tokio::test]
async fn updates_only_the_plugin_without_pulling_or_replacing_the_agent() {
    let setup = Setup::new();
    let image = format!("{CS2}:v2");

    setup.docker.pull_ids.lock().unwrap().insert(image.clone(), "sha256:cs2-v2".to_owned());
    assert_eq!(setup.cs2(&image, true).await.unwrap(), image_reply(&image, ImageUpdateStep::Pulling));
    setup.owed.run_owed().await;
    assert_eq!(setup.pulled(), std::slice::from_ref(&image));
    assert_eq!(setup.config.lock().unwrap().agent_image, format!("{AGENT}:v1"));
    assert!(setup.yaml().contains(&format!("agent_image: {AGENT}:v1")));
    assert!(setup.yaml().contains(&format!("cs2_image: {image}")));
    assert_eq!(*setup.overlay.calls.lock().unwrap(), 1);
    assert_eq!(*setup.overlay.images.lock().unwrap(), ["sha256:cs2-v2"]);
    // No swap: the switch ends in cs2.image_update, in the node_status the guard's release sends.
    assert!(setup.detached().is_empty());
    assert_eq!(
        setup.image_updates(),
        [format!("{} {image}", wire(ImageUpdateStep::Pulling)), wire(ImageUpdateStep::Switching), wire(Cs2ImageUpdateOutcome::Completed)]
    );
}

#[tokio::test]
async fn holds_the_maintenance_guard_from_its_reply_until_the_plugin_update_ends() {
    let setup = Setup::new();
    let image = format!("{CS2}:v2");

    setup.docker.pull_ids.lock().unwrap().insert(image.clone(), "sha256:cs2-v2".to_owned());
    setup.cs2(&image, true).await.unwrap();
    assert!(setup.updates.running());
    setup.owed.run_owed().await;
    assert!(!setup.updates.running());
}

fn ours(id: &str, kind: &str, tags: &[String]) -> (ImageSummary, BTreeMap<String, String>) {
    (ImageSummary { id: id.to_owned(), repo_tags: tags.to_vec() }, BTreeMap::from([("ankor.image".to_owned(), kind.to_owned())]))
}

#[tokio::test]
async fn removes_the_image_a_plugin_update_superseded_unless_a_running_game_still_uses_it() {
    let setup = Setup::new();
    let image = format!("{CS2}:v2");

    setup.docker.pull_ids.lock().unwrap().insert(image.clone(), "sha256:cs2-v2".to_owned());
    *setup.docker.image_list.lock().unwrap() = vec![
        ours("sha256:cs2-v2", "cs2", std::slice::from_ref(&image)),
        ours("sha256:cs2-v1", "cs2", &[format!("{CS2}:v1")]),
        ours("sha256:cs2-v0", "cs2", &[format!("{CS2}:v0")]),
    ];
    *setup.docker.image_ids_in_use.lock().unwrap() = vec!["sha256:cs2-v1".to_owned()];

    setup.cs2(&image, true).await.unwrap();
    setup.owed.run_owed().await;
    assert_eq!(*setup.docker.removed_images.lock().unwrap(), [format!("{CS2}:v0")]);
}

/// A launch that picked its slot before the switch creates its container from the old image
/// after it, so the sweep keeps that image while the launch holds it.
#[tokio::test]
async fn keeps_the_image_a_launch_picked_before_the_switch_and_has_no_container_from_yet() {
    let setup = Setup::new();
    let image = format!("{CS2}:v2");

    setup.docker.pull_ids.lock().unwrap().insert(image.clone(), "sha256:cs2-v2".to_owned());
    *setup.docker.image_list.lock().unwrap() = vec![
        ours("sha256:cs2-v2", "cs2", std::slice::from_ref(&image)),
        ours("sha256:cs2-v1", "cs2", &[format!("{CS2}:v1")]),
        ours("sha256:cs2-v0", "cs2", &[format!("{CS2}:v0")]),
    ];
    *setup.held.lock().unwrap() = vec![format!("{CS2}:v1")];

    setup.cs2(&image, true).await.unwrap();
    setup.owed.run_owed().await;
    assert_eq!(*setup.docker.removed_images.lock().unwrap(), [format!("{CS2}:v0")]);
}

#[tokio::test]
async fn saves_the_plugin_image_only_after_successful_overlay_preparation() {
    let setup = Setup::new();
    let image = format!("{CS2}:v2");

    setup.docker.pull_ids.lock().unwrap().insert(image.clone(), "sha256:cs2-v2".to_owned());
    setup.overlay.close();
    setup.cs2(&image, true).await.unwrap();

    let owed = setup.owed.clone();
    let updating = tokio::spawn(async move { owed.run_owed().await });

    until("overlay preparation", || *setup.overlay.calls.lock().unwrap() == 1).await;
    assert_eq!(setup.config.lock().unwrap().cs2_image, format!("{CS2}:v1"));
    assert_eq!(setup.yaml(), original());
    setup.overlay.gate.add_permits(1);
    updating.await.unwrap();
    assert_eq!(setup.config.lock().unwrap().cs2_image, image);
    assert!(setup.yaml().contains(&format!("cs2_image: {image}")));
}

#[tokio::test]
async fn keeps_the_saved_plugin_image_when_extraction_fails_and_allows_retry() {
    let setup = Setup::new();
    let image = format!("{CS2}:v2");

    setup.docker.pull_ids.lock().unwrap().insert(image.clone(), "sha256:cs2-v2".to_owned());
    *setup.overlay.error.lock().unwrap() = Some("extraction failed".to_owned());
    setup.cs2(&image, true).await.unwrap();
    setup.owed.run_owed().await;
    assert_eq!(setup.image_updates().last().unwrap(), "overlay_failed: extracting the addon overlay failed: extraction failed");
    assert_eq!(setup.config.lock().unwrap().cs2_image, format!("{CS2}:v1"));
    assert_eq!(setup.yaml(), original());

    *setup.overlay.error.lock().unwrap() = None;
    assert_eq!(setup.cs2(&image, false).await.unwrap(), image_reply(&image, ImageUpdateStep::Switching));
    setup.owed.run_owed().await;
    assert_eq!(setup.config.lock().unwrap().cs2_image, image);
    assert!(setup.yaml().contains(&format!("cs2_image: {image}")));
}

#[tokio::test]
async fn keeps_the_container_when_the_pulled_reference_resolves_to_the_agent_image_already_running_and_still_moves_agent_yml_to_it() {
    let setup = Setup::new();
    let image = format!("{AGENT}:v2");

    setup.pulls_to("sha256:cs2-v1", "sha256:agent-v1");

    // The registry cannot tell here, so only the pull shows the image is the running one.
    assert_eq!(setup.agent(&image, true).await.unwrap(), agent_reply(&image, UpdateAgentReplyStep::Pulling));
    setup.owed.run_owed().await;
    assert_eq!(setup.pulled(), std::slice::from_ref(&image));
    assert!(setup.detached().is_empty());
    assert_eq!(setup.agent_updates(), [format!("{} {image}", wire(ImageUpdateStep::Pulling)), format!("{} {image}", wire(UpdateAgentReplyStep::Unchanged))]);
    assert_eq!(*setup.overlay.calls.lock().unwrap(), 0);
    assert!(setup.yaml().contains(&format!("cs2_image: {CS2}:v1")));
    assert!(setup.yaml().contains(&format!("agent_image: {image}")));
}

#[tokio::test]
async fn extracts_the_overlay_for_a_reference_that_was_reloaded_under_the_same_name_before_the_request() {
    // `docker load` moved `v1` to new bits already: the tag's id says "current" while the overlay on
    // disk is for the old build.
    let setup = Setup::new();
    let image = format!("{CS2}:v1");

    setup
        .docker
        .images
        .lock()
        .unwrap()
        .extend([(image.clone(), "sha256:cs2-v1-rebuilt".to_owned()), (format!("{AGENT}:v1"), "sha256:agent-v1-rebuilt".to_owned())]);
    assert_eq!(setup.cs2(&image, false).await.unwrap(), image_reply(&image, ImageUpdateStep::Switching));
    setup.owed.run_owed().await;
    assert_eq!(*setup.overlay.calls.lock().unwrap(), 1);
    assert_eq!(*setup.overlay.images.lock().unwrap(), ["sha256:cs2-v1-rebuilt"]);
    assert!(setup.detached().is_empty());
}

#[tokio::test]
async fn updates_agent_image_in_agent_yml_without_disturbing_the_rest_of_the_file() {
    let setup = Setup::new();

    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v2");
    setup.agent(&format!("{AGENT}:v2"), true).await.unwrap();
    setup.owed.run_owed().await;

    let yaml = setup.yaml();

    assert!(yaml.contains(&format!("cs2_image: {CS2}:v1")));
    assert!(yaml.contains(&format!("agent_image: {AGENT}:v2")));
    assert!(yaml.contains("# agent.yml for an Ankor node."));
    assert!(yaml.contains("# the only slot on this box"));
    assert_eq!(setup.config.lock().unwrap().cs2_image, format!("{CS2}:v1"));
    assert_eq!(setup.config.lock().unwrap().agent_image, format!("{AGENT}:v2"));
    assert_eq!(*setup.overlay.calls.lock().unwrap(), 0);
}

fn install_options(setup: &Setup) -> InstallOptions {
    InstallOptions {
        config: setup.store.reader(),
        docker: Arc::clone(&setup.docker) as Arc<dyn DockerPort>,
        on_progress: Arc::new(|_| {}),
        on_log: None,
        log: Logger::null(),
        any_busy: Arc::new(|| false),
        updates: setup.updates.clone(),
        mode: Cs2UpdateMode::Update,
        automatic: false,
        state: None,
        overlay: Arc::clone(&setup.overlay) as Arc<dyn OverlayPort>,
        short_id: None,
    }
}

#[tokio::test]
async fn starts_the_helper_only_once_the_reply_has_left_and_holds_the_guard_until_it_has() {
    let setup = Setup::new();
    let image = format!("{AGENT}:v2");

    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v2");
    *setup.docker.self_update_helpers.lock().unwrap() =
        vec![SelfUpdateHelper { name: "ankor-self-update-0c0ffe".to_owned(), image: Some(format!("{AGENT}:v1")), running: false }];
    setup.agent(&image, true).await.unwrap();

    assert!(setup.detached().is_empty());
    assert_eq!(setup.updates.holder(), Some(MaintenanceHolder { operation: MaintenanceOperation::AgentUpdate, detail: "agent update".to_owned() }));
    // The last swap's helper goes before anything else, so the one on the host afterwards is this swap's.
    assert!(setup.docker.self_update_helpers.lock().unwrap().is_empty());
    // The guard is still this agent's, so nothing else can start an update in the gap.
    assert!(update_cs2(install_options(&setup)).await.unwrap_err().to_string().contains("is already running"));

    setup.owed.run_next().await;

    let (name, helper) = setup.detached()[0].clone();
    let suffix = name.strip_prefix("ankor-self-update-").unwrap();

    assert_eq!(helper["Image"], image.as_str());
    assert_eq!(helper["name"], name.as_str());
    assert!(suffix.len() == 6 && suffix.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')), "{name}");
    assert_eq!(helper["Cmd"], json!(SELF_UPDATE_COMMAND));
    assert_eq!(helper["Labels"], json!({ "ankor.role": "self-update" }));

    let host_config = &helper["HostConfig"];

    // The exited helper is the swap's record, so it stays until the next swap.
    assert!(host_config.get("AutoRemove").is_none());
    // Docker restarts a helper that crashes or runs out of memory, and the rerun ends the swap.
    assert_eq!(host_config["RestartPolicy"], json!({ "Name": "on-failure", "MaximumRetryCount": 3 }));
    assert_eq!(host_config["Binds"], json!(["/var/run/docker.sock:/var/run/docker.sock", "/nonexistent/ankor-data:/nonexistent/ankor-data"]));
    assert_eq!(host_config["LogConfig"], json!({ "Type": "local", "Config": { "mode": "non-blocking", "max-buffer-size": "4m" } }));

    let env: Vec<String> = serde_json::from_value(helper["Env"].clone()).unwrap();

    assert!(env.contains(&format!("{}=agent-container", self_update_env::PREVIOUS_ID)));
    assert!(env.contains(&format!("{}=/nonexistent/ankor-data/agent-update-failed.json", self_update_env::FAILURE)));

    let prefix = format!("{}=", self_update_env::SPEC);
    let spec: Value = serde_json::from_str(env.iter().find_map(|entry| entry.strip_prefix(&prefix)).unwrap()).unwrap();
    let mut keys: Vec<&String> = spec.as_object().unwrap().keys().collect();

    keys.sort();

    // The replacement is what `docker run` was told, with another image under it. The old image's
    // ENV and LABEL stay behind, because the new image brings its own, as it does its entrypoint
    // and command.
    assert_eq!(keys, ["Env", "HostConfig", "Image", "Labels", "name"]);
    assert_eq!(spec["name"], "ankor-agent");
    assert_eq!(spec["Image"], image.as_str());
    assert_eq!(spec["Env"], json!(["TZ=Europe/Copenhagen"]));
    assert_eq!(spec["Labels"], json!({ "ankor.node": "basement-box", "ankor.role": "agent" }));
    assert_eq!(spec["HostConfig"]["NetworkMode"], "host");
    assert_eq!(spec["HostConfig"]["RestartPolicy"], json!({ "Name": "unless-stopped" }));
    assert_eq!(spec["HostConfig"]["LogConfig"], json!({ "Type": "local", "Config": { "mode": "non-blocking", "max-buffer-size": "4m" } }));
}

#[tokio::test]
async fn refuses_while_the_last_swap_still_proves_its_replacement_touching_nothing() {
    let setup = Setup::new();
    let before = setup.yaml();

    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v2");
    *setup.docker.self_update_helpers.lock().unwrap() =
        vec![SelfUpdateHelper { name: "ankor-self-update-beef00".to_owned(), image: Some(format!("{AGENT}:v1")), running: true }];

    assert_eq!(code(&setup.agent(&format!("{AGENT}:v2"), true).await.unwrap_err()), Some(AgentErrorCode::MaintenanceRunning));
    assert!(setup.pulled().is_empty());
    assert_eq!(setup.owed.count(), 0);
    assert_eq!(setup.yaml(), before);
}

#[tokio::test]
async fn leaves_a_recorded_failure_to_the_helper_whatever_an_update_agent_comes_to() {
    let mut setup = Setup::new();
    let data = tempfile::tempdir().unwrap();
    let failure_path = data.path().join("agent-update-failed.json");
    let failure = json!({ "image": format!("{AGENT}:v0"), "detail": "rolled back" }).to_string();

    setup.data_dir = data.path().to_owned();
    setup.docker.images.lock().unwrap().clear();
    setup.docker.images.lock().unwrap().insert(format!("{AGENT}:v1"), "sha256:agent-v1".to_owned());
    setup.docker.image_defaults_by_id.lock().unwrap().insert("sha256:agent-v1".to_owned(), ImageDefaults::default());
    std::fs::write(&failure_path, &failure).unwrap();

    // The node already runs the image, an image it lacks and may not pull, and a swap: only a swap
    // that holds removes the failure, and its helper does that.
    setup.agent(&format!("{AGENT}:v1"), false).await.unwrap();
    assert_eq!(code(&setup.agent(&format!("{AGENT}:v3"), false).await.unwrap_err()), Some(AgentErrorCode::ImageMissing));
    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v2");
    setup.agent(&format!("{AGENT}:v2"), true).await.unwrap();
    setup.owed.run_next().await;

    assert_eq!(setup.detached().len(), 1);
    assert_eq!(std::fs::read_to_string(&failure_path).unwrap(), failure);
}

#[tokio::test]
async fn keeps_running_records_the_swap_failed_and_gives_the_host_back_when_the_helper_will_not_start() {
    let setup = Setup::new();
    let image = format!("{AGENT}:v2");

    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v2");
    *setup.docker.detach_error.lock().unwrap() = Some("no space left on device".to_owned());

    assert_eq!(setup.agent(&image, true).await.unwrap(), agent_reply(&image, UpdateAgentReplyStep::Pulling));
    setup.owed.run_next().await;

    assert!(setup.detached().is_empty());
    assert_eq!(
        setup.failed_swaps(),
        [(image.clone(), SwapFailureOutcome::NotStarted, "the self-update helper did not start: no space left on device".to_owned())]
    );
    // The guard was released, so the next request can try again.
    *setup.docker.detach_error.lock().unwrap() = None;
    assert_eq!(setup.agent(&image, false).await.unwrap(), agent_reply(&image, UpdateAgentReplyStep::Switching));
    setup.owed.run_next().await;
    assert_eq!(setup.detached().len(), 1);
    // The swap of an image already on the host starts at its switch and reports it once.
    assert_eq!(setup.agent_updates().last().unwrap(), &format!("switching {image}"));
}

#[tokio::test]
async fn holds_the_maintenance_guard_after_the_helper_starts_until_docker_reports_the_helper_gone() {
    let setup = Setup::new();
    let image = format!("{AGENT}:v2");

    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v2");
    assert_eq!(setup.agent(&image, true).await.unwrap(), agent_reply(&image, UpdateAgentReplyStep::Pulling));
    setup.owed.run_next().await;
    assert_eq!(setup.detached().len(), 1);
    // The swap proves its replacement: another update is refused, and the controller sees the node in
    // maintenance.
    assert!(setup.updates.running());
    assert_eq!(code(&setup.agent(&image, false).await.unwrap_err()), Some(AgentErrorCode::MaintenanceRunning));

    until("the helper to be followed", || setup.docker.followed.lock().unwrap().iter().any(|entry| entry.id == "helper-1")).await;

    let hooks = setup.docker.followed.lock().unwrap().iter().find(|entry| entry.id == "helper-1").unwrap().hooks.clone();

    (hooks.on_exit)(Some(0));
    assert!(!setup.updates.running());
}

#[tokio::test]
async fn updates_the_plugin_without_requiring_the_agent_container_to_be_found() {
    let setup = Setup::new();
    let image = format!("{CS2}:v2");

    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v1");
    *setup.docker.agent_container.lock().unwrap() = None;

    assert_eq!(setup.cs2(&image, true).await.unwrap(), image_reply(&image, ImageUpdateStep::Pulling));
    setup.owed.run_owed().await;
    assert!(setup.detached().is_empty());
    assert!(setup.yaml().contains(&format!("cs2_image: {image}")));
}

#[tokio::test]
async fn fails_without_touching_anything_when_an_image_is_missing_and_no_pull_was_asked_for() {
    let setup = Setup::new();
    let image = format!("{AGENT}:v2");
    let error = setup.agent(&image, false).await.unwrap_err();

    assert_eq!(code(&error), Some(AgentErrorCode::ImageMissing));
    assert!(error.to_string().contains("is not on this host"));
    assert!(setup.pulled().is_empty());
    assert_eq!(setup.owed.count(), 0);
    assert_eq!(setup.yaml(), original());

    // The guard was released, so the request can be repeated once the image is on the host.
    assert!(setup.agent(&image, false).await.unwrap_err().to_string().contains("is not on this host"));
}

#[tokio::test]
async fn reports_a_failed_pull_of_either_update_in_its_state_with_docker_s_message_after_replying() {
    for agent in [true, false] {
        let setup = Setup::new();
        let image = format!("{}:v2", if agent { AGENT } else { CS2 });

        *setup.docker.pull_error.lock().unwrap() = Some("pull access denied".to_owned());

        if agent {
            assert_eq!(setup.agent(&image, true).await.unwrap(), agent_reply(&image, UpdateAgentReplyStep::Pulling));
        } else {
            assert_eq!(setup.cs2(&image, true).await.unwrap(), image_reply(&image, ImageUpdateStep::Pulling));
        }

        setup.owed.run_owed().await;

        let detail = format!("pulling {image} failed: pull access denied");

        if agent {
            assert_eq!(setup.failed_swaps(), [(image.clone(), SwapFailureOutcome::PullFailed, detail)]);
        } else {
            assert_eq!(setup.image_updates().last().unwrap(), &format!("pull_failed: {detail}"));
        }

        assert!(setup.detached().is_empty());
        assert!(!setup.updates.running());
        assert_eq!(setup.yaml(), original());
    }
}

#[tokio::test]
async fn fails_either_update_without_touching_anything_when_the_image_is_gone_right_after_its_pull() {
    for agent in [true, false] {
        let setup = Setup::new();
        let image = format!("{}:v2", if agent { AGENT } else { CS2 });

        // The pull succeeds, but nothing on the host answers to the reference afterwards.
        if agent {
            assert_eq!(setup.agent(&image, true).await.unwrap(), agent_reply(&image, UpdateAgentReplyStep::Pulling));
        } else {
            assert_eq!(setup.cs2(&image, true).await.unwrap(), image_reply(&image, ImageUpdateStep::Pulling));
        }

        setup.owed.run_owed().await;

        let detail = format!("{image} disappeared from this host while it was read");

        assert_eq!(setup.pulled(), std::slice::from_ref(&image));

        if agent {
            assert_eq!(setup.failed_swaps(), [(image.clone(), SwapFailureOutcome::NotStarted, detail)]);
        } else {
            assert_eq!(setup.image_updates().last().unwrap(), &format!("failed: {detail}"));
        }

        assert!(setup.detached().is_empty());
        assert!(!setup.updates.running());
        assert_eq!(setup.yaml(), original());
    }
}

#[tokio::test]
async fn fails_update_cs2_image_with_image_missing_when_the_image_is_missing_and_no_pull_was_asked_for() {
    let setup = Setup::new();

    assert_eq!(code(&setup.cs2(&format!("{CS2}:v9"), false).await.unwrap_err()), Some(AgentErrorCode::ImageMissing));
    assert_eq!(setup.yaml(), original());
    assert!(setup.image_updates().is_empty());
    assert!(!setup.updates.running());
}

fn labelled() -> UpdateKind {
    UpdateKind { id: "00000000-0000-4000-8000-00000000a0a1".to_owned(), mode: Cs2UpdateMode::Update, automatic: false }
}

#[tokio::test]
async fn refuses_while_another_update_holds_the_host() {
    let setup = Setup::new();

    *setup.docker.update_container.lock().unwrap() = Some(("ankor-update-ab12cd".to_owned(), Some(labelled())));

    assert!(setup.agent(&format!("{AGENT}:v2"), true).await.unwrap_err().to_string().contains("is already running"));
    assert!(setup.pulled().is_empty());
    assert_eq!(setup.yaml(), original());
}

#[tokio::test]
async fn refuses_when_it_cannot_find_its_own_container() {
    let setup = Setup::new();

    *setup.docker.agent_container.lock().unwrap() = None;
    setup.pulls_to("sha256:cs2-v2", "sha256:agent-v2");

    assert!(setup.agent(&format!("{AGENT}:v2"), true).await.unwrap_err().to_string().contains("ankor.role=agent"));
    assert_eq!(setup.yaml(), original());
}

fn counting_state() -> (Cs2ImageUpdateState, Arc<AtomicUsize>) {
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);

    (
        Cs2ImageUpdateState::new(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })),
        changes,
    )
}

fn switch(step: Option<ImageUpdateStep>, outcome: Option<Cs2ImageUpdateOutcome>, detail: Option<&str>) -> Cs2ImageUpdate {
    Cs2ImageUpdate { image: format!("{CS2}:v2"), step, outcome, detail: detail.map(str::to_owned) }
}

#[test]
fn follows_a_switch_from_its_pull_to_how_it_ended_and_reports_each_step() {
    let (state, changes) = counting_state();
    let image = format!("{CS2}:v2");

    state.started(&image, ImageUpdateStep::Pulling);
    assert_eq!(state.get(), Some(switch(Some(ImageUpdateStep::Pulling), None, None)));
    state.switching();
    assert_eq!(state.get(), Some(switch(Some(ImageUpdateStep::Switching), None, None)));
    state.ended(Cs2ImageUpdateOutcome::Completed, None);
    assert_eq!(state.get(), Some(switch(None, Some(Cs2ImageUpdateOutcome::Completed), None)));
    assert_eq!(changes.load(Ordering::SeqCst), 3);
}

#[test]
fn cuts_a_long_reason_and_reports_an_empty_one_as_none() {
    let (state, _) = counting_state();
    let image = format!("{CS2}:v2");
    let max = usize::try_from(CS2_UPDATE_DETAIL_MAX_CHARS).unwrap();

    state.started(&image, ImageUpdateStep::Switching);
    state.ended(Cs2ImageUpdateOutcome::OverlayFailed, Some(&"x".repeat(max + 5)));
    assert_eq!(state.get(), Some(switch(None, Some(Cs2ImageUpdateOutcome::OverlayFailed), Some(&"x".repeat(max)))));
    state.started(&image, ImageUpdateStep::Switching);
    state.ended(Cs2ImageUpdateOutcome::Failed, Some(""));
    assert_eq!(state.get(), Some(switch(None, Some(Cs2ImageUpdateOutcome::Failed), None)));
}

#[test]
fn takes_no_switch_or_end_without_a_switch_under_way_and_no_switch_after_one_ended() {
    let (state, changes) = counting_state();

    state.switching();
    state.ended(Cs2ImageUpdateOutcome::Completed, None);
    assert_eq!(state.get(), None);
    state.started(&format!("{CS2}:v2"), ImageUpdateStep::Pulling);
    state.ended(Cs2ImageUpdateOutcome::PullFailed, Some("manifest unknown"));
    state.switching();
    assert_eq!(state.get(), Some(switch(None, Some(Cs2ImageUpdateOutcome::PullFailed), Some("manifest unknown"))));
    assert_eq!(changes.load(Ordering::SeqCst), 2);
}

/// A node that booted on the `v2` pair, with these images on it.
fn boot(images: Vec<(ImageSummary, BTreeMap<String, String>)>, agent_image_id: &str) -> (ConfigStore, Arc<FakeDocker>, MaintenanceGuard) {
    let config = ConfigStore::new(
        PathBuf::from("/nonexistent/agent.yml"),
        agent_config(&json!({ "cs2_image": format!("{CS2}:v2"), "agent_image": format!("{AGENT}:v2") })),
    );
    let docker = FakeDocker::new();

    *docker.agent_container.lock().unwrap() = Some(inspected(agent_image_id));
    docker.images.lock().unwrap().extend([(format!("{CS2}:v2"), "sha256:cs2-v2".to_owned()), (format!("{AGENT}:v2"), "sha256:agent-v2".to_owned())]);
    *docker.image_list.lock().unwrap() = images;

    let updates = MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn DockerPort>));

    (config, docker, updates)
}

fn tagged(name: &str, tag: &str) -> Vec<String> {
    vec![format!("{name}:{tag}")]
}

fn warnings() -> (Logger, Arc<Mutex<Vec<String>>>) {
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let logged = Arc::clone(&lines);
    let log = Logger::new(
        "sweep",
        Some(Arc::new(move |entry: Entry| {
            if entry.level == ankor_agent::logger::Level::Warn {
                logged.lock().unwrap().push(entry.message.trim_start_matches("[sweep] ").to_owned());
            }
        })),
    );

    (log, lines)
}

#[tokio::test]
async fn removes_what_the_pair_superseded_dangling_leftovers_included_and_keeps_what_agent_yml_names_or_a_container_uses() {
    let (config, docker, updates) = boot(
        vec![
            ours("sha256:agent-v2", "agent", &tagged(AGENT, "v2")),
            ours("sha256:agent-v1", "agent", &tagged(AGENT, "v1")),
            // A `docker load` under the same tag took this one's name; the label still says whose it is.
            ours("sha256:agent-v0", "agent", &[]),
            ours("sha256:cs2-v2", "cs2", &tagged(CS2, "v2")),
            ours("sha256:cs2-v1", "cs2", &tagged(CS2, "v1")),
            ours("sha256:cs2-v0", "cs2", &tagged(CS2, "v0")),
            (ImageSummary { id: "sha256:busybox".to_owned(), repo_tags: vec!["busybox:latest".to_owned()] }, BTreeMap::new()),
        ],
        "sha256:agent-v2",
    );
    let held_for: Arc<Mutex<Option<MaintenanceOperation>>> = Arc::default();
    let held = Arc::clone(&held_for);
    let guard = updates.clone();

    // A container still on it: a labelled image is not garbage by its label alone.
    *docker.image_ids_in_use.lock().unwrap() = vec!["sha256:cs2-v1".to_owned()];
    *docker.on_images_in_use.lock().unwrap() = Some(Arc::new(move || *held.lock().unwrap() = guard.holder().map(|holder| holder.operation)));

    prune_superseded_images(&config.reader(), docker.as_ref(), &updates, &Logger::null()).await;

    assert_eq!(*docker.removed_images.lock().unwrap(), [format!("{AGENT}:v1"), "sha256:agent-v0".to_owned(), format!("{CS2}:v0")]);
    assert_eq!(*held_for.lock().unwrap(), Some(MaintenanceOperation::ImageCleanup));
}

#[tokio::test]
async fn untags_every_name_of_a_superseded_build_that_two_tags_share() {
    let (config, docker, updates) = boot(
        vec![
            ours("sha256:agent-v2", "agent", &tagged(AGENT, "v2")),
            // Two commits built the same agent: one id, two tags. Removing the id would need force.
            ours("sha256:agent-v1", "agent", &[format!("{AGENT}:v1"), format!("{AGENT}:v1-again")]),
            ours("sha256:cs2-v2", "cs2", &tagged(CS2, "v2")),
        ],
        "sha256:agent-v2",
    );

    prune_superseded_images(&config.reader(), docker.as_ref(), &updates, &Logger::null()).await;

    assert_eq!(*docker.removed_images.lock().unwrap(), [format!("{AGENT}:v1"), format!("{AGENT}:v1-again")]);
}

#[tokio::test]
async fn keeps_the_image_agent_yml_names_when_a_rolled_back_swap_left_the_old_agent_running() {
    let (config, docker, updates) =
        boot(vec![ours("sha256:agent-v2", "agent", &tagged(AGENT, "v2")), ours("sha256:agent-v1", "agent", &tagged(AGENT, "v1"))], "sha256:agent-v1");

    prune_superseded_images(&config.reader(), docker.as_ref(), &updates, &Logger::null()).await;

    assert!(docker.removed_images.lock().unwrap().is_empty());
}

#[tokio::test]
async fn skips_the_sweep_while_an_update_holds_the_host() {
    let (config, docker, updates) = boot(vec![ours("sha256:agent-v1", "agent", &tagged(AGENT, "v1"))], "sha256:agent-v2");

    *docker.update_container.lock().unwrap() = Some(("ankor-update-ab12cd".to_owned(), Some(labelled())));
    prune_superseded_images(&config.reader(), docker.as_ref(), &updates, &Logger::null()).await;

    assert!(docker.removed_images.lock().unwrap().is_empty());
}

#[tokio::test]
async fn removes_nothing_when_inspection_of_either_configured_image_fails() {
    for field in ["cs2_image", "agent_image"] {
        let (config, docker, updates) = boot(vec![ours("sha256:agent-v1", "agent", &tagged(AGENT, "v1"))], "sha256:agent-v2");
        let configured = serde_json::to_value(config.read()).unwrap()[field].as_str().unwrap().to_owned();
        let (log, warned) = warnings();

        *docker.on_image_id.lock().unwrap() = Some(Arc::new(move |image| (image == configured).then(|| "image inspection failed".to_owned())));
        prune_superseded_images(&config.reader(), docker.as_ref(), &updates, &log).await;
        assert!(docker.removed_images.lock().unwrap().is_empty(), "{field}");
        assert!(warned.lock().unwrap().contains(&"the image sweep failed: image inspection failed".to_owned()), "{field}");
        assert!(!updates.running());
    }
}

#[tokio::test]
async fn reports_a_docker_lookup_failure_while_claiming_the_guard_without_calling_it_a_busy_host() {
    let (config, docker, updates) = boot(vec![ours("sha256:agent-v1", "agent", &tagged(AGENT, "v1"))], "sha256:agent-v2");
    let (log, warned) = warnings();

    *docker.update_container_error.lock().unwrap() = Some("Docker connection reset".to_owned());
    prune_superseded_images(&config.reader(), docker.as_ref(), &updates, &log).await;
    assert!(docker.removed_images.lock().unwrap().is_empty());
    assert!(warned.lock().unwrap().contains(&"skipping the image sweep: Docker connection reset".to_owned()));
    assert!(!updates.running());
}

#[tokio::test]
async fn carries_on_past_an_image_the_daemon_will_not_remove() {
    let (config, docker, updates) =
        boot(vec![ours("sha256:agent-v1", "agent", &tagged(AGENT, "v1")), ours("sha256:cs2-v0", "cs2", &tagged(CS2, "v0"))], "sha256:agent-v2");

    *docker.remove_image_error.lock().unwrap() = Some("conflict: image is being used".to_owned());
    prune_superseded_images(&config.reader(), docker.as_ref(), &updates, &Logger::null()).await;

    assert!(docker.removed_images.lock().unwrap().is_empty());
    // And the guard came back, so the sweep did its cleanup.
    assert!(!updates.running());
}
