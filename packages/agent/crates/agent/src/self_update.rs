//! Moves the node to another agent or cs2 image, and removes the images an update superseded.
//!
//! The agent cannot replace its own container from inside it, so an agent swap is handed to a
//! helper container built from the new agent image (`self_update_helper`). The container that says
//! hello next, and the image it names, is the result.

use crate::sync::lock;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::constants::CS2_UPDATE_DETAIL_MAX_CHARS;
use ankor_protocol::types::{
    AgentErrorCode, Cs2ImageUpdate, Cs2ImageUpdateOutcome, ImageUpdateStep, MaintenanceOperation, UpdateAgentReply, UpdateAgentReplyStep, UpdateCs2ImageReply,
};
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent_update::{AgentUpdateState, SwapFailureOutcome};
use crate::config::{ConfigPatch, ConfigReader, ConfigStore};
use crate::container_env::env_entry;
use crate::container_spec::{AGENT_IMAGE, ContainerRole, IMAGE_LABEL, labels, log_config, role_filter};
use crate::contract::constants::{SELF_UPDATE_COMMAND, self_update_env};
use crate::docker::{DockerPort, ImageDefaults, InspectedContainer, REGISTRY_LOOKUP_TIMEOUT};
use crate::failure::{AgentFailure, FailureOutcome, OpError, StepFailure, failure_outcome, truncate};
use crate::helper_container::short_id;
use crate::host_layout::{DOCKER_SOCKET, agent_update_failure_path};
use crate::install::MaintenanceGuard;
use crate::logger::Logger;
use crate::overlay::OverlayPort;

