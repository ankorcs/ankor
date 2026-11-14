//! The agent owns slot state, and the controller mirrors it. A server becomes `running` when its plugin
//! reports it ready.
//!
//! A slot is idle exactly when it holds no allocation: every way out of idle names one, and every
//! way back goes through `SlotManager::settle_idle`, which reports the server's end once.

use crate::sync::lock;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::config::AgentConfig;
use ankor_protocol::constants::{QUIT_TIMEOUT, SERVER_EXIT_DETAIL_MAX_CHARS, SLOT_RESERVATION, START_TIMEOUT, STOP_TIMEOUT};
use ankor_protocol::helpers::{slot_takes_launch, wire_name};
use ankor_protocol::types::{
    AgentErrorCode, AgentStopReason, ConfiguredSlot, GsltState, ServerExit, ServerExitReason, ServerLaunchConfig, SlotState, SlotStatus, StartRefusal,
    StartServerReply,
};
use indexmap::IndexMap;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::container_env::{
    blocked_launch_commands, captures_debug, env_to_array, match_id_of, render_container_env, render_launch_cfg, render_match_config, render_plugin_config,
    render_restore_file,
};
use crate::container_spec::{ContainerRole, GAME_CPU_WEIGHT, LabelInput, container_hardening, container_labels, container_name, cpu_shares_for_weight};
use crate::contract::constants::{MATCH_CONFIG_FILE, RESTORE_FILE};
use crate::contract::types::ServerStatus;
use crate::docker::{AdoptedContainer, ContainerHooks, DockerPort, NetworkMode, StartContainerSpec};
use crate::docker_api::{DockerError, WaitCondition};
use crate::event_spool::SpoolError;
use crate::failure::{AgentFailure, truncate};
use crate::gslt_keep_alive::{GsltSlotsPort, Release, SlotToken};
use crate::helper_container::short_id;
use crate::host_layout::{DEBUG_CAPTURE_DIR, DEBUG_CAPTURE_VOLUME, LAUNCH_CFG_PATH, MountSources, PLUGIN_CONFIG_PATH, VolumeMount, resolve_mounts};
use crate::logger::Logger;
use crate::overlay::{AcquiredOverlay, ExtractedOverlay, OverlayError, OverlayPort};
use crate::plugin_relay::new_relay_key;
use crate::slot_files::{CfgFile, PreparedSlot, SlotFilesPort};
use crate::spool_client::SpoolPort;
use crate::spool_client::queue_now;

