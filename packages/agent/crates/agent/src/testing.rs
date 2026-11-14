//! Test doubles the agent's tests share: a Docker that records every call and lets a test drive
//! containers' lines and exits, an overlay a test can hold back, slot files and the spool's record
//! of launched allocations in memory.

use crate::sync::lock;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::types::ServerExit;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::container_spec::{ContainerRole, IMAGE_LABEL};
use crate::controller_connection::{ControllerConnection, Hooks, Timings};
use crate::docker::{
    AdoptedContainer, ContainerHooks, DockerPort, ImageDefaults, ImageSummary, InspectedContainer, NetworkMode, RunningUpdate, SelfUpdateHelper,
    StartContainerSpec,
};
use crate::docker_api::{DockerError, WaitCondition};
use crate::event_spool::{SpoolAck, SpoolAppend, SpoolBacklog, SpoolError, SpoolFrame, SpoolStream};
use crate::host_layout::{DATA_DIR, node_cfg_dir, shared_dir, slot_demo_dir, slot_root, swiftly_dir};
use crate::logger::Logger;
use crate::overlay::{AcquiredOverlay, ExtractedOverlay, OverlayError, OverlayPort};
use crate::slot_files::{CfgFile, PreparedSlot, SlotFilesPort};
use crate::spool_client::SpoolPort;

#[derive(Clone)]
pub struct Started {
    pub id: String,
    pub spec: StartContainerSpec,
    pub hooks: ContainerHooks,
}

/// SIGTERM's exit code, which a stopped container exits with.
pub const STOP_EXIT_CODE: i64 = 143;

/// Records every call. A started container runs until the test calls [`FakeDocker::exit`], or
/// exits at once with [`FakeDocker::exit_on_start`]; a stop makes it exit with
/// [`STOP_EXIT_CODE`] before the stop answers.
#[derive(Default)]
pub struct FakeDocker {
    /// Every container started, in order.
    pub started: Mutex<Vec<Started>>,
    /// Every call to start a container, the refused ones among them.
    pub start_calls: Mutex<usize>,
    /// Holds each start until the test adds a permit.
    pub start_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Fails each start after the container started and its `exit_on_start` ran.
    pub fail_after_start: Mutex<Option<String>>,
    /// Every container followed, in order.
    pub followed: Mutex<Vec<Started>>,
    /// What each follow in `followed` waited for, by container.
    pub followed_until: Mutex<Vec<(String, WaitCondition)>>,
    /// Fails the next follow, which then watches nothing.
    pub follow_error: Mutex<Option<String>>,
    /// What `list_ankor_containers` answers.
    pub adoptable: Mutex<Vec<AdoptedContainer>>,
    /// Every stop asked for, with its timeout, the failed ones among them.
    pub stopped: Mutex<Vec<(String, i64)>>,
    /// Fails every stop.
    pub stop_error: Mutex<Option<String>>,
    /// Each takes the place of one stop's work, in order, once the stop is recorded.
    pub stop_overrides: Mutex<VecDeque<StopOverride>>,
    /// Every removal asked for, copy containers and failed removals among them.
    pub removed: Mutex<Vec<String>>,
    /// Every pull that worked.
    pub pulled: Mutex<Vec<String>>,
    /// Fails every pull.
    pub pull_error: Mutex<Option<String>>,
    /// Every pull asked for, the failed ones among them.
    pub pull_attempts: Mutex<Vec<String>>,
    /// How long each pull takes before it answers.
    pub pull_delay: Mutex<Option<Duration>>,
    /// What a pull of each reference installs; a pull of anything else leaves the host as it was.
    pub pull_ids: Mutex<BTreeMap<String, String>>,
    /// Lines each started container prints, then the code it exits with.
    pub exit_on_start: Mutex<Option<(Vec<String>, Option<i64>)>>,
    /// Fails every start once through the gate, before the container is recorded.
    pub start_error: Mutex<Option<String>>,
    /// Fails every removal.
    pub remove_error: Mutex<Option<String>>,
    /// The images on the host, reference to id. While it is empty, `default_image_id` answers for
    /// every reference; once filled, it says which references exist. An id answers for itself.
    pub images: Mutex<BTreeMap<String, String>>,
    /// The id every reference resolves to while `images` is empty; `sha256:image` when unset.
    pub default_image_id: Mutex<Option<String>>,
    /// Makes every image missing from the host.
    pub no_images: Mutex<bool>,
    /// Runs before each image lookup; a message it returns fails the lookup with it.
    pub on_image_id: Mutex<Option<ImageIdHook>>,
    /// How long a lookup of each reference takes, as a slow daemon's would.
    pub image_id_delays: Mutex<BTreeMap<String, Duration>>,
    /// The image id the registry's digest names, by reference; a reference it lacks is one the
    /// registry cannot tell about.
    pub registry: Mutex<BTreeMap<String, String>>,
    /// What `repo_digests` answers, by image id.
    pub repo_digests_by_id: Mutex<BTreeMap<String, Vec<String>>>,
    /// The agent's container, or `None` for a host that lacks one.
    pub agent_container: Mutex<Option<InspectedContainer>>,
    /// What each image bakes in, by id; an image not named bakes in nothing.
    pub image_defaults_by_id: Mutex<BTreeMap<String, ImageDefaults>>,
    /// Agent containers running beside this one.
    pub other_agents: Mutex<Vec<String>>,
    /// Every container started detached, with its name: the self-update helper is the only one.
    pub detached: Mutex<Vec<(String, Value)>>,
    /// Fails every detached start.
    pub detach_error: Mutex<Option<String>>,
    /// The host's images with their labels; `list_project_images` answers with the labelled ones.
    pub image_list: Mutex<Vec<(ImageSummary, BTreeMap<String, String>)>>,
    /// Image ids containers use besides the agent's own.
    pub image_ids_in_use: Mutex<Vec<String>>,
    /// Runs as each read of the images in use begins.
    pub on_images_in_use: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Every image removed.
    pub removed_images: Mutex<Vec<String>>,
    /// Fails every image removal.
    pub remove_image_error: Mutex<Option<String>>,
    /// What `running_overlay_image_ids` answers.
    pub overlays_in_use: Mutex<Vec<String>>,
    /// Fails every read of the overlays in use.
    pub overlays_error: Mutex<Option<String>>,
    /// The work of each helper by its role: it prints and exits once its start has answered.
    pub helper_runs: Mutex<HashMap<ContainerRole, HelperRun>>,
    /// Each container a copy was made from, with the path copied.
    pub copied: Mutex<Vec<(StartContainerSpec, String)>>,
    /// Fails every copy with a 404, after the hook ran and the container was removed.
    pub copy_error: Mutex<Option<String>>,
    /// Lays down what a copy unpacks: it gets the path copied and the folder it unpacks into.
    pub on_copy: Mutex<Option<CopyHook>>,
    /// The self-update helpers on the host, the one the agent would find first leading.
    pub self_update_helpers: Mutex<Vec<SelfUpdateHelper>>,
    /// A running cs2 update container, and what its labels say.
    pub update_container: Mutex<Option<(String, Option<crate::docker::UpdateKind>)>>,
    /// How many times the agent looked for a running update container.
    pub update_container_checks: Mutex<usize>,
    /// Holds each look for an update container this long first.
    pub update_container_delay: Mutex<Option<Duration>>,
    /// Fails the next look for a running update container.
    pub update_container_error: Mutex<Option<String>>,
}

