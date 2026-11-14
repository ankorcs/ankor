//! Installs and updates CS2. An update is the only operation that mounts the shared game volume
//! writable, so it is refused while slots are busy or another operation holds the maintenance guard.

use crate::sync::lock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use ankor_protocol::config::AgentConfig;
use ankor_protocol::constants::CS2_UPDATE_DETAIL_MAX_CHARS;
use ankor_protocol::helpers::wire_name;
use ankor_protocol::types::{AgentCs2UpdateProgress, AgentErrorCode, Cs2Update, Cs2UpdateMode, Cs2UpdateOutcome, MaintenanceOperation};
use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::config::ConfigReader;
use crate::container_env::{env_to_array, render_update_env};
use crate::container_spec::{ContainerRole, labels};
use crate::docker::{ContainerHooks, DockerPort, LineHook, RunningUpdate, StartContainerSpec};
use crate::docker_api::{DockerError, WaitCondition};
use crate::failure::{AgentFailure, BoxError, FailureOutcome, OpError, StepFailure, failure_outcome, truncate};
use crate::helper_container::{Activity, HelperError, HelperSpec, helper_spec, run_to_exit, short_id};
use crate::host_layout::{DATA_VOLUME, INSTALL_DIR, VolumeMount};
use crate::log_markers::{Cs2UpdateProgress, parse_cs2_update_progress};
use crate::logger::Logger;
use crate::overlay::OverlayPort;
use crate::self_update::sweep_superseded_images;

#[derive(Debug, thiserror::Error)]
pub(crate) enum InstallError {
    /// Docker failed to start or follow the update container.
    #[error("install failed to start: {0}")]
    Start(#[source] DockerError),
    /// The update container went quiet for [`UPDATE_QUIET`] and was removed.
    #[error("{0}")]
    Quiet(#[source] HelperError),
    /// The update container exited with a failure code, or with no observed status.
    #[error("install/update container exited with {outcome}")]
    Exited { outcome: String },
}

/// The guard stays held until this container is gone, so no launch starts on game files it may still
/// be writing.
fn left_running(error: &OpError) -> Option<String> {
    match error {
        OpError::Failed(error) => match error.downcast_ref::<InstallError>() {
            Some(InstallError::Quiet(quiet)) => quiet.left_running().map(str::to_owned),
            _ => None,
        },
        _ => None,
    }
}

/// steamcmd prints progress while it downloads, verifies and installs. An update container quiet this
/// long has hung, and it would hold the maintenance guard until a restart.
pub const UPDATE_QUIET: std::time::Duration = std::time::Duration::from_mins(30);

/// Progress as the controller receives it, without the frame's type.
pub type InstallProgress = AgentCs2UpdateProgress;

pub(crate) type ProgressHook = Arc<dyn Fn(InstallProgress) + Send + Sync>;

fn update_message(mode: Cs2UpdateMode) -> &'static str {
    match mode {
        Cs2UpdateMode::Update => "installing/updating CS2 with steamcmd",
        Cs2UpdateMode::Verify => "installing/updating CS2 with steamcmd, verifying every game file",
        Cs2UpdateMode::Reinstall => "reinstalling CS2 with steamcmd, downloading the whole game from scratch",
    }
}

/// What holds the maintenance guard, and its message for people.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceHolder {
    pub operation: MaintenanceOperation,
    pub detail: String,
}

fn operation_detail(operation: MaintenanceOperation) -> &'static str {
    match operation {
        MaintenanceOperation::Cs2Update => "CS2 update",
        MaintenanceOperation::AgentUpdate => "agent update",
        MaintenanceOperation::Cs2ImageUpdate => "CS2 image update",
        MaintenanceOperation::ImageCleanup => "image cleanup",
    }
}

pub type Hook = Arc<dyn Fn() + Send + Sync>;
pub(crate) type StartedHook = Arc<dyn Fn(&str) + Send + Sync>;