/// What the slot manager tells the rest of the agent, in order.
pub enum SlotEvent {
    /// Ahead of the `idle` status of its slot.
    ServerExited(ServerExit),
    /// The slot's status changed.
    SlotState(SlotStatus),
    /// A line the server's container wrote.
    ContainerLog { allocation_id: String, line: String },
    /// The slot is idle again; `allocation_id` names the launch that left it, if one held it.
    SlotIdle { slot_id: String, allocation_id: Option<String> },
    /// `set_config` removed the slot; the controller learns it from the layout in the reply.
    SlotRemoved(String),
    /// A server logged on to Steam with this token: its first `ready`.
    TokenUsed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SlotError {
    /// A refusal the controller branches on.
    #[error(transparent)]
    Failure(AgentFailure),
    /// A `start_server` refusal from before any slot was taken; the allocation stays unused.
    #[error(transparent)]
    Refused(AgentFailure),
    /// A stop ended the launch of this allocation between its steps. The launch cleaned up and
    /// started no server.
    #[error("allocation {0} was stopped before its server started")]
    Stopped(String),
    /// A Docker call failed.
    #[error(transparent)]
    Docker(#[from] DockerError),
    /// The addon overlay for the cs2 image could not be acquired.
    #[error(transparent)]
    Overlay(#[from] OverlayError),
    /// The spool could not say whether the allocation ran before, or could not record it.
    #[error(transparent)]
    Spool(#[from] SpoolError),
    /// Writing the launch's files into its slot failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The launch task ended without a result, as when it panicked.
    #[error("the launch ended without an answer")]
    NoAnswer,
    /// A stop found the server without a container after its launch had ended.
    #[error("slot {slot_id} holds {allocation_id} without a container")]
    NoContainer { slot_id: String, allocation_id: String },
    /// Adoption found a second running server with an allocation that already holds a slot.
    #[error("cannot adopt {container}: allocation {allocation_id} already holds a slot")]
    AllocationHeld { container: String, allocation_id: String },
    /// Adoption could not stop a running server that it does not track.
    #[error("could not stop untracked container {container}: {source}")]
    StopUntracked {
        container: String,
        #[source]
        source: DockerError,
    },
}

/// A refusal keeps its code in the reply; anything else is `failed`.
impl From<SlotError> for crate::failure::OpError {
    fn from(error: SlotError) -> Self {
        match error {
            SlotError::Failure(failure) | SlotError::Refused(failure) => Self::Refused(failure),
            other => Self::Failed(Box::new(other)),
        }
    }
}

fn failure(code: AgentErrorCode, detail: impl Into<String>) -> SlotError {
    SlotError::Failure(AgentFailure::new(code, detail))
}

/// How a launch ended, as a stop waiting on it sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchOutcome {
    Started,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchPhase {
    /// Before the slot pick. The launch may yet take a slot, or be refused and take none.
    Picking,
    /// The launch holds a slot.
    Picked,
    /// The launch ended, with this outcome.
    Ended(LaunchOutcome),
}

/// Waits for the launch to hold a slot or end without one.
async fn picked(mut launching: watch::Receiver<LaunchPhase>) {
    let _ = launching.wait_for(|phase| *phase != LaunchPhase::Picking).await;
}

/// A launch whose task died without saying counts as failed.
async fn ended(mut launching: watch::Receiver<LaunchPhase>) -> LaunchOutcome {
    match launching.wait_for(|phase| matches!(phase, LaunchPhase::Ended(_))).await.map(|phase| *phase) {
        Ok(LaunchPhase::Ended(outcome)) => outcome,
        _ => LaunchOutcome::Failed,
    }
}

/// What is known of a server beyond its container.
#[derive(Default)]
struct Game {
    /// How long the server may stay empty once running; `None` runs until stopped.
    idle_shutdown: Option<Duration>,
    /// Since when the running server has had nobody on it.
    empty_since: Option<Instant>,
    /// Whether the server's plugin is connected to the relay.
    plugin_online: bool,
    /// Why the agent is stopping the server itself, for the `server_exited` that reports it.
    stop_reason: Option<AgentStopReason>,
    /// The detail for the `server_exited`, set only with `stop_reason`.
    stop_detail: Option<String>,
    /// The controller's `stop_server` is stopping the server.
    stop_requested: bool,
    /// The container's exit code, once the exit watch reports it.
    exit_code: Option<i64>,
    /// A stop of the agent's own, or its retry of one Docker refused, is under way.
    own_stop: bool,
    /// Docker stop calls not yet answered, the controller's and the agent's, so a retry waits for them.
    stop_calls: u32,
    /// The Steam ID of the server's token, once its plugin reports it.
    server_steam_id: Option<String>,
}

impl Game {
    fn new(idle_shutdown_seconds: Option<i64>) -> Self {
        Self { idle_shutdown: idle_shutdown_seconds.and_then(|seconds| u64::try_from(seconds).ok()).map(Duration::from_secs), ..Self::default() }
    }

    /// A stop by the agent, then one by the controller, decides first, because a stop makes the
    /// server exit with any code. Otherwise the exit code decides, or `lost` without one.
    fn exit_reason(&self) -> ServerExitReason {
        if let Some(reason) = self.stop_reason {
            return stop_to_exit(reason);
        }

        if self.stop_requested {
            return ServerExitReason::Requested;
        }

        match self.exit_code {
            None => ServerExitReason::Lost,
            Some(0) => ServerExitReason::CleanExit,
            Some(_) => ServerExitReason::Crashed,
        }
    }
}

/// A server on a slot, from its launch or adoption until it leaves the slot.
struct Server {
    /// The allocation the server was launched for.
    allocation_id: String,
    /// The match the server plays, if it was launched with one.
    match_id: Option<String>,
    /// The token the server started with; a token edit while it runs is for the next.
    token: String,
    /// `None` until Docker answers the start, which may come after the plugin reports ready.
    container: Option<String>,
    /// Held from the launch's first step, so a stop before the container releases it with the slot.
    overlay: Option<Arc<AcquiredOverlay>>,
    /// The cs2 image the launch picked its slot with, so an image switch meanwhile neither changes
    /// nor sweeps it. `None` for an adopted server, whose container already holds its image.
    image: Option<String>,
    /// What is known of the server beyond its container.
    game: Game,
}

impl Server {
    fn new(allocation_id: String, match_id: Option<String>, token: String, container: Option<String>, idle_shutdown_seconds: Option<i64>) -> Self {
        Self { allocation_id, match_id, token, container, overlay: None, image: None, game: Game::new(idle_shutdown_seconds) }
    }
}

/// A slot's phase, with what exists only in it. Dropping a timer cancels it.
enum Phase {
    /// No server. `reservation` is the timer that ends the slot's reservation, if it has one.
    Idle { reservation: Option<AbortOnDropHandle<()>> },
    /// Waiting for the plugin's first `ready`. `timer` stops the server at the start timeout.
    Starting { server: Server, timer: Option<AbortOnDropHandle<()>> },
    /// The plugin reported the server ready.
    Running(Server),
    /// The slot leaves this phase only when the exit watch reports the server gone. A stop Docker
    /// refused is tried again by `check_games`. `settled` holds the stops waiting for the exit.
    Stopping { server: Server, settled: Vec<oneshot::Sender<()>> },
}

impl Phase {
    const IDLE: Self = Self::Idle { reservation: None };

    fn state(&self) -> SlotState {
        match self {
            Self::Idle { .. } => SlotState::Idle,
            Self::Starting { .. } => SlotState::Starting,
            Self::Running(_) => SlotState::Running,
            Self::Stopping { .. } => SlotState::Stopping,
        }
    }

    fn is_idle(&self) -> bool {
        matches!(self, Self::Idle { .. })
    }

    fn server(&self) -> Option<&Server> {
        match self {
            Self::Idle { .. } => None,
            Self::Starting { server, .. } | Self::Running(server) | Self::Stopping { server, .. } => Some(server),
        }
    }

    fn server_mut(&mut self) -> Option<&mut Server> {
        match self {
            Self::Idle { .. } => None,
            Self::Starting { server, .. } | Self::Running(server) | Self::Stopping { server, .. } => Some(server),
        }
    }

    /// The server and the stops waiting on it, dropping the phase's timers; an idle phase comes back.
    fn into_server(self) -> Result<(Server, Vec<oneshot::Sender<()>>), Self> {
        match self {
            Self::Idle { .. } => Err(self),
            Self::Starting { server, .. } | Self::Running(server) => Ok((server, Vec::new())),
            Self::Stopping { server, settled } => Ok((server, settled)),
        }
    }
}

struct Runtime {
    /// Distinguishes this runtime from one that replaced it under the same slot id.
    instance: u64,
    /// The slot as configured now.
    slot: ConfiguredSlot,
    /// What the agent last learned of the slot's token.
    gslt_state: GsltState,
    /// As last reported; a reservation timer cancelled at shutdown leaves it set.
    reserved: bool,
    /// Bumped on every launch and settle, so a late exit cannot clobber a newer container.
    generation: u64,
    /// Logons of the slot's token under way; the slot is reserved while any is.
    logons: u32,
    /// The slot's phase and its server, if it holds one.
    phase: Phase,
}

impl Runtime {
    fn new(slot: ConfiguredSlot, instance: u64) -> Self {
        Self { instance, slot, gslt_state: GsltState::Unchecked, reserved: false, generation: 0, logons: 0, phase: Phase::IDLE }
    }

    fn status(&self) -> SlotStatus {
        let server = self.phase.server();

        SlotStatus {
            slot_id: self.slot.id.clone(),
            state: self.phase.state(),
            allocation_id: server.map(|server| server.allocation_id.clone()),
            match_id: server.and_then(|server| server.match_id.clone()),
            plugin_online: server.is_some_and(|server| server.game.plugin_online),
            gslt_state: self.gslt_state,
            reserved: self.reserved,
            server_steam_id: server.and_then(|server| server.game.server_steam_id.clone()),
        }
    }

    fn allocation_id(&self) -> Option<&str> {
        self.phase.server().map(|server| server.allocation_id.as_str())
    }

    fn begin(&mut self, server: Server) -> u64 {
        self.generation += 1;
        self.phase = Phase::Starting { server, timer: None };
        self.generation
    }

    fn take_phase(&mut self) -> Phase {
        std::mem::replace(&mut self.phase, Phase::IDLE)
    }

    fn clear_reservation(&mut self) {
        if let Phase::Idle { reservation } = &mut self.phase {
            *reservation = None;
        }
    }
}

fn idle_too_long(runtime: &Runtime, now: Instant) -> bool {
    matches!(runtime.phase, Phase::Running(_))
        && runtime.phase.server().is_some_and(
            |server| matches!((server.game.idle_shutdown, server.game.empty_since), (Some(limit), Some(since)) if now.duration_since(since) >= limit),
        )
}

fn stop_to_exit(reason: AgentStopReason) -> ServerExitReason {
    match reason {
        AgentStopReason::IdleEmpty => ServerExitReason::IdleEmpty,
        AgentStopReason::ConfigRejected => ServerExitReason::ConfigRejected,
        AgentStopReason::StartTimeout => ServerExitReason::StartTimeout,
        AgentStopReason::QuitTimeout => ServerExitReason::QuitTimeout,
        AgentStopReason::SlotRemoved => ServerExitReason::SlotRemoved,
    }
}

fn reason_name(reason: AgentStopReason) -> String {
    wire_name(reason)
}

/// The per-launch files, each with its contents or `None` to remove an earlier launch's.
#[must_use]
pub(crate) fn render_cfg_files(launch: &ServerLaunchConfig) -> Vec<CfgFile> {
    vec![
        CfgFile { rel_path: LAUNCH_CFG_PATH.clone(), contents: Some(render_launch_cfg(launch)) },
        CfgFile { rel_path: PLUGIN_CONFIG_PATH.to_owned(), contents: Some(render_plugin_config(launch)) },
        CfgFile { rel_path: MATCH_CONFIG_FILE.to_owned(), contents: render_match_config(launch) },
        CfgFile { rel_path: RESTORE_FILE.to_owned(), contents: render_restore_file(launch) },
    ]
}

#[must_use]
#[allow(clippy::too_many_arguments, reason = "each argument is a separate input to the container spec")]
pub(crate) fn build_server_launch(
    config: &AgentConfig,
    slot: &ConfiguredSlot,
    launch: &ServerLaunchConfig,
    plugin_url: &str,
    relay_key: &str,
    id: &str,
    prepared: &PreparedSlot,
    overlay: &ExtractedOverlay,
    allocation_id: &str,
) -> StartContainerSpec {
    let path = |path: &std::path::Path| path.to_string_lossy().into_owned();
    let (slot_root, shared_root, overlay_dir, swiftly_root, cfg_dir, demo_dir) = (
        path(&prepared.root),
        path(&prepared.shared_root),
        path(&overlay.dir),
        path(&prepared.swiftly_root),
        path(&prepared.cfg_dir),
        path(&prepared.demo_dir),
    );
    let mounts = resolve_mounts(&MountSources {
        slot_root: &slot_root,
        shared_root: &shared_root,
        overlay_dir: &overlay_dir,
        swiftly_root: &swiftly_root,
        cfg_dir: &cfg_dir,
        node_plugins: &prepared.node_plugins,
        per_launch_files: &prepared.files,
        demo_dir: &demo_dir,
    });
    let mut volumes = mounts.volumes;

    if captures_debug(launch) {
        volumes.push(VolumeMount { volume: DEBUG_CAPTURE_VOLUME.to_owned(), target: DEBUG_CAPTURE_DIR.to_owned(), read_only: false });
    }

    let hardening = container_hardening(config.memory_mb);
    let match_id = match_id_of(launch);

    StartContainerSpec {
        name: container_name(&slot.id, ContainerRole::Game, id),
        image: overlay.image_id.clone(),
        env: env_to_array(&render_container_env(slot, launch, plugin_url, allocation_id)),
        labels: container_labels(
            ContainerRole::Game,
            &LabelInput {
                slot_id: Some(&slot.id),
                overlay_image_id: Some(&overlay.image_id),
                allocation_id: Some(allocation_id),
                match_id: match_id.as_deref(),
                idle_shutdown_seconds: launch.idle_shutdown_seconds,
                relay_key: Some(relay_key),
                gslt: Some(&slot.gslt),
            },
        ),
        network_mode: NetworkMode::Host,
        cap_drop: hardening.cap_drop,
        security_opt: hardening.security_opt,
        pids_limit: hardening.pids_limit,
        memory_bytes: hardening.memory_bytes,
        cpu_shares: Some(cpu_shares_for_weight(GAME_CPU_WEIGHT)),
        auto_remove: true,
        volumes,
        binds: mounts.binds,
        tmpfs: mounts.tmpfs,
        cmd: None,
        entrypoint: None,
    }
}

pub(crate) type PluginUrlFn = Arc<dyn Fn(&str, &str, &str) -> String + Send + Sync>;
pub(crate) type PollFn = Arc<dyn Fn(&str) + Send + Sync>;
pub(crate) type ConfigFn = Arc<dyn Fn() -> AgentConfig + Send + Sync>;

pub struct SlotManagerDeps {
    /// Reads the config the node runs with now.
    pub config: ConfigFn,
    /// The Docker Engine that runs the game servers.
    pub docker: Arc<dyn DockerPort>,
    /// Writes each launch's files into its slot.
    pub files: Arc<dyn SlotFilesPort>,
    /// Extracts and holds the addon overlay for each cs2 image.
    pub overlay: Arc<dyn OverlayPort>,
    /// Prepares the relay entry for a server and returns the URL that its plugin dials.
    pub plugin_url: PluginUrlFn,
    /// The allocations the node launched, so none launches twice; `None` while the spool is
    /// unavailable, when the node refuses launches anyway.
    pub allocations: Option<Arc<dyn SpoolPort>>,
    /// Asks a server's plugin for a `server_status`, which says whether anybody is on it.
    pub poll_plugin: PollFn,
    /// Takes each event as it happens, in the same lock hold as the change behind it, so the
    /// frames it queues are in the order of the changes. It must not call back into the manager.
    pub report: Option<ReportFn>,
    /// The slot manager's logger.
    pub log: Logger,
    /// Cancelled when the agent shuts down. After that, no launch or reservation timer runs out.
    pub stop: CancellationToken,
}

pub(crate) type ReportFn = Arc<dyn Fn(&SlotEvent) + Send + Sync>;

struct State {
    /// The slots by id, in configured order.
    slots: IndexMap<String, Runtime>,
    /// Launches still in flight, by allocation.
    in_flight: HashMap<String, watch::Receiver<LaunchPhase>>,
    /// Logons under way, by token. A token `set_config` moved to another slot meanwhile keeps that
    /// slot from launches too, as a server that logged on with it would be logged off.
    logging_on: HashMap<String, u32>,
}

struct Inner {
    deps: SlotManagerDeps,
    state: Mutex<State>,
    events: mpsc::UnboundedSender<SlotEvent>,
    instances: AtomicU64,
}

#[derive(Clone)]
pub struct SlotManager {
    inner: Arc<Inner>,
}

impl SlotManager {
    #[must_use]
    pub fn new(deps: SlotManagerDeps) -> (Self, mpsc::UnboundedReceiver<SlotEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let instances = AtomicU64::new(0);
        let slots = (deps.config)().slots.into_iter().map(|slot| (slot.id.clone(), Runtime::new(slot, instances.fetch_add(1, Ordering::SeqCst)))).collect();
        let state = State { slots, in_flight: HashMap::new(), logging_on: HashMap::new() };

        (Self { inner: Arc::new(Inner { deps, state: Mutex::new(state), events, instances }) }, receiver)
    }

    fn emit(&self, event: SlotEvent) {
        if let Some(report) = &self.inner.deps.report {
            report(&event);
        }

        let _ = self.inner.events.send(event);
    }

    fn log(&self) -> &Logger {
        &self.inner.deps.log
    }

    fn report(&self, runtime: &Runtime) {
        self.emit(SlotEvent::SlotState(runtime.status()));
    }

    #[must_use]
    pub fn statuses(&self) -> Vec<SlotStatus> {
        lock(&self.inner.state).slots.values().map(Runtime::status).collect()
    }

    fn holding<'a>(state: &'a mut State, allocation_id: &str) -> Option<&'a mut Runtime> {
        state.slots.values_mut().find(|runtime| runtime.allocation_id() == Some(allocation_id))
    }

    #[must_use]
    pub fn holds(&self, allocation_id: &str) -> bool {
        Self::holding(&mut lock(&self.inner.state), allocation_id).is_some()
    }

    #[must_use]
    pub(crate) fn slot_of(&self, allocation_id: &str) -> Option<String> {
        Self::holding(&mut lock(&self.inner.state), allocation_id).map(|runtime| runtime.slot.id.clone())
    }

    #[must_use]
    pub fn any_busy(&self) -> bool {
        lock(&self.inner.state).slots.values().any(|runtime| !runtime.phase.is_idle())
    }

    /// The allocation holding the slot in any phase, not only `running`.
    #[must_use]
    pub fn running_allocation(&self, slot_id: &str) -> Option<String> {
        lock(&self.inner.state).slots.get(slot_id).and_then(|runtime| runtime.allocation_id().map(str::to_owned))
    }

    pub fn plugin_online_changed(&self, allocation_id: &str, online: bool) {
        let mut state = lock(&self.inner.state);

        if let Some(runtime) = Self::holding(&mut state, allocation_id)
            && let Some(server) = runtime.phase.server_mut()
            && server.game.plugin_online != online
        {
            server.game.plugin_online = online;
            self.report(runtime);
        }
    }

    /// A refused config stops the server; the first `ready` status makes a starting slot running,
    /// and later ones say whether anybody is on it.
    pub fn plugin_status(&self, allocation_id: &str, status: &ServerStatus) {
        let mut state = lock(&self.inner.state);
        let Some(runtime) = Self::holding(&mut state, allocation_id) else {
            return;
        };

        if let Some(error) = &status.config_error {
            self.log().error(format!("slot {}: the plugin refused its match config: {error}", runtime.slot.id));

            let slot_id = runtime.slot.id.clone();

            drop(state);
            self.stop_on_own(&slot_id, allocation_id, Some(AgentStopReason::ConfigRejected), Some(error.clone()), STOP_TIMEOUT.as_secs(), |_| true);
            return;
        }

        // Steam gives a token one SteamID, so the first status that carries it is the one reported.
        let learned = match (runtime.phase.server_mut(), &status.server_steam_id) {
            (Some(server), Some(steam_id)) if server.game.server_steam_id.is_none() => {
                server.game.server_steam_id = Some(steam_id.clone());
                true
            }
            _ => false,
        };

        // The idle clock starts when the server can take players, as a workshop map can take the
        // better part of a minute to load.
        match runtime.take_phase() {
            Phase::Starting { server, .. } if status.ready => {
                let token = server.token.clone();

                runtime.phase = Phase::Running(server);

                // Ready means the server logged on to Steam, which CS2 does only with a token Steam
                // accepts. The keep-alive never checks a token in use, so this is how a slot that is
                // rarely idle learns its token is good.
                if token == runtime.slot.gslt {
                    runtime.gslt_state = GsltState::Ok;
                }

                self.report(runtime);
                self.log().info(format!("slot {}: ready", runtime.slot.id));
                self.emit(SlotEvent::TokenUsed(token));
            }
            starting @ Phase::Starting { .. } => {
                runtime.phase = starting;

                if learned {
                    self.report(runtime);
                }
                return;
            }
            phase => {
                runtime.phase = phase;

                if learned {
                    self.report(runtime);
                }
            }
        }

        if let Some(server) = runtime.phase.server_mut() {
            if status.players.is_empty() {
                server.game.empty_since.get_or_insert_with(Instant::now);
            } else {
                server.game.empty_since = None;
            }
        }
    }

    /// A server still holding this allocation once the timeout runs out hung on its way out.
    pub fn plugin_quitting(&self, allocation_id: &str) {
        let manager = self.clone();
        let allocation_id = allocation_id.to_owned();

        tokio::spawn(async move {
            // Game servers outlive the agent, so no quit timeout stops one once shutdown has begun.
            if manager.inner.deps.stop.run_until_cancelled(tokio::time::sleep(QUIT_TIMEOUT)).await.is_none() {
                return;
            }

            let slot = manager.slot_of(&allocation_id);

            if let Some(slot_id) = slot {
                manager.log().error(format!("slot {slot_id}: game hung, stopping container"));
                manager.stop_on_own(&slot_id, &allocation_id, Some(AgentStopReason::QuitTimeout), None, STOP_TIMEOUT.as_secs(), |_| true);
            }
        });
    }

    /// Polls the servers whose plugin is online, stops those empty for longer than their launch
    /// allows, and tries again a stop of the agent's own that failed.
    pub fn check_games(&self, now: Instant) {
        let mut retries = Vec::new();
        let mut idle = Vec::new();
        let mut polls = Vec::new();

        {
            let state = lock(&self.inner.state);

            for runtime in state.slots.values() {
                let Some(Server { allocation_id, game, .. }) = runtime.phase.server() else {
                    continue;
                };

                // Once no stop call is waiting for Docker, the last one failed, so it is retried.
                // While a launch of the allocation is in flight, a stop waits for it before it
                // calls Docker.
                if game.stop_reason.is_some() || game.stop_requested {
                    if game.stop_calls == 0 && !state.in_flight.contains_key(allocation_id) {
                        retries.push((runtime.slot.id.clone(), allocation_id.clone()));
                    }

                    continue;
                }

                if game.plugin_online {
                    polls.push(allocation_id.clone());
                }

                if idle_too_long(runtime, now)
                    && let Some(limit) = game.idle_shutdown
                {
                    self.log().info(format!("slot {}: empty for {}s, stopping", runtime.slot.id, limit.as_secs()));
                    idle.push((runtime.slot.id.clone(), allocation_id.clone()));
                }
            }
        }

        for allocation_id in polls {
            (self.inner.deps.poll_plugin)(&allocation_id);
        }

        for (slot_id, allocation_id) in retries {
            self.stop_on_own(&slot_id, &allocation_id, None, None, STOP_TIMEOUT.as_secs(), |_| true);
        }

        for (slot_id, allocation_id) in idle {
            self.stop_on_own(&slot_id, &allocation_id, Some(AgentStopReason::IdleEmpty), None, STOP_TIMEOUT.as_secs(), |runtime| idle_too_long(runtime, now));
        }
    }

    /// Runs one stop at a time per server. The first reason stays, so a retry, which passes none,
    /// reports why the server was stopped. A stop that ends after its server left the slot leaves
    /// the next server's stop alone.
    ///
    /// The caller decided in an earlier hold of the lock. So it names the server, and `still` the
    /// condition it decided on. If the slot now holds another server, or the condition no longer
    /// holds, nothing is stopped.
    fn stop_on_own(
        &self,
        slot_id: &str,
        expected: &str,
        reason: Option<AgentStopReason>,
        detail: Option<String>,
        timeout_seconds: u64,
        still: impl FnOnce(&Runtime) -> bool,
    ) {
        let (allocation_id, generation) = {
            let mut state = lock(&self.inner.state);
            let Some(runtime) = state.slots.get_mut(slot_id).filter(|runtime| still(runtime)) else {
                return;
            };
            let generation = runtime.generation;
            let Some(server) = runtime.phase.server_mut() else {
                return;
            };

            if server.allocation_id != expected || server.game.own_stop {
                return;
            }

            if server.game.stop_reason.is_none() && reason.is_some() {
                server.game.stop_reason = reason;
                server.game.stop_detail = detail;
            }

            server.game.own_stop = true;
            (server.allocation_id.clone(), generation)
        };
        let manager = self.clone();
        let slot_id = slot_id.to_owned();

        tokio::spawn(async move {
            if let Err(error) = manager.stop(&slot_id, timeout_seconds, &allocation_id).await {
                let reason = lock(&manager.inner.state)
                    .slots
                    .get(&slot_id)
                    .and_then(|runtime| runtime.phase.server())
                    .and_then(|server| server.game.stop_reason)
                    .or(reason)
                    .map_or_else(|| "requested".to_owned(), reason_name);

                manager.log().warn(format!("slot {slot_id}: {reason} stop failed: {error}; trying again on the next check"));
            }

            if let Some(runtime) = lock(&manager.inner.state).slots.get_mut(&slot_id)
                && runtime.generation == generation
                && let Some(server) = runtime.phase.server_mut()
            {
                server.game.own_stop = false;
            }
        });
    }

    /// Busy slots that a layout would remove or move, or whose server's token it would hand to
    /// another slot, since two servers cannot run on one token.
    fn blocked_by_running(state: &State, next: &[ConfiguredSlot]) -> Vec<String> {
        state
            .slots
            .iter()
            .filter_map(|(id, runtime)| runtime.phase.server().map(|server| (id, runtime, server)))
            .filter(|(id, runtime, server)| {
                let replacement = next.iter().find(|slot| &slot.id == *id);

                replacement.is_none_or(|replacement| replacement.port != runtime.slot.port || replacement.tv_port != runtime.slot.tv_port)
                    || next.iter().any(|slot| &slot.id != *id && slot.gslt == server.token)
            })
            .map(|(id, _, _)| id.clone())
            .collect()
    }

    fn layout_refusal(state: &State, next: &[ConfiguredSlot]) -> Option<SlotError> {
        let blocked = Self::blocked_by_running(state, next);

        (!blocked.is_empty()).then(|| failure(AgentErrorCode::SlotHeld, format!("held slots would change: {}", blocked.join(", "))))
    }

    /// Refuses a layout that would move or remove a busy slot, or hand its token to another slot.
    /// The refusal, `check`, `commit` and the change share one hold of the slot lock, so no launch
    /// takes a slot between the checks and the change. If `commit` answers false, the layout stays
    /// and the result is `None`.
    ///
    /// # Errors
    ///
    /// The layout's refusal through `refused`, or what `check` or `commit` return.
    pub(crate) fn change_slots<E>(
        &self,
        next: &[ConfiguredSlot],
        refused: impl FnOnce(SlotError) -> E,
        check: impl FnOnce(bool) -> Result<(), E>,
        commit: impl FnOnce() -> Result<bool, E>,
    ) -> Result<Option<Vec<String>>, E> {
        let mut state = lock(&self.inner.state);

        if let Some(refusal) = Self::layout_refusal(&state, next) {
            return Err(refused(refusal));
        }

        check(state.slots.values().any(|runtime| !runtime.phase.is_idle()))?;

        if !commit()? {
            return Ok(None);
        }

        Ok(Some(self.apply_locked(&mut state, next)))
    }

    /// Applies a layout, and returns the ids of the slots whose token is new, added slots among them.
    /// A server keeps the token it started with. Nothing is reported.
    fn apply_locked(&self, state: &mut State, next: &[ConfiguredSlot]) -> Vec<String> {
        let mut previous = std::mem::take(&mut state.slots);
        let mut new_tokens = Vec::new();

        for config in next {
            let runtime = match previous.shift_remove(&config.id) {
                Some(mut existing) => {
                    if existing.slot.gslt != config.gslt {
                        new_tokens.push(config.id.clone());
                    }

                    existing.slot = config.clone();
                    existing
                }
                None => {
                    new_tokens.push(config.id.clone());
                    Runtime::new(config.clone(), self.inner.instances.fetch_add(1, Ordering::SeqCst))
                }
            };

            // Rebuilt in order, as the controller assigns slot ids by position.
            state.slots.insert(config.id.clone(), runtime);
        }

        for (id, runtime) in previous {
            drop(runtime);
            self.emit(SlotEvent::SlotRemoved(id));
        }

        self.log().info(format!("slots are now {}", next.iter().map(|slot| slot.id.as_str()).collect::<Vec<_>>().join(", ")));

        for id in &new_tokens {
            if let Some(runtime) = state.slots.get_mut(id) {
                runtime.clear_reservation();
                runtime.gslt_state = GsltState::Unchecked;
                runtime.reserved = runtime.logons > 0;
            }
        }

        new_tokens
    }

    /// Reports these slots as they are now, once the controller has the layout that names them.
    pub fn announce(&self, slot_ids: &[String]) {
        let state = lock(&self.inner.state);

        for id in slot_ids {
            if let Some(runtime) = state.slots.get(id) {
                self.report(runtime);
            }
        }
    }

    /// A stop of the allocation waits from this call on, also while `read` is pending. `admit` gets
    /// what `read` gave and runs with the slot pick, nothing awaited between, so it can refuse the
    /// launch.
    ///
    /// # Errors
    ///
    /// Refuses a launch for an allocation in use or used before, with no free slot, or one `admit`
    /// refuses; fails as the launch does.
    pub async fn start_with<T: Send + 'static>(
        &self,
        launch: ServerLaunchConfig,
        allocation_id: &str,
        read: impl Future<Output = T> + Send + 'static,
        admit: impl FnOnce(T) -> Result<(), SlotError> + Send + 'static,
    ) -> Result<StartServerReply, SlotError> {
        // Checked and claimed in one hold of the lock, so two launches of one allocation cannot both pass.
        let (phase, used_query) = {
            let mut state = lock(&self.inner.state);

            // A launch that comes once shutdown began starts nothing, so another node can take it.
            if self.inner.deps.stop.is_cancelled() {
                return Err(SlotError::Refused(AgentFailure::start_refused(StartRefusal::ShuttingDown, None)));
            }

            if state.in_flight.contains_key(allocation_id) {
                return Err(failure(AgentErrorCode::AllocationInUse, format!("allocation {allocation_id} already has a launch in progress")));
            }

            // Asked before anything else: the spool answers in order, so a record queued before this is seen.
            let used_query = self.inner.deps.allocations.clone().map(|spool| {
                let allocation = allocation_id.to_owned();

                queue_now(async move { spool.allocation_used(allocation).await })
            });
            let (phase, launching) = watch::channel(LaunchPhase::Picking);

            state.in_flight.insert(allocation_id.to_owned(), launching);
            (phase, used_query)
        };
        let manager = self.clone();
        let allocation = allocation_id.to_owned();
        // A task of its own, so the launch runs on if the caller stops waiting.
        let launch = tokio::spawn(async move {
            let launched_before = async {
                match used_query {
                    Some(query) => query.await,
                    None => Ok(false),
                }
            };
            let (value, used) = tokio::join!(read, launched_before);
            let result = match used {
                // Unknown counts as used, so a launch that may have run never runs twice.
                Err(error) => {
                    manager.record_allocation_quietly(&allocation).await;
                    Err(SlotError::Spool(error))
                }
                Ok(true) => Err(failure(
                    AgentErrorCode::AllocationReused,
                    format!("allocation {allocation} was launched on this node before; every launch needs a new one"),
                )),
                Ok(false) => manager.start_allocated(&launch, &allocation, &phase, move || admit(value)).await,
            };
            let outcome = match &result {
                Ok(_) => LaunchOutcome::Started,
                Err(SlotError::Stopped(_)) => LaunchOutcome::Stopped,
                Err(_) => LaunchOutcome::Failed,
            };
            // The entry is this launch's, as a second launch of the allocation was refused while it stood.
            lock(&manager.inner.state).in_flight.remove(&allocation);
            phase.send_replace(LaunchPhase::Ended(outcome));
            result
        });

        launch.await.unwrap_or(Err(SlotError::NoAnswer))
    }

    /// Queued in the spool when called, and the spool answers in order, so a launch that follows
    /// sees it. The future settles once the spool has written it or failed to.
    fn record_allocation(&self, allocation_id: &str) -> impl Future<Output = Result<(), SpoolError>> + Send + 'static {
        let spool = self.inner.deps.allocations.clone();
        let allocation_id = allocation_id.to_owned();

        queue_now(async move {
            match spool {
                Some(spool) => spool.record_allocation(allocation_id).await,
                None => Ok(()),
            }
        })
    }