pub type ImageIdHook = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A helper container's work: the lines it prints and the code it exits with.
pub type HelperRun = Arc<dyn Fn(&StartContainerSpec) -> (Vec<String>, Option<i64>) + Send + Sync>;
pub type CopyHook = Arc<dyn Fn(&str, &std::path::Path) + Send + Sync>;
/// Gets the container id and the stop's timeout.
pub type StopOverride = Box<dyn FnOnce(String, i64) -> BoxFuture<'static, Result<(), DockerError>> + Send>;

impl FakeDocker {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The index of the container started last.
    ///
    /// # Panics
    ///
    /// When none started.
    #[must_use]
    pub fn last(&self) -> usize {
        lock(&self.started).len().checked_sub(1).expect("a started container")
    }

    /// Queues `stop` to take the place of the next stop's work.
    pub fn override_stop<F>(&self, stop: impl FnOnce(String, i64) -> F + Send + 'static)
    where
        F: Future<Output = Result<(), DockerError>> + Send + 'static,
    {
        lock(&self.stop_overrides).push_back(Box::new(move |id, timeout| Box::pin(stop(id, timeout))));
    }

    /// # Panics
    ///
    /// When no such container started.
    pub fn exit(&self, index: usize, code: Option<i64>) {
        let hooks = lock(&self.started)[index].hooks.clone();

        (hooks.on_exit)(code);
    }

    /// # Panics
    ///
    /// When no such container started.
    pub fn line(&self, index: usize, line: &str) {
        let hooks = lock(&self.started)[index].hooks.clone();

        (hooks.on_line)(line.to_owned());
    }

    #[must_use]
    pub fn specs(&self) -> Vec<StartContainerSpec> {
        lock(&self.started).iter().map(|started| started.spec.clone()).collect()
    }

    fn hooks_of(&self, id: &str) -> Option<ContainerHooks> {
        lock(&self.started).iter().chain(lock(&self.followed).iter()).find(|started| started.id == id).map(|started| started.hooks.clone())
    }
}

/// For fakes that have no create payload.
#[must_use]
pub fn empty_spec() -> StartContainerSpec {
    StartContainerSpec {
        name: String::new(),
        image: String::new(),
        env: Vec::new(),
        labels: BTreeMap::new(),
        network_mode: NetworkMode::Host,
        cap_drop: Vec::new(),
        security_opt: Vec::new(),
        pids_limit: 0,
        memory_bytes: None,
        cpu_shares: None,
        auto_remove: true,
        volumes: Vec::new(),
        binds: Vec::new(),
        tmpfs: Vec::new(),
        cmd: None,
        entrypoint: None,
    }
}

#[async_trait]
impl DockerPort for FakeDocker {
    async fn require_supported_engine(&self) -> Result<String, DockerError> {
        Ok("29.0.0".to_owned())
    }

    async fn start_container(&self, spec: &StartContainerSpec, hooks: ContainerHooks) -> Result<String, DockerError> {
        *lock(&self.start_calls) += 1;

        let gate = lock(&self.start_gate).clone();

        if let Some(gate) = gate {
            gate.acquire().await.map_err(|error| DockerError::Http(error.to_string()))?.forget();
        }

        if let Some(error) = lock(&self.start_error).clone() {
            return Err(DockerError::Http(error));
        }

        let id = format!("container{}", lock(&self.started).len());

        lock(&self.started).push(Started { id: id.clone(), spec: spec.clone(), hooks: hooks.clone() });

        let run = spec
            .labels
            .get(crate::container_spec::labels::ROLE)
            .and_then(|role| ContainerRole::parse(role))
            .and_then(|role| lock(&self.helper_runs).get(&role).cloned());

        if let Some(run) = run {
            let (lines, code) = run(spec);
            let hooks = hooks.clone();

            tokio::spawn(async move {
                tokio::task::yield_now().await;

                for line in lines {
                    (hooks.on_line)(line);
                }

                (hooks.on_exit)(code);
            });
        }

        if let Some((lines, code)) = lock(&self.exit_on_start).clone() {
            for line in lines {
                (hooks.on_line)(line);
            }

            (hooks.on_exit)(code);
        }

        match lock(&self.fail_after_start).clone() {
            Some(error) => Err(DockerError::Http(error)),
            None => Ok(id),
        }
    }

