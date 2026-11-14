//! The request handler built on a [`Slots`] fixture, with the plugins, demos and token checks it
//! talks to as fakes that record what was asked of them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::helpers::{from_wire_name, wire_name};
use ankor_protocol::types::{
    AgentErrorCode, AgentToControllerMessage, ControllerToAgentMessage, Cs2ImageUpdateOutcome, ImageUpdateStep, RequestType, ServerState,
};
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::Value;

use super::lock;
use super::slots::Slots;
use crate::agent_reporting::update_feed;
use crate::agent_update::SwapFailureOutcome;
use crate::config::ConfigStore;
use crate::controller_connection::{Defer, HandlerError, Request};
use crate::docker::DockerPort;
use crate::failure::AgentFailure;
use crate::install::{InstallOptions, MaintenanceGuard, MaintenanceGuardOptions, begin_cs2_update};
use crate::logger::Logger;
use crate::overlay::OverlayPort;
use crate::request_handler::{DemoStore, PluginCommandKind, Plugins, PublicBuild, RequestHandlerDeps, TokenChecks, handle};
use crate::self_update::{AgentUpdateDeps, AgentUpdateReport, Cs2ImageUpdateDeps, ImageUpdateReport};

/// A command a fake plugin took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TakenCommand {
    Exec(String),
    RestoreRound(i64),
    StartMatch,
}

/// The newest public build, unknown until `set_latest`, and the automatic update's catch-ups.
#[derive(Default)]
pub struct FakePublicBuild {
    pub latest: Mutex<Option<String>>,
    pub catch_ups: Mutex<usize>,
}

impl FakePublicBuild {
    pub fn set_latest(&self, latest: Option<&str>) {
        *lock(&self.latest) = latest.map(str::to_owned);
    }
}

impl PublicBuild for FakePublicBuild {
    fn latest(&self) -> Option<String> {
        lock(&self.latest).clone()
    }

    fn catch_up(&self) {
        *lock(&self.catch_ups) += 1;
    }
}

/// The plugin relay. A status request answers with `answer` or fails with `error`, and commands
/// reach a plugin while `online`.
pub struct FakePlugins {
    pub asked: Mutex<Vec<(String, Duration)>>,
    pub commands: Mutex<Vec<(String, TakenCommand)>>,
    pub answer: Mutex<Option<ServerState>>,
    pub error: Mutex<Option<AgentFailure>>,
    pub online: Mutex<bool>,
}

impl Default for FakePlugins {
    fn default() -> Self {
        Self { asked: Mutex::default(), commands: Mutex::default(), answer: Mutex::default(), error: Mutex::default(), online: Mutex::new(true) }
    }
}

#[async_trait]
impl Plugins for FakePlugins {
    async fn request_status(&self, allocation_id: &str, timeout: Duration) -> Result<ServerState, AgentFailure> {
        lock(&self.asked).push((allocation_id.to_owned(), timeout));

        if let Some(error) = lock(&self.error).clone() {
            return Err(error);
        }

        lock(&self.answer).clone().ok_or_else(|| AgentFailure::new(AgentErrorCode::Failed, "no answer set"))
    }

    async fn run_command(&self, allocation_id: &str, command: PluginCommandKind, timeout: Duration) -> Result<(), AgentFailure> {
        lock(&self.asked).push((allocation_id.to_owned(), timeout));

        if !*lock(&self.online) {
            return Err(AgentFailure::code(AgentErrorCode::PluginOffline));
        }

        if let Some(error) = lock(&self.error).clone() {
            return Err(error);
        }

        let taken = match command {
            PluginCommandKind::Exec(command) => TakenCommand::Exec(command),
            PluginCommandKind::RestoreRound(round) => TakenCommand::RestoreRound(round),
            PluginCommandKind::StartMatch => TakenCommand::StartMatch,
        };

        lock(&self.commands).push((allocation_id.to_owned(), taken));
        Ok(())
    }
}

/// The node's demos without a filesystem, recording what the handler asked of them.
#[derive(Default)]
pub struct FakeDemos {
    pub removed: Mutex<Vec<(String, String)>>,
    pub remove_error: Mutex<Option<String>>,
    pub scans: Mutex<usize>,
}

#[async_trait]
impl DemoStore for FakeDemos {
    async fn remove(&self, slot_id: &str, file_name: &str) -> Result<(), crate::failure::OpError> {
        if let Some(error) = lock(&self.remove_error).clone() {
            return Err(error.into());
        }

        lock(&self.removed).push((slot_id.to_owned(), file_name.to_owned()));
        Ok(())
    }

    fn scan_all(&self) -> BoxFuture<'static, Result<(), crate::demo_uploader::ScanError>> {
        *lock(&self.scans) += 1;
        Box::pin(async { Ok(()) })
    }
}

/// The slots whose new token the handler had checked.
#[derive(Default)]
pub struct FakeTokens {
    pub checked: Mutex<Vec<String>>,
}

#[async_trait]
impl TokenChecks for FakeTokens {
    async fn changed(&self, slot_ids: &[String]) {
        lock(&self.checked).extend(slot_ids.iter().cloned());
    }
}