/// What `cs2.update` reports: the running install or update, or the last to end since the agent
/// started. Every update container's exit ends here. `on_ended` runs first, because the game volume
/// may hold another build. `on_started` runs before the update's first line, so its output starts a
/// separate log.
#[derive(Clone)]
pub struct Cs2UpdateState {
    current: Arc<Mutex<Option<Cs2Update>>>,
    on_change: Hook,
    on_ended: Hook,
    on_started: StartedHook,
}

impl Default for Cs2UpdateState {
    fn default() -> Self {
        Self::new(Arc::new(|| {}), Arc::new(|| {}), Arc::new(|_| {}))
    }
}

impl Cs2UpdateState {
    #[must_use]
    pub fn new(on_change: Hook, on_ended: Hook, on_started: StartedHook) -> Self {
        Self { current: Arc::default(), on_change, on_ended, on_started }
    }

    #[must_use]
    pub fn get(&self) -> Option<Cs2Update> {
        lock(&self.current).clone()
    }

    pub fn started(&self, id: &str, mode: Cs2UpdateMode, automatic: bool) {
        *lock(&self.current) = Some(Cs2Update { id: id.to_owned(), mode, automatic, outcome: None, detail: None });
        (self.on_started)(id);
        (self.on_change)();
    }

    /// `on_ended` always runs. The state changes only while an update runs, so a second report of an
    /// end changes nothing. Nothing is reported here: the guard's release, which follows, reports the
    /// outcome and the free node together.
    pub fn ended(&self, outcome: Cs2UpdateOutcome, detail: Option<&str>) {
        (self.on_ended)();

        {
            let mut current = lock(&self.current);

            match current.as_mut() {
                Some(update) if update.outcome.is_none() => {
                    update.outcome = Some(outcome);
                    update.detail = detail.map(|detail| truncate(detail, CS2_UPDATE_DETAIL_MAX_CHARS)).filter(|detail| !detail.is_empty());
                }
                _ => {}
            }
        }
    }
}

pub struct MaintenanceGuardOptions {
    /// Lists, follows and removes update containers.
    pub docker: Arc<dyn DockerPort>,
    /// Receives the guard's lines about inherited update containers.
    pub log: Logger,
    /// Progress of an inherited update, read off its log as if this agent had started it.
    pub on_progress: ProgressHook,
    /// Every line an inherited update prints, for the admin log view.
    pub on_log: Option<LineHook>,
    /// Where an inherited update's kind and end are reported.
    pub state: Option<Cs2UpdateState>,
    /// The images `agent.yml` names, which the sweep after an inherited update keeps.
    pub config: Option<ConfigReader>,
    /// Cancelled when the agent shuts down. No claim succeeds and no exit hook runs after that.
    pub stop: CancellationToken,
}

impl MaintenanceGuardOptions {
    #[must_use]
    pub fn new(docker: Arc<dyn DockerPort>) -> Self {
        Self { docker, log: Logger::null(), on_progress: Arc::new(|_| {}), on_log: None, state: None, config: None, stop: CancellationToken::new() }
    }
}

/// A container holding the guard that this agent did not start.
struct Foreign {
    name: String,
    holder: MaintenanceHolder,
}

#[derive(Default)]
struct Held {
    /// What this agent holds the guard for; `None` when it does not.
    mine: Option<MaintenanceOperation>,
    /// The container holding the guard that this agent did not start, if any.
    foreign: Option<Foreign>,
}

/// Only the first call counts. Naming a container this operation started holds the guard until that
/// container exits.
pub struct Release(Mutex<Option<Free>>);

type Free = Box<dyn FnOnce(Option<String>) + Send>;

impl Release {
    pub fn release(&self, handed_to: Option<String>) {
        let free = lock(&self.0).take();

        if let Some(free) = free {
            free(handed_to);
        }
    }
}