    /// The exit is watched before the output is attached, as the real service does, so a failed
    /// attach still reports the exit.
    async fn follow_container(&self, id: &str, hooks: ContainerHooks, until: WaitCondition) -> Result<(), DockerError> {
        lock(&self.followed).push(Started { id: id.to_owned(), spec: empty_spec(), hooks });
        lock(&self.followed_until).push((id.to_owned(), until));

        match lock(&self.follow_error).take() {
            Some(error) => Err(DockerError::Http(error)),
            None => Ok(()),
        }
    }

    async fn stop_container(&self, id: &str, timeout_seconds: i64) -> Result<(), DockerError> {
        lock(&self.stopped).push((id.to_owned(), timeout_seconds));

        let stop = lock(&self.stop_overrides).pop_front();

        if let Some(stop) = stop {
            return stop(id.to_owned(), timeout_seconds).await;
        }

        if let Some(error) = lock(&self.stop_error).clone() {
            return Err(DockerError::Http(error));
        }

        if let Some(hooks) = self.hooks_of(id) {
            (hooks.on_exit)(Some(STOP_EXIT_CODE));
        }

        Ok(())
    }

    async fn remove_container(&self, id: &str, _force: bool) -> Result<(), DockerError> {
        lock(&self.removed).push(id.to_owned());

        match lock(&self.remove_error).clone() {
            Some(error) => Err(DockerError::Http(error)),
            None => Ok(()),
        }
    }

    async fn list_ankor_containers(&self) -> Result<Vec<AdoptedContainer>, DockerError> {
        Ok(lock(&self.adoptable).clone())
    }

    async fn sweep_helper_containers(&self) -> Result<Vec<String>, DockerError> {
        Ok(Vec::new())
    }

    async fn self_update_helper(&self) -> Result<Option<SelfUpdateHelper>, DockerError> {
        Ok(lock(&self.self_update_helpers).first().cloned())
    }

    async fn clear_self_update_helpers(&self) -> Result<(), DockerError> {
        lock(&self.self_update_helpers).retain(|helper| helper.running);
        Ok(())
    }

    async fn running_overlay_image_ids(&self) -> Result<Vec<String>, DockerError> {
        match lock(&self.overlays_error).clone() {
            Some(error) => Err(DockerError::Http(error)),
            None => Ok(lock(&self.overlays_in_use).clone()),
        }
    }

    async fn running_update_container(&self) -> Result<Option<RunningUpdate>, DockerError> {
        *lock(&self.update_container_checks) += 1;

        let delay = *lock(&self.update_container_delay);

        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }

        if let Some(error) = lock(&self.update_container_error).take() {
            return Err(DockerError::Http(error));
        }

        if let Some((name, kind)) = lock(&self.update_container).clone() {
            return Ok(Some(RunningUpdate::Cs2 { name, kind }));
        }

        Ok(lock(&self.self_update_helpers).iter().find(|helper| helper.running).map(|helper| RunningUpdate::AgentSwap { name: helper.name.clone() }))
    }

    async fn image_id(&self, image: &str) -> Result<Option<String>, DockerError> {
        let delay = lock(&self.image_id_delays).get(image).copied();

        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }

        let hook = lock(&self.on_image_id).clone();

        if let Some(error) = hook.and_then(|hook| hook(image)) {
            return Err(DockerError::Http(error));
        }

        if *lock(&self.no_images) {
            return Ok(None);
        }

        let images = lock(&self.images);

        if images.values().any(|id| id == image) {
            return Ok(Some(image.to_owned()));
        }

        if images.is_empty() {
            return Ok(Some(lock(&self.default_image_id).clone().unwrap_or_else(|| "sha256:image".to_owned())));
        }

        Ok(images.get(image).cloned())
    }

    async fn image_defaults(&self, id: &str) -> Result<ImageDefaults, DockerError> {
        Ok(lock(&self.image_defaults_by_id).get(id).cloned().unwrap_or_default())
    }

    async fn running_agent_containers(&self) -> Result<Vec<String>, DockerError> {
        let own = lock(&self.agent_container).as_ref().map(|agent| agent.name.clone());

        Ok(own.into_iter().chain(lock(&self.other_agents).iter().cloned()).collect())
    }

    async fn inspect_agent_container(&self) -> Result<Option<InspectedContainer>, DockerError> {
        Ok(lock(&self.agent_container).clone())
    }

    async fn run_detached(&self, name: &str, spec: &Value) -> Result<String, DockerError> {
        if let Some(error) = lock(&self.detach_error).clone() {
            return Err(DockerError::Http(error));
        }

        let mut detached = lock(&self.detached);

        detached.push((name.to_owned(), spec.clone()));
        Ok(format!("helper-{}", detached.len()))
    }

    async fn list_project_images(&self) -> Result<Vec<ImageSummary>, DockerError> {
        Ok(lock(&self.image_list).iter().filter(|(_, labels)| labels.contains_key(IMAGE_LABEL)).map(|(image, _)| image.clone()).collect())
    }

    /// Every real host has the agent's container on its image, so that id is always in use.
    async fn images_in_use(&self) -> Result<Vec<String>, DockerError> {
        let hook = lock(&self.on_images_in_use).clone();

        if let Some(hook) = hook {
            hook();
        }

        let own = lock(&self.agent_container).as_ref().map(|agent| agent.image_id.clone());

        Ok(own.into_iter().chain(lock(&self.image_ids_in_use).iter().cloned()).collect())
    }

    async fn remove_image(&self, id: &str) -> Result<(), DockerError> {
        if let Some(error) = lock(&self.remove_image_error).clone() {
            return Err(DockerError::Http(error));
        }

        lock(&self.removed_images).push(id.to_owned());
        Ok(())
    }

    async fn pull_image(&self, image: &str) -> Result<(), DockerError> {
        lock(&self.pull_attempts).push(image.to_owned());

        let delay = *lock(&self.pull_delay);

        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }

        if let Some(error) = lock(&self.pull_error).clone() {
            return Err(DockerError::Http(error));
        }

        lock(&self.pulled).push(image.to_owned());

        if let Some(id) = lock(&self.pull_ids).get(image).cloned() {
            lock(&self.images).insert(image.to_owned(), id);
        }

        Ok(())
    }

    async fn registry_has(&self, image: &str, image_id: &str, _within: Duration) -> bool {
        lock(&self.registry).get(image).is_some_and(|id| id == image_id)
    }

    async fn repo_digests(&self, image_id: &str) -> Result<Vec<String>, DockerError> {
        Ok(lock(&self.repo_digests_by_id).get(image_id).cloned().unwrap_or_default())
    }

    async fn copy_from_image(&self, spec: &StartContainerSpec, path: &str, target: &std::path::Path) -> Result<(), DockerError> {
        lock(&self.copied).push((spec.clone(), path.to_owned()));

        let hook = lock(&self.on_copy).clone();

        if let Some(hook) = hook {
            hook(path, target);
        }

        // Docker removes the container whether or not the copy worked.
        lock(&self.removed).push(spec.name.clone());

        match lock(&self.copy_error).clone() {
            Some(message) => Err(DockerError::Api { status: 404, message }),
            None => Ok(()),
        }
    }
}

