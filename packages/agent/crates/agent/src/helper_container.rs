//! One-shot containers the agent runs to completion: the game update, overlay extraction and the
//! build-id probe.

use crate::sync::lock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, oneshot};

use crate::container_spec::{ContainerRole, LabelInput, container_hardening, container_labels};
use crate::docker::{ContainerHooks, DockerPort, LineHook, NetworkMode, StartContainerSpec};
use crate::docker_api::DockerError;
use crate::host_layout::{BindMount, VolumeMount};
use crate::logger::Logger;

/// Tells a wait when a container's output last showed a line, so a container that prints nothing
/// for too long counts as hung.
#[derive(Clone, Default)]
pub(crate) struct Activity(Arc<Notify>);

impl Activity {
    pub fn watch(&self, on_line: LineHook) -> LineHook {
        let activity = Arc::clone(&self.0);

        Arc::new(move |line| {
            activity.notify_one();
            on_line(line);
        })
    }

    /// A line that came before the call counts, because `notify_one` keeps it until it is waited for.
    pub async fn quiet_for(&self, quiet: Duration) {
        loop {
            tokio::select! {
                () = self.0.notified() => {}
                () = tokio::time::sleep(quiet) => return,
            }
        }
    }
}

/// Keeps container names unique on the host.
#[must_use]
pub fn short_id() -> String {
    format!("{:06x}", rand::random::<u32>() & 0x00ff_ffff)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HelperSpec {
    /// Becomes `ankor-<name>-<short id>`.
    pub name: String,
    /// The image the helper runs.
    pub image: String,
    /// The `ankor.role` label; `Probe` when `None`.
    pub role: Option<ContainerRole>,
    /// Only for a helper that outlives the wait on it. Docker would remove a quick one before its
    /// exit code is read.
    pub auto_remove: bool,
    /// Environment variables, as `KEY=value`.
    pub env: Vec<String>,
    /// Named volumes mounted into the helper.
    pub volumes: Vec<VolumeMount>,
    /// Host paths mounted into the helper.
    pub binds: Vec<BindMount>,
    /// Overrides the image's entrypoint.
    pub entrypoint: Option<Vec<String>>,
    /// Overrides the image's command.
    pub cmd: Option<Vec<String>>,
}

/// A helper runs on the bridge network without CPU weight, so it never competes with a match, and
/// without a memory cap, so steamcmd's verification is not killed.
#[must_use]
pub(crate) fn helper_spec(input: HelperSpec, id: &str) -> StartContainerSpec {
    let hardening = container_hardening(None);

    StartContainerSpec {
        name: format!("ankor-{}-{id}", input.name),
        image: input.image,
        env: input.env,
        labels: container_labels(input.role.unwrap_or(ContainerRole::Probe), &LabelInput::default()),
        network_mode: NetworkMode::Bridge,
        cap_drop: hardening.cap_drop,
        security_opt: hardening.security_opt,
        pids_limit: hardening.pids_limit,
        memory_bytes: hardening.memory_bytes,
        cpu_shares: None,
        auto_remove: input.auto_remove,
        volumes: input.volumes,
        binds: input.binds,
        tmpfs: Vec::new(),
        cmd: input.cmd,
        entrypoint: input.entrypoint,
    }
}

/// Why a helper container's run failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HelperError {
    /// The daemon refused or failed a call.
    #[error(transparent)]
    Docker(#[from] DockerError),
    /// The container printed nothing for this long, and was removed; `left` names it when the
    /// removal failed, so it may still be running.
    #[error("{name} printed nothing for {} s", quiet.as_secs())]
    Quiet { name: String, quiet: Duration, left: Option<String> },
}

impl HelperError {
    /// The container a failed run may have left running, which whatever it guarded must wait for.
    #[must_use]
    pub(crate) fn left_running(&self) -> Option<&str> {
        match self {
            Self::Quiet { left, .. } => left.as_deref(),
            Self::Docker(_) => None,
        }
    }
}

/// Returns `None` when the exit went unobserved. A container that prints nothing for `quiet` is
/// removed.
///
/// # Errors
///
/// Fails when the container could not start, or went quiet.
pub(crate) async fn run_to_exit(
    docker: &dyn DockerPort,
    spec: &StartContainerSpec,
    on_line: LineHook,
    quiet: Duration,
    log: &Logger,
) -> Result<Option<i64>, HelperError> {
    // Registered before the start, as a one-shot container may finish before the start returns.
    // Docker's wait comes after it, so one that also removes itself that fast reports no code.
    let (exited, mut exit) = oneshot::channel();
    let exited = Arc::new(Mutex::new(Some(exited)));
    let activity = Activity::default();
    let hooks = ContainerHooks {
        on_line: activity.watch(on_line),
        on_exit: Arc::new(move |code| {
            if let Some(exited) = lock(&exited).take() {
                let _ = exited.send(code);
            }
        }),
    };
    let id = docker.start_container(spec, hooks).await?;
    let ended = tokio::select! {
        code = &mut exit => Ok(code.unwrap_or(None)),
        () = activity.quiet_for(quiet) => Err(HelperError::Quiet { name: spec.name.clone(), quiet, left: None }),
    };

    if let Err(error) = docker.remove_container(&id, true).await {
        log.warn(format!("could not remove {}: {error}", spec.name));

        // One that went quiet may still be running; one that exited writes nothing more.
        if let Err(HelperError::Quiet { name, quiet, .. }) = ended {
            return Err(HelperError::Quiet { name, quiet, left: Some(id) });
        }
    }

    ended
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
mod tests {
    use super::*;
    use crate::testing::FakeDocker;

    #[test]
    fn a_helper_has_no_env_mounts_or_entrypoint_unless_asked_and_is_removed_by_the_agent() {
        let spec = helper_spec(
            HelperSpec {
                name: "probe".to_owned(),
                image: "ghcr.io/example/ankor-cs2:v1".to_owned(),
                role: Some(ContainerRole::Probe),
                ..HelperSpec::default()
            },
            "ab12cd",
        );

        assert_eq!(spec.name, "ankor-probe-ab12cd");
        assert!(spec.env.is_empty() && spec.volumes.is_empty() && spec.binds.is_empty());
        assert!(!spec.auto_remove);
        assert_eq!(spec.labels["ankor.role"], "probe");
        assert_eq!(spec.entrypoint, None);
        assert_eq!(spec.cmd, None);
    }

    #[test]
    fn carries_what_it_is_given() {
        let spec = helper_spec(
            HelperSpec {
                name: "update".to_owned(),
                image: "img".to_owned(),
                role: Some(ContainerRole::Update),
                auto_remove: true,
                env: vec!["A=1".to_owned()],
                entrypoint: Some(vec!["/bin/sh".to_owned()]),
                cmd: Some(vec!["-c".to_owned(), "true".to_owned()]),
                ..HelperSpec::default()
            },
            "ab12cd",
        );

        assert_eq!(spec.env, ["A=1"]);
        assert!(spec.auto_remove);
        assert_eq!(spec.entrypoint.as_deref(), Some(&["/bin/sh".to_owned()][..]));
        assert_eq!(spec.cmd.as_deref(), Some(&["-c".to_owned(), "true".to_owned()][..]));
    }

    #[test]
    fn names_each_helper_apart() {
        let id = short_id();

        assert!(id.len() == 6 && id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(short_id(), short_id());
    }

    #[tokio::test(start_paused = true)]
    async fn removes_a_container_that_stays_quiet_and_lets_one_that_prints_run_on() {
        let docker = FakeDocker::new();
        let spec = helper_spec(
            HelperSpec { name: "update".to_owned(), image: "img".to_owned(), role: Some(ContainerRole::Update), ..HelperSpec::default() },
            "ab12cd",
        );
        let quiet = Duration::from_mins(1);
        let running = tokio::spawn({
            let (docker, spec) = (Arc::clone(&docker), spec.clone());

            async move { run_to_exit(docker.as_ref(), &spec, Arc::new(|_| {}), quiet, &Logger::null()).await }
        });

        // A line every 50 s keeps it running past several quiet periods.
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_secs(50)).await;
            docker.line(0, "progress");
        }
        assert!(!running.is_finished());

        tokio::time::sleep(quiet).await;

        let error = running.await.unwrap().unwrap_err();

        assert!(matches!(error, HelperError::Quiet { .. }));
        assert_eq!(error.to_string(), format!("{} printed nothing for 60 s", spec.name));
        assert_eq!(error.left_running(), None);
        assert_eq!(*docker.removed.lock().unwrap(), ["container0"]);
    }

    #[tokio::test(start_paused = true)]
    async fn names_a_quiet_container_it_could_not_remove_as_left_running() {
        let docker = FakeDocker::new();
        let spec = helper_spec(HelperSpec { name: "update".to_owned(), image: "img".to_owned(), ..HelperSpec::default() }, "ab12cd");

        *docker.remove_error.lock().unwrap() = Some("daemon busy".to_owned());

        let error = run_to_exit(docker.as_ref(), &spec, Arc::new(|_| {}), Duration::from_mins(1), &Logger::null()).await.unwrap_err();

        assert_eq!(error.left_running(), Some("container0"));
    }

    #[tokio::test]
    async fn answers_the_exit_code_even_when_the_container_cannot_be_removed() {
        let docker = FakeDocker::new();

        *docker.exit_on_start.lock().unwrap() = Some((Vec::new(), Some(3)));
        *docker.remove_error.lock().unwrap() = Some("device or resource busy".to_owned());

        let spec =
            helper_spec(HelperSpec { name: "probe".to_owned(), image: "img".to_owned(), role: Some(ContainerRole::Probe), ..HelperSpec::default() }, "ab12cd");

        assert_eq!(run_to_exit(docker.as_ref(), &spec, Arc::new(|_| {}), Duration::from_mins(1), &Logger::null()).await.unwrap(), Some(3));
    }
}