/// A claim dropped unreleased frees the guard, so it never stays held by nothing.
impl Drop for Release {
    fn drop(&mut self) {
        self.release(None);
    }
}

/// Frees a claim's reservation when the claim fails or is dropped, unless disarmed on success.
struct Reservation<'a> {
    guard: &'a MaintenanceGuard,
    armed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.guard.held().mine = None;
            self.guard.changed();
        }
    }
}

/// Prevents concurrent updates with an in-process reservation and a Docker check. An update
/// container this agent did not start holds the guard until Docker reports its exit. An agent swap's
/// helper holds it until it has proved the replacement. Docker's wait retries by itself, so nothing
/// here polls.
#[derive(Clone)]
pub struct MaintenanceGuard {
    /// The Docker access, hooks and shutdown token the guard works with.
    options: Arc<MaintenanceGuardOptions>,
    /// This agent's operation or the foreign container that holds the guard.
    held: Arc<Mutex<Held>>,
    /// Whether anything holds the guard, published with every change.
    running: Arc<tokio::sync::watch::Sender<bool>>,
}

impl MaintenanceGuard {
    #[must_use]
    pub fn new(options: MaintenanceGuardOptions) -> Self {
        Self { options: Arc::new(options), held: Arc::default(), running: Arc::new(tokio::sync::watch::Sender::new(false)) }
    }