/// An overlay that hands out a fixed folder, holding each acquire until the test adds a permit.
pub struct FakeOverlay {
    /// Holds each ensure and acquire until it takes a permit.
    pub gate: tokio::sync::Semaphore,
    /// Every ensure and acquire, counted as it begins.
    pub calls: Mutex<usize>,
    /// The image each ensure and acquire asked for.
    pub images: Mutex<Vec<String>>,
    /// Fails each ensure and acquire once through the gate.
    pub error: Mutex<Option<String>>,
    /// Counts the acquired overlays that are not released yet.
    references: Arc<Mutex<BTreeMap<String, usize>>>,
}

impl FakeOverlay {
    /// `open` lets every acquire through without waiting for a permit.
    #[must_use]
    pub fn new(open: bool) -> Arc<Self> {
        Arc::new(Self {
            gate: tokio::sync::Semaphore::new(if open { 1 << 20 } else { 0 }),
            calls: Mutex::new(0),
            images: Mutex::default(),
            error: Mutex::new(None),
            references: Arc::default(),
        })
    }

    /// Holds every acquire from now on, until the test adds a permit.
    pub fn close(&self) {
        self.gate.forget_permits(usize::MAX);
    }

    /// How many acquired overlays are not released yet.
    #[must_use]
    pub fn held(&self) -> usize {
        lock(&self.references).values().sum()
    }
}

#[async_trait]
impl OverlayPort for FakeOverlay {
    async fn ensure(&self, image: &str) -> Result<ExtractedOverlay, OverlayError> {
        *lock(&self.calls) += 1;
        lock(&self.images).push(image.to_owned());
        self.gate.acquire().await.map_err(std::io::Error::other)?.forget();

        if let Some(error) = lock(&self.error).clone() {
            return Err(OverlayError::Io(std::io::Error::other(error)));
        }

        Ok(ExtractedOverlay { image_id: "sha256:image".to_owned(), dir: "/srv/ankor/overlay/sha256_image".into() })
    }

    async fn acquire(&self, image: &str) -> Result<AcquiredOverlay, OverlayError> {
        let overlay = self.ensure(image).await?;

        Ok(AcquiredOverlay::counted(overlay, &self.references))
    }
}

#[derive(Clone, Debug)]
pub struct PrepareCall {
    pub slot_id: String,
    pub allocation_id: String,
    pub cfg_files: Vec<CfgFile>,
}

/// Slot files that write nothing: each preparation is recorded and answers with the default data
/// directory's paths.
#[derive(Default)]
pub struct FakeSlotFiles {
    /// Every preparation, in order.
    pub calls: Mutex<Vec<PrepareCall>>,
    /// Fails every preparation.
    pub error: Mutex<Option<String>>,
}

#[async_trait]
impl SlotFilesPort for FakeSlotFiles {
    async fn prepare(&self, slot_id: &str, allocation_id: &str, cfg_files: &[CfgFile]) -> std::io::Result<PreparedSlot> {
        lock(&self.calls).push(PrepareCall { slot_id: slot_id.to_owned(), allocation_id: allocation_id.to_owned(), cfg_files: cfg_files.to_vec() });

        if let Some(error) = lock(&self.error).clone() {
            return Err(std::io::Error::other(error));
        }

        let data_dir = Path::new(DATA_DIR);
        Ok(PreparedSlot {
            root: slot_root(data_dir, slot_id),
            demo_dir: slot_demo_dir(data_dir, slot_id).join(allocation_id),
            shared_root: shared_dir(data_dir),
            swiftly_root: swiftly_dir(data_dir),
            cfg_dir: node_cfg_dir(data_dir),
            node_plugins: Vec::new(),
            files: cfg_files.iter().filter(|cfg| cfg.contents.is_some()).map(|cfg| cfg.rel_path.clone()).collect(),
        })
    }
}

/// The spool's record of the allocations the node launched, in memory; every other spool call
/// answers empty.
#[derive(Default)]
pub struct FakeAllocations {
    /// Each allocation recorded, in the order it first was.
    pub recorded: Mutex<Vec<String>>,
    /// Fails every record.
    pub record_error: Mutex<Option<String>>,
    /// Fails every question whether an allocation was launched.
    pub used_error: Mutex<Option<String>>,
    /// Each exit kept and each set of exits dropped, as `record <id>` and `forget <id>,<id>`.
    pub exit_calls: Mutex<Vec<String>>,
    /// Fails every exit kept.
    pub exit_error: Mutex<Option<String>>,
}