/// What the handler reported of agent updates and cs2 image switches, in order.
#[derive(Default)]
pub struct UpdateLog {
    /// The agent update's steps and outcomes, one line each.
    pub agent: Mutex<Vec<String>>,
    /// The cs2 image switch's steps and outcomes, one line each.
    pub images: Mutex<Vec<String>>,
    /// Swaps whose helper would not start, as the handler recorded them.
    pub failed_swaps: Mutex<Vec<(String, SwapFailureOutcome, String)>>,
}

struct AgentUpdates(Arc<UpdateLog>);

#[async_trait]
impl AgentUpdateReport for AgentUpdates {
    fn started(&self, image: &str, step: ImageUpdateStep) {
        lock(&self.0.agent).push(format!("{} {image}", wire_name(step)));
    }

    fn switching(&self) {
        lock(&self.0.agent).push(wire_name(ImageUpdateStep::Switching));
    }

    fn unchanged(&self, image: &str) {
        lock(&self.0.agent).push(format!("unchanged {image}"));
    }

    fn record_failure(&self, image: &str, outcome: SwapFailureOutcome, detail: &str) {
        lock(&self.0.agent).push(format!("{} {image}", wire_name(outcome)));
        lock(&self.0.failed_swaps).push((image.to_owned(), outcome, detail.to_owned()));
    }

    async fn follow_helper(&self, _helper: &str, _image: &str) {}
}

struct ImageUpdates(Arc<UpdateLog>);

impl ImageUpdateReport for ImageUpdates {
    fn started(&self, image: &str, step: ImageUpdateStep) {
        lock(&self.0.images).push(format!("{} {image}", wire_name(step)));
    }

    fn switching(&self) {
        lock(&self.0.images).push(wire_name(ImageUpdateStep::Switching));
    }

    fn ended(&self, outcome: Cs2ImageUpdateOutcome, detail: Option<&str>) {
        let outcome = wire_name(outcome);

        lock(&self.0.images).push(detail.map_or_else(|| outcome.clone(), |detail| format!("{outcome}: {detail}")));
    }
}

pub struct Handler {
    /// The dependencies the handler runs with, after `change` edited them.
    pub deps: RequestHandlerDeps,
    /// Every frame the handler's updates pushed to the controller.
    pub pushed: Arc<Mutex<Vec<AgentToControllerMessage>>>,
    /// The demo store the handler was built with.
    pub demos: Arc<FakeDemos>,
    /// The token checks the handler was built with.
    pub tokens: Arc<FakeTokens>,
    /// The plugin relay the handler was built with.
    pub plugins: Arc<FakePlugins>,
    /// The public build the handler was built with.
    pub public_build: Arc<FakePublicBuild>,
    /// The maintenance guard `make_handler` built, before `change` ran.
    pub updates: MaintenanceGuard,
    /// The `agent.yml` that `make_handler` gave the config store.
    pub config_path: PathBuf,
    /// Keeps the directory that holds `config_path` until the handler is dropped.
    _dir: tempfile::TempDir,
}

impl Handler {
    /// Runs a request; work it defers runs once it has answered, as after a reply leaves the socket.
    ///
    /// # Errors
    ///
    /// Fails as the request does.
    pub async fn handle(&self, message: ControllerToAgentMessage) -> Result<Value, HandlerError> {
        let defer = Defer::default();
        let request = Request { id: "r1".to_owned(), request_type: request_type(&message), message };
        let result = handle(&self.deps, request, defer.clone()).await;

        for work in defer.take() {
            tokio::spawn(work);
        }

        result
    }

    /// Runs a request given as the JSON the controller sends.
    ///
    /// # Errors
    ///
    /// Fails as the request does.
    ///
    /// # Panics
    ///
    /// When the JSON is no request.
    pub async fn request(&self, frame: Value) -> Result<Value, HandlerError> {
        self.handle(serde_json::from_value(frame).expect("a request")).await
    }
}

fn request_type(message: &ControllerToAgentMessage) -> RequestType {
    let tag = serde_json::to_value(message).ok().and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();

    from_wire_name(&tag).expect("a request")
}

/// A path nothing writes, because the handler leaves a swap's result to its helper.
const DATA_DIR: &str = "/nonexistent/ankor-data";

/// The cs2 image switch, which keeps the images that the slots' launches hold.
fn cs2_image_update(
    deps: &RequestHandlerDeps,
    docker: &Arc<dyn DockerPort>,
    overlay: &Arc<dyn OverlayPort>,
    update_log: &Arc<UpdateLog>,
    slots: &crate::slot_manager::SlotManager,
) -> crate::self_update::Cs2ImageUpdateFactory {
    let (config, docker, overlay, updates, reports, log, slots) =
        (deps.config.clone(), Arc::clone(docker), Arc::clone(overlay), deps.updates.clone(), Arc::clone(update_log), deps.log.clone(), slots.clone());

    Arc::new(move |schedule| Cs2ImageUpdateDeps {
        config: config.clone(),
        docker: Arc::clone(&docker),
        overlay: Arc::clone(&overlay),
        updates: updates.clone(),
        log: log.clone(),
        state: Arc::new(ImageUpdates(Arc::clone(&reports))),
        schedule,
        held_images: {
            let slots = slots.clone();

            Arc::new(move || slots.held_images())
        },
    })
}