    /// A release rewrote something the hello reports, such as the game volume or an image. The node
    /// report therefore follows this too. A new receiver has seen its starting value, and sees only
    /// the latest of several changes that come together.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.running.subscribe()
    }

    fn held(&self) -> MutexGuard<'_, Held> {
        lock(&self.held)
    }

    #[must_use]
    pub fn running(&self) -> bool {
        self.holder().is_some()
    }

    #[must_use]
    pub fn holder(&self) -> Option<MaintenanceHolder> {
        let held = self.held();

        if let Some(operation) = held.mine {
            return Some(MaintenanceHolder { operation, detail: operation_detail(operation).to_owned() });
        }

        held.foreign.as_ref().map(|foreign| foreign.holder.clone())
    }

    fn held_by(held: &Held) -> Option<String> {
        if let Some(operation) = held.mine {
            return Some(format!("this agent's {}", operation_detail(operation)));
        }

        held.foreign.as_ref().map(|foreign| format!("the container {}", foreign.name))
    }

    /// Adopts a surviving update container before requests are served.
    ///
    /// # Errors
    ///
    /// Fails when Docker cannot list the containers.
    pub async fn adopt_running(&self) -> Result<(), OpError> {
        if let Some(running) = self.options.docker.running_update_container().await? {
            self.watch(running, self.held());
        }

        Ok(())
    }

    /// # Errors
    ///
    /// Refuses with `maintenance_running` while the guard is held, and fails when Docker cannot
    /// say whether an update container runs.
    pub async fn claim(&self, operation: MaintenanceOperation) -> Result<Release, OpError> {
        {
            let mut held = self.held();

            if self.options.stop.is_cancelled() {
                return Err(OpError::Refused(AgentFailure::new(AgentErrorCode::MaintenanceRunning, "the agent is shutting down")));
            }

            // The caller logs the refusal, which names the holder.
            if let Some(holder) = Self::held_by(&held) {
                return Err(OpError::Refused(AgentFailure::new(AgentErrorCode::MaintenanceRunning, format!("{holder} is already running"))));
            }

            // Reserved before Docker is asked, so concurrent claims cannot both pass.
            held.mine = Some(operation);
        }

        self.changed();

        // Frees the reservation on every way out but success, a dropped claim included.
        let mut reservation = Reservation { guard: self, armed: true };
        let running = self.options.docker.running_update_container().await.map_err(|error| Box::new(error) as BoxError)?;

        if let Some(running) = running {
            let name = match &running {
                RunningUpdate::Cs2 { name, .. } | RunningUpdate::AgentSwap { name } => name.clone(),
            };

            // Handed to the update that holds the guard in the same hold, so no claim or launch
            // sees the guard free in between.
            reservation.armed = false;

            let mut held = self.held();

            held.mine = None;
            self.watch(running, held);

            return Err(OpError::Refused(AgentFailure::new(AgentErrorCode::MaintenanceRunning, format!("{name} is already running"))));
        }

        reservation.armed = false;

        let guard = self.clone();

        Ok(Release(Mutex::new(Some(Box::new(move |handed_to: Option<String>| {
            let free = {
                let guard = guard.clone();

                move || {
                    guard.held().mine = None;
                    guard.changed();
                }
            };

            match handed_to {
                None => free(),
                // Also a self-update helper, which Docker never removes, so this waits only for the exit.
                // The guard stays held, so this report is what tells the controller how the update ended.
                Some(container) => {
                    guard.follow(container, Arc::new(move |_| free()), None, WaitCondition::NotRunning);
                    guard.changed();
                }
            }
        })))))
    }

    /// Published in the hold that reads the state, so two changes on two threads cannot publish
    /// their readings in the opposite order.
    fn changed(&self) {
        let held = self.held();

        self.running.send_replace(Self::held_by(&held).is_some());
    }

    /// Takes the guard's state locked, so a claim hands its reservation over in one hold.
    fn watch(&self, running: RunningUpdate, mut held: MutexGuard<'_, Held>) {
        let (name, holder) = match &running {
            RunningUpdate::AgentSwap { name } => {
                (name, MaintenanceHolder { operation: MaintenanceOperation::AgentUpdate, detail: "the last agent update, proving its replacement".to_owned() })
            }
            RunningUpdate::Cs2 { name, .. } => {
                (name, MaintenanceHolder { operation: MaintenanceOperation::Cs2Update, detail: "an update container a previous agent started".to_owned() })
            }
        };

        held.foreign = Some(Foreign { name: name.clone(), holder });
        drop(held);

        let (container, kind) = match running {
            RunningUpdate::AgentSwap { name } => {
                self.changed();

                let guard = self.clone();
                let container = name.clone();

                self.follow(
                    name,
                    Arc::new(move |_| {
                        let mut held = guard.held();

                        if held.foreign.as_ref().is_some_and(|foreign| foreign.name == container) {
                            held.foreign = None;
                            drop(held);
                            guard.changed();
                        }
                    }),
                    None,
                    WaitCondition::NotRunning,
                );

                return;
            }
            RunningUpdate::Cs2 { name, kind } => (name, kind),
        };

        // The agent labels every update container it starts; one without its labels still writes the volume.
        match &kind {
            None => self.options.log.error(format!("the update container {container} is missing required labels; its update is not reported")),
            Some(kind) => {
                if let Some(state) = &self.options.state {
                    state.started(&kind.id, kind.mode, kind.automatic);
                }
            }
        }

        self.changed();
        self.options
            .log
            .warn(format!("an update container ({container}) is running on this host — this agent did not start it, so update_cs2 is refused until it exits"));

        let on_progress: Option<StepHook> = kind.map(|kind| {
            let on_progress = Arc::clone(&self.options.on_progress);

            Arc::new(move |progress: Cs2UpdateProgress| {
                on_progress(InstallProgress { update_id: kind.id.clone(), percent: progress.percent, message: progress.message });
            }) as StepHook
        });
        let guard = self.clone();
        let name = container.clone();
        let quiet = Arc::new(AtomicBool::new(false));
        let removed_quiet = Arc::clone(&quiet);
        let sweep_progress = on_progress.clone();
        let on_exit = Arc::new(move |code: Option<i64>| {
            if guard.held().foreign.as_ref().is_none_or(|foreign| foreign.name != name) {
                return;
            }

            // A container already gone leaves the outcome unknown.
            let (outcome, detail) = if removed_quiet.load(Ordering::SeqCst) {
                (Cs2UpdateOutcome::Failed, Some(format!("{name} printed nothing for {} s", UPDATE_QUIET.as_secs())))
            } else {
                match code {
                    None => (Cs2UpdateOutcome::Unobserved, None),
                    Some(0) => (Cs2UpdateOutcome::Completed, None),
                    Some(code) => (Cs2UpdateOutcome::Failed, Some(format!("the update container exited with {code}"))),
                }
            };

            let (guard, name, progress) = (guard.clone(), name.clone(), sweep_progress.clone());

            tokio::spawn(async move {
                // Swept under the guard, as after an update of this agent's own, so the node is free
                // only once the sweep is done.
                if let Some(config) = &guard.options.config {
                    if let Some(progress) = &progress {
                        progress(Cs2UpdateProgress { percent: None, message: SWEEP_STEP.to_owned() });
                    }

                    sweep_superseded_images(config, guard.options.docker.as_ref(), &[], &guard.options.log).await;
                }

                if let Some(state) = &guard.options.state {
                    state.ended(outcome, detail.as_deref());
                }

                // The check above ran in another hold, so it runs again where the guard is freed.
                {
                    let mut held = guard.held();

                    if held.foreign.as_ref().is_none_or(|foreign| foreign.name != name) {
                        return;
                    }

                    held.foreign = None;
                }

                guard.changed();
                guard.options.log.info(format!("the update container {name} has exited; updates are allowed again"));
            });
        });

        let activity = Activity::default();
        let on_line = activity.watch(update_line(self.options.on_log.clone(), on_progress));

        self.remove_when_quiet(container.clone(), activity, quiet);
        // Freed only once Docker removed it: the image sweep the free guard starts would otherwise
        // still list it as running and adopt it again.
        self.follow(container, on_exit, Some(on_line), WaitCondition::Removed);
    }

    /// Applies [`UPDATE_QUIET`] to an inherited update as to one this agent starts. The removed
    /// container's exit then frees the guard.
    fn remove_when_quiet(&self, container: String, activity: Activity, quiet: Arc<AtomicBool>) {
        let guard = self.clone();

        tokio::spawn(async move {
            if guard.options.stop.run_until_cancelled(activity.quiet_for(UPDATE_QUIET)).await.is_none() {
                return;
            }

            // An exit also ends the output, so check that the container still holds the guard.
            if guard.held().foreign.as_ref().is_none_or(|foreign| foreign.name != container) {
                return;
            }

            guard.options.log.error(format!("the update container {container} printed nothing for {} s; removing it", UPDATE_QUIET.as_secs()));
            quiet.store(true, Ordering::SeqCst);

            if let Err(error) = guard.options.docker.remove_container(&container, true).await {
                guard.options.log.warn(format!("could not remove {container}: {error}"));
            }
        });
    }

    /// `on_exit` is not called once shutdown has begun.
    fn follow(&self, container: String, on_exit: Arc<dyn Fn(Option<i64>) + Send + Sync>, on_line: Option<LineHook>, until: WaitCondition) {
        let guard = self.clone();
        let hooks = ContainerHooks {
            on_line: on_line.unwrap_or_else(|| Arc::new(|_| {})),
            on_exit: Arc::new(move |code| {
                if !guard.options.stop.is_cancelled() {
                    on_exit(code);
                }
            }),
        };
        let docker = Arc::clone(&self.options.docker);
        let log = self.options.log.clone();

        tokio::spawn(async move {
            // A container already gone has no log to stream; its exit still arrives.
            if let Err(error) = docker.follow_container(&container, hooks, until).await {
                log.info(format!("no log stream for {container}: {error}"));
            }
        });
    }
}