#[async_trait]
impl SpoolPort for FakeAllocations {
    async fn version(&self) -> Result<u32, SpoolError> {
        Ok(0)
    }

    async fn position(&self, _allocation_id: String) -> Result<Option<SpoolAck>, SpoolError> {
        Ok(None)
    }

    async fn append(&self, _streams: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>, SpoolError> {
        Ok(Vec::new())
    }

    async fn pending(&self, _allocation_id: String) -> Result<Vec<SpoolFrame>, SpoolError> {
        Ok(Vec::new())
    }

    async fn backlog(&self) -> Result<Vec<SpoolBacklog>, SpoolError> {
        Ok(Vec::new())
    }

    async fn acknowledge(&self, _allocation_id: String, _to_seq: i64) -> Result<(), SpoolError> {
        Ok(())
    }

    async fn prune(&self, _active: Vec<String>) -> Result<Vec<SpoolBacklog>, SpoolError> {
        Ok(Vec::new())
    }

    async fn record_exit(&self, exit: ServerExit) -> Result<(), SpoolError> {
        lock(&self.exit_calls).push(format!("record {}", exit.allocation_id));

        match lock(&self.exit_error).clone() {
            Some(error) => Err(SpoolError::Unavailable(error)),
            None => Ok(()),
        }
    }

    async fn exits(&self) -> Result<Vec<ServerExit>, SpoolError> {
        Ok(Vec::new())
    }

    async fn forget_exits(&self, allocation_ids: Vec<String>) -> Result<(), SpoolError> {
        lock(&self.exit_calls).push(format!("forget {}", allocation_ids.join(",")));

        Ok(())
    }

    async fn allocation_used(&self, allocation_id: String) -> Result<bool, SpoolError> {
        if let Some(error) = lock(&self.used_error).clone() {
            return Err(SpoolError::Unavailable(error));
        }

        Ok(lock(&self.recorded).contains(&allocation_id))
    }

    async fn record_allocation(&self, allocation_id: String) -> Result<(), SpoolError> {
        if let Some(error) = lock(&self.record_error).clone() {
            return Err(SpoolError::Unavailable(error));
        }

        let mut recorded = lock(&self.recorded);

        if !recorded.contains(&allocation_id) {
            recorded.push(allocation_id);
        }

        Ok(())
    }

    async fn close(&self) -> Result<(), SpoolError> {
        Ok(())
    }
}

/// Milliseconds since the epoch that move with tokio's clock, so paused time moves them too.
#[must_use]
pub fn tokio_clock() -> crate::node_log::Clock {
    let start = tokio::time::Instant::now();
    let base = jiff::Timestamp::now().as_millisecond();

    Arc::new(move || base + i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX))
}

/// An authority, and a certificate it issued for `127.0.0.1`, for HTTPS servers in tests.
pub mod loopback_tls {
    /// The authority a test client trusts.
    pub const CA: &str = "-----BEGIN CERTIFICATE-----
MIIBdTCCARygAwIBAgIUe5md1YXC3mRJJR8Kvx3IB69Of1QwCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNYW5rb3IgdGVzdCBDQTAgFw0yNjExMDExNjAwNDdaGA8yMTI2
MTAwODE2MDA0N1owGDEWMBQGA1UEAwwNYW5rb3IgdGVzdCBDQTBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABLhnsws7H91wrHK2e2TTzZ+Kw3JUIw+N8FhAytA6VQjw
AitdycjbT0ZcV5woIRWpE8fnj4FbUt4BGImGbOFijxajQjBAMB0GA1UdDgQWBBT1
yNiGUDPppL6RCmgBoA73lk358TAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQE
AwICBDAKBggqhkjOPQQDAgNHADBEAiAX/aHDR+Lo97hXT1s23rJMzvizS2pQhtLp
95MXB8WQzAIgP+BqO2JnCIelSgRIj727nq3xkhPGpA2JuQOglkQMsOM=
-----END CERTIFICATE-----\n";
    /// The server's certificate, for `127.0.0.1`.
    pub const CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBbzCCARagAwIBAgIUQJe8vjv5zKghOIiHm+KKPjX1ZXAwCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNYW5rb3IgdGVzdCBDQTAgFw0yNjExMDExNjAwNDdaGA8yMTI2
MTAwODE2MDA0N1owFDESMBAGA1UEAwwJMTI3LjAuMC4xMFkwEwYHKoZIzj0CAQYI
KoZIzj0DAQcDQgAEwGIHp+BMcmXnPEXL0aMTcYiUxC4klruTm2KAhsfKMDJpla4d
7B8hkA1zdSdyEVVnK5KSdDtP1HKpWZhaJ+79TKNAMD4wDwYDVR0RBAgwBocEfwAA
ATAJBgNVHRMEAjAAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMAsGA1UdDwQEAwIHgDAK
BggqhkjOPQQDAgNHADBEAiAI/Z65a2YBPc3fHEVzX+PEM90Omv6CmjCKYaUenil9
zQIgYHZVgCuiUN5HkwOwZB/BlWBfHkrnzzocUqz2vQuygmw=
-----END CERTIFICATE-----\n";
    /// The server certificate's key, as PKCS #8.
    pub const KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgJN8f+cWxns5pb+fh
XLs749s6RRW7hwSFmBVJjxkqeMmhRANCAATAYgen4ExyZec8RcvRoxNxiJTELiSW
u5ObYoCGx8owMmmVrh3sHyGQDXN1J3IRVWcrkpJ0O0/UcqlZmFon7v1M
-----END PRIVATE KEY-----\n";
}

