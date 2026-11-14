//! The container swap of `update_agent`, run from a throwaway container made of the new agent
//! image. It cannot run in the agent, because the first thing it does is stop the container it
//! would be running in.
//!
//! The old container is kept, renamed, until the replacement has proved it stays up. A broken
//! image then rolls back to the agent that asked for it, so the node keeps an agent.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::time::Duration;

use ankor_protocol::constants::STOP_TIMEOUT;
use hyper::StatusCode;
use serde_json::Value;
use tokio::time::Instant;

use crate::agent_update::{SwapFailure, SwapFailureOutcome, write_swap_failure};
use crate::contract::constants::self_update_env;
use crate::docker_api::{DockerApi, DockerApiPort, DockerError};
use crate::host_layout::DOCKER_SOCKET;
use crate::self_update::spec_keys;

/// How long a replacement agent container must stay up, with no restart, before the helper
/// discards the previous one. Boot failures surface within seconds; later crashes are the restart
/// policy's.
const SELF_UPDATE_PROVE: Duration = Duration::from_secs(10);

/// Suffix of the renamed old container, which frees its name for the replacement.
pub const PREVIOUS_SUFFIX: &str = ".previous";

const PROVE_POLL: Duration = Duration::from_secs(1);

/// Why a swap failed, or why the environment describes none.
#[derive(Debug, thiserror::Error)]
pub enum SelfUpdateHelperError {
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error("the replacement container exited on its own")]
    Exited,
    #[error("the replacement container crashed and was restarted {count} time(s)")]
    Restarted { count: i64 },
    #[error("{key} is not set")]
    Unset { key: String },
    #[error(transparent)]
    Spec(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelfUpdateParams {
    /// The running agent container, to be stopped and replaced.
    pub previous_id: String,
    /// Create payload for the replacement. Its `name` is the name the old container gives up.
    pub spec: Value,
    /// How long the replacement must stay up; `SELF_UPDATE_PROVE` when `None`.
    pub prove_running: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapResult {
    pub ok: bool,
    pub message: String,
}

/// 304 (already stopped) is the outcome this wants anyway; a 404 means the container to roll back
/// to is gone.
async fn stop_quietly(docker: &dyn DockerApiPort, id: &str, timeout_seconds: i64) -> Result<(), DockerError> {
    match docker.stop_container(id, timeout_seconds).await {
        Err(error) if error.status() != Some(304) => Err(error),
        _ => Ok(()),
    }
}

/// The restart count catches a crash the restart policy healed between two polls.
async fn prove_running(docker: &dyn DockerApiPort, id: &str, period: Duration) -> Result<(), SelfUpdateHelperError> {
    let deadline = Instant::now() + period;

    while Instant::now() < deadline {
        tokio::time::sleep(PROVE_POLL.min(deadline.saturating_duration_since(Instant::now()))).await;

        let state = docker.inspect_container(id).await?;

        if !state.state.is_some_and(|state| state.running) {
            return Err(SelfUpdateHelperError::Exited);
        }

        if let Some(count) = state.restart_count.filter(|&count| count > 0) {
            return Err(SelfUpdateHelperError::Restarted { count });
        }
    }

    Ok(())
}

/// What the swap did to the daemon. A call that got no answer may still have taken effect, so it
/// counts as done, because undoing a step that did not happen costs a refused call, while leaving
/// one that did can leave the node with no agent.
#[derive(Default)]
struct Progress {
    /// The old container may be stopped, so a rollback must start it again.
    stopped: bool,
    /// The old container may carry the `.previous` name, so a rollback must rename it back.
    renamed: bool,
    /// A replacement whose id is unknown and which may hold the name: one whose create got no
    /// answer, or one an earlier run of the helper made.
    unknown_replacement: bool,
}

fn unanswered<T>(result: &Result<T, DockerError>) -> bool {
    matches!(result, Err(DockerError::NoAnswer(_)))
}

/// On any failure before the proof is in, the old container gets its name back and starts again.
pub async fn run_self_update(docker: &dyn DockerApiPort, params: &SelfUpdateParams) -> SwapResult {
    let name = params.spec.get(spec_keys::NAME).and_then(Value::as_str).unwrap_or_default();
    let image = params.spec.get(spec_keys::IMAGE).and_then(Value::as_str).unwrap_or_default();
    let previous = params.previous_id.as_str();
    let mut progress = Progress::default();
    let mut replacement: Option<String> = None;
    let timeout = i64::try_from(STOP_TIMEOUT.as_secs()).unwrap_or(i64::MAX);
    let swapped: Result<(), SelfUpdateHelperError> = async {
        let stopped = stop_quietly(docker, previous, timeout).await;

        progress.stopped = stopped.is_ok() || unanswered(&stopped);
        stopped?;

        let renamed = park(docker, previous, &format!("{name}{PREVIOUS_SUFFIX}")).await;

        progress.renamed = renamed.is_ok() || unanswered(&renamed);
        renamed?;

        let created = docker.create_container(name, &params.spec).await;

        progress.unknown_replacement = unanswered(&created);

        let id = created?;

        replacement = Some(id.clone());
        docker.start_container(&id).await?;
        prove_running(docker, &id, params.prove_running.unwrap_or(SELF_UPDATE_PROVE)).await
    }
    .await;

    if let Err(failure) = swapped {
        let rollback = restore(docker, previous, replacement.as_deref(), name, &progress).await;

        return SwapResult { ok: false, message: format!("{name} was not replaced with {image}: {failure} ({rollback})") };
    }

    // The swap has held; the old container is cleanup, and a daemon that keeps it costs disk only.
    match docker.remove_container(previous, true, false).await {
        Ok(()) => SwapResult { ok: true, message: format!("{name} now runs {image}") },
        Err(error) => SwapResult { ok: true, message: format!("{name} now runs {image}; {name}{PREVIOUS_SUFFIX} could not be removed: {error}") },
    }
}

/// Where the swap stands as this helper starts, read off the old agent. Docker restarts a helper
/// that crashed or ran out of memory, and the rerun must finish or undo the swap the first run
/// left, not start it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// The old agent runs under its name, so the swap has not begun.
    NotBegun,
    /// The old agent is gone. The helper removes it only once its replacement has held.
    Replaced,
    /// The old agent is stopped, or parked under another name: an earlier run stopped partway.
    /// This includes a run that died after the proof but before it removed the old agent; its
    /// replacement held, but rolling it back loses nothing, and the automatic update tries again.
    Interrupted { parked: bool },
}

async fn stage(docker: &dyn DockerApiPort, previous: &str, name: &str) -> Result<Stage, DockerError> {
    match docker.inspect_container(previous).await {
        Ok(old) => {
            let parked = old.name.trim_start_matches('/') != name;

            Ok(if !parked && old.state.is_some_and(|state| state.running) { Stage::NotBegun } else { Stage::Interrupted { parked } })
        }
        Err(error) if error.has_status(&[StatusCode::NOT_FOUND]) => Ok(Stage::Replaced),
        Err(error) => Err(error),
    }
}

/// Ends the swap an earlier run of this helper left. A replacement that runs once the old agent is
/// gone has held; an interrupted swap is rolled back, as its replacement never proved itself.
async fn resume(docker: &dyn DockerApiPort, params: &SelfUpdateParams, parked: Option<bool>) -> SwapResult {
    let name = params.spec.get(spec_keys::NAME).and_then(Value::as_str).unwrap_or_default();
    let image = params.spec.get(spec_keys::IMAGE).and_then(Value::as_str).unwrap_or_default();

    let Some(parked) = parked else {
        return match docker.inspect_container(name).await {
            Ok(replacement) if replacement.state.as_ref().is_some_and(|state| state.running) => {
                SwapResult { ok: true, message: format!("{name} now runs {image}") }
            }
            _ => SwapResult { ok: false, message: format!("{name} was not replaced with {image}: the previous container is gone and no replacement runs") },
        };
    };
    let progress = Progress { stopped: true, renamed: parked, unknown_replacement: parked };
    let rollback = restore(docker, &params.previous_id, None, name, &progress).await;

    SwapResult { ok: false, message: format!("{name} was not replaced with {image}: an earlier run of the helper stopped partway ({rollback})") }
}

/// Renames the old container out of the way. A container an earlier swap left under that name,
/// because its cleanup failed, would refuse every later swap, so it is removed and the rename tried
/// again.
async fn park(docker: &dyn DockerApiPort, previous: &str, parked: &str) -> Result<(), DockerError> {
    match docker.rename_container(previous, parked).await {
        Err(error) if error.has_status(&[StatusCode::CONFLICT]) => {
            docker.remove_container(parked, true, false).await?;
            docker.rename_container(previous, parked).await
        }
        renamed => renamed,
    }
}

/// Removes the replacement, gives the old container its name back and starts it again. A `docker
/// stop` defeats `unless-stopped`, so the start happens whenever the stop did, name or no name.
async fn restore(docker: &dyn DockerApiPort, previous: &str, replacement: Option<&str>, name: &str, progress: &Progress) -> String {
    let mut notes: Vec<String> = Vec::new();

    if let Some(replacement) = replacement
        && let Err(error) = docker.remove_container(replacement, true, false).await
    {
        notes.push(format!("the replacement container could not be removed: {error}"));
    }

    // Removed by name, as its id never came back; one that was never made is a 404.
    if progress.unknown_replacement
        && let Err(error) = docker.remove_container(name, true, false).await
        && !error.has_status(&[StatusCode::NOT_FOUND])
    {
        notes.push(format!("a replacement the daemon may have created could not be removed: {error}"));
    }

    if progress.renamed
        && let Err(error) = docker.rename_container(previous, name).await
    {
        notes.push(format!("the previous container keeps the name {name}{PREVIOUS_SUFFIX}: {error}"));
    }

    let mut outcome = "the previous container could not be stopped, so nothing was rolled back".to_owned();

    if progress.stopped {
        // A stop that got no answer may not have happened, and a running container answers 304.
        outcome = match docker.start_container(previous).await {
            Err(error) if error.status() != Some(304) => format!("the previous container is stopped: {error}"),
            _ => "the previous container was restarted".to_owned(),
        };
    }

    std::iter::once(outcome).chain(notes).collect::<Vec<_>>().join("; ")
}

fn read_env(env: &BTreeMap<String, String>, key: &str) -> Result<String, SelfUpdateHelperError> {
    env.get(key).filter(|value| !value.is_empty()).cloned().ok_or_else(|| SelfUpdateHelperError::Unset { key: key.to_owned() })
}

/// # Errors
///
/// Fails when a variable is missing or empty, or the spec is not JSON.
pub fn params_from_env(env: &BTreeMap<String, String>) -> Result<SelfUpdateParams, SelfUpdateHelperError> {
    let previous_id = read_env(env, self_update_env::PREVIOUS_ID)?;
    let spec = serde_json::from_str(&read_env(env, self_update_env::SPEC)?)?;

    Ok(SelfUpdateParams { previous_id, spec, prove_running: None })
}

/// Runs the swap, or ends the one an earlier run left, and records the result for the next agent.
/// Returns exit code 1 when the stage is unknown or a failure could not be recorded. The restart
/// policy reruns the helper on that exit and on a crash.
///
/// # Errors
///
/// Fails before any swap when the environment does not describe one.
pub async fn run_helper(
    env: &BTreeMap<String, String>,
    docker: &dyn DockerApiPort,
    prove_running: Option<Duration>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<i32, SelfUpdateHelperError> {
    let mut params = params_from_env(env)?;

    if prove_running.is_some() {
        params.prove_running = prove_running;
    }

    let failure_path = read_env(env, self_update_env::FAILURE)?;
    let name = params.spec.get(spec_keys::NAME).and_then(Value::as_str).unwrap_or_default();
    // An interrupted swap is not blamed on the build, so the automatic update tries it again. A
    // helper that crashes the same way every time is retried at each image check, which is the
    // price of not skipping a good build after an out-of-memory kill.
    let (result, outcome) = match stage(docker, &params.previous_id, name).await {
        Ok(Stage::NotBegun) => (run_self_update(docker, &params).await, SwapFailureOutcome::Failed),
        Ok(Stage::Replaced) => (resume(docker, &params, None).await, SwapFailureOutcome::Failed),
        Ok(Stage::Interrupted { parked }) => (resume(docker, &params, Some(parked)).await, SwapFailureOutcome::NotStarted),
        // Recording a result would end the restarts with the old agent perhaps stopped, so the
        // helper exits with an error instead, and Docker runs it again.
        Err(error) => {
            let _ = writeln!(err, "Ankor self-update: could not tell where the swap stands: {error}");
            return Ok(1);
        }
    };
    let _ = writeln!(out, "Ankor self-update: {}: {}", if result.ok { "ok" } else { "failed" }, result.message);

    // The file says the last swap failed, and why; a swap that held removes an earlier one's.
    let recorded = if result.ok {
        match std::fs::remove_file(&failure_path) {
            Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    } else {
        let image = params.spec.get(spec_keys::IMAGE).and_then(Value::as_str).unwrap_or_default().to_owned();

        // A failed swap blames the build, unless it was interrupted, so the automatic update skips
        // it: this helper runs from the new image, so even a failed stop or rename can be its fault.
        write_swap_failure(Path::new(&failure_path), &SwapFailure { image, outcome, detail: result.message.clone() })
    };
    // Only this exit is non-zero, so the agent learns of a failure the file could not hold. Docker
    // then reruns the helper, which finds the old agent back under its name and swaps again, a
    // few times at most; a data directory that cannot be written breaks more than updates.
    let unrecorded_failure = !result.ok && recorded.is_err();

    if let Err(error) = recorded {
        let _ = writeln!(err, "Ankor self-update: could not record the result in {failure_path}: {error}");
    }

    Ok(i32::from(unrecorded_failure))
}

pub async fn start_self_update() -> i32 {
    let env: BTreeMap<String, String> = std::env::vars_os().filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))).collect();

    match run_helper(&env, &DockerApi::new(DOCKER_SOCKET), None, &mut std::io::stdout(), &mut std::io::stderr()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Ankor self-update: {error}");
            1
        }
    }
}