#[must_use]
pub fn build_update_spec(config: &AgentConfig, id: &str, update_id: &str, mode: Cs2UpdateMode, automatic: bool) -> StartContainerSpec {
    let mut spec = helper_spec(
        HelperSpec {
            name: "update".to_owned(),
            image: config.cs2_image.clone(),
            role: Some(ContainerRole::Update),
            // Also gone once it exits after an agent restart, so it cannot pin its image.
            auto_remove: true,
            env: env_to_array(&render_update_env(mode)),
            volumes: vec![VolumeMount { volume: DATA_VOLUME.to_owned(), target: INSTALL_DIR.to_owned(), read_only: false }],
            ..HelperSpec::default()
        },
        id,
    );

    // An agent that starts while the update runs reads these back to report it.
    spec.labels.insert(labels::UPDATE_ID.to_owned(), update_id.to_owned());
    spec.labels.insert(labels::UPDATE_MODE.to_owned(), wire_name(mode));
    spec.labels.insert(labels::UPDATE_AUTOMATIC.to_owned(), automatic.to_string());

    spec
}

#[derive(Clone)]
pub struct InstallOptions {
    /// The agent configuration, which names the CS2 image.
    pub config: ConfigReader,
    /// Pulls the image and runs the update container.
    pub docker: Arc<dyn DockerPort>,
    /// Receives the update's steps and steamcmd's progress.
    pub on_progress: ProgressHook,
    /// Every line the update container prints, for the admin log view.
    pub on_log: Option<LineHook>,
    /// Receives the update's start and finish lines.
    pub log: Logger,
    /// Whether any slot is busy, which refuses the update.
    pub any_busy: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The maintenance guard, shared by every operation on this host.
    pub updates: MaintenanceGuard,
    /// How thoroughly steamcmd updates the game files.
    pub mode: Cs2UpdateMode,
    /// True when the agent started it itself, to reach the newest public build.
    pub automatic: bool,
    /// Where it is reported as running, then as ended.
    pub state: Option<Cs2UpdateState>,
    /// Extracts during the update, so the next launch does not pay for it.
    pub overlay: Arc<dyn OverlayPort>,
    /// Names the update container, or a random short id when `None`.
    pub short_id: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}