/// Sample configurations, launches and reported state the tests share.
pub mod fixtures {
    use std::path::Path;

    use ankor_protocol::config::AgentConfig;
    use ankor_protocol::schemas::{s_agent_config_schema, s_server_launch_config};
    use ankor_protocol::types::{GsltState, ServerLaunchConfig, SlotState, SlotStatus};
    use ankor_protocol::validate::{format_issues, safe_parse};
    use serde_json::{Value, json};

    use crate::contract::types::ServerStatus;
    use crate::host_layout::{DATA_DIR, node_cfg_dir, per_launch_files, shared_dir, slot_demo_dir, slot_root, swiftly_dir};
    use crate::slot_files::PreparedSlot;

    pub const GSLT_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001";
    pub const GSLT_B: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAA0002";
    pub const MATCH_ID: &str = "0199a0b4-5c00-7000-8000-000000000001";
    pub const OTHER_MATCH_ID: &str = "0199a0b4-5c00-7000-8000-000000000002";
    pub const ALLOCATION_ID: &str = "0199a0b4-5c00-7000-8000-0000000000a1";

    fn merged(mut base: Value, overrides: &Value) -> Value {
        if let (Some(base), Some(overrides)) = (base.as_object_mut(), overrides.as_object()) {
            for (key, value) in overrides {
                base.insert(key.clone(), value.clone());
            }
        }

        base
    }

    fn parsed<T: serde::de::DeserializeOwned>(run: fn(&mut ankor_protocol::validate::Payload), value: Value) -> T {
        let parsed = safe_parse(run, value).unwrap_or_else(|issues| panic!("invalid fixture: {}", format_issues(&issues)));

        serde_json::from_value(parsed).expect("a fixture reads into its type")
    }

    /// Two slots, `a` and `b`, with the given keys replaced.
    ///
    /// # Panics
    ///
    /// When the overrides make the config invalid.
    #[must_use]
    pub fn agent_config(overrides: &Value) -> AgentConfig {
        let base = json!({
            "token": "0123456789abcdef0123456789abcdef",
            "controller": { "host": "cs.example.com", "tls": true },
            "pinned_ip": "203.0.113.7",
            "cs2_image": "ghcr.io/example/ankor-cs2:latest",
            "agent_image": "ghcr.io/example/ankor-agent:latest",
            // The fixture's images are stand-ins, outside the official repositories.
            "official_images_only": false,
            "slots": [
                { "id": "a", "port": 27015, "tv_port": 27020, "gslt": GSLT_A },
                { "id": "b", "port": 27016, "tv_port": 27021, "gslt": GSLT_B },
            ],
        });

        parsed(s_agent_config_schema, merged(base, overrides))
    }

    fn launch_base() -> Value {
        json!({
            "hostname": "Ankor | cs.example.com",
            "steam_web_api_key": null,
            "game_password": "letmein",
            "chat_tag": { "text": "Ankor", "color": "yellow" },
            "game_mode": "competitive",
            "workshop_id": null,
            "map_name": "de_mirage",
        })
    }

    /// A server without a match, as ankor.sh launches its warmups.
    ///
    /// # Panics
    ///
    /// When the overrides make the launch invalid.
    #[must_use]
    pub fn server_launch_config(overrides: &Value) -> ServerLaunchConfig {
        let base = merged(
            launch_base(),
            &json!({ "max_players": 12, "gotv": null, "idle_shutdown_seconds": 180, "exec": [], "commands": ["mp_respawn_on_death_ct 1", "mp_respawn_on_death_t 1"], "match": null }),
        );

        parsed(s_server_launch_config, merged(base, overrides))
    }

    #[must_use]
    pub fn match_config(overrides: &Value) -> Value {
        merged(
            json!({
                "match_id": MATCH_ID,
                "roster": {
                    "team1": { "name": "Team One", "players": ["76561198000000001"] },
                    "team2": { "name": "Team Two", "players": ["76561198000000002"] },
                    "spectators": [],
                },
                "sides": "knife",
                "knife_decision_seconds": 60,
                "ready_mode": "manual",
                "spectators_ready_up": false,
                "ready_timeout_seconds": 300,
                "record_demo": true,
                "send_statistics": true,
                "debug_capture": false,
                "rules": { "max_rounds": 24, "overtime": true, "overtime_max_rounds": 6, "friendly_fire": false },
                "restore": null,
            }),
            overrides,
        )
    }

    /// A restore from the start of round 15 of an MR12 map, team 1 on CT.
    #[must_use]
    pub fn restore_file(overrides: &Value) -> Value {
        merged(
            json!({
                "team1_side": "ct",
                "rules": { "max_rounds": 24, "overtime": true, "overtime_max_rounds": 6, "friendly_fire": false },
                "file": "\"SaveFile\"\n{\n\t\"round\"\t\"14\"\n\t\"FirstHalfScore\"\n\t{\n\t\t\"team1\"\t\"8\"\n\t\t\"team2\"\t\"4\"\n\t}\n}\n",
            }),
            overrides,
        )
    }

    /// A match with GOTV.
    ///
    /// # Panics
    ///
    /// When the overrides make the launch invalid.
    #[must_use]
    pub fn launch_config(overrides: &Value) -> ServerLaunchConfig {
        let base = merged(
            launch_base(),
            &json!({ "max_players": 2, "gotv": { "password": "tv-secret", "delay_seconds": 45 }, "idle_shutdown_seconds": null, "exec": [], "commands": [], "match": match_config(&json!({})) }),
        );

        parsed(s_server_launch_config, merged(base, overrides))
    }