/// The keys of a container spec. The old agent writes a replacement's spec, its helper reads it, and
/// the Docker client reads the same keys back from an inspected container.
///
/// The Pascal-case keys are fields of Docker's create request. `name` is the agent's key, because
/// Docker takes a container's name as a query parameter and ignores this key. An old agent and a
/// newer helper share this format, so renaming a key breaks a swap between versions.
pub(crate) mod spec_keys {
    pub(crate) const NAME: &str = "name";
    pub(crate) const IMAGE: &str = "Image";
    pub(crate) const ENV: &str = "Env";
    pub(crate) const LABELS: &str = "Labels";
    pub(crate) const CMD: &str = "Cmd";
    pub(crate) const HOST_CONFIG: &str = "HostConfig";
    pub(crate) const LOG_CONFIG: &str = "LogConfig";
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SelfUpdateError {
    #[error("no container carries {}", role_filter(ContainerRole::Agent))]
    NoOwnContainer,
    #[error("{image} disappeared from this host while it was read")]
    Disappeared { image: String },
    #[error("{image} is not an Ankor agent image: it has no {IMAGE_LABEL}={AGENT_IMAGE} label")]
    NotAgentImage { image: String },
}

/// Runs work once the reply has left the socket.
pub type Schedule = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

pub(crate) type AgentUpdateFactory = Arc<dyn Fn(Schedule) -> AgentUpdateDeps + Send + Sync>;

pub(crate) type Cs2ImageUpdateFactory = Arc<dyn Fn(Schedule) -> Cs2ImageUpdateDeps + Send + Sync>;

/// The replacement container: what `docker run` was told (environment, labels, mounts, restart
/// policy) with another image under it. The daemon reports a container's environment and labels
/// merged with its image's, so the old image's share is taken out. The new image supplies its own,
/// along with entrypoint, command, user and working directory.
fn replacement_spec(own: &InspectedContainer, image_defaults: &ImageDefaults, image: &str) -> Value {
    let env: Vec<Value> = own
        .config
        .get(spec_keys::ENV)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| entry.as_str().is_none_or(|entry| !image_defaults.env.iter().any(|default| default == entry)))
        .cloned()
        .collect();
    let labels: Map<String, Value> = own
        .config
        .get(spec_keys::LABELS)
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(key, value)| image_defaults.labels.get(*key).is_none_or(|default| value.as_str() != Some(default)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    // The log options are the agent's own, so a node set up before them takes them with its next
    // update instead of keeping its first agent's. A log driver set by hand is replaced too.
    let mut host_config = own.host_config.clone();

    if let Some(host_config) = host_config.as_object_mut() {
        host_config.insert(spec_keys::LOG_CONFIG.to_owned(), log_config());
    }

    json!({
        (spec_keys::NAME): own.name,
        (spec_keys::IMAGE): image,
        (spec_keys::ENV): env,
        (spec_keys::LABELS): labels,
        (spec_keys::HOST_CONFIG): host_config,
    })
}

const HELPER_RESTARTS: u32 = 3;

/// The helper stays on the host after it exits as the swap's record, rollback included. The next
/// swap removes it first. Docker restarts a helper that crashes or runs out of memory, and the
/// rerun ends the swap the first run left.
fn helper_spec(agent_image: &str, replacement: &Value, previous_id: &str, data_dir: &Path) -> (String, Value) {
    let name = format!("ankor-self-update-{}", short_id());
    let failure_path = agent_update_failure_path(data_dir);
    let data_dir = data_dir.display();
    let spec = json!({
        (spec_keys::NAME): name,
        (spec_keys::IMAGE): agent_image,
        (spec_keys::CMD): SELF_UPDATE_COMMAND,
        (spec_keys::ENV): [
            env_entry(self_update_env::SPEC, replacement),
            env_entry(self_update_env::PREVIOUS_ID, previous_id),
            env_entry(self_update_env::FAILURE, failure_path.display()),
        ],
        (spec_keys::LABELS): { (labels::ROLE): ContainerRole::SelfUpdate.as_str() },
        (spec_keys::HOST_CONFIG): {
            "Binds": [format!("{DOCKER_SOCKET}:{DOCKER_SOCKET}"), format!("{data_dir}:{data_dir}")],
            "RestartPolicy": { "Name": "on-failure", "MaximumRetryCount": HELPER_RESTARTS },
            (spec_keys::LOG_CONFIG): log_config(),
        },
    });

    (name, spec)
}

#[async_trait]
pub trait AgentUpdateReport: Send + Sync {
    fn started(&self, image: &str, step: ImageUpdateStep);
    fn switching(&self);
    fn unchanged(&self, image: &str);
    fn record_failure(&self, image: &str, outcome: SwapFailureOutcome, detail: &str);
    /// Follows the swap's helper, so a swap that fails without stopping this agent is reported.
    async fn follow_helper(&self, helper: &str, image: &str);
}

#[async_trait]
impl AgentUpdateReport for AgentUpdateState {
    fn started(&self, image: &str, step: ImageUpdateStep) {
        AgentUpdateState::started(self, image, step);
    }

    fn switching(&self) {
        AgentUpdateState::switching(self);
    }

    fn unchanged(&self, image: &str) {
        AgentUpdateState::unchanged(self, image);
    }

    fn record_failure(&self, image: &str, outcome: SwapFailureOutcome, detail: &str) {
        AgentUpdateState::record_failure(self, image, outcome, detail);
    }