    /// Records the allocation where nothing waits for the answer, and logs a failure.
    fn record_allocation_quietly(&self, allocation_id: &str) -> impl Future<Output = ()> + Send + 'static {
        let (recorded, log, allocation_id) = (self.record_allocation(allocation_id), self.log().clone(), allocation_id.to_owned());

        async move {
            if let Err(error) = recorded.await {
                log.warn(format!("could not record allocation {allocation_id} as launched: {error}"));
            }
        }
    }

    /// Stops the allocation's server. A launch still under way sees the stop after its current step
    /// and cleans up instead of starting the server, so the reply never waits for a whole launch.
    ///
    /// # Errors
    ///
    /// Refuses an allocation that holds no slot; fails as Docker does.
    pub async fn stop_allocation(&self, allocation_id: &str) -> Result<(), SlotError> {
        let launching = lock(&self.inner.state).in_flight.get(allocation_id).cloned();

        if let Some(launching) = &launching {
            picked(launching.clone()).await;
        }

        let slot_id = {
            let mut state = lock(&self.inner.state);
            let Some(runtime) = Self::holding(&mut state, allocation_id) else {
                return Err(SlotError::Failure(AgentFailure::code(AgentErrorCode::UnknownAllocation)));
            };

            if let Some(server) = runtime.phase.server_mut() {
                server.game.stop_requested = true;
            }

            runtime.slot.id.clone()
        };

        if let Some(launching) = launching
            && ended(launching).await == LaunchOutcome::Stopped
        {
            return Ok(());
        }

        self.stop(&slot_id, STOP_TIMEOUT.as_secs(), allocation_id).await
    }

    fn stop_between_steps(&self, slot_id: &str, allocation_id: &str) -> Result<(), SlotError> {
        let stopped = lock(&self.inner.state).slots.get(slot_id).and_then(|runtime| runtime.phase.server()).is_some_and(|server| server.game.stop_requested);

        if stopped { Err(SlotError::Stopped(allocation_id.to_owned())) } else { Ok(()) }
    }

    async fn start_allocated(
        &self,
        launch: &ServerLaunchConfig,
        allocation_id: &str,
        phase: &watch::Sender<LaunchPhase>,
        admit: impl FnOnce() -> Result<(), SlotError>,
    ) -> Result<StartServerReply, SlotError> {
        // Admitted, picked and marked starting in one hold of the lock: concurrent launches take
        // different slots, and an update that claims the guard either is seen here or sees the
        // slot busy.
        let (slot, generation, instance, image) =
            {
                let mut state = lock(&self.inner.state);

                admit()?;

                if Self::holding(&mut state, allocation_id).is_some() {
                    return Err(failure(AgentErrorCode::AllocationInUse, format!("allocation {allocation_id} already holds a slot")));
                }

                let state = &mut *state;
                let logging_on = &state.logging_on;
                let Some(runtime) = state.slots.values_mut().find(|runtime| {
                    slot_takes_launch(runtime.phase.state(), runtime.gslt_state, runtime.reserved) && !logging_on.contains_key(&runtime.slot.gslt)
                }) else {
                    return Err(SlotError::Refused(AgentFailure::start_refused(StartRefusal::NoFreeSlot, None)));
                };

                // The server starts with the slot and image as picked, so a token edit or image switch
                // while it prepares is for the next server.
                let image = (self.inner.deps.config)().cs2_image;
                let mut server = Server::new(allocation_id.to_owned(), match_id_of(launch), runtime.slot.gslt.clone(), None, launch.idle_shutdown_seconds);

                server.image = Some(image.clone());

                let generation = runtime.begin(server);

                self.report(runtime);
                (runtime.slot.clone(), generation, runtime.instance, image)
            };
        phase.send_replace(LaunchPhase::Picked);
        // Used once it holds a slot, and written before Docker is asked for anything, so an agent
        // that dies mid-launch leaves the allocation used, whatever became of its server.
        // One that cannot be recorded could run again after this one, so it does not run at all.
        let recorded = self.record_allocation(allocation_id).await;
        // A stop that waited for the pick marks the slot before the launch's first step.
        tokio::task::yield_now().await;

        let result = match recorded {
            Ok(()) => self.launch_steps(launch, allocation_id, &slot, &image, generation, instance).await,
            Err(error) => Err(SlotError::Spool(error)),
        };

        if let Err(error) = &result {
            // An early exit that settled the slot already moved its generation on, so this settles
            // nothing. Docker refused the create or removed what it could not start, unless the
            // start got no answer and the removal failed too.
            match error {
                SlotError::Stopped(_) => self.settle_idle(&slot.id, instance, generation, None, None),
                other => self.settle_idle(&slot.id, instance, generation, Some(ServerExitReason::LaunchFailed), Some(other.to_string())),
            }
        }

        result
    }

    async fn launch_steps(
        &self,
        launch: &ServerLaunchConfig,
        allocation_id: &str,
        slot: &ConfiguredSlot,
        image: &str,
        generation: u64,
        instance: u64,
    ) -> Result<StartServerReply, SlotError> {
        let relay_key = new_relay_key();
        let plugin_url = (self.inner.deps.plugin_url)(&slot.id, allocation_id, &relay_key);

        self.stop_between_steps(&slot.id, allocation_id)?;

        let mut config = (self.inner.deps.config)();

        config.cs2_image = image.to_owned();

        let overlay = Arc::new(self.inner.deps.overlay.acquire(&config.cs2_image).await?);

        if let Some(server) = lock(&self.inner.state)
            .slots
            .get_mut(&slot.id)
            .filter(|runtime| runtime.instance == instance && runtime.generation == generation)
            .and_then(|runtime| runtime.phase.server_mut())
        {
            server.overlay = Some(Arc::clone(&overlay));
        }

        self.stop_between_steps(&slot.id, allocation_id)?;

        for (blocked, line) in blocked_launch_commands(launch) {
            self.log().warn(format!("slot {}: left out the launch command {line:?}, {blocked} is blocked during a match", slot.id));
        }

        let prepared = self.inner.deps.files.prepare(&slot.id, allocation_id, &render_cfg_files(launch)).await?;

        self.stop_between_steps(&slot.id, allocation_id)?;

        let spec = build_server_launch(&config, slot, launch, &plugin_url, &relay_key, &short_id(), &prepared, &overlay.overlay, allocation_id);
        let hooks = self.container_hooks(&slot.id, instance, generation);
        let id = self.inner.deps.docker.start_container(&spec, hooks).await?;

        {
            let mut state = lock(&self.inner.state);

            if let Some(runtime) = state.slots.get_mut(&slot.id).filter(|runtime| runtime.instance == instance && runtime.generation == generation) {
                let stop_requested = runtime.phase.server_mut().is_some_and(|server| {
                    server.container = Some(id.clone());
                    server.game.stop_requested
                });

                self.report(runtime);

                // A stop asked for while the container started stops it as it would a running server.
                if !stop_requested {
                    self.arm_start_timeout(runtime, generation);
                }
            }
        }

        self.log().info(format!("slot {}: started {} ({})", slot.id, spec.name, id.chars().take(12).collect::<String>()));

        Ok(StartServerReply { allocation_id: allocation_id.to_owned(), slot_id: slot.id.clone() })
    }

    fn container_hooks(&self, slot_id: &str, instance: u64, generation: u64) -> ContainerHooks {
        let lines = self.clone();
        let exits = self.clone();
        let (line_slot, exit_slot) = (slot_id.to_owned(), slot_id.to_owned());

        ContainerHooks {
            on_line: Arc::new(move |line| lines.forward_line(&line_slot, instance, generation, line)),
            on_exit: Arc::new(move |code| exits.handle_exit(&exit_slot, instance, generation, code)),
        }
    }

    async fn stop(&self, slot_id: &str, timeout_seconds: u64, allocation_id: &str) -> Result<(), SlotError> {
        let launching = lock(&self.inner.state).in_flight.get(allocation_id).cloned();

        if let Some(launching) = launching {
            ended(launching).await;
        }

        let (container_id, settled, instance, generation) = {
            let mut state = lock(&self.inner.state);
            let Some(runtime) = state.slots.get_mut(slot_id) else {
                return Err(SlotError::Failure(AgentFailure::code(AgentErrorCode::UnknownAllocation)));
            };

            // An idle slot names no allocation, so this also refuses a server that has since ended.
            if runtime.allocation_id() != Some(allocation_id) {
                return Err(SlotError::Failure(AgentFailure::code(AgentErrorCode::UnknownAllocation)));
            }

            // The launch has ended, so Docker has answered its start.
            let Some(container_id) = runtime.phase.server().and_then(|server| server.container.clone()) else {
                return Err(SlotError::NoContainer { slot_id: slot_id.to_owned(), allocation_id: allocation_id.to_owned() });
            };
            // The exit watch settles the slot, with the exit code a stop's answer lacks.
            let (settled_sender, settled) = oneshot::channel();

            if let Phase::Stopping { settled, .. } = &mut runtime.phase {
                // Joins the stop under way: both wait for the same exit. A stop that failed has
                // stopped waiting, so its sender goes.
                settled.retain(|sender| !sender.is_closed());
                settled.push(settled_sender);
            } else {
                runtime.phase = match runtime.take_phase().into_server() {
                    Ok((server, _)) => Phase::Stopping { server, settled: vec![settled_sender] },
                    Err(idle) => idle,
                };
                self.report(runtime);
            }

            if let Some(server) = runtime.phase.server_mut() {
                server.game.stop_calls += 1;
            }

            (container_id, settled, runtime.instance, runtime.generation)
        };

        let answer = self.inner.deps.docker.stop_container(&container_id, i64::try_from(timeout_seconds).unwrap_or(i64::MAX)).await;
        // False once the exit settled the slot, because then the server stopped, whatever Docker answered.
        let still_here = {
            let mut state = lock(&self.inner.state);
            let server = state
                .slots
                .get_mut(slot_id)
                .filter(|runtime| runtime.instance == instance && runtime.generation == generation)
                .and_then(|runtime| runtime.phase.server_mut());

            server.map(|server| server.game.stop_calls = server.game.stop_calls.saturating_sub(1)).is_some()
        };

        // The caller logs a failure. The slot stays `stopping`, and `check_games` tries the stop again.
        if let Err(error) = answer
            && still_here
        {
            return Err(error.into());
        }

        let _ = settled.await;
        Ok(())
    }

    /// Keeps an idle slot from launches until it gets a new token or the reservation runs out.
    /// Marked before returning, so a launch handled after it cannot pick the slot.
    ///
    /// # Errors
    ///
    /// Refuses an unknown slot, and one that holds a server.
    pub fn reserve(&self, slot_id: &str) -> Result<(), SlotError> {
        let mut state = lock(&self.inner.state);
        let Some(runtime) = state.slots.get_mut(slot_id) else {
            return Err(failure(AgentErrorCode::UnknownSlot, format!("unknown slot {slot_id}")));
        };

        if !runtime.phase.is_idle() {
            return Err(failure(AgentErrorCode::SlotHeld, format!("slot {slot_id} holds a server")));
        }

        let manager = self.clone();
        let slot = slot_id.to_owned();
        let instance = runtime.instance;
        let timer = tokio::spawn(async move {
            if manager.inner.deps.stop.run_until_cancelled(tokio::time::sleep(SLOT_RESERVATION)).await.is_none() {
                return;
            }

            manager.log().warn(format!("slot {slot}: reservation ran out without a new token"));

            let mut state = lock(&manager.inner.state);
            // Only this timer's reservation, as a newer one replaced its handle too late to abort it
            // once it waits for the lock.
            let me = tokio::task::id();

            if let Some(runtime) = state
                .slots
                .get_mut(&slot)
                .filter(|runtime| runtime.instance == instance)
                .filter(|runtime| matches!(&runtime.phase, Phase::Idle { reservation: Some(timer) } if timer.abort_handle().id() == me))
            {
                runtime.clear_reservation();
                manager.sync_reserved(runtime, None);
            }
        });

        runtime.phase = Phase::Idle { reservation: Some(AbortOnDropHandle::new(timer)) };
        self.sync_reserved(runtime, None);
        Ok(())
    }

    /// With a logon's answer, the token's state changes in the same status as the hold's end.
    fn sync_reserved(&self, runtime: &mut Runtime, answer: Option<GsltState>) {
        let reserved = matches!(runtime.phase, Phase::Idle { reservation: Some(_) }) || runtime.logons > 0;
        let reserve_changed = runtime.reserved != reserved;
        let answer = answer.filter(|answer| *answer != runtime.gslt_state);

        if reserve_changed || answer.is_some() {
            runtime.reserved = reserved;

            if let Some(answer) = answer {
                runtime.gslt_state = answer;
            }

            self.report(runtime);
        }
    }

    /// Adopts running game containers and removes exited ones. An adopted server is `starting`
    /// until its plugin, which reports on reconnecting, says it is ready.
    ///
    /// # Errors
    ///
    /// Fails when Docker cannot list containers or stop an untracked running server, or when two
    /// running servers claim one allocation. A second server on an adopted slot is stopped as
    /// untracked.
    pub async fn adopt(&self) -> Result<(), SlotError> {
        let containers = self.inner.deps.docker.list_ankor_containers().await?;

        for container in containers.into_iter().filter(|container| container.role == Some(ContainerRole::Game)) {
            if !container.running {
                self.log().info(format!("reaping exited container {}", container.name));

                if let Err(error) = self.inner.deps.docker.remove_container(&container.id, true).await {
                    self.log().warn(format!("could not remove {}: {error}", container.name));
                }

                continue;
            }

            let known = container.slot_id.as_ref().is_some_and(|slot| lock(&self.inner.state).slots.contains_key(slot));
            // A game the agent launched carries all four labels; one without them is not its own.
            let (Some(slot_id), Some(allocation_id), Some(relay_key), Some(gslt), true) =
                (container.slot_id.clone(), container.allocation_id.clone(), container.relay_key.clone(), container.gslt.clone(), known)
            else {
                self.stop_untracked(&container).await?;
                continue;
            };

            if self.holds(&allocation_id) {
                return Err(SlotError::AllocationHeld { container: container.name, allocation_id });
            }

            // Recorded for an orphan too, since its allocation ran.
            tokio::spawn(self.record_allocation_quietly(&allocation_id));

            let occupied = lock(&self.inner.state).slots.get(&slot_id).is_some_and(|runtime| !runtime.phase.is_idle());

            // A second server on one slot is an orphan, as of a start Docker carried out after its
            // launch gave up; failing the boot over it would leave the node down until someone
            // removes it by hand.
            if occupied {
                self.log().warn(format!("container {} claims slot {slot_id}, which another adopted server holds", container.name));
                self.stop_untracked(&container).await?;
                continue;
            }

            let (instance, generation) = {
                let mut state = lock(&self.inner.state);
                let Some(runtime) = state.slots.get_mut(&slot_id) else {
                    continue;
                };

                if !runtime.phase.is_idle() {
                    continue;
                }

                // The token it started on, which a set_config may since have replaced for the slot.
                let server =
                    Server::new(allocation_id.clone(), container.match_id.clone(), gslt.clone(), Some(container.id.clone()), container.idle_shutdown_seconds);
                let generation = runtime.begin(server);

                self.report(runtime);
                self.arm_start_timeout(runtime, generation);
                (runtime.instance, generation)
            };

            // Prepared only once the slot holds the server, so a plugin that reconnects finds it,
            // and an orphan's entry never retires the adopted server's connection.
            let _ = (self.inner.deps.plugin_url)(&slot_id, &allocation_id, &relay_key);

            if let Err(error) =
                self.inner.deps.docker.follow_container(&container.id, self.container_hooks(&slot_id, instance, generation), WaitCondition::NotRunning).await
            {
                self.log().warn(format!("could not re-attach to {}: {error}", container.name));
            }

            self.log().info(format!("adopted {} into slot {slot_id}", container.name));
        }

        Ok(())
    }

    /// Untracked, a server would hold ports unseen. The stop's grace lets it close its demo. If the
    /// container names its slot and allocation, the controller may still hold that server, so it
    /// receives a `server_exited`.
    async fn stop_untracked(&self, container: &AdoptedContainer) -> Result<(), SlotError> {
        self.log().warn(format!(
            "container {} has no configured slot or allocation; stopping it with {}s to finish its demo",
            container.name,
            STOP_TIMEOUT.as_secs()
        ));

        let server = container.slot_id.clone().zip(container.allocation_id.clone());
        // Watched before the stop, so the exit code a stop's answer lacks is not missed.
        let exited = if server.is_some() {
            let (sender, receiver) = oneshot::channel();
            let sender = Arc::new(Mutex::new(Some(sender)));
            let hooks = ContainerHooks {
                on_line: Arc::new(|_| {}),
                on_exit: Arc::new(move |code| {
                    if let Some(sender) = lock(&sender).take() {
                        let _ = sender.send(code);
                    }
                }),
            };

            let _ = self.inner.deps.docker.follow_container(&container.id, hooks, WaitCondition::NotRunning).await;
            Some(receiver)
        } else {
            None
        };

        self.inner
            .deps
            .docker
            .stop_container(&container.id, i64::try_from(STOP_TIMEOUT.as_secs()).unwrap_or(10))
            .await
            .map_err(|source| SlotError::StopUntracked { container: container.name.clone(), source })?;

        if let (Some((slot_id, allocation_id)), Some(exited)) = (server, exited) {
            let exit_code = exited.await.unwrap_or(None);

            self.emit(SlotEvent::ServerExited(ServerExit { slot_id, allocation_id, reason: ServerExitReason::SlotRemoved, exit_code, detail: None }));
        }

        Ok(())
    }

    fn forward_line(&self, slot_id: &str, instance: u64, generation: u64, line: String) {
        let allocation_id = lock(&self.inner.state)
            .slots
            .get(slot_id)
            .filter(|runtime| runtime.instance == instance && runtime.generation == generation)
            .and_then(|runtime| runtime.allocation_id().map(str::to_owned));

        if let Some(allocation_id) = allocation_id {
            self.emit(SlotEvent::ContainerLog { allocation_id, line });
        }
    }

    fn handle_exit(&self, slot_id: &str, instance: u64, generation: u64, code: Option<i64>) {
        {
            let mut state = lock(&self.inner.state);
            let Some(runtime) = state.slots.get_mut(slot_id).filter(|runtime| runtime.instance == instance && runtime.generation == generation) else {
                return;
            };

            self.log().info(format!("slot {slot_id}: container exited (code={})", code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())));

            if let Some(server) = runtime.phase.server_mut() {
                server.game.exit_code = code;
            }
        }

        self.settle_idle(slot_id, instance, generation, None, None);
    }

    /// Every way a slot comes free ends here, so its end is reported once. Without a `reason`, the
    /// server's own exit reason applies, with the agent's stop detail when that is the reason. Only
    /// the server of `instance` and `generation` is settled, as the caller decided in another hold.
    fn settle_idle(&self, slot_id: &str, instance: u64, generation: u64, reason: Option<ServerExitReason>, detail: Option<String>) {
        let mut state = lock(&self.inner.state);
        let Some(runtime) = state.slots.get_mut(slot_id).filter(|runtime| runtime.instance == instance && runtime.generation == generation) else {
            return;
        };
        let (server, settled) = match runtime.take_phase().into_server() {
            Ok((server, settled)) => (Some(server), settled),
            Err(idle) => {
                runtime.phase = idle;
                (None, Vec::new())
            }
        };

        if let Some(overlay) = server.as_ref().and_then(|server| server.overlay.as_ref()) {
            overlay.release();
        }

        runtime.generation += 1;

        if let Some(Server { allocation_id, game, .. }) = &server {
            // The agent's stop detail is set only with its stop reason.
            let (reason, detail) = match reason {
                Some(reason) => (reason, detail),
                None => (game.exit_reason(), game.stop_detail.clone()),
            };
            let detail = detail.map(|detail| truncate(&detail, SERVER_EXIT_DETAIL_MAX_CHARS)).filter(|detail| !detail.is_empty());

            self.emit(SlotEvent::ServerExited(ServerExit {
                slot_id: slot_id.to_owned(),
                allocation_id: allocation_id.clone(),
                reason,
                exit_code: game.exit_code,
                detail,
            }));
        }

        runtime.reserved = false;
        self.report(runtime);
        self.emit(SlotEvent::SlotIdle { slot_id: slot_id.to_owned(), allocation_id: server.map(|server| server.allocation_id) });

        for settled in settled {
            let _ = settled.send(());
        }
    }

    fn arm_start_timeout(&self, runtime: &mut Runtime, generation: u64) {
        let manager = self.clone();
        let slot_id = runtime.slot.id.clone();
        let instance = runtime.instance;
        let Phase::Starting { timer, .. } = &mut runtime.phase else {
            return;
        };

        *timer = None;

        let started = tokio::spawn(async move {
            if manager.inner.deps.stop.run_until_cancelled(tokio::time::sleep(START_TIMEOUT)).await.is_none() {
                return;
            }

            let starting = lock(&manager.inner.state)
                .slots
                .get(&slot_id)
                .filter(|runtime| runtime.instance == instance && runtime.generation == generation && matches!(runtime.phase, Phase::Starting { .. }))
                .and_then(|runtime| runtime.phase.server().map(|server| server.allocation_id.clone()));

            if let Some(allocation_id) = starting {
                manager.log().warn(format!("slot {slot_id}: plugin did not report ready within {}s, stopping", START_TIMEOUT.as_secs()));
                manager.stop_on_own(&slot_id, &allocation_id, Some(AgentStopReason::StartTimeout), None, 0, |runtime| {
                    runtime.generation == generation && matches!(runtime.phase, Phase::Starting { .. })
                });
            }
        });

        *timer = Some(AbortOnDropHandle::new(started));
    }
}

