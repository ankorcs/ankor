//! Moves the agent and cs2 images to newer builds while `auto_update` is on. A node on `latest`
//! follows every release, as ankor.sh's nodes do. Any other release tag stays within its release
//! line, a major, or for `0.x` a minor, because a `0.x` minor may break.
//! The component reads the tags of each repository that `agent.yml` names from its registry. It
//! runs the update that `update_cs2_image` or `update_agent` runs for the newest `X.Y.Z` above the
//! running version. If there is none, and the registry's digest for the tag differs from the running
//! image, the update is to a rebuild of the running version. The cs2 image goes first, because the
//! agent's swap ends this process.
//!
//! - A reference by digest, or with a tag other than `X`, `X.Y`, `X.Y.Z` or `latest`, is left alone.
//! - The new reference keeps the tag. `latest` stays `latest` and moves to the newest release of
//!   all. Otherwise it keeps the tag's precision: `:1` stays `:1`, `:1.2` becomes `:1.3` and
//!   `:1.2.0` becomes `:1.3.0`. In `0.x`, `:0` becomes the minor tag, such as `:0.9`, which stays
//!   on its line.
//! - If a cs2 image pull fails, the next poll tries again.
//! - If an agent swap fails, the helper rolls back to the old agent, which starts afresh. The old
//!   agent recognises the failed build by `agent-update-failed.json` and the local `agent_image`
//!   tag, which the swap's pull moved to that build, and does not swap to it again.
//! - The component polls at the start, every `IMAGE_UPDATE_POLL_INTERVAL`, and when `set_config`
//!   turns `auto_update` on. When maintenance such as an update ends, it polls if `agent.yml` names
//!   other images than the last poll read, or if an update waited for that maintenance.

use crate::docker_api::{DEFAULT_TAG, pulled_at};
use crate::sync::lock;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use ankor_protocol::types::AgentErrorCode;
use async_trait::async_trait;
use futures_util::FutureExt as _;
use futures_util::future::{BoxFuture, Shared};
use regex::Regex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent_update::{SwapFailureOutcome, read_failure};
use crate::config::ConfigReader;
use crate::docker::DockerPort;
use crate::failure::OpError;
use crate::host_layout::agent_update_failure_path;
use crate::logger::Logger;
use crate::registry_tags::{ImageName, RegistryFetch, list_tags, parse_image_reference, tag_digest};
use crate::self_update::{AgentUpdateFactory, Cs2ImageUpdateFactory, Schedule, update_agent, update_cs2_image};

/// The image label that release builds carry their version in.
pub const VERSION_LABEL: &str = "org.opencontainers.image.version";

#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static SEMVER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static RELEASE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+(\.[0-9]+){0,2}$").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static MAJOR_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+$").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static MINOR_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+\.[0-9]+$").expect("valid pattern"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Cs2,
    Agent,
}

impl ImageKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cs2 => "cs2",
            Self::Agent => "agent",
        }
    }
}

/// The cs2 image comes first, because the agent's swap ends this process.
const KINDS: [ImageKind; 2] = [ImageKind::Cs2, ImageKind::Agent];

type Version = [u64; 3];

#[must_use]
pub(crate) fn parse_version(text: Option<&str>) -> Option<Version> {
    let captures = SEMVER.captures(text?)?;
    let part = |index: usize| captures.get(index)?.as_str().parse::<u64>().ok();

    Some([part(1)?, part(2)?, part(3)?])
}

/// Whether two versions are on the same release line: the same major, and in `0.x` the same minor.
fn same_line(a: [u64; 3], b: [u64; 3]) -> bool {
    a[0] == b[0] && (a[0] > 0 || a[1] == b[1])
}

/// The newest `X.Y.Z` among `tags` above `current`, on its release line unless `any_line`; `None`
/// without one.
#[must_use]
pub fn newest_release(tags: &[String], current: &str, any_line: bool) -> Option<String> {
    let running = parse_version(Some(current))?;
    let newest =
        tags.iter().filter_map(|tag| parse_version(Some(tag))).filter(|version| (any_line || same_line(*version, running)) && *version > running).max()?;

    Some(newest.map(|part| part.to_string()).join("."))
}

