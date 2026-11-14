//! The Docker service against a fake API client: the payloads it builds, what it cleans up and
//! how it reads the daemon's answers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
#![allow(clippy::too_many_lines, reason = "the fake daemon answers every call")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::container_spec::labels;
use ankor_agent::contract::constants::self_update_env;
use ankor_agent::docker::{ContainerHooks, DockerPort as _, DockerService, NetworkMode, RunningUpdate, SelfUpdateHelper, StartContainerSpec, UpdateKind};
use ankor_agent::docker_api::{
    ContainerInspect, ContainerState, DockerApiPort, DockerError, Filters, ImageConfig, ImageInspect, ListedContainer, ListedImage, OutputStream, PendingExit,
    WaitCondition, container_state,
};
use ankor_agent::host_layout::{BindMount, TmpfsMount, VolumeMount};
use ankor_agent::logger::Logger;
use ankor_protocol::types::Cs2UpdateMode;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

const CREATED_ID: &str = "deadbeefcafe0000";

fn api_error(status: u16, message: &str) -> DockerError {
    DockerError::Api { status, message: message.to_owned() }
}

fn copy(error: &DockerError) -> DockerError {
    match error {
        DockerError::Api { status, message } => DockerError::Api { status: *status, message: message.clone() },
        DockerError::NoAnswer(limit) => DockerError::NoAnswer(*limit),
        other => DockerError::Http(other.to_string()),
    }
}

#[derive(Default)]
struct Daemon {
    engine_error: Option<DockerError>,
    image: Option<ImageInspect>,
    image_error: Option<DockerError>,
    create_error: Option<DockerError>,
    registry: Option<Result<String, String>>,
    registry_delay: Duration,
    attach_error: Option<DockerError>,
    start_error: Option<DockerError>,
    remove_error: Option<DockerError>,
    stop_error: Option<DockerError>,
    wait_errors: usize,
    waits: usize,
    exit_code: Option<i64>,
    wait_gone: bool,
    /// Docker removes the container as it exits, so a wait that comes after the start finds none.
    removed_on_exit: bool,
    starts: usize,
    /// Each wait's condition, and whether the container had started when it came.
    wait_conditions: Vec<(WaitCondition, bool)>,
    containers: Vec<ListedContainer>,
    images: Vec<ListedImage>,
    remove_image_error: Option<DockerError>,
    inspect: Option<(Option<ContainerState>, Value)>,
    streams: Vec<tokio::io::DuplexStream>,
    copy_error: Option<DockerError>,
    /// The copy never ends, as a hung daemon's would not.
    copy_hangs: bool,
    pull_error: Option<DockerError>,
}

#[derive(Default)]
struct Recorded {
    created: Vec<(String, Value)>,
    removed: Vec<String>,
    remove_options: Vec<(bool, bool)>,
    listed: Vec<(bool, Option<Filters>)>,
    image_lists: Vec<Option<Filters>>,
    copies: Vec<(String, String, std::path::PathBuf)>,
}

#[derive(Clone, Default)]
struct FakeApi {
    daemon: Arc<Mutex<Daemon>>,
    recorded: Arc<Mutex<Recorded>>,
}

fn container(id: &str, name: &str, state: &str, labels: &[(&str, &str)]) -> ListedContainer {
    serde_json::from_value(json!({
        "Id": id,
        "Names": [name],
        "ImageID": "sha256:busybox",
        "State": state,
        "Labels": labels.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect::<BTreeMap<_, _>>(),
    }))
    .unwrap()
}

#[async_trait]
impl DockerApiPort for FakeApi {
    async fn engine(&self) -> Result<(String, String), DockerError> {
        match &self.daemon.lock().unwrap().engine_error {
            Some(error) => Err(copy(error)),
            None => Ok(("29.0.0".to_owned(), "1.52".to_owned())),
        }
    }

    async fn create_container(&self, name: &str, spec: &Value) -> Result<String, DockerError> {
        self.recorded.lock().unwrap().created.push((name.to_owned(), spec.clone()));
        self.daemon.lock().unwrap().create_error.as_ref().map_or(Ok(CREATED_ID.to_owned()), |error| Err(copy(error)))
    }

    async fn start_container(&self, _id: &str) -> Result<(), DockerError> {
        let mut daemon = self.daemon.lock().unwrap();

        if let Some(error) = &daemon.start_error {
            return Err(copy(error));
        }

        daemon.starts += 1;
        Ok(())
    }

    async fn stop_container(&self, _id: &str, _timeout_seconds: i64) -> Result<(), DockerError> {
        self.daemon.lock().unwrap().stop_error.as_ref().map_or(Ok(()), |error| Err(copy(error)))
    }

    async fn rename_container(&self, _id: &str, _name: &str) -> Result<(), DockerError> {
        unimplemented!()
    }

    async fn remove_container(&self, id: &str, force: bool, volumes: bool) -> Result<(), DockerError> {
        let mut recorded = self.recorded.lock().unwrap();

        recorded.remove_options.push((force, volumes));

        if let Some(error) = &self.daemon.lock().unwrap().remove_error {
            return Err(copy(error));
        }

        recorded.removed.push(id.to_owned());
        Ok(())
    }

    async fn inspect_container(&self, id: &str) -> Result<ContainerInspect, DockerError> {
        let (state, config) = self.daemon.lock().unwrap().inspect.clone().unwrap_or((None, json!({ "Image": "cs2" })));

        Ok(ContainerInspect {
            id: id.to_owned(),
            name: format!("/{id}"),
            image: "sha256:cs2".to_owned(),
            config,
            host_config: None,
            state,
            restart_count: None,
        })
    }