/// The handler over `base`, with `change` editing its dependencies before use. Its config store
/// writes a temporary file holding the base config, so `set_config` writes there.
///
/// # Panics
///
/// When the temporary directory cannot be made.
#[must_use]
pub fn make_handler(base: &Slots, change: impl FnOnce(&mut RequestHandlerDeps)) -> Handler {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let config_path = dir.path().join("agent.yml");
    let pushed: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
    let update_lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let (pushing, lines) = (Arc::clone(&pushed), Arc::clone(&update_lines));
    let feed = update_feed(Arc::new(move |frame| lock(&pushing).push(frame)), Arc::new(move |line: String| lock(&lines).push(line)));
    let docker: Arc<dyn DockerPort> = Arc::clone(&base.docker) as Arc<dyn DockerPort>;
    let overlay: Arc<dyn OverlayPort> = Arc::clone(&base.overlay) as Arc<dyn OverlayPort>;
    let updates = MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(&docker)));
    let demos = Arc::new(FakeDemos::default());
    let tokens = Arc::new(FakeTokens::default());
    let plugins = Arc::new(FakePlugins::default());
    let public_build = Arc::new(FakePublicBuild::default());
    let update_log = Arc::new(UpdateLog::default());
    let store = ConfigStore::over(config_path.clone(), Arc::clone(&base.config));
    let begin = {
        let (config, docker, overlay, updates, slots, feed) =
            (store.clone(), Arc::clone(&docker), Arc::clone(&overlay), updates.clone(), base.slots.clone(), feed);

        Arc::new(move |mode| {
            let options = InstallOptions {
                config: config.reader(),
                docker: Arc::clone(&docker),
                on_progress: Arc::clone(&feed.on_progress),
                on_log: Some(Arc::clone(&feed.on_log)),
                log: Logger::null(),
                any_busy: {
                    let slots = slots.clone();

                    Arc::new(move || slots.any_busy())
                },
                updates: updates.clone(),
                mode,
                automatic: false,
                state: None,
                overlay: Arc::clone(&overlay),
                short_id: None,
            };

            Box::pin(begin_cs2_update(options)) as BoxFuture<'static, _>
        })
    };
    let mut deps = RequestHandlerDeps {
        config: store,
        slots: base.slots.clone(),
        plugins: Arc::clone(&plugins) as Arc<dyn Plugins>,
        gslts: Arc::clone(&tokens) as Arc<dyn TokenChecks>,
        demos: Arc::clone(&demos) as Arc<dyn DemoStore>,
        updates: updates.clone(),
        begin_cs2_update: begin,
        data_dir: PathBuf::from(DATA_DIR),
        // Built below, over the config and guard `change` leaves.
        agent_update: Arc::new(|_| panic!("replaced below")),
        cs2_image_update: Arc::new(|_| panic!("replaced below")),
        log: Logger::null(),
        installed_build: Arc::new(|| Box::pin(async { Some("19000".to_owned()) })),
        public_build: Arc::clone(&public_build) as Arc<dyn PublicBuild>,
        poll_images: Arc::new(|| {}),
        unavailable: Arc::new(|| None),
        check_disk: Arc::new(|| Box::pin(async {})),
        redial: Arc::new(|| {}),
    };

    let config = lock(&base.config).clone();
    #[allow(clippy::format_collect, reason = "a small test file, built for reading")]
    let yml = format!(
        "token: {}\ncontroller:\n  host: {}\ncs2_image: {}\nagent_image: {}\nofficial_images_only: {}\nslots:\n{}",
        config.token,
        config.controller.host,
        config.cs2_image,
        config.agent_image,
        config.official_images_only,
        config
            .slots
            .iter()
            .map(|slot| format!("  - id: {}\n    port: {}\n    tv_port: {}\n    gslt: {}\n", slot.id, slot.port, slot.tv_port, slot.gslt))
            .collect::<String>(),
    );

    std::fs::write(&config_path, yml).expect("a config file");
    change(&mut deps);
    deps.agent_update = {
        let (config, docker, updates, reports, data_dir, log) =
            (deps.config.clone(), Arc::clone(&docker), deps.updates.clone(), Arc::clone(&update_log), deps.data_dir.clone(), deps.log.clone());

        Arc::new(move |schedule| AgentUpdateDeps {
            config: config.clone(),
            docker: Arc::clone(&docker),
            data_dir: data_dir.clone(),
            agent_update: Arc::new(AgentUpdates(Arc::clone(&reports))),
            updates: updates.clone(),
            log: log.clone(),
            schedule,
        })
    };
    deps.cs2_image_update = cs2_image_update(&deps, &docker, &overlay, &update_log, &base.slots);

    Handler { deps, pushed, demos, tokens, plugins, public_build, updates, config_path, _dir: dir }
}