    /// Every slot of `config`, idle and with an unchecked token.
    #[must_use]
    pub fn idle_statuses(config: &AgentConfig) -> Vec<SlotStatus> {
        config
            .slots
            .iter()
            .map(|slot| SlotStatus {
                slot_id: slot.id.clone(),
                state: SlotState::Idle,
                allocation_id: None,
                match_id: None,
                plugin_online: false,
                gslt_state: GsltState::Unchecked,
                reserved: false,
                server_steam_id: None,
            })
            .collect()
    }

    /// What a plugin sends once its server can take players.
    #[must_use]
    pub fn ready_status() -> ServerStatus {
        ServerStatus {
            map_name: "de_mirage".to_owned(),
            r#match: None,
            players: Vec::new(),
            config_error: None,
            ready: true,
            request_id: None,
            server_steam_id: None,
        }
    }

    /// Slot `a` as slot files prepare it under the default data directory, every launch file written.
    #[must_use]
    pub fn prepared_slot() -> PreparedSlot {
        let data_dir = Path::new(DATA_DIR);

        PreparedSlot {
            root: slot_root(data_dir, "a"),
            shared_root: shared_dir(data_dir),
            swiftly_root: swiftly_dir(data_dir),
            cfg_dir: node_cfg_dir(data_dir),
            demo_dir: slot_demo_dir(data_dir, "a").join("11111111-1111-4111-8111-111111111111"),
            node_plugins: Vec::new(),
            files: per_launch_files().to_vec(),
        }
    }
}

/// A slot manager over the fakes, with every event it reports collected.
pub mod slots {
    use std::sync::{Arc, Mutex};

    use ankor_protocol::config::AgentConfig;
    use ankor_protocol::types::{ConfiguredSlot, ServerExit, ServerLaunchConfig, SlotState, SlotStatus, StartServerReply};
    use tokio::sync::mpsc;

    use super::{FakeAllocations, FakeDocker, FakeOverlay, FakeSlotFiles, lock};
    use crate::docker::DockerPort;
    use crate::logger::Logger;
    use crate::overlay::OverlayPort;
    use crate::slot_files::SlotFilesPort;
    use crate::slot_manager::{SlotError, SlotEvent, SlotManager, SlotManagerDeps};
    use crate::spool_client::SpoolPort;