    async fn list_containers(&self, all: bool, filters: Option<&Filters>) -> Result<Vec<ListedContainer>, DockerError> {
        self.recorded.lock().unwrap().listed.push((all, filters.cloned()));

        // As the daemon does: a role label filter, and running containers only unless `all`.
        let roles: Vec<String> = filters
            .and_then(|filters| filters.get("label"))
            .into_iter()
            .flatten()
            .filter_map(|label| label.strip_prefix(labels::ROLE).and_then(|rest| rest.strip_prefix('=')).map(str::to_owned))
            .collect();

        Ok(self
            .daemon
            .lock()
            .unwrap()
            .containers
            .iter()
            .filter(|container| {
                roles.is_empty() || roles.iter().any(|role| container.labels.as_ref().and_then(|labels| labels.get(labels::ROLE)) == Some(role))
            })
            .filter(|container| all || container.state == container_state::RUNNING)
            .cloned()
            .collect())
    }

    async fn wait_container(&self, _id: &str, condition: WaitCondition) -> Result<PendingExit, DockerError> {
        let mut daemon = self.daemon.lock().unwrap();
        let started = daemon.starts > 0;

        daemon.waits += 1;
        daemon.wait_conditions.push((condition, started));

        if daemon.wait_gone || (daemon.removed_on_exit && started) {
            return Err(api_error(404, "No such container"));
        }

        if daemon.waits <= daemon.wait_errors {
            return Err(DockerError::Http("socket hang up".to_owned()));
        }

        let code = daemon.exit_code.unwrap_or(0);

        Ok(Box::pin(async move { Ok(Some(code)) }))
    }

    async fn attach_container(&self, _id: &str) -> Result<OutputStream, DockerError> {
        let mut daemon = self.daemon.lock().unwrap();

        if let Some(error) = &daemon.attach_error {
            return Err(copy(error));
        }

        let (writer, reader) = tokio::io::duplex(1 << 20);

        daemon.streams.push(writer);
        Ok(Box::pin(reader))
    }

    async fn inspect_image(&self, _reference: &str) -> Result<ImageInspect, DockerError> {
        let daemon = self.daemon.lock().unwrap();

        match &daemon.image_error {
            Some(error) => Err(copy(error)),
            None => Ok(daemon.image.clone().unwrap_or(ImageInspect { id: Some("sha256:cs2".to_owned()), repo_digests: None, config: None })),
        }
    }

    async fn list_images(&self, filters: Option<&Filters>) -> Result<Vec<ListedImage>, DockerError> {
        self.recorded.lock().unwrap().image_lists.push(filters.cloned());
        Ok(self.daemon.lock().unwrap().images.clone())
    }

    async fn remove_image(&self, _reference: &str) -> Result<(), DockerError> {
        self.daemon.lock().unwrap().remove_image_error.as_ref().map_or(Ok(()), |error| Err(copy(error)))
    }

    async fn registry_digest(&self, _reference: &str) -> Result<String, DockerError> {
        let (delay, answer) = {
            let daemon = self.daemon.lock().unwrap();

            (daemon.registry_delay, daemon.registry.clone())
        };

        tokio::time::sleep(delay).await;
        answer.unwrap_or_else(|| Err("no registry".to_owned())).map_err(DockerError::Http)
    }

    async fn pull_image(&self, _reference: &str) -> Result<(), DockerError> {
        self.daemon.lock().unwrap().pull_error.as_ref().map_or(Ok(()), |error| Err(copy(error)))
    }

    async fn copy_from_container(&self, id: &str, path: &str, target: &std::path::Path) -> Result<(), DockerError> {
        self.recorded.lock().unwrap().copies.push((id.to_owned(), path.to_owned(), target.to_owned()));

        if self.daemon.lock().unwrap().copy_hangs {
            std::future::pending::<()>().await;
        }

        match &self.daemon.lock().unwrap().copy_error {
            Some(error) => Err(copy(error)),
            None => Ok(()),
        }
    }
}

fn service(daemon: Daemon) -> (DockerService, FakeApi) {
    let api = FakeApi { daemon: Arc::new(Mutex::new(daemon)), recorded: Arc::default() };

    (DockerService::new(api.clone(), Logger::null()), api)
}

fn no_hooks() -> ContainerHooks {
    ContainerHooks { on_line: Arc::new(|_| {}), on_exit: Arc::new(|_| {}) }
}