/// Whether the reference names `latest`, which follows every release rather than one line.
fn follows_latest(image: &ImageName) -> bool {
    image.digest.is_none() && image.tag.as_deref() == Some(DEFAULT_TAG)
}

/// Whether the reference names a release tag, which the agent follows, rather than a digest or a
/// custom tag.
#[must_use]
pub(crate) fn follows_releases(image: &ImageName) -> bool {
    image.digest.is_none() && image.tag.as_deref().is_some_and(|tag| tag == DEFAULT_TAG || RELEASE_TAG.is_match(tag))
}

/// The reference to update to: `latest` as it is, otherwise keeping the tag's precision; `None` for
/// a reference that is not followed.
#[must_use]
pub fn target_reference(image: &ImageName, version: &str) -> Option<String> {
    let mut parts = version.split('.');
    let (major, minor) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    let tag = image.tag.as_deref().filter(|_| follows_releases(image))?;

    // In `0.x` the major tag spans minors that may break, so the line is the minor.
    let target = if tag == DEFAULT_TAG {
        DEFAULT_TAG.to_owned()
    } else if MAJOR_TAG.is_match(tag) && major != "0" {
        major.to_owned()
    } else if MAJOR_TAG.is_match(tag) || MINOR_TAG.is_match(tag) {
        format!("{major}.{minor}")
    } else {
        parse_version(Some(tag)).map(|_| version.to_owned())?
    };

    Some(format!("{}:{target}", image.name))
}

/// A newer release, or a rebuild of the running one under the same tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The release to update to; for a rebuild, the running version.
    pub version: String,
    /// The rebuild's digest; `None` for a newer release.
    pub digest: Option<String>,
}

#[async_trait]
pub trait ImageUpdates: Send + Sync {
    /// `auto_update` as the node runs with it now; `set_config` changes it.
    fn enabled(&self) -> bool;
    /// The reference `agent.yml` names for `kind`.
    fn image(&self, kind: ImageKind) -> String;
    /// The version of the image `kind` runs; `None` when it carries none.
    async fn running_version(&self, kind: ImageKind) -> Result<Option<String>, OpError>;
    /// Runs the update `update_cs2_image` or `update_agent` runs, and returns once it has ended.
    async fn update(&self, kind: ImageKind, image: &str) -> Result<(), OpError>;
    /// The digest `image` points to in the registry when that is another build than `kind` runs;
    /// `None` when it is the same.
    async fn rebuild(&self, kind: ImageKind, image: &str) -> Result<Option<String>, OpError>;
    /// Whether `candidate` is the build that an earlier agent swap failed with. The swap rolled back
    /// to this agent, which started afresh and so does not remember it. A cs2 image needs no such
    /// check, because nothing can tell whether it works without a launch.
    async fn agent_swap_failed(&self, image: &str, candidate: &Candidate) -> Result<bool, OpError>;
}

type Poll = Shared<BoxFuture<'static, ()>>;

#[derive(Default)]
struct State {
    /// The task that polls every interval.
    timer: Option<JoinHandle<()>>,
    /// The poll under way, which callers share meanwhile.
    polling: Option<Poll>,
    /// The references the last poll read, cs2 first.
    polled: Option<(String, String)>,
    /// References already logged as not followed, so the log says it once rather than every poll.
    unversioned: BTreeSet<String>,
    /// An update waited for other maintenance, so the end of that maintenance brings the next poll.
    waiting: bool,
    /// Maintenance ended during a poll, which may have read the images before it changed them.
    again: bool,
}

