//! How the last `update_agent` swap went, for the agent that comes up after it. A swap that fails
//! leaves a file in the data directory saying why, and one that holds removes it. An agent that sees
//! the helper running reads the file once the helper exits.

use crate::sync::lock;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ankor_protocol::constants::AGENT_UPDATE_DETAIL_MAX_CHARS;
use ankor_protocol::types::{AgentUpdate, AgentUpdateOutcome, ImageUpdateStep};
use serde::{Deserialize, Serialize};

use crate::config::write_atomic;
use crate::docker::{ContainerHooks, DockerPort};
use crate::docker_api::{DockerError, WaitCondition};
use crate::failure::truncate;
use crate::logger::Logger;

/// How a swap that the file records ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapFailureOutcome {
    /// The new image could not be pulled, so no swap began.
    PullFailed,
    /// The build ran and did not hold, so the automatic update skips it.
    Failed,
    /// The swap failed for a reason that is not the build's, so the automatic update tries it again.
    /// Examples are Docker not starting the helper, or an earlier helper run stopping partway.
    NotStarted,
}

/// The record a failed swap leaves in the data directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapFailure {
    pub image: String,
    pub outcome: SwapFailureOutcome,
    pub detail: String,
}

/// Replaces the file whole, so a reader never sees half of it.
///
/// # Errors
///
/// Fails when the file cannot be written.
pub fn write_swap_failure(path: &Path, failure: &SwapFailure) -> std::io::Result<()> {
    let text = serde_json::to_string(failure).map_err(std::io::Error::other)?;

    write_atomic(path, &format!("{text}\n"))
}

/// `None` without a file, or for one that cannot be read or names no image.
pub(crate) async fn read_failure(path: &Path) -> Option<SwapFailure> {
    let failure: SwapFailure = serde_json::from_str(&tokio::fs::read_to_string(path).await.ok()?).ok()?;

    (!failure.image.is_empty()).then_some(failure)
}

fn failed(failure: &SwapFailure) -> AgentUpdate {
    let detail = truncate(&failure.detail, AGENT_UPDATE_DETAIL_MAX_CHARS);
    let outcome = match failure.outcome {
        SwapFailureOutcome::PullFailed => AgentUpdateOutcome::PullFailed,
        SwapFailureOutcome::Failed | SwapFailureOutcome::NotStarted => AgentUpdateOutcome::Failed,
    };

    AgentUpdate { image: failure.image.clone(), step: None, outcome: Some(outcome), detail: (!detail.is_empty()).then_some(detail) }
}

pub(crate) type ChangeHook = Arc<dyn Fn() + Send + Sync>;

/// What `agent_update` reports, read from the last swap's helper and its result file.
#[derive(Clone)]
pub struct AgentUpdateState {
    /// The daemon the helper is found and followed on.
    docker: Arc<dyn DockerPort>,
    /// The file a failed swap leaves.
    failure_path: PathBuf,
    /// This agent's image, reported for a helper whose image cannot be read.
    own_image: String,
    /// Where a failure to follow the helper or record a result is logged.
    log: Logger,
    /// The state changed after it was first read, as when a helper still proving the swap exits.
    on_change: ChangeHook,
    /// The `agent_update` to report, or `None` before anything was read.
    current: Arc<Mutex<Option<AgentUpdate>>>,
}

impl AgentUpdateState {
    /// `own_image` names this agent's image, for a helper whose spec cannot be read.
    #[must_use]
    pub fn new(docker: Arc<dyn DockerPort>, failure_path: PathBuf, own_image: String, log: Logger, on_change: ChangeHook) -> Self {
        Self { docker, failure_path, own_image, log, on_change, current: Arc::default() }
    }

    #[must_use]
    pub fn get(&self) -> Option<AgentUpdate> {
        lock(&self.current).clone()
    }

    fn set(&self, update: Option<AgentUpdate>) {
        *lock(&self.current) = update;
    }

    /// Follows a swap still under way until its helper exits; otherwise the file says whether the
    /// last one failed.
    ///
    /// # Errors
    ///
    /// Fails when Docker cannot list the helpers, leaving nothing reported.
    pub async fn load(&self) -> Result<(), DockerError> {
        let Some(helper) = self.docker.self_update_helper().await?.filter(|helper| helper.running) else {
            self.set(read_failure(&self.failure_path).await.as_ref().map(failed));
            return Ok(());
        };
        let image = helper.image.clone().unwrap_or_else(|| self.own_image.clone());

        self.set(Some(AgentUpdate { image: image.clone(), step: Some(ImageUpdateStep::Switching), outcome: None, detail: None }));
        self.follow_helper(&helper.name, &image).await;
        Ok(())
    }

    /// Reports how the swap ended once its helper exits, as its file says. A helper that fails
    /// without stopping this agent leaves it running, and only its exit says so.
    pub async fn follow_helper(&self, helper: &str, image: &str) {
        let (state, image) = (self.clone(), image.to_owned());
        let hooks = ContainerHooks {
            on_line: Arc::new(|_| {}),
            on_exit: Arc::new(move |code| {
                let (state, image) = (state.clone(), image.clone());

                // The exit hook is synchronous, so the file is read in a task.
                tokio::spawn(async move {
                    match (read_failure(&state.failure_path).await, code) {
                        (Some(failure), _) => state.set(Some(failed(&failure))),
                        // The helper failed before recording a result, as a build that cannot
                        // start does. Recording it stops the image auto-update retrying it. If
                        // Docker restarts the helper, the rerun's result replaces this one.
                        (None, Some(code)) if code != 0 => {
                            return state.record_failure(
                                &image,
                                SwapFailureOutcome::Failed,
                                &format!("the helper exited with {code} before recording a result"),
                            );
                        }
                        (None, _) => state.set(Some(AgentUpdate { image, step: None, outcome: Some(AgentUpdateOutcome::Completed), detail: None })),
                    }

                    (state.on_change)();
                });
            }),
        };

        if let Err(error) = self.docker.follow_container(helper, hooks, WaitCondition::NotRunning).await {
            self.log.warn(format!("could not follow {helper}: {error}"));
        }
    }

    /// An accepted update: its image is pulled or its swap starts, and how it ends follows.
    pub fn started(&self, image: &str, step: ImageUpdateStep) {
        self.set(Some(AgentUpdate { image: image.to_owned(), step: Some(step), outcome: None, detail: None }));
        (self.on_change)();
    }

    /// The pulled image differs from the running one, so the helper replaces this agent.
    pub fn switching(&self) {
        {
            let mut current = lock(&self.current);

            match current.as_mut() {
                Some(update) if update.outcome.is_none() => update.step = Some(ImageUpdateStep::Switching),
                _ => return,
            }
        }

        (self.on_change)();
    }

    /// The node already ran the image, so nothing was swapped.
    pub fn unchanged(&self, image: &str) {
        self.set(Some(AgentUpdate { image: image.to_owned(), step: None, outcome: Some(AgentUpdateOutcome::Unchanged), detail: None }));
        (self.on_change)();
    }

    /// An update that failed before its helper could record it, recorded as the helper would.
    pub fn record_failure(&self, image: &str, outcome: SwapFailureOutcome, detail: &str) {
        let failure = SwapFailure { image: image.to_owned(), outcome, detail: detail.to_owned() };

        if let Err(error) = write_swap_failure(&self.failure_path, &failure) {
            self.log.warn(format!("could not record the failed agent update: {error}"));
        }

        self.set(Some(failed(&failure)));
        (self.on_change)();
    }
}