fn spec() -> StartContainerSpec {
    StartContainerSpec {
        name: "ankor-a-match-ab12cd".to_owned(),
        image: "ghcr.io/example/ankor-cs2:latest".to_owned(),
        env: Vec::new(),
        labels: BTreeMap::new(),
        network_mode: NetworkMode::Host,
        cap_drop: vec!["ALL".to_owned()],
        security_opt: vec!["no-new-privileges=true".to_owned()],
        pids_limit: 1024,
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

fn host_config(api: &FakeApi, index: usize) -> Value {
    api.recorded.lock().unwrap().created[index].1["HostConfig"].clone()
}

#[tokio::test]
async fn the_engine_check_names_the_release_it_needs() {
    assert_eq!(service(Daemon::default()).0.require_supported_engine().await.unwrap(), "29.0.0");

    let too_old =
        service(Daemon { engine_error: Some(api_error(400, "client version 1.44 is too new. Maximum supported API version is 1.43")), ..Daemon::default() }).0;

    assert_eq!(
        too_old.require_supported_engine().await.unwrap_err().to_string(),
        "Docker Engine is too old for this agent (client version 1.44 is too new. Maximum supported API version is 1.43); it needs Docker Engine 25.0 or newer, API 1.44"
    );
    assert_eq!(
        service(Daemon { engine_error: Some(api_error(500, "boom")), ..Daemon::default() }).0.require_supported_engine().await.unwrap_err().to_string(),
        "boom"
    );
}

#[tokio::test]
async fn image_inspection_answers_none_only_for_an_absent_image() {
    let (docker, api) = service(Daemon::default());

    assert_eq!(docker.image_id("ankor-cs2:latest").await.unwrap().as_deref(), Some("sha256:cs2"));
    api.daemon.lock().unwrap().image_error = Some(api_error(404, "no such image"));
    assert_eq!(docker.image_id("ankor-cs2:latest").await.unwrap(), None);
    api.daemon.lock().unwrap().image_error = Some(api_error(500, "daemon failure"));
    assert_eq!(docker.image_id("ankor-cs2:latest").await.unwrap_err().to_string(), "daemon failure");

    for id in [None, Some(String::new())] {
        let docker = service(Daemon { image: Some(ImageInspect { id, repo_digests: None, config: None }), ..Daemon::default() }).0;

        assert!(docker.image_id("ankor-cs2:latest").await.unwrap_err().to_string().contains("Docker returned no image id"));
    }
}

#[tokio::test]
async fn maps_the_hardening_fields_onto_the_create_payload() {
    let (docker, api) = service(Daemon::default());
    let mut capped = spec();

    capped.memory_bytes = Some(2048_i64 * 1024 * 1024);
    docker.start_container(&capped, no_hooks()).await.unwrap();

    let host = host_config(&api, 0);

    assert_eq!(
        (host["CapDrop"].clone(), host["SecurityOpt"].clone(), host["PidsLimit"].clone()),
        (json!(["ALL"]), json!(["no-new-privileges=true"]), json!(1024))
    );
    assert_eq!(host["Memory"], 2048_i64 * 1024 * 1024);
    assert_eq!(host["LogConfig"], json!({ "Type": "local", "Config": { "mode": "non-blocking", "max-buffer-size": "4m" } }));
    assert!(host.get("CapAdd").is_none() && host.get("Privileged").is_none());
    assert!(!host.to_string().contains("unconfined"));
}

#[tokio::test]
async fn sends_volumes_binds_and_tmpfs_as_mounts() {
    let (docker, api) = service(Daemon::default());
    let mut mounted = spec();

    mounted.volumes = vec![VolumeMount { volume: "ankor-cs2".to_owned(), target: "/cs2".to_owned(), read_only: true }];
    mounted.binds = vec![BindMount { source: "/srv/ankor/slots/a/demos".to_owned(), target: "/demos".to_owned(), read_only: false }];
    docker.start_container(&mounted, no_hooks()).await.unwrap();

    let mut tmpfs = spec();

    tmpfs.tmpfs = vec![TmpfsMount { target: "/plugins".to_owned(), size_bytes: 1_048_576, mode: 0o755 }];
    docker.start_container(&tmpfs, no_hooks()).await.unwrap();

    assert_eq!(
        host_config(&api, 0)["Mounts"],
        json!([{ "Type": "volume", "Source": "ankor-cs2", "Target": "/cs2", "ReadOnly": true }, { "Type": "bind", "Source": "/srv/ankor/slots/a/demos", "Target": "/demos", "ReadOnly": false }])
    );
    assert_eq!(host_config(&api, 1)["Mounts"], json!([{ "Type": "tmpfs", "Target": "/plugins", "TmpfsOptions": { "SizeBytes": 1_048_576, "Mode": 0o755 } }]));
}

#[tokio::test]
async fn omits_what_is_unset() {
    let (docker, api) = service(Daemon::default());
    let mut bare = spec();

    bare.cap_drop.clear();
    bare.security_opt.clear();
    bare.pids_limit = 0;
    docker.start_container(&bare, no_hooks()).await.unwrap();

    let mut weighted = spec();

    weighted.cpu_shares = Some(26193);
    docker.start_container(&weighted, no_hooks()).await.unwrap();
    docker.start_container(&spec(), no_hooks()).await.unwrap();

    let host = host_config(&api, 0);

    assert!(host.get("CapDrop").is_none() && host.get("SecurityOpt").is_none() && host.get("PidsLimit").is_none() && host.get("Memory").is_none());
    assert_eq!(host_config(&api, 1)["CpuShares"], 26193);
    assert!(host_config(&api, 2).get("CpuShares").is_none());
}

fn refused_start() -> DockerError {
    api_error(500, "failed to create task for container: error setting rlimits for ready process: error setting rlimit type 13: operation not permitted")
}

#[tokio::test]
async fn removes_a_container_it_created_but_could_not_start() {
    let (docker, api) = service(Daemon { start_error: Some(refused_start()), ..Daemon::default() });

    assert!(docker.start_container(&spec(), no_hooks()).await.unwrap_err().to_string().contains("operation not permitted"));
    assert_eq!(api.recorded.lock().unwrap().removed, [CREATED_ID]);
    assert_eq!(api.recorded.lock().unwrap().remove_options, [(true, false)]);

    let (detached, api) = service(Daemon { start_error: Some(refused_start()), ..Daemon::default() });

    assert!(detached.run_detached("ankor-self-update-ab12cd", &json!({ "Image": "ankor-agent" })).await.is_err());
    assert_eq!(api.recorded.lock().unwrap().removed, [CREATED_ID]);
}

#[tokio::test]
async fn removes_by_name_a_container_whose_create_got_no_answer_and_leaves_one_docker_refused() {
    let no_answer = || DockerError::NoAnswer(ankor_agent::docker_api::ANSWER_WITHIN);
    let (docker, api) = service(Daemon { create_error: Some(no_answer()), ..Daemon::default() });

    assert!(matches!(docker.start_container(&spec(), no_hooks()).await.unwrap_err(), DockerError::NoAnswer(_)));
    assert!(docker.run_detached("ankor-self-update-ab12cd", &json!({ "Image": "ankor-agent" })).await.is_err());
    assert_eq!(api.recorded.lock().unwrap().removed, [spec().name, "ankor-self-update-ab12cd".to_owned()]);

    let (docker, api) = service(Daemon { create_error: Some(api_error(409, "name in use")), ..Daemon::default() });

    assert!(docker.start_container(&spec(), no_hooks()).await.is_err());
    assert!(api.recorded.lock().unwrap().removed.is_empty());
}

/// The only detached container is the self-update helper, which may start at any time after an
/// unanswered start and is then mid-swap.
#[tokio::test]
async fn never_removes_a_detached_container_whose_start_got_no_answer() {
    let no_answer = || DockerError::NoAnswer(ankor_agent::docker_api::ANSWER_WITHIN);
    let state = |running| Some((Some(ContainerState { running, exit_code: None }), json!({ "Image": "ankor-agent" })));
    let (docker, api) = service(Daemon { start_error: Some(no_answer()), inspect: state(true), ..Daemon::default() });

    assert_eq!(docker.run_detached("ankor-self-update-ab12cd", &json!({ "Image": "ankor-agent" })).await.unwrap(), CREATED_ID);
    assert!(api.recorded.lock().unwrap().removed.is_empty());

    let (docker, api) = service(Daemon { start_error: Some(no_answer()), inspect: state(false), ..Daemon::default() });

    assert!(matches!(docker.run_detached("ankor-self-update-ab12cd", &json!({ "Image": "ankor-agent" })).await.unwrap_err(), DockerError::NoAnswer(_)));
    assert!(api.recorded.lock().unwrap().removed.is_empty());
}

#[tokio::test]
async fn passes_the_start_failure_on_when_the_cleanup_fails_too() {
    let (docker, api) =
        service(Daemon { start_error: Some(refused_start()), remove_error: Some(DockerError::Http("daemon went away".to_owned())), ..Daemon::default() });

    assert!(docker.start_container(&spec(), no_hooks()).await.unwrap_err().to_string().contains("operation not permitted"));
    assert!(api.recorded.lock().unwrap().removed.is_empty());
}

#[tokio::test]
async fn hands_back_the_id_of_a_started_container_and_leaves_a_detached_one_running() {
    let (docker, api) = service(Daemon::default());

    assert_eq!(docker.start_container(&spec(), no_hooks()).await.unwrap(), CREATED_ID);
    assert_eq!(docker.run_detached("ankor-self-update-ab12cd", &json!({ "Image": "ankor-agent" })).await.unwrap(), CREATED_ID);
    assert!(api.recorded.lock().unwrap().removed.is_empty());
}

#[tokio::test]
async fn the_boot_sweep_removes_leftover_helpers_and_leaves_running_ones_and_games() {
    let (docker, api) = service(Daemon {
        containers: vec![
            container("probe1", "/ankor-nice-03863b", "created", &[("ankor.role", "probe")]),
            container("extract1", "/ankor-overlay-ff00ff", "exited", &[("ankor.role", "overlay")]),
            container("swap1", "/ankor-self-update-0c0ffe", "exited", &[("ankor.role", "self-update")]),
            container("extract2", "/ankor-overlay-aa11bb", "running", &[("ankor.role", "overlay")]),
            // An agent stopped between the update's create and its start; a running update is adopted.
            container("update1", "/ankor-update-0a0b0c", "created", &[("ankor.role", "update")]),
            container("update2", "/ankor-update-1d2e3f", "running", &[("ankor.role", "update")]),
            container("game1", "/ankor-a-match-ab12cd", "running", &[("ankor.slot", "a"), ("ankor.role", "game")]),
            container("game2", "/ankor-b-match-ab12cd", "exited", &[("ankor.slot", "b"), ("ankor.role", "game")]),
        ],
        ..Daemon::default()
    });

    assert_eq!(docker.sweep_helper_containers().await.unwrap(), ["ankor-nice-03863b", "ankor-overlay-ff00ff", "ankor-update-0a0b0c"]);

    let recorded = api.recorded.lock().unwrap();

    assert_eq!(recorded.removed, ["probe1", "extract1", "update1"]);
    assert_eq!(recorded.remove_options, [(true, false), (true, false), (true, false)]);
    assert_eq!(recorded.listed, [(true, Some(Filters::from([("label".to_owned(), vec!["ankor.role".to_owned()])])))]);
}

#[tokio::test]
async fn clears_exited_self_update_helpers_and_leaves_one_still_proving() {
    let helpers = || {
        vec![
            container("swap1", "/ankor-self-update-0c0ffe", "exited", &[("ankor.role", "self-update")]),
            container("swap2", "/ankor-self-update-beef00", "running", &[("ankor.role", "self-update")]),
            container("swap3", "/ankor-self-update-cafe00", "restarting", &[("ankor.role", "self-update")]),
        ]
    };
    let (docker, api) = service(Daemon { containers: helpers(), ..Daemon::default() });

    docker.clear_self_update_helpers().await.unwrap();
    assert_eq!(api.recorded.lock().unwrap().removed, ["swap1"]);
    assert_eq!(api.recorded.lock().unwrap().listed, [(true, Some(Filters::from([("label".to_owned(), vec!["ankor.role=self-update".to_owned()])])))]);

    let stuck = service(Daemon { containers: helpers(), remove_error: Some(DockerError::Http("device or resource busy".to_owned())), ..Daemon::default() }).0;

    assert_eq!(stuck.clear_self_update_helpers().await.unwrap_err().to_string(), "device or resource busy");
}

#[tokio::test]
async fn reads_the_last_swaps_helper() {
    let spec = json!({ "name": "ankor-agent", "Image": "ghcr.io/example/ankor-agent:2" }).to_string();
    let helper = |running: Option<bool>, env: Vec<String>| {
        let state = running.map(|running| ContainerState { running, exit_code: None });
        let listed_state = if running == Some(true) { container_state::RUNNING } else { container_state::EXITED };

        service(Daemon {
            containers: vec![container("swap1", "/ankor-self-update-0c0ffe", listed_state, &[("ankor.role", "self-update")])],
            inspect: Some((state, json!({ "Image": "ghcr.io/example/ankor-agent:2", "Env": env }))),
            ..Daemon::default()
        })
        .0
    };
    let with_spec = |value: &str| vec![format!("{}={value}", self_update_env::SPEC)];

    assert_eq!(
        helper(Some(false), with_spec(&spec)).self_update_helper().await.unwrap(),
        Some(SelfUpdateHelper { name: "ankor-self-update-0c0ffe".to_owned(), image: Some("ghcr.io/example/ankor-agent:2".to_owned()), running: false })
    );
    assert!(helper(Some(true), with_spec(&spec)).self_update_helper().await.unwrap().unwrap().running);
    assert_eq!(helper(Some(false), with_spec("{broken")).self_update_helper().await.unwrap().unwrap().image, None);
    assert_eq!(
        helper(Some(false), vec!["PATH=/usr/bin".to_owned(), format!("{}={spec}", self_update_env::SPEC)])
            .self_update_helper()
            .await
            .unwrap()
            .unwrap()
            .image
            .as_deref(),
        Some("ghcr.io/example/ankor-agent:2")
    );
    assert_eq!(
        helper(None, Vec::new()).self_update_helper().await.unwrap(),
        Some(SelfUpdateHelper { name: "ankor-self-update-0c0ffe".to_owned(), image: None, running: false })
    );
    assert_eq!(service(Daemon::default()).0.self_update_helper().await.unwrap(), None);
}

#[tokio::test]
async fn reads_the_running_update_container_or_an_agent_swaps_helper() {
    let update =
        |labels: &[(&str, &str)]| service(Daemon { containers: vec![container("u1", "/ankor-update-ff00ff", "running", labels)], ..Daemon::default() }).0;
    let full = [
        ("ankor.role", "update"),
        ("ankor.update-id", "00000000-0000-4000-8000-00000000a0a0"),
        ("ankor.update-mode", "verify"),
        ("ankor.update-automatic", "true"),
    ];

    assert_eq!(
        update(&full).running_update_container().await.unwrap(),
        Some(RunningUpdate::Cs2 {
            name: "ankor-update-ff00ff".to_owned(),
            kind: Some(UpdateKind { id: "00000000-0000-4000-8000-00000000a0a0".to_owned(), mode: Cs2UpdateMode::Verify, automatic: true })
        })
    );

    assert_eq!(
        update(&[full[0], full[1], full[2], ("ankor.update-automatic", "false")]).running_update_container().await.unwrap(),
        Some(RunningUpdate::Cs2 {
            name: "ankor-update-ff00ff".to_owned(),
            kind: Some(UpdateKind { id: "00000000-0000-4000-8000-00000000a0a0".to_owned(), mode: Cs2UpdateMode::Verify, automatic: false })
        })
    );

    for broken in [
        &full[..1],
        &[full[0], full[1], ("ankor.update-mode", "sideways"), full[3]][..],
        &[full[0], ("ankor.update-id", "NOT-A-UUID"), full[2], full[3]][..],
        &[full[0], full[1], full[2], ("ankor.update-automatic", "yes")][..],
    ] {
        assert_eq!(
            update(broken).running_update_container().await.unwrap(),
            Some(RunningUpdate::Cs2 { name: "ankor-update-ff00ff".to_owned(), kind: None }),
            "{broken:?}"
        );
    }

    let swap =
        service(Daemon { containers: vec![container("s1", "/ankor-self-update-0c0ffe", "running", &[("ankor.role", "self-update")])], ..Daemon::default() }).0;

    assert_eq!(swap.running_update_container().await.unwrap(), Some(RunningUpdate::AgentSwap { name: "ankor-self-update-0c0ffe".to_owned() }));
    assert_eq!(service(Daemon::default()).0.running_update_container().await.unwrap(), None);
}

fn lines_and_exit() -> (ContainerHooks, Arc<Mutex<Vec<String>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let (lines, exits) = (Arc::clone(&events), Arc::clone(&events));
    let hooks = ContainerHooks {
        on_line: Arc::new(move |line| lines.lock().unwrap().push(line)),
        on_exit: Arc::new(move |code| exits.lock().unwrap().push(format!("exit {code:?}"))),
    };

    (hooks, events)
}

/// Docker can answer the wait before the attach stream has given its last bytes, so the exit is
/// reported once the output has ended, after each of its lines.
#[tokio::test(start_paused = true)]
async fn reports_the_exit_once_the_output_has_ended() {
    let (docker, api) = service(Daemon { exit_code: Some(0), ..Daemon::default() });
    let (hooks, events) = lines_and_exit();

    docker.start_container(&spec(), hooks).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(events.lock().unwrap().is_empty(), "{:?}", events.lock().unwrap());

    let mut stream = api.daemon.lock().unwrap().streams.remove(0);

    stream.write_all(b"build 123\ntail").await.unwrap();
    drop(stream);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(*events.lock().unwrap(), ["build 123", "tail", "exit Some(0)"]);
}

/// A stream that never ends holds the exit back for a while only.
#[tokio::test(start_paused = true)]
async fn reports_the_exit_of_a_container_whose_output_never_ends_after_a_while() {
    let (docker, _api) = service(Daemon { exit_code: Some(3), ..Daemon::default() });
    let (hooks, events) = lines_and_exit();

    docker.start_container(&spec(), hooks).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1_900)).await;
    assert!(events.lock().unwrap().is_empty(), "{:?}", events.lock().unwrap());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*events.lock().unwrap(), ["exit Some(3)"]);
}