struct Inner {
    /// What the polls read from the node and how they update it.
    deps: Arc<dyn ImageUpdates>,
    /// Reads tags and digests from the registries.
    fetch: Arc<dyn RegistryFetch>,
    /// Where polls log updates and failures.
    log: Logger,
    /// The time between timed polls.
    interval: Duration,
    /// Once cancelled, nothing polls and no update starts.
    stop: CancellationToken,
    /// The poll timer, the poll under way and what the last poll read.
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct ImageAutoUpdate {
    inner: Arc<Inner>,
}

impl ImageAutoUpdate {
    /// Nothing polls or updates once `stop` is cancelled.
    #[must_use]
    pub fn new(deps: Arc<dyn ImageUpdates>, fetch: Arc<dyn RegistryFetch>, log: Logger, interval: Duration, stop: CancellationToken) -> Self {
        Self { inner: Arc::new(Inner { deps, fetch, log, interval, stop, state: Mutex::default() }) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        lock(&self.inner.state)
    }

    /// Polls now and every interval after.
    pub fn start(&self) {
        let mut state = self.state();

        if state.timer.is_some() {
            return;
        }

        let this = self.clone();
        let period = self.inner.interval;

        state.timer = Some(tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);

            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            while this.inner.stop.run_until_cancelled(ticks.tick()).await.is_some() {
                drop(this.poll());
            }
        }));
        drop(state);
        drop(self.poll());
    }

    /// Polls now if the images differ from the ones the last poll read, or if an update waited for
    /// the maintenance.
    pub fn maintenance_ended(&self) -> Poll {
        // Read before the lock: the config has a lock of its own.
        let images = (self.inner.deps.image(ImageKind::Cs2), self.inner.deps.image(ImageKind::Agent));
        let mut state = self.state();

        if let Some(polling) = state.polling.clone() {
            state.again = true;

            return polling;
        }

        let changed = state.polled.as_ref().is_none_or(|polled| *polled != images);

        if state.waiting || changed {
            drop(state);

            return self.poll();
        }

        futures_util::future::ready(()).boxed().shared()
    }

    /// One poll at a time; whoever asks meanwhile shares it.
    pub fn poll(&self) -> Poll {
        if self.inner.stop.is_cancelled() {
            return futures_util::future::ready(()).boxed().shared();
        }

        let mut state = self.state();

        if let Some(polling) = &state.polling {
            return polling.clone();
        }

        let (done, finished) = tokio::sync::oneshot::channel::<()>();
        let polling = async move {
            let _ = finished.await;
        }
        .boxed()
        .shared();

        state.polling = Some(polling.clone());
        drop(state);

        let this = self.clone();

        tokio::spawn(async move {
            this.poll_all().await;

            let again = {
                let mut state = this.state();

                state.polling = None;
                std::mem::take(&mut state.again)
            };

            if again {
                drop(this.maintenance_ended());
            }

            let _ = done.send(());
        });

        polling
    }

    async fn poll_all(&self) {
        let deps = &self.inner.deps;

        if !deps.enabled() {
            return;
        }

        // Read before the state lock, as the config is another component's.
        let polled = (deps.image(ImageKind::Cs2), deps.image(ImageKind::Agent));

        {
            let mut state = self.state();

            state.waiting = false;
            state.polled = Some(polled);
        }

        for kind in KINDS {
            if !deps.enabled() {
                return;
            }

            if let Err(error) = self.consider(kind).await {
                self.inner.log.warn(format!("could not check for a newer {} image: {error}", kind.as_str()));
            }
        }
    }

    async fn consider(&self, kind: ImageKind) -> Result<(), OpError> {
        let deps = &self.inner.deps;
        let log = &self.inner.log;
        let reference = deps.image(kind);
        let image = parse_image_reference(&reference);

        // A digest or a custom tag is the operator's pin; probing would only tell what the tag cannot follow.
        if !follows_releases(&image) {
            return Ok(());
        }

        let Some(running) = deps.running_version(kind).await?.filter(|running| parse_version(Some(running)).is_some()) else {
            if self.state().unversioned.insert(reference.clone()) {
                log.info(format!("{reference} is not updated automatically, because the image on this host is missing or carries no release version"));
            }

            return Ok(());
        };
        let newest = newest_release(&list_tags(&reference, self.inner.fetch.as_ref(), log).await?, &running, follows_latest(&image));
        let Some(target) = target_reference(&image, newest.as_deref().unwrap_or(&running)) else {
            return Ok(());
        };
        // Without a newer release, a rebuild of the running one under the same tags is the update.
        let candidate = match newest {
            Some(version) => Candidate { version, digest: None },
            None => match deps.rebuild(kind, &target).await? {
                Some(digest) => Candidate { version: running.clone(), digest: Some(digest) },
                None => return Ok(()),
            },
        };
        let build = candidate.digest.as_ref().map_or_else(|| candidate.version.clone(), |digest| format!("{}@{digest}", candidate.version));
        let name = kind.as_str();

        if kind == ImageKind::Agent && deps.agent_swap_failed(&target, &candidate).await? {
            return Ok(());
        }

        log.info(match &candidate.digest {
            None => format!("updating the {name} image from {running} to {} as {target}", candidate.version),
            Some(digest) => format!("updating the {name} image to a rebuild of {running} as {target} ({digest})"),
        });

        if let Err(error) = deps.update(kind, &target).await {
            if error.is_refusal(AgentErrorCode::MaintenanceRunning) {
                self.state().waiting = true;
                log.info(format!("the {name} image update to {build} waits: {error}"));

                return Ok(());
            }

            log.error(format!("the {name} image update to {build} failed: {error}"));
        }

        Ok(())
    }
}

