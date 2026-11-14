//! Keeps an installed node on the newest public CS2 build. It reads the public build every
//! `CS2_BUILD_POLL_INTERVAL`, or when the controller asks. It updates once every slot is idle, with
//! one attempt per build. A refusal because a slot became busy or other maintenance is running is
//! not an attempt. The first install stays the operator's call. While `auto_update` is false, it
//! still reads the public build for the node's report but updates nothing.

use crate::sync::{SharedRun, join_or_spawn, lock};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::helpers::cs2_behind;
use ankor_protocol::schemas::s_build_id;
use ankor_protocol::types::{AgentErrorCode, Cs2UpdateMode};
use ankor_protocol::validate::safe_parse;
use async_trait::async_trait;
use futures_util::FutureExt as _;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::ConfigReader;
use crate::cs2_install::CS2_APP_ID;
use crate::cs2_install::InstalledBuild;
use crate::failure::OpError;
use crate::http_body::is_success;
use crate::install::{InstallOptions, MaintenanceGuard, update_cs2};
use crate::logger::Logger;
use crate::slot_manager::SlotManager;

/// steamcmd.net's mirror of CS2's app info, which carries the latest public build.
#[must_use]
pub fn steamcmd_info_url() -> String {
    format!("https://api.steamcmd.net/v1/info/{CS2_APP_ID}")
}
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteamAnswer {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Why the newest public build could not be read.
#[derive(Debug, thiserror::Error)]
pub enum Cs2AutoUpdateError {
    /// The HTTP client could not be built.
    #[error("no HTTP client: {0}")]
    NoClient(#[source] Arc<reqwest::Error>),
    /// The HTTP client failed the request.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// The request failed on the way, outside the HTTP client.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The mirror answered with a status other than success.
    #[error("HTTP {0}")]
    Status(u16),
    /// The answer was not JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The answer carried no public build id.
    #[error("the answer carried no public build id")]
    NoBuildId,
    /// The answer was larger than `ANSWER_MAX_BYTES`.
    #[error("{}", crate::http_body::too_large(crate::http_body::ANSWER_MAX_BYTES))]
    TooLarge,
}

/// The read of the public build, so tests can answer it.
#[async_trait]
pub trait SteamFetch: Send + Sync {
    /// `Err` is a request that got no answer.
    async fn fetch(&self, url: &str) -> Result<SteamAnswer, Cs2AutoUpdateError>;
}

pub struct HttpSteam(Result<reqwest::Client, Arc<reqwest::Error>>);

impl HttpSteam {
    #[must_use]
    pub fn new() -> Self {
        crate::init_tls();

        Self(reqwest::Client::builder().timeout(FETCH_TIMEOUT).build().map_err(Arc::new))
    }
}

impl Default for HttpSteam {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SteamFetch for HttpSteam {
    async fn fetch(&self, url: &str) -> Result<SteamAnswer, Cs2AutoUpdateError> {
        let client = self.0.as_ref().map_err(|error| Cs2AutoUpdateError::NoClient(Arc::clone(error)))?;
        let response = client.get(url).send().await?;
        let status = response.status().as_u16();
        let body = crate::http_body::read_limited(response, crate::http_body::ANSWER_MAX_BYTES).await?.ok_or(Cs2AutoUpdateError::TooLarge)?;

        Ok(SteamAnswer { status, body })
    }
}

/// The public branch's build from steamcmd.net's app info, given as text or a positive whole number.
#[must_use]
pub fn parse_build_id(payload: &Value) -> Option<String> {
    if payload.get("status")?.as_str()? != "success" {
        return None;
    }

    let build_id = payload.get("data")?.get(CS2_APP_ID.to_string().as_str())?.get("depots")?.get("branches")?.get("public")?.get("buildid")?;

    if let Ok(text) = safe_parse(s_build_id, build_id.clone()) {
        return text.as_str().map(str::to_owned);
    }

    build_id.as_u64().filter(|&number| number > 0).map(|number| number.to_string())
}

/// What the updates read from the node and how they run.
#[async_trait]
pub trait Cs2Updates: Send + Sync {
    /// `auto_update` as the node runs with it now; `set_config` changes it.
    fn enabled(&self) -> bool;
    async fn installed_build(&self) -> Option<String>;
    fn any_busy(&self) -> bool;
    fn guard_held(&self) -> bool;
    /// Runs `update_cs2` in mode `update`.
    async fn update(&self) -> Result<(), OpError>;
    /// Called when the newest public build changed.
    fn on_latest(&self, build_id: &str);
}

type Read = SharedRun<Result<(), Arc<Cs2AutoUpdateError>>>;

#[derive(Default)]
struct State {
    /// The newest public build last read.
    latest_build_id: Option<String>,
    /// The build the last automatic update went for.
    attempted: Option<String>,
    /// The poll loop, once `start` ran.
    timer: Option<JoinHandle<()>>,
    /// Whether an update decision is in progress, so two never overlap.
    considering: bool,
    /// The read in flight, which later callers join.
    reading: Option<Read>,
    /// When the last `check_cs2_build` was let through; polls never count.
    checked_at: Option<Instant>,
}

struct Inner {
    /// The node the updates read from and run on.
    deps: Arc<dyn Cs2Updates>,
    /// Reads the public build.
    fetch: Arc<dyn SteamFetch>,
    /// Where reads and updates are logged.
    log: Logger,
    /// How often the public build is polled.
    interval: Duration,
    /// The least time between two `check_cs2_build` polls.
    check_interval: Duration,
    /// Once cancelled, nothing polls and no update starts.
    stop: CancellationToken,
    /// The polling and update state.
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct Cs2AutoUpdate {
    inner: Arc<Inner>,
}

impl Cs2AutoUpdate {
    /// Polls every `interval` once started; `check_cs2_build` is let through once per `check_interval`.
    /// Nothing polls or updates once `stop` is cancelled.
    #[must_use]
    pub fn new(
        deps: Arc<dyn Cs2Updates>,
        fetch: Arc<dyn SteamFetch>,
        log: Logger,
        interval: Duration,
        check_interval: Duration,
        stop: CancellationToken,
    ) -> Self {
        Self { inner: Arc::new(Inner { deps, fetch, log, interval, check_interval, stop, state: Mutex::default() }) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        lock(&self.inner.state)
    }

    #[must_use]
    pub fn latest(&self) -> Option<String> {
        self.state().latest_build_id.clone()
    }

    pub fn start(&self) {
        let mut state = self.state();

        if state.timer.is_some() {
            return;
        }

        let this = self.clone();
        let period = self.inner.interval;

        state.timer = Some(tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(Instant::now() + period, period);

            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            while this.inner.stop.run_until_cancelled(ticks.tick()).await.is_some() {
                let poll = this.clone();

                tokio::spawn(async move { poll.poll().await });
            }
        }));
        drop(state);

        let poll = self.clone();

        tokio::spawn(async move { poll.poll().await });
    }

    /// A poll failure keeps the last known build.
    pub async fn poll(&self) {
        if self.inner.stop.is_cancelled() {
            return;
        }

        if let Err(error) = self.read().await {
            self.inner.log.warn(format!("could not read the newest public cs2 build: {error}"));
        }

        self.consider().await;
    }

    /// Handles `check_cs2_build`.
    pub async fn check(&self) {
        let interval = self.inner.check_interval;

        {
            let mut state = self.state();

            if state.checked_at.is_some_and(|at| at.elapsed() < interval) {
                drop(state);
                self.inner.log.info(format!("dropped a check_cs2_build: the last one was less than {} s ago", interval.as_secs_f64()));

                return;
            }

            state.checked_at = Some(Instant::now());
        }

        self.poll().await;
    }

    /// Callers meanwhile share one read. It runs in a spawned task, so it ends even when every
    /// caller stops waiting.
    fn read(&self) -> Read {
        join_or_spawn(
            &self.inner.state,
            |state| &mut state.reading,
            || {
                let this = self.clone();

                async move {
                    let read = this.fetch_latest().await.map_err(Arc::new);

                    this.state().reading = None;
                    read
                }
                .boxed()
            },
        )
    }

    async fn fetch_latest(&self) -> Result<(), Cs2AutoUpdateError> {
        let answer = self.inner.fetch.fetch(&steamcmd_info_url()).await?;

        if !is_success(answer.status) {
            return Err(Cs2AutoUpdateError::Status(answer.status));
        }

        let payload: Value = serde_json::from_slice(&answer.body)?;
        let build_id = parse_build_id(&payload).ok_or(Cs2AutoUpdateError::NoBuildId)?;
        let changed = {
            let mut state = self.state();
            let changed = state.latest_build_id.as_ref() != Some(&build_id);

            if changed {
                state.latest_build_id = Some(build_id.clone());
            }

            changed
        };

        if changed {
            self.inner.log.info(format!("newest public cs2 build is {build_id}"));
            self.inner.deps.on_latest(&build_id);
        }

        Ok(())
    }

    /// Runs after a read and whenever a slot goes idle.
    pub async fn consider(&self) {
        let deps = &self.inner.deps;

        if self.inner.stop.is_cancelled() {
            return;
        }

        // Read before taking the lock, because the config, the guard and the slots take locks of their
        // own. They may change right after; the update's claim of the guard is what decides.
        if !deps.enabled() || deps.guard_held() || deps.any_busy() {
            return;
        }

        let latest = {
            let mut state = self.state();
            let Some(latest) = state.latest_build_id.clone() else {
                return;
            };

            if state.considering || state.attempted.as_ref() == Some(&latest) {
                return;
            }

            state.considering = true;
            latest
        };

        self.update_to(&latest).await;
        self.state().considering = false;
    }

    async fn update_to(&self, latest: &str) {
        let deps = &self.inner.deps;
        let log = &self.inner.log;
        let installed = deps.installed_build().await;

        if !cs2_behind(installed.as_deref(), Some(latest)) {
            return;
        }

        self.state().attempted = Some(latest.to_owned());
        log.info(format!("updating cs2 from build {} to {latest}", installed.unwrap_or_default()));

        if let Err(error) = deps.update().await {
            if error.is_refusal(AgentErrorCode::SlotsHeld) || error.is_refusal(AgentErrorCode::MaintenanceRunning) {
                self.state().attempted = None;
                log.info(format!("cs2 update to {latest} waits: {error}"));

                return;
            }

            // Not retried on the next poll, transient or not, so a failing update does not run every
            // half hour. The controller can start one by hand.
            log.error(format!("cs2 update to {latest} failed: {error}; it is not tried again until the next build, or by hand"));
        }
    }
}

/// What the automatic CS2 update needs of the node.
pub(crate) struct NodeCs2 {
    pub config: ConfigReader,
    pub build: InstalledBuild,
    pub slots: SlotManager,
    pub updates: MaintenanceGuard,
    pub install: Arc<dyn Fn(Cs2UpdateMode, bool) -> InstallOptions + Send + Sync>,
    pub on_latest: Arc<dyn Fn() + Send + Sync>,
}

#[async_trait]
impl Cs2Updates for NodeCs2 {
    fn enabled(&self) -> bool {
        self.config.read().auto_update
    }

    async fn installed_build(&self) -> Option<String> {
        self.build.get().await
    }

    fn any_busy(&self) -> bool {
        self.slots.any_busy()
    }

    fn guard_held(&self) -> bool {
        self.updates.running()
    }

    async fn update(&self) -> Result<(), OpError> {
        update_cs2((self.install)(Cs2UpdateMode::Update, true)).await
    }

    fn on_latest(&self, _build_id: &str) {
        (self.on_latest)();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::unimplemented, reason = "a test fails by crashing")]
mod node_tests {
    use ankor_protocol::types::MaintenanceOperation;

    use super::*;
    use crate::docker::DockerPort;
    use crate::install::MaintenanceGuardOptions;
    use crate::testing::FakeDocker;
    use crate::testing::fixtures::{agent_config, launch_config};
    use crate::testing::slots::{launch_server, make_slots};

    #[tokio::test]
    async fn the_node_is_busy_while_a_slot_holds_a_server_and_held_while_the_guard_is_claimed() {
        let base = make_slots(agent_config(&serde_json::json!({})));
        let docker = FakeDocker::new();
        let config = crate::config::ConfigStore::new("/tmp/agent.yml".into(), agent_config(&serde_json::json!({})));
        let node = NodeCs2 {
            config: config.reader(),
            build: InstalledBuild::new(std::env::temp_dir(), Logger::null()),
            slots: base.slots.clone(),
            updates: MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn DockerPort>)),
            install: Arc::new(|_, _| unimplemented!("no update runs")),
            on_latest: Arc::new(|| {}),
        };

        assert!(!node.any_busy());
        assert!(!node.guard_held());

        launch_server(&base.slots, launch_config(&serde_json::json!({})), "0199a0b4-5c00-7000-8000-0000000000c1").await.unwrap();
        assert!(node.any_busy());

        let claim = node.updates.claim(MaintenanceOperation::Cs2Update).await.unwrap();

        assert!(node.guard_held());
        drop(claim);
        assert!(!node.guard_held());
    }
}