async fn lines_of(chunks: &[&[u8]], end: bool) -> Vec<String> {
    let (docker, api) = service(Daemon::default());
    let lines = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&lines);

    docker
        .start_container(&spec(), ContainerHooks { on_line: Arc::new(move |line| seen.lock().unwrap().push(line)), on_exit: Arc::new(|_| {}) })
        .await
        .unwrap();

    let mut stream = api.daemon.lock().unwrap().streams.remove(0);

    for chunk in chunks {
        stream.write_all(chunk).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    if end {
        drop(stream);
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    lines.lock().unwrap().clone()
}

#[tokio::test]
async fn passes_on_whole_lines_joining_one_split_across_chunks() {
    assert_eq!(lines_of(&[b"first li", b"ne\nsecond line\n"], false).await, ["first line", "second line"]);
}

#[tokio::test]
async fn keeps_a_character_whose_bytes_are_split_across_chunks() {
    let line = "Schlüssel\n".as_bytes();
    let cut = line.iter().position(|&byte| byte == 0xc3).unwrap() + 1;

    assert_eq!(lines_of(&[&line[..cut], &line[cut..]], false).await, ["Schlüssel"]);
}

#[tokio::test]
async fn passes_on_a_last_line_without_a_newline_when_the_output_ends() {
    assert_eq!(lines_of(&[b"done\nno newline"], true).await, ["done", "no newline"]);
}

#[tokio::test]
async fn keeps_only_the_tail_of_a_line_that_never_ends() {
    let long = "x".repeat(70 * 1024);
    let lines = lines_of(&[long.as_bytes(), b"END\n"], false).await;

    // The tail kept is 8 KiB, and at most one read more arrives before the line ends.
    assert_eq!(lines.len(), 1);
    assert!((8 * 1024 + 3..=24 * 1024 + 3).contains(&lines[0].len()) && lines[0].ends_with("END"), "{}", lines[0].len());

    let under = "y".repeat(60 * 1024);

    assert_eq!(lines_of(&[under.as_bytes(), b"\n"], false).await, [under]);

    let at_limit = "z".repeat(64 * 1024);

    assert_eq!(lines_of(&[at_limit.as_bytes(), b"\n"], false).await, [at_limit]);
}

#[tokio::test]
async fn stopping_takes_already_stopped_or_gone_as_stopped() {
    for status in [304, 404] {
        let docker = service(Daemon { stop_error: Some(api_error(status, "x")), ..Daemon::default() }).0;

        docker.stop_container("abc", 10).await.unwrap();
    }

    let docker = service(Daemon { stop_error: Some(api_error(500, "boom")), ..Daemon::default() }).0;

    assert_eq!(docker.stop_container("abc", 10).await.unwrap_err().to_string(), "boom");
}

#[tokio::test]
async fn image_and_container_collection() {
    let (docker, api) = service(Daemon {
        images: vec![
            serde_json::from_value(json!({ "Id": "sha256:a", "RepoTags": ["ghcr.io/x/agent:1"] })).unwrap(),
            serde_json::from_value(json!({ "Id": "sha256:b", "RepoTags": ["<none>:<none>"] })).unwrap(),
            serde_json::from_value(json!({ "Id": "sha256:c", "RepoTags": null })).unwrap(),
        ],
        ..Daemon::default()
    });
    let images = docker.list_project_images().await.unwrap();

    assert_eq!(images.iter().map(|image| (image.id.as_str(), image.repo_tags.len())).collect::<Vec<_>>(), [("sha256:a", 1), ("sha256:b", 0), ("sha256:c", 0)]);
    assert_eq!(api.recorded.lock().unwrap().image_lists, [Some(Filters::from([("label".to_owned(), vec!["ankor.image".to_owned()])]))]);

    for status in [404] {
        service(Daemon { remove_image_error: Some(api_error(status, "gone")), ..Daemon::default() }).0.remove_image("sha256:a").await.unwrap();
    }

    assert!(service(Daemon { remove_image_error: Some(api_error(409, "in use")), ..Daemon::default() }).0.remove_image("sha256:a").await.is_err());

    for status in [404, 409] {
        service(Daemon { remove_error: Some(api_error(status, "x")), ..Daemon::default() }).0.remove_container("abc", true).await.unwrap();
    }

    assert!(service(Daemon { remove_error: Some(api_error(500, "x")), ..Daemon::default() }).0.remove_container("abc", true).await.is_err());

    let baked = service(Daemon {
        image: Some(ImageInspect {
            id: Some("sha256:x".to_owned()),
            repo_digests: None,
            config: Some(ImageConfig { env: Some(vec!["A=1".to_owned()]), labels: None }),
        }),
        ..Daemon::default()
    })
    .0;
    let defaults = baked.image_defaults("sha256:x").await.unwrap();

    assert_eq!((defaults.env, defaults.labels.len()), (vec!["A=1".to_owned()], 0));
}

#[tokio::test]
async fn finds_the_agents_own_container_by_its_label() {
    let (docker, api) = service(Daemon { containers: vec![container("agent1", "/ankor-agent", "running", &[("ankor.role", "agent")])], ..Daemon::default() });
    let agent = docker.inspect_agent_container().await.unwrap().unwrap();

    assert_eq!((agent.id.as_str(), agent.name.as_str(), agent.image.as_str()), ("agent1", "agent1", "cs2"));
    assert_eq!(docker.running_agent_containers().await.unwrap(), ["ankor-agent"]);
    assert_eq!(api.recorded.lock().unwrap().listed[0], (false, Some(Filters::from([("label".to_owned(), vec!["ankor.role=agent".to_owned()])]))));
    assert_eq!(service(Daemon::default()).0.inspect_agent_container().await.unwrap(), None);
}

#[tokio::test]
async fn registry_has_compares_digests_and_answers_no_when_late() {
    let digests = |registry: Option<Result<String, String>>, repo: Option<Vec<String>>, delay: Duration| {
        service(Daemon {
            registry,
            registry_delay: delay,
            image: Some(ImageInspect { id: Some("sha256:local".to_owned()), repo_digests: repo, config: None }),
            ..Daemon::default()
        })
        .0
    };
    let pulled = Some(vec!["ghcr.io/x/agent@sha256:abc".to_owned()]);

    assert!(
        digests(Some(Ok("sha256:abc".to_owned())), pulled.clone(), Duration::ZERO)
            .registry_has("ghcr.io/x/agent:1", "sha256:local", Duration::from_secs(1))
            .await
    );
    assert!(
        !digests(Some(Ok("sha256:new".to_owned())), pulled.clone(), Duration::ZERO)
            .registry_has("ghcr.io/x/agent:1", "sha256:local", Duration::from_secs(1))
            .await
    );
    assert!(
        !digests(Some(Err("unreachable".to_owned())), pulled.clone(), Duration::ZERO)
            .registry_has("ghcr.io/x/agent:1", "sha256:local", Duration::from_secs(1))
            .await
    );
    assert!(!digests(Some(Ok("sha256:abc".to_owned())), None, Duration::ZERO).registry_has("ghcr.io/x/agent:1", "sha256:local", Duration::from_secs(1)).await);
    assert!(
        !digests(Some(Ok("sha256:abc".to_owned())), pulled, Duration::from_millis(200))
            .registry_has("ghcr.io/x/agent:1", "sha256:local", Duration::from_millis(20))
            .await
    );
}

#[tokio::test]
async fn reads_adoption_facts_off_labels_and_names_unnamed_containers_by_short_id() {
    let mut unnamed = container(
        "0123456789abcdef",
        "/x",
        "running",
        &[("ankor.role", "game"), ("ankor.slot", "a"), ("ankor.allocation", "alloc"), ("ankor.idle-shutdown", "600"), ("ankor.match", "m")],
    );

    unnamed.names.clear();

    let docker = service(Daemon {
        containers: vec![unnamed, container("g2", "/ankor-b", "exited", &[("ankor.role", "game"), ("ankor.idle-shutdown", "nonsense")])],
        ..Daemon::default()
    })
    .0;
    let adopted = docker.list_ankor_containers().await.unwrap();

    assert_eq!(adopted[0].name, "0123456789ab");
    assert_eq!(
        (
            adopted[0].slot_id.as_deref(),
            adopted[0].allocation_id.as_deref(),
            adopted[0].match_id.as_deref(),
            adopted[0].idle_shutdown_seconds,
            adopted[0].running
        ),
        (Some("a"), Some("alloc"), Some("m"), Some(600), true)
    );
    assert_eq!((adopted[1].idle_shutdown_seconds, adopted[1].running), (None, false));
}

#[tokio::test]
async fn an_exit_is_observed_even_when_attaching_fails() {
    let (docker, api) = service(Daemon { attach_error: Some(DockerError::Http("attach refused".to_owned())), exit_code: Some(3), ..Daemon::default() });
    let exited = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&exited);

    assert!(
        docker
            .follow_container(
                "game1",
                ContainerHooks { on_line: Arc::new(|_| {}), on_exit: Arc::new(move |code| *seen.lock().unwrap() = Some(code)) },
                WaitCondition::NotRunning
            )
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*exited.lock().unwrap(), Some(Some(3)));
    assert_eq!(api.daemon.lock().unwrap().wait_conditions, [(WaitCondition::NotRunning, false)]);
}