    /// A [`SlotEvent`] as the test reads it.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Reported {
        /// With the state the slot last reported before the exit.
        ServerExited(ServerExit, Option<SlotState>),
        /// A `slot_status` report.
        SlotState(SlotStatus),
        /// A container line forwarded for its allocation.
        ContainerLog { allocation_id: String, line: String },
        /// A slot that came free.
        SlotIdle(String),
        /// A slot that `set_config` removed.
        SlotRemoved(String),
        /// The token of a server that got onto Steam.
        TokenUsed(String),
    }

    pub struct Slots {
        /// The configuration the manager reads, which a handler's `set_config` changes.
        pub config: crate::config::SharedConfig,
        /// The manager under test.
        pub slots: SlotManager,
        /// The Docker the manager starts servers on.
        pub docker: Arc<FakeDocker>,
        /// The overlay the manager acquires for each launch.
        pub overlay: Arc<FakeOverlay>,
        /// The slot files the manager prepares.
        pub files: Arc<FakeSlotFiles>,
        /// Where the manager records launched allocations.
        pub allocations: Arc<FakeAllocations>,
        /// Every allocation whose plugin the manager asked for a status, in order.
        pub polled: Arc<Mutex<Vec<String>>>,
        /// The allocation each slot's relay entry names, kept as the relay keeps one per slot.
        pub relay: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
        /// The manager's stop, which the agent's shutdown cancels.
        pub stop: tokio_util::sync::CancellationToken,
        /// The manager's events that no read has collected yet.
        receiver: Mutex<mpsc::UnboundedReceiver<SlotEvent>>,
        /// The events collected so far, in order.
        events: Mutex<Vec<Reported>>,
    }

    impl Slots {
        /// # Panics
        ///
        /// When no slot has the id.
        #[must_use]
        pub fn status(&self, slot_id: &str) -> SlotStatus {
            self.slots.statuses().into_iter().find(|status| status.slot_id == slot_id).expect("a configured slot")
        }

        /// Every event the manager reported so far, in order.
        pub fn events(&self) -> Vec<Reported> {
            let mut events = lock(&self.events);
            let mut receiver = lock(&self.receiver);

            while let Ok(event) = receiver.try_recv() {
                let reported = match event {
                    SlotEvent::ServerExited(exit) => {
                        let state = events.iter().rev().find_map(|event| match event {
                            Reported::SlotState(status) if status.slot_id == exit.slot_id => Some(status.state),
                            _ => None,
                        });

                        Reported::ServerExited(exit, state)
                    }
                    SlotEvent::SlotState(status) => Reported::SlotState(status),
                    SlotEvent::ContainerLog { allocation_id, line } => Reported::ContainerLog { allocation_id, line },
                    SlotEvent::SlotIdle { slot_id, .. } => Reported::SlotIdle(slot_id),
                    SlotEvent::SlotRemoved(slot_id) => Reported::SlotRemoved(slot_id),
                    SlotEvent::TokenUsed(token) => Reported::TokenUsed(token),
                };

                events.push(reported);
            }

            events.clone()
        }

        /// Every `slot_status` the manager reported, in order.
        pub fn reported(&self) -> Vec<SlotStatus> {
            self.events().into_iter().filter_map(|event| if let Reported::SlotState(status) = event { Some(status) } else { None }).collect()
        }

        /// Every server that ended, with the state its slot last reported before.
        pub fn exits(&self) -> Vec<(ServerExit, Option<SlotState>)> {
            self.events().into_iter().filter_map(|event| if let Reported::ServerExited(exit, state) = event { Some((exit, state)) } else { None }).collect()
        }

        /// Every line forwarded, with its allocation.
        pub fn logs(&self) -> Vec<(String, String)> {
            self.events()
                .into_iter()
                .filter_map(|event| if let Reported::ContainerLog { allocation_id, line } = event { Some((allocation_id, line)) } else { None })
                .collect()
        }

        /// Every slot that came free.
        pub fn idled(&self) -> Vec<String> {
            self.events().into_iter().filter_map(|event| if let Reported::SlotIdle(slot_id) = event { Some(slot_id) } else { None }).collect()
        }

        /// Every slot `set_config` removed.
        pub fn removed(&self) -> Vec<String> {
            self.events().into_iter().filter_map(|event| if let Reported::SlotRemoved(slot_id) = event { Some(slot_id) } else { None }).collect()
        }

        /// The token of each server that got onto Steam.
        pub fn tokens_used(&self) -> Vec<String> {
            self.events().into_iter().filter_map(|event| if let Reported::TokenUsed(token) = event { Some(token) } else { None }).collect()
        }
    }

    /// Launches a server with no reads or checks of the caller's, as `start_with` otherwise.
    ///
    /// # Errors
    ///
    /// As [`SlotManager::start_with`].
    pub async fn launch_server(slots: &SlotManager, launch: ServerLaunchConfig, allocation_id: &str) -> Result<StartServerReply, SlotError> {
        slots.start_with(launch, allocation_id, async {}, |()| Ok(())).await
    }

    /// Checks and applies a layout in one hold, as `set_config` does, with nothing else to check
    /// or commit. Returns the ids of the slots whose token is new.
    ///
    /// # Errors
    ///
    /// Refuses with `slot_held` as `set_config` would.
    pub fn change_layout(slots: &SlotManager, next: &[ConfiguredSlot]) -> Result<Vec<String>, SlotError> {
        slots.change_slots(next, |refusal| refusal, |_| Ok(()), || Ok(true)).map(Option::unwrap_or_default)
    }

    /// The manager over every fake, recording launched allocations in [`FakeAllocations`].
    #[must_use]
    pub fn make_slots(config: AgentConfig) -> Slots {
        make_slots_with(config, |_| {})
    }

    /// As [`make_slots`], with `change` editing the manager's dependencies before it is built.
    #[must_use]
    pub fn make_slots_with(config: AgentConfig, change: impl FnOnce(&mut SlotManagerDeps)) -> Slots {
        let docker = FakeDocker::new();
        let overlay = FakeOverlay::new(true);
        let files = Arc::new(FakeSlotFiles::default());
        let allocations = Arc::new(FakeAllocations::default());
        let polled: Arc<Mutex<Vec<String>>> = Arc::default();
        let polls = Arc::clone(&polled);
        let relay: Arc<Mutex<std::collections::BTreeMap<String, String>>> = Arc::default();
        let entries = Arc::clone(&relay);
        let shared: crate::config::SharedConfig = Arc::new(Mutex::new(config));
        let reading = Arc::clone(&shared);
        let mut deps = SlotManagerDeps {
            config: Arc::new(move || lock(&reading).clone()),
            docker: Arc::clone(&docker) as Arc<dyn DockerPort>,
            files: Arc::clone(&files) as Arc<dyn SlotFilesPort>,
            overlay: Arc::clone(&overlay) as Arc<dyn OverlayPort>,
            plugin_url: Arc::new(move |slot, allocation, key| {
                lock(&entries).insert(slot.to_owned(), allocation.to_owned());
                format!("ws://127.0.0.1:4500/ws/plugin?slot_id={slot}&key={key}")
            }),
            allocations: Some(Arc::clone(&allocations) as Arc<dyn SpoolPort>),
            poll_plugin: Arc::new(move |allocation_id| lock(&polls).push(allocation_id.to_owned())),
            report: None,
            log: Logger::null(),
            stop: tokio_util::sync::CancellationToken::new(),
        };

        change(&mut deps);

        let stop = deps.stop.clone();
        let (slots, receiver) = SlotManager::new(deps);

        Slots { config: shared, slots, docker, overlay, files, allocations, polled, relay, stop, receiver: Mutex::new(receiver), events: Mutex::default() }
    }
}

/// A controller connection whose driver dials the controller immediately.
#[must_use]
pub fn start_connection(token: String, hooks: Arc<dyn Hooks>, timings: Timings, log: Logger) -> ControllerConnection {
    let (connection, driver) = ControllerConnection::new(token, timings, log);

    driver.start(hooks);
    connection
}

/// Fails the test, naming `what`, after eight seconds of wall-clock time. On paused time, the polls
/// advance the clock.
///
/// # Panics
///
/// When `done` does not hold in time.
pub async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);

    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub mod agent_boot;
pub mod controller_peer;
pub mod handler;

/// A contract value as it goes on the wire, such as a `CloseReason` a fake controller closes with,
/// so a test sends what the contract defines instead of a copy of it.
pub use ankor_protocol::helpers::wire_name as wire;

/// A frame the agent sent the controller, read through the contract's type, so a test picks frames by message
/// kind rather than by their `type` text. `None` for a frame that does not parse.
#[must_use]
pub fn agent_message(frame: &Value) -> Option<ankor_protocol::types::AgentToControllerMessage> {
    serde_json::from_value(frame.clone()).ok()
}

/// A frame the relay sent a plugin, read through the contract's type as [`agent_message`] reads one
/// sent the controller.
#[must_use]
pub fn plugin_message(frame: &Value) -> Option<crate::contract::types::AgentToPluginMessage> {
    serde_json::from_value(frame.clone()).ok()
}

/// The controller's `welcome`, built from the contract's type.
#[must_use]
pub fn welcome() -> Value {
    serde_json::to_value(ankor_protocol::types::ControllerToAgentMessage::Welcome(ankor_protocol::types::ControllerWelcome {})).unwrap_or_default()
}