    async fn follow_helper(&self, helper: &str, image: &str) {
        AgentUpdateState::follow_helper(self, helper, image).await;
    }
}

#[derive(Clone)]
pub struct AgentUpdateDeps {
    /// The node's settings, where the target agent image is recorded.
    pub config: ConfigStore,
    /// The Docker calls the update makes.
    pub docker: Arc<dyn DockerPort>,
    /// The agent's data directory, where a swap that failed is recorded.
    pub data_dir: PathBuf,
    /// Where the update's steps and outcome are reported.
    pub agent_update: Arc<dyn AgentUpdateReport>,
    /// The maintenance guard the update claims, so no other update runs alongside it.
    pub updates: MaintenanceGuard,
    /// Where the update logs its progress and failures.
    pub log: Logger,
    /// Runs the update once the reply has left the socket; the maintenance guard is released after it.
    pub schedule: Schedule,
}

/// Where an `update_agent` request stands once it is answered.
enum Accepted {
    /// The node already runs the image.
    Unchanged,
    /// The pull or the swap runs after the reply.
    HandedOff(ImageUpdateStep, Option<String>, Box<InspectedContainer>),
}

/// What the node already has decides between unchanged, a swap and a pull. The pull and the swap
/// run after the reply, and `agent_update` reports how they end.
///
/// # Errors
///
/// Refuses as the maintenance guard does, with `image_missing` for an image the host lacks when no
/// pull was asked for, and fails when the agent's container cannot be found.
pub async fn update_agent(image: &str, pull: bool, deps: &AgentUpdateDeps) -> Result<UpdateAgentReply, OpError> {
    let release = deps.updates.claim(MaintenanceOperation::AgentUpdate).await?;
    let (step, local_id, own) = match accept_agent_update(image, pull, deps).await {
        Ok(Accepted::HandedOff(step, local_id, own)) => (step, local_id, own),
        Ok(Accepted::Unchanged) => {
            release.release(None);

            return Ok(UpdateAgentReply { image: image.to_owned(), step: UpdateAgentReplyStep::Unchanged });
        }
        Err(error) => {
            release.release(None);

            return Err(error);
        }
    };

    deps.agent_update.started(image, step);

    let work = deps.clone();
    let target = image.to_owned();

    (deps.schedule)(Box::pin(async move {
        // The swap holds the guard until its helper has proved the replacement and exited.
        let helper = swap_agent(&target, local_id, &own, &work).await;

        release.release(helper);
    }));

    Ok(UpdateAgentReply {
        image: image.to_owned(),
        step: match step {
            ImageUpdateStep::Pulling => UpdateAgentReplyStep::Pulling,
            ImageUpdateStep::Switching => UpdateAgentReplyStep::Switching,
        },
    })
}

async fn accept_agent_update(image: &str, pull: bool, deps: &AgentUpdateDeps) -> Result<Accepted, OpError> {
    // The last swap's helper goes first, so the one on the host afterwards is this swap's. Its
    // result stays, because only a swap that holds removes it.
    deps.docker.clear_self_update_helpers().await?;

    let own = deps.docker.inspect_agent_container().await?.ok_or(SelfUpdateError::NoOwnContainer)?;
    let local_id = if pull { None } else { Some(local_image(deps.docker.as_ref(), image).await?) };
    // The registry's digest tells, without a pull, that the tag still names the running image.
    let unchanged = match &local_id {
        None => deps.docker.registry_has(image, &own.image_id, REGISTRY_LOOKUP_TIMEOUT).await,
        Some(id) => *id == own.image_id,
    };

    if unchanged {
        keep_agent_image(&deps.config, image)?;
        deps.log.info(format!("{} already runs {image}", own.name));
        deps.agent_update.unchanged(image);

        return Ok(Accepted::Unchanged);
    }

    let step = if local_id.is_none() { ImageUpdateStep::Pulling } else { ImageUpdateStep::Switching };

    Ok(Accepted::HandedOff(step, local_id, Box::new(own)))
}

/// Returns the helper's id once it started. A failure is recorded and leaves this agent running.
async fn swap_agent(image: &str, local_id: Option<String>, own: &InspectedContainer, deps: &AgentUpdateDeps) -> Option<String> {
    let mut starting = false;
    let result: Result<Option<String>, OpError> = async {
        let id = match &local_id {
            Some(id) => id.clone(),
            None => pulled(deps.docker.as_ref(), image).await?,
        };

        // Checked before anything changes: an image that is no agent's would run as the helper,
        // with the Docker socket and the data directory, and could hold the guard permanently.
        if id != own.image_id && deps.docker.image_defaults(&id).await?.labels.get(IMAGE_LABEL).map(String::as_str) != Some(AGENT_IMAGE) {
            return Err(SelfUpdateError::NotAgentImage { image: image.to_owned() }.into());
        }

        keep_agent_image(&deps.config, image)?;

        if id == own.image_id {
            deps.log.info(format!("{} already runs {image}", own.name));
            deps.agent_update.unchanged(image);

            return Ok(None);
        }

        if local_id.is_none() {
            deps.agent_update.switching();
        }

        let replacement = replacement_spec(own, &deps.docker.image_defaults(&own.image_id).await?, image);
        let (name, spec) = helper_spec(image, &replacement, &own.id, &deps.data_dir);

        starting = true;

        let helper = deps.docker.run_detached(&name, &spec).await?;

        deps.log.info(format!("self-update helper {} is replacing {} with {image}", helper.chars().take(12).collect::<String>(), own.name));
        deps.agent_update.follow_helper(&helper, image).await;

        Ok(Some(helper))
    }
    .await;

    match result {
        Ok(helper) => helper,
        Err(error) => {
            let detail = if starting { format!("the self-update helper did not start: {error}") } else { error.to_string() };
            // Nothing here ran the new build: only the helper's own failure blames the build.
            let outcome = if failure_outcome(&error) == FailureOutcome::PullFailed { SwapFailureOutcome::PullFailed } else { SwapFailureOutcome::NotStarted };

            deps.log.error(format!("{detail}; this agent keeps running"));
            deps.agent_update.record_failure(image, outcome, &detail);

            None
        }
    }
}

/// Written before the swap, so the new agent boots on a file that names it. After a rollback the
/// file names the image that failed; the automatic update then follows the major the node runs.
fn keep_agent_image(config: &ConfigStore, image: &str) -> Result<(), OpError> {
    config.change(|current, file| {
        if current.agent_image != image {
            file.write(&ConfigPatch { agent_image: Some(image.to_owned()), ..ConfigPatch::default() })?;
        }

        Ok(())
    })
}

pub(crate) type ChangeHook = Arc<dyn Fn() + Send + Sync>;

/// What `cs2.image_update` reports: the switch that runs now, or the last one to end since the
/// agent started.
#[derive(Clone)]
pub struct Cs2ImageUpdateState {
    current: Arc<Mutex<Option<Cs2ImageUpdate>>>,
    on_change: ChangeHook,
}

impl Cs2ImageUpdateState {
    #[must_use]
    pub fn new(on_change: ChangeHook) -> Self {
        Self { current: Arc::default(), on_change }
    }