/// A follow that waits for the container's removal asks Docker for that, and still reports the exit
/// code. The attach is refused, so the exit does not wait for output to end.
#[tokio::test]
async fn follows_a_container_until_docker_removes_it_when_asked() {
    let (docker, api) = service(Daemon { attach_error: Some(DockerError::Http("attach refused".to_owned())), exit_code: Some(0), ..Daemon::default() });
    let exited = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&exited);

    assert!(
        docker
            .follow_container(
                "game1",
                ContainerHooks { on_line: Arc::new(|_| {}), on_exit: Arc::new(move |code| *seen.lock().unwrap() = Some(code)) },
                WaitCondition::Removed,
            )
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*exited.lock().unwrap(), Some(Some(0)));
    assert_eq!(api.daemon.lock().unwrap().wait_conditions, [(WaitCondition::Removed, false)]);
}

/// The wait is registered before the start, so a server that exits and is removed at once keeps
/// its exit code.
#[tokio::test]
async fn waits_for_the_exit_before_the_start_so_an_auto_removed_server_keeps_its_exit_code() {
    let (docker, api) = service(Daemon { removed_on_exit: true, exit_code: Some(2), ..Daemon::default() });
    let exited = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&exited);

    docker
        .start_container(&spec(), ContainerHooks { on_line: Arc::new(|_| {}), on_exit: Arc::new(move |code| *seen.lock().unwrap() = Some(code)) })
        .await
        .unwrap();
    // Its output ends as it exits.
    api.daemon.lock().unwrap().streams.clear();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(*exited.lock().unwrap(), Some(Some(2)));
    assert_eq!(api.daemon.lock().unwrap().wait_conditions, [(WaitCondition::NextExit, false)]);
}