pub(crate) struct NodeImages {
    pub config: ConfigReader,
    pub docker: Arc<dyn DockerPort>,
    pub registry: Arc<dyn RegistryFetch>,
    pub own_image_id: String,
    pub data_dir: PathBuf,
    pub agent_update: AgentUpdateFactory,
    pub cs2_image_update: Cs2ImageUpdateFactory,
}

impl NodeImages {
    async fn running_id(&self, kind: ImageKind) -> Result<Option<String>, OpError> {
        match kind {
            ImageKind::Cs2 => Ok(self.docker.image_id(&self.config.read().cs2_image).await?),
            ImageKind::Agent => Ok(Some(self.own_image_id.clone())),
        }
    }

    /// The digest the tag in `image` points to now, when the local image `image_id` was not pulled
    /// as it; `None` when it was. A release rebuilt under the same tags moves them.
    async fn moved_tag_digest(&self, image: &str, image_id: &str) -> Result<Option<String>, OpError> {
        let (digest, pulled_as) = tokio::join!(tag_digest(image, self.registry.as_ref()), self.docker.repo_digests(image_id));
        let digest = digest?;

        Ok((!pulled_at(&pulled_as?, &digest)).then_some(digest))
    }
}

#[async_trait]
impl ImageUpdates for NodeImages {
    fn enabled(&self) -> bool {
        self.config.read().auto_update
    }

    fn image(&self, kind: ImageKind) -> String {
        let config = self.config.read();

        match kind {
            ImageKind::Cs2 => config.cs2_image,
            ImageKind::Agent => config.agent_image,
        }
    }

    // Release builds carry their version as a label; a local build carries none and is left alone.
    async fn running_version(&self, kind: ImageKind) -> Result<Option<String>, OpError> {
        let Some(id) = self.running_id(kind).await? else {
            return Ok(None);
        };

        Ok(self.docker.image_defaults(&id).await?.labels.get(VERSION_LABEL).cloned())
    }

    /// Returns once the update's work has ended, so the next image is not refused by this one's guard.
    async fn update(&self, kind: ImageKind, image: &str) -> Result<(), OpError> {
        let work: Arc<Mutex<Option<BoxFuture<'static, ()>>>> = Arc::default();
        let scheduled = Arc::clone(&work);
        let schedule: Schedule = Arc::new(move |start| *lock(&scheduled) = Some(start));

        match kind {
            ImageKind::Cs2 => {
                update_cs2_image(image, true, &(self.cs2_image_update)(schedule)).await?;
            }
            ImageKind::Agent => {
                update_agent(image, true, &(self.agent_update)(schedule)).await?;
            }
        }

        let pending = lock(&work).take();

        if let Some(pending) = pending {
            pending.await;
        }

        Ok(())
    }

    async fn rebuild(&self, kind: ImageKind, image: &str) -> Result<Option<String>, OpError> {
        match self.running_id(kind).await? {
            Some(id) => self.moved_tag_digest(image, &id).await,
            None => Ok(None),
        }
    }