    #[must_use]
    pub fn get(&self) -> Option<Cs2ImageUpdate> {
        lock(&self.current).clone()
    }

    fn update(&self, change: impl FnOnce(&mut Option<Cs2ImageUpdate>) -> bool) {
        let changed = change(&mut lock(&self.current));

        if changed {
            (self.on_change)();
        }
    }
}

pub trait ImageUpdateReport: Send + Sync {
    fn started(&self, image: &str, step: ImageUpdateStep);
    /// The image is on the node: the switch extracts its plugin files and takes it into use.
    fn switching(&self);
    fn ended(&self, outcome: Cs2ImageUpdateOutcome, detail: Option<&str>);
}

impl ImageUpdateReport for Cs2ImageUpdateState {
    fn started(&self, image: &str, step: ImageUpdateStep) {
        self.update(|current| {
            *current = Some(Cs2ImageUpdate { image: image.to_owned(), step: Some(step), outcome: None, detail: None });
            true
        });
    }

    fn switching(&self) {
        self.update(|current| match current {
            Some(update) if update.outcome.is_none() => {
                update.step = Some(ImageUpdateStep::Switching);
                true
            }
            _ => false,
        });
    }

    fn ended(&self, outcome: Cs2ImageUpdateOutcome, detail: Option<&str>) {
        self.update(|current| match current {
            Some(update) => {
                update.step = None;
                update.outcome = Some(outcome);
                update.detail = detail.map(|detail| truncate(detail, CS2_UPDATE_DETAIL_MAX_CHARS)).filter(|detail| !detail.is_empty());
                true
            }
            None => false,
        });
    }
}

#[derive(Clone)]
pub struct Cs2ImageUpdateDeps {
    /// The node's settings, where the new cs2 image is recorded.
    pub config: ConfigStore,
    /// The Docker calls the switch makes.
    pub docker: Arc<dyn DockerPort>,
    /// Extracts the new image's plugin files.
    pub overlay: Arc<dyn OverlayPort>,
    /// The maintenance guard the switch claims, so no other update runs alongside it.
    pub updates: MaintenanceGuard,
    /// Where the switch logs its failures and the sweep's removals.
    pub log: Logger,
    /// Where the switch's steps and outcome are reported.
    pub state: Arc<dyn ImageUpdateReport>,
    /// Runs the switch once the reply has left the socket.
    pub schedule: Schedule,
    /// The images launches picked and still hold, which the sweep keeps.
    pub held_images: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
}

/// Replies immediately; the pull, the extraction and the sweep run after the reply, and
/// `cs2.image_update` reports how they end, in the same `node_status` as the new image when the
/// guard is released.
///
/// # Errors
///
/// Refuses as the maintenance guard does, and with `image_missing` for an image the host lacks
/// when no pull was asked for.
pub async fn update_cs2_image(image: &str, pull: bool, deps: &Cs2ImageUpdateDeps) -> Result<UpdateCs2ImageReply, OpError> {
    let release = deps.updates.claim(MaintenanceOperation::Cs2ImageUpdate).await?;
    let local_id = if pull {
        None
    } else {
        match local_image(deps.docker.as_ref(), image).await {
            Ok(id) => Some(id),
            Err(error) => {
                release.release(None);

                return Err(error);
            }
        }
    };
    let step = if local_id.is_none() { ImageUpdateStep::Pulling } else { ImageUpdateStep::Switching };

    deps.state.started(image, step);

    let work = deps.clone();
    let target = image.to_owned();

    (deps.schedule)(Box::pin(async move {
        match switch_cs2_image(&target, local_id, &work).await {
            Ok(()) => work.state.ended(Cs2ImageUpdateOutcome::Completed, None),
            Err(error) => {
                let outcome = match failure_outcome(&error) {
                    FailureOutcome::PullFailed => Cs2ImageUpdateOutcome::PullFailed,
                    FailureOutcome::OverlayFailed => Cs2ImageUpdateOutcome::OverlayFailed,
                    FailureOutcome::Failed => Cs2ImageUpdateOutcome::Failed,
                };

                work.log.error(format!("switching to {target} failed: {error}"));
                work.state.ended(outcome, Some(&error.to_string()));
            }
        }

        release.release(None);
    }));

    Ok(UpdateCs2ImageReply { image: image.to_owned(), step })
}

async fn switch_cs2_image(image: &str, local_id: Option<String>, deps: &Cs2ImageUpdateDeps) -> Result<(), OpError> {
    let image_id = match local_id {
        Some(id) => id,
        None => pulled(deps.docker.as_ref(), image).await?,
    };

    deps.state.switching();
    deps.overlay.ensure(&image_id).await.map_err(StepFailure::overlay)?;

    deps.config.change(|current, file| {
        if current.cs2_image != image {
            file.write(&ConfigPatch { cs2_image: Some(image.to_owned()), ..ConfigPatch::default() })?;
        }

        Ok::<_, OpError>(())
    })?;

    sweep_superseded_images(&deps.config.reader(), deps.docker.as_ref(), &(deps.held_images)(), &deps.log).await;

    Ok(())
}

async fn local_image(docker: &dyn DockerPort, image: &str) -> Result<String, OpError> {
    docker.image_id(image).await?.ok_or_else(|| {
        OpError::Refused(AgentFailure::new(AgentErrorCode::ImageMissing, format!("{image} is not on this host and the request asked for no pull")))
    })
}

async fn pulled(docker: &dyn DockerPort, image: &str) -> Result<String, OpError> {
    docker.pull_image(image).await.map_err(|error| StepFailure::pull(image, error))?;

    Ok(docker.image_id(image).await?.ok_or_else(|| SelfUpdateError::Disappeared { image: image.to_owned() })?)
}

/// Whether the boot's sweep ran, or the maintenance guard refused it and why.
#[derive(Debug, PartialEq, Eq)]
pub enum BootSweep {
    Swept,
    Refused(String),
}

/// The boot's sweep, under its own claim of the maintenance guard. It does not run while an update
/// holds the guard, and the caller decides whether to log that or wait and try again.
pub async fn prune_superseded_images(config: &ConfigReader, docker: &dyn DockerPort, updates: &MaintenanceGuard, log: &Logger) -> BootSweep {
    let release = match updates.claim(MaintenanceOperation::ImageCleanup).await {
        Ok(release) => release,
        Err(error) => return BootSweep::Refused(error.to_string()),
    };

    // Before any launch, so no server holds an image yet. A sweep that waited for the guard runs
    // later, but a launch that picked an image has its container by then, which keeps the image.
    sweep_superseded_images(config, docker, &[], log).await;
    release.release(None);
    BootSweep::Swept
}

/// How many times the sweep after a swap claims the guard.
const SWEEP_AFTER_SWAP_ATTEMPTS: u32 = 3;

/// The pause before the sweep after a swap claims the guard again.
pub const SWEEP_AFTER_SWAP_PAUSE: Duration = Duration::from_secs(1);

/// The boot's sweep after a swap, once the helper that proves this agent frees the guard.
///
/// Docker can answer the helper's wait a moment before its container list stops showing the helper.
/// The claim then finds the helper and hands the guard back to it. That wait returns immediately.
/// The sweep therefore pauses before it claims again, and the list catches up in the pause. If the
/// list never catches up, the sweep gives up after a few claims. The next boot sweeps again.
pub async fn sweep_after_swap(config: &ConfigReader, docker: &dyn DockerPort, updates: &MaintenanceGuard, log: &Logger, stop: &CancellationToken) {
    let mut running = updates.subscribe();

    for attempt in 1..=SWEEP_AFTER_SWAP_ATTEMPTS {
        // The guard's borrow ends here, before the sweep awaits anything.
        let freed = stop.run_until_cancelled(running.wait_for(|held| !held)).await.is_some_and(|waited| waited.is_ok());

        if !freed {
            return;
        }

        // A sweep cut short frees the guard as its claim drops.
        let Some(BootSweep::Refused(reason)) = stop.run_until_cancelled(prune_superseded_images(config, docker, updates, log)).await else {
            return;
        };

        // A shutdown that begins during the claim also refuses it, which is no reason to log.
        if stop.is_cancelled() {
            return;
        }

        if attempt == SWEEP_AFTER_SWAP_ATTEMPTS {
            log.warn(format!("skipping the image sweep: {reason}"));
            return;
        }

        log.info(format!("the image sweep tries again: {reason}"));

        if stop.run_until_cancelled(tokio::time::sleep(SWEEP_AFTER_SWAP_PAUSE)).await.is_none() {
            return;
        }
    }
}

/// Removes images that carry the project's label, except those `agent.yml` names, the `held` ones
/// and any a container uses. `agent.yml` can name the agent image a rolled-back swap was heading
/// for, so that one stays. The caller holds the maintenance guard, so no pull moves a tag mid-sweep.
pub(crate) async fn sweep_superseded_images(config: &ConfigReader, docker: &dyn DockerPort, held: &[String], log: &Logger) {
    if let Err(error) = sweep(config, docker, held, log).await {
        log.warn(format!("the image sweep failed: {error}"));
    }
}

async fn sweep(config: &ConfigReader, docker: &dyn DockerPort, held: &[String], log: &Logger) -> Result<(), OpError> {
    let config = config.read();
    let (cs2_image, agent_image) = (config.cs2_image, config.agent_image);
    let mut keep: Vec<String> = Vec::new();

    keep.extend(docker.image_id(&cs2_image).await?);
    keep.extend(docker.image_id(&agent_image).await?);
    keep.extend(docker.images_in_use().await?);

    for image in held {
        keep.extend(docker.image_id(image).await?);
    }

    for image in docker.list_project_images().await? {
        if keep.contains(&image.id) {
            continue;
        }

        // Two builds that came out identical share one id under several tags. Removing the id then
        // needs force, while untagging each name deletes the image with its last tag.
        let names = if image.repo_tags.is_empty() { vec![image.id.clone()] } else { image.repo_tags.clone() };

        for name in names {
            match docker.remove_image(&name).await {
                Ok(()) => log.info(format!("removed superseded {name}")),
                Err(error) => log.error(format!("could not remove {name}: {error}; remove it by hand")),
            }
        }
    }

    Ok(())
}