#[tokio::test(start_paused = true)]
async fn keeps_watching_through_failed_waits_and_reports_a_gone_container_at_once() {
    let (docker, api) = service(Daemon { wait_errors: 3, exit_code: Some(7), ..Daemon::default() });
    let exited = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&exited);

    docker
        .follow_container(
            "game1",
            ContainerHooks { on_line: Arc::new(|_| {}), on_exit: Arc::new(move |code| *seen.lock().unwrap() = Some(code)) },
            WaitCondition::NotRunning,
        )
        .await
        .unwrap();
    // The waits fail at 0 s, 1 s and 3 s, each delay twice the one before, and the fourth answers at 7 s.
    tokio::time::sleep(Duration::from_millis(6_500)).await;
    assert_eq!((*exited.lock().unwrap(), api.daemon.lock().unwrap().waits), (None, 3));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(*exited.lock().unwrap(), Some(Some(7)));
    assert_eq!(api.daemon.lock().unwrap().waits, 4);

    let (gone, _) = service(Daemon { wait_gone: true, attach_error: Some(api_error(404, "No such container")), ..Daemon::default() });
    let missing = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&missing);

    assert!(
        gone.follow_container(
            "game1",
            ContainerHooks { on_line: Arc::new(|_| {}), on_exit: Arc::new(move |code| *seen.lock().unwrap() = Some(code)) },
            WaitCondition::NotRunning
        )
        .await
        .is_err()
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(*missing.lock().unwrap(), Some(None));
}