/// Holds the guard. `run` does the work and gives the guard back.
pub struct BegunUpdate {
    pub update_id: String,
    pub run: BoxFuture<'static, Result<(), OpError>>,
}

fn slots_held() -> OpError {
    OpError::Refused(AgentFailure::code(AgentErrorCode::SlotsHeld))
}

/// The update is reported in `cs2.update` from here on. Busy slots refuse it before and after the
/// guard is taken, because the game files that every slot mounts are rewritten.
///
/// # Errors
///
/// Refuses with `slots_held` while a slot is busy, and as the guard refuses.
pub async fn begin_cs2_update(options: InstallOptions) -> Result<BegunUpdate, OpError> {
    if (options.any_busy)() {
        return Err(slots_held());
    }

    let release = options.updates.claim(MaintenanceOperation::Cs2Update).await?;

    if (options.any_busy)() {
        release.release(None);

        return Err(slots_held());
    }

    let update_id = crate::plugin_relay::uuid();

    if let Some(state) = &options.state {
        state.started(&update_id, options.mode, options.automatic);
    }

    let id = update_id.clone();

    Ok(BegunUpdate {
        update_id,
        run: Box::pin(async move {
            let result = run_install(&options, &id).await;

            release.release(result.as_ref().err().and_then(left_running));
            result
        }),
    })
}

/// A failed update returns its error after `cs2.update` reports the failure.
///
/// # Errors
///
/// Fails as [`begin_cs2_update`] refuses or the update fails.
pub async fn update_cs2(options: InstallOptions) -> Result<(), OpError> {
    (begin_cs2_update(options).await?.run).await
}