    // After a failed swap, the local agent_image tag names the build the swap pulled and failed with.
    async fn agent_swap_failed(&self, image: &str, candidate: &Candidate) -> Result<bool, OpError> {
        // The file, not the reported state, which may still say the swap is under way. The swap's
        // pull moved the tag the file names to the build that failed, whatever agent.yml names since.
        let failure = read_failure(&agent_update_failure_path(&self.data_dir)).await;
        let failed_id = match failure.filter(|failure| failure.outcome == SwapFailureOutcome::Failed) {
            Some(failure) => self.docker.image_id(&failure.image).await?,
            None => None,
        };
        let Some(failed_id) = failed_id.filter(|id| *id != self.own_image_id) else {
            return Ok(false);
        };

        // Only the exact build that failed is skipped; a rebuild of the same release is not.
        if candidate.digest.is_none() && self.docker.image_defaults(&failed_id).await?.labels.get(VERSION_LABEL) != Some(&candidate.version) {
            return Ok(false);
        }

        Ok(self.moved_tag_digest(image, &failed_id).await?.is_none())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
#[allow(clippy::unreachable, reason = "a test fails by crashing")]
mod tests {
    use super::*;
    use crate::config::ConfigStore;
    use crate::registry_tags::{Method, RegistryAnswer, RegistryError};
    use crate::testing::FakeDocker;
    use crate::testing::fixtures::agent_config;
    use serde_json::json;

    /// A registry whose every tag points to `sha256:new`.
    struct Moved;

    #[async_trait]
    impl RegistryFetch for Moved {
        async fn fetch(&self, _url: &str, _method: Method, _headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError> {
            Ok(RegistryAnswer { status: 200, headers: [("docker-content-digest".to_owned(), "sha256:new".to_owned())].into(), body: Vec::new() })
        }
    }

    fn node(docker: &Arc<FakeDocker>, auto_update: bool) -> NodeImages {
        let config = ConfigStore::new("/nonexistent/agent.yml".into(), agent_config(&json!({ "auto_update": auto_update })));

        NodeImages {
            config: config.reader(),
            docker: Arc::clone(docker) as Arc<dyn DockerPort>,
            registry: Arc::new(Moved),
            own_image_id: "sha256:own".to_owned(),
            data_dir: "/nonexistent".into(),
            agent_update: Arc::new(|_| unreachable!("no update runs")),
            cs2_image_update: Arc::new(|_| unreachable!("no update runs")),
        }
    }

    #[test]
    fn follows_a_release_or_latest_tag_without_a_digest_only() {
        let follows = |reference: &str| follows_releases(&parse_image_reference(reference));

        assert!(follows("ghcr.io/ankorcs/ankor-cs2:1.2"));
        assert!(follows("ghcr.io/ankorcs/ankor-cs2:latest"));
        assert!(!follows("ghcr.io/ankorcs/ankor-cs2:review"));
        assert!(!follows(&format!("ghcr.io/ankorcs/ankor-cs2:1.2@sha256:{}", "a".repeat(64))));
    }

    #[test]
    fn is_enabled_as_agent_yml_says() {
        let docker = FakeDocker::new();

        assert!(node(&docker, true).enabled());
        assert!(!node(&docker, false).enabled());
    }

    #[tokio::test]
    async fn finds_a_rebuild_when_the_tag_moved_off_the_running_image() {
        let docker = FakeDocker::new();
        let images = node(&docker, true);

        assert_eq!(images.rebuild(ImageKind::Cs2, "ghcr.io/example/ankor-cs2:1").await.unwrap().as_deref(), Some("sha256:new"));
        assert_eq!(images.rebuild(ImageKind::Agent, "ghcr.io/example/ankor-agent:1").await.unwrap().as_deref(), Some("sha256:new"));

        lock(&docker.repo_digests_by_id).insert("sha256:image".to_owned(), vec!["ghcr.io/example/ankor-cs2@sha256:new".to_owned()]);
        lock(&docker.repo_digests_by_id).insert("sha256:own".to_owned(), vec!["ghcr.io/example/ankor-agent@sha256:old".to_owned()]);
        assert_eq!(images.rebuild(ImageKind::Cs2, "ghcr.io/example/ankor-cs2:1").await.unwrap(), None);
        assert_eq!(images.rebuild(ImageKind::Agent, "ghcr.io/example/ankor-agent:1").await.unwrap().as_deref(), Some("sha256:new"));

        *lock(&docker.no_images) = true;
        assert_eq!(images.rebuild(ImageKind::Cs2, "ghcr.io/example/ankor-cs2:1").await.unwrap(), None);
    }
}