fn overlay_spec() -> StartContainerSpec {
    StartContainerSpec { name: "ankor-overlay-1a2b3c".to_owned(), image: "sha256:cs2".to_owned(), ..ankor_agent::testing::empty_spec() }
}

#[tokio::test]
async fn copies_out_of_a_container_it_never_starts_and_removes_it_after() {
    let (service, api) = service(Daemon::default());
    let target = std::path::Path::new("/srv/ankor/overlay/sha256_cs2.tmp");

    service.copy_from_image(&overlay_spec(), "/opt/ankor/overlay", target).await.unwrap();

    let recorded = api.recorded.lock().unwrap();

    assert_eq!(recorded.created.len(), 1);
    assert_eq!(recorded.created[0].0, "ankor-overlay-1a2b3c");
    assert_eq!(recorded.copies, [(CREATED_ID.to_owned(), "/opt/ankor/overlay".to_owned(), target.to_owned())]);
    assert_eq!(recorded.removed, [CREATED_ID]);
    assert_eq!(recorded.remove_options, [(true, false)]);
}

#[tokio::test(start_paused = true)]
async fn gives_up_a_copy_that_runs_past_the_limit_and_still_removes_the_container() {
    let (service, api) = service(Daemon { copy_hangs: true, ..Daemon::default() });
    let error = service.copy_from_image(&overlay_spec(), "/opt/ankor/overlay", std::path::Path::new("/tmp/x")).await.unwrap_err();

    assert!(matches!(error, DockerError::CopyTimedOut));
    assert_eq!(error.to_string(), format!("the copy took longer than {} s", ankor_agent::docker::COPY_TIMEOUT.as_secs()));
    assert_eq!(api.recorded.lock().unwrap().removed, [CREATED_ID]);
}