/// The CS2 image comes from `agent.yml`, and `update_cs2_image` changes it.
async fn run_install(options: &InstallOptions, update_id: &str) -> Result<(), OpError> {
    match install_steps(options, update_id).await {
        Ok(()) => {
            // Every slot is idle, so no game holds an image a previous update superseded.
            (options.on_progress)(InstallProgress { update_id: update_id.to_owned(), percent: None, message: SWEEP_STEP.to_owned() });
            sweep_superseded_images(&options.config, options.docker.as_ref(), &[], &options.log).await;

            if let Some(state) = &options.state {
                state.ended(Cs2UpdateOutcome::Completed, None);
            }

            Ok(())
        }
        Err(error) => {
            if let Some(state) = &options.state {
                let outcome = match failure_outcome(&error) {
                    FailureOutcome::PullFailed => Cs2UpdateOutcome::PullFailed,
                    FailureOutcome::OverlayFailed => Cs2UpdateOutcome::OverlayFailed,
                    FailureOutcome::Failed => Cs2UpdateOutcome::Failed,
                };
                let message = error.to_string();

                state.ended(outcome, Some(if message.is_empty() { "the update failed" } else { &message }));
            }

            Err(error)
        }
    }
}

type StepHook = Arc<dyn Fn(Cs2UpdateProgress) + Send + Sync>;

/// The progress line of the image sweep that ends an update, under the guard.
const SWEEP_STEP: &str = "removing images an update superseded";

async fn install_steps(options: &InstallOptions, update_id: &str) -> Result<(), OpError> {
    let on_progress = Arc::clone(&options.on_progress);
    let id = update_id.to_owned();
    let progress: StepHook =
        Arc::new(move |step: Cs2UpdateProgress| on_progress(InstallProgress { update_id: id.clone(), percent: step.percent, message: step.message }));
    let step = |message: String| progress(Cs2UpdateProgress { percent: None, message });
    let config = options.config.read();
    let image = config.cs2_image.clone();

    // An image already on the host is used as it is, so an image loaded over ssh survives an update.
    if options.docker.image_id(&image).await?.is_none() {
        step(format!("pulling {image}"));
        options.docker.pull_image(&image).await.map_err(|error| StepFailure::pull(&image, error))?;
    }

    // Extracted during the update, so an extraction failure is reported here.
    step("extracting the addon overlay".to_owned());

    let extracted = options.overlay.ensure(&image).await.map_err(StepFailure::overlay)?;

    step(format!("addon overlay ready for {}", extracted.image_id));
    step(update_message(options.mode).to_owned());

    let short = options.short_id.as_ref().map_or_else(short_id, |short_id| short_id());
    let spec = build_update_spec(&config, &short, update_id, options.mode, options.automatic);
    let log = &options.log;
    let mode = wire_name(options.mode);

    log.info(format!("cs2 {mode}: starting {} from {}", spec.name, spec.image));

    let exit_code = run_to_exit(options.docker.as_ref(), &spec, update_line(options.on_log.clone(), Some(Arc::clone(&progress))), UPDATE_QUIET, log)
        .await
        // The caller logs the failure, and `cs2.update` carries it to the controller.
        .map_err(|error| match error {
            HelperError::Docker(error) => InstallError::Start(error),
            quiet @ HelperError::Quiet { .. } => InstallError::Quiet(quiet),
        })?;

    if exit_code != Some(0) {
        let outcome = exit_code.map_or_else(|| "no observed status".to_owned(), |code| code.to_string());

        return Err(InstallError::Exited { outcome }.into());
    }

    log.info(format!("cs2 {mode}: {} finished", spec.name));
    progress(Cs2UpdateProgress { percent: Some(100.0), message: "install/update complete".to_owned() });

    Ok(())
}

/// `on_progress` is `None` for an inherited update whose container lacks its labels.
fn update_line(on_log: Option<LineHook>, on_progress: Option<StepHook>) -> LineHook {
    Arc::new(move |line: String| {
        if let Some(on_log) = &on_log {
            on_log(line.clone());
        }

        if let Some(on_progress) = &on_progress
            && let Some(progress) = parse_cs2_update_progress(&line)
        {
            on_progress(progress);
        }
    })
}