impl SlotManager {
    /// The cs2 images launches picked and still hold, which an image sweep must keep: the reference
    /// each picked, and the image id its overlay resolved, which its container is created from.
    #[must_use]
    pub fn held_images(&self) -> Vec<String> {
        lock(&self.inner.state)
            .slots
            .values()
            .filter_map(|runtime| runtime.phase.server())
            .flat_map(|server| server.image.iter().cloned().chain(server.overlay.iter().map(|overlay| overlay.overlay.image_id.clone())))
            .collect()
    }
}

impl GsltSlotsPort for SlotManager {
    fn tokens(&self) -> Vec<SlotToken> {
        lock(&self.inner.state)
            .slots
            .values()
            .map(|runtime| SlotToken { slot_id: runtime.slot.id.clone(), gslt: runtime.slot.gslt.clone(), gslt_state: runtime.gslt_state })
            .collect()
    }

    fn running_tokens(&self) -> HashSet<String> {
        lock(&self.inner.state).slots.values().filter_map(|runtime| runtime.phase.server().map(|server| server.token.clone())).collect()
    }

    fn set_gslt_state(&self, slot_id: &str, gslt: &str, gslt_state: GsltState) {
        let mut state = lock(&self.inner.state);

        // The token is checked in this hold, so a state about an old token never lands on a new one.
        if let Some(runtime) = state.slots.get_mut(slot_id)
            && runtime.slot.gslt == gslt
            && runtime.gslt_state != gslt_state
        {
            runtime.gslt_state = gslt_state;
            self.report(runtime);
        }
    }