#[tokio::test]
async fn removes_the_container_when_the_copy_fails_and_passes_on_the_daemons_answer() {
    let (service, api) = service(Daemon { copy_error: Some(api_error(404, "Could not find the file /opt/ankor/overlay in container x")), ..Daemon::default() });
    let error = service.copy_from_image(&overlay_spec(), "/opt/ankor/overlay", std::path::Path::new("/tmp/x")).await.unwrap_err();

    assert_eq!(error.status(), Some(404));
    assert_eq!(api.recorded.lock().unwrap().removed, [CREATED_ID]);
}

#[tokio::test]
async fn names_the_overlays_running_containers_mount_once_each() {
    let overlay = |id: &str, state: &str, image_id: &str| container(id, &format!("/{id}"), state, &[("ankor.role", "match"), ("ankor.overlay", image_id)]);
    let (docker, api) = service(Daemon {
        containers: vec![
            overlay("g1", "running", "sha256:b"),
            overlay("g2", "running", "sha256:a"),
            overlay("g3", "running", "sha256:a"),
            overlay("g4", "running", ""),
            overlay("g5", "exited", "sha256:c"),
            container("g6", "/g6", "running", &[("ankor.role", "match")]),
        ],
        ..Daemon::default()
    });

    assert_eq!(docker.running_overlay_image_ids().await.unwrap(), ["sha256:a", "sha256:b"]);
    assert_eq!(api.recorded.lock().unwrap().listed, [(false, Some(Filters::from([("label".to_owned(), vec!["ankor.overlay".to_owned()])])))]);
}

#[tokio::test]
async fn names_the_images_every_container_uses_in_any_state() {
    let with_image = |id: &str, state: &str, image_id: &str| -> ListedContainer {
        serde_json::from_value(json!({ "Id": id, "Names": [format!("/{id}")], "ImageID": image_id, "State": state })).unwrap()
    };
    let (docker, api) = service(Daemon {
        containers: vec![with_image("a", "running", "sha256:one"), with_image("b", "exited", "sha256:two"), with_image("c", "created", "")],
        ..Daemon::default()
    });

    assert_eq!(docker.images_in_use().await.unwrap(), ["sha256:one", "sha256:two"]);
    assert_eq!(api.recorded.lock().unwrap().listed, [(true, None)]);
}

#[tokio::test]
async fn passes_on_a_failed_pull_and_reads_the_digests_an_image_was_pulled_as() {
    let refused = service(Daemon { pull_error: Some(api_error(404, "manifest unknown")), ..Daemon::default() }).0;

    assert_eq!(refused.pull_image("ghcr.io/example/ankor-cs2:9").await.unwrap_err().to_string(), "manifest unknown");
    service(Daemon::default()).0.pull_image("ghcr.io/example/ankor-cs2:1").await.unwrap();

    let digests = vec!["ghcr.io/example/ankor-cs2@sha256:aa".to_owned(), "ghcr.io/mirror/ankor-cs2@sha256:bb".to_owned()];
    let pulled = service(Daemon {
        image: Some(ImageInspect { id: Some("sha256:x".to_owned()), repo_digests: Some(digests.clone()), config: None }),
        ..Daemon::default()
    })
    .0;

    assert_eq!(pulled.repo_digests("sha256:x").await.unwrap(), digests);
    assert!(service(Daemon { image_error: Some(api_error(404, "No such image")), ..Daemon::default() }).0.repo_digests("sha256:x").await.is_err());
}