    /// Keeps an idle slot from launches while its token is logged on to Steam, as a server that
    /// logged on with it meanwhile would be logged off. A slot holding a server needs no hold, only
    /// the answer. The check for a server on `gslt` and the hold share one hold of the lock, so a
    /// launch either sees the slot reserved or is seen here.
    fn hold_for_logon(&self, slot_id: &str, gslt: &str) -> Option<Release> {
        let manager = self.clone();
        let slot = slot_id.to_owned();
        let instance = {
            let mut state = lock(&self.inner.state);

            if state.slots.values().any(|runtime| runtime.phase.server().is_some_and(|server| server.token == gslt)) {
                return None;
            }

            *state.logging_on.entry(gslt.to_owned()).or_default() += 1;

            match state.slots.get_mut(slot_id).filter(|runtime| runtime.phase.is_idle()) {
                None => None,
                Some(runtime) => {
                    runtime.logons += 1;
                    self.sync_reserved(runtime, None);
                    Some(runtime.instance)
                }
            }
        };

        let token = gslt.to_owned();

        Some(Release::new(move |answer| {
            let mut state = lock(&manager.inner.state);

            if let Some(count) = state.logging_on.get_mut(&token) {
                *count -= 1;

                if *count == 0 {
                    state.logging_on.remove(&token);
                }
            }

            // Steam's answer is about this token, so a slot given another meanwhile keeps its state;
            // checked in this hold, so no edit falls between the check and the answer.
            let Some(runtime) = state.slots.get_mut(&slot) else {
                return;
            };
            let answer = answer.filter(|_| runtime.slot.gslt == token);

            match instance {
                // A logon's release can outlive its slot, as set_config may have replaced it.
                Some(instance) if runtime.instance == instance => {
                    runtime.logons -= 1;
                    manager.sync_reserved(runtime, answer);
                }
                Some(_) => {}
                None => {
                    if let Some(answer) = answer
                        && runtime.gslt_state != answer
                    {
                        runtime.gslt_state = answer;
                        manager.report(runtime);
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use ankor_protocol::types::SlotState;

    use super::*;
    use crate::testing::fixtures::{agent_config, launch_config};
    use crate::testing::slots::{launch_server, make_slots};

    #[tokio::test]
    async fn keeps_no_waiter_for_a_stop_docker_refused() {
        let base = make_slots(agent_config(&serde_json::json!({})));
        let allocation_id = "11111111-1111-4111-8111-111111111111";
        let waiters = |slots: &SlotManager| match &lock(&slots.inner.state).slots["a"].phase {
            Phase::Stopping { settled, .. } => settled.len(),
            _ => 0,
        };

        launch_server(&base.slots, launch_config(&serde_json::json!({})), allocation_id).await.unwrap();
        *lock(&base.docker.stop_error) = Some("Docker unavailable".to_owned());

        for _ in 0..3 {
            assert!(base.slots.stop_allocation(allocation_id).await.is_err());
        }

        // Each stop drops the senders of the refused ones before it, so only the last one is left.
        assert_eq!(waiters(&base.slots), 1);

        // Docker takes the next stop, and the container exits once the test lets it.
        let (exit, exits) = tokio::sync::oneshot::channel::<()>();
        let docker = Arc::clone(&base.docker);

        base.docker.override_stop(move |_, _| async move {
            let _ = exits.await;
            docker.exit(docker.last(), Some(0));

            Ok(())
        });

        let stopping = tokio::spawn({
            let slots = base.slots.clone();

            async move { slots.stop_allocation(allocation_id).await }
        });

        crate::testing::until("the stop to join", || lock(&base.docker.stopped).len() == 4).await;
        assert_eq!(waiters(&base.slots), 1);
        let _ = exit.send(());
        stopping.await.unwrap().unwrap();
        assert_eq!(base.status("a").state, SlotState::Idle);
    }
}
