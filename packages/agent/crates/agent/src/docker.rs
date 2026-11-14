//! Containers as the agent sees them: started and followed, listed by their labels, and the
//! host's images.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use ankor_protocol::helpers::from_wire_name;
use ankor_protocol::schemas::s_update_id;
use ankor_protocol::types::Cs2UpdateMode;
use ankor_protocol::validate::safe_parse;
use async_trait::async_trait;
use hyper::StatusCode;
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::sync::oneshot;

use crate::container_env::env_value;
use crate::container_spec::{ContainerRole, IMAGE_LABEL, SWEPT_ROLES, labels, log_config, role_filter};
use crate::contract::constants::self_update_env;
use crate::docker_api::{
    DOCKER_API_VERSION, DockerApiPort, DockerError, Filters, ListedContainer, MIN_DOCKER_ENGINE, PendingExit, WaitCondition, container_state, pulled_at,
};
use crate::host_layout::{BindMount, TmpfsMount, VolumeMount};
use crate::log_markers::split_log_chunk;
use crate::logger::Logger;
use crate::self_update::spec_keys;

/// The addon overlay is the copy the agent makes, and it is a few megabytes. A copy this slow has
/// hung, and it would hold the maintenance guard until a restart.
pub const COPY_TIMEOUT: Duration = Duration::from_mins(5);

/// How long `update_agent` waits on the registry before it pulls to find out. A lookup takes
/// about a second warm and about five cold.
pub(crate) const REGISTRY_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

const EXIT_WATCH_MIN: Duration = Duration::from_secs(1);
const EXIT_WATCH_MAX: Duration = Duration::from_mins(1);

/// Docker can answer the wait before the container's output ends. A stream still open after this
/// no longer holds the exit back.
const OUTPUT_END_WAIT: Duration = Duration::from_secs(2);

/// A line with no newline is cut to its tail past this, so it cannot eat memory.
const MAX_PENDING_LINE: usize = 64 * 1024;
const KEPT_PENDING_LINE: usize = 8 * 1024;

pub(crate) type LineHook = Arc<dyn Fn(String) + Send + Sync>;
pub(crate) type ExitHook = Arc<dyn Fn(Option<i64>) + Send + Sync>;

#[derive(Clone)]
pub struct ContainerHooks {
    /// Receives each line of the container's output.
    pub on_line: LineHook,
    /// The exit code, or `None` when the container is gone without one.
    pub on_exit: ExitHook,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkMode {
    Host,
    Bridge,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StartContainerSpec {
    /// The container's name on the host.
    pub name: String,
    /// The image reference the container is created from.
    pub image: String,
    /// Environment variables as `KEY=value` strings.
    pub env: Vec<String>,
    /// The labels the agent later finds and adopts the container by.
    pub labels: BTreeMap<String, String>,
    /// Whether the container shares the host's network or joins the bridge.
    pub network_mode: NetworkMode,
    /// The Linux capabilities removed from the container.
    pub cap_drop: Vec<String>,
    /// Docker security options for the container.
    pub security_opt: Vec<String>,
    /// The process limit, with none set when it is not positive.
    pub pids_limit: i64,
    /// Bytes, or `None` for no memory cap.
    pub memory_bytes: Option<i64>,
    /// The relative CPU weight, or `None` for Docker's default.
    pub cpu_shares: Option<i64>,
    /// Whether Docker removes the container once it exits.
    pub auto_remove: bool,
    /// Named volumes mounted into the container.
    pub volumes: Vec<VolumeMount>,
    /// Host paths mounted into the container.
    pub binds: Vec<BindMount>,
    /// In-memory mounts.
    pub tmpfs: Vec<TmpfsMount>,
    /// Replaces the image's command when set.
    pub cmd: Option<Vec<String>>,
    /// Replaces the image's entrypoint, for helpers that must not boot a server.
    pub entrypoint: Option<Vec<String>>,
}

impl StartContainerSpec {
    #[must_use]
    pub fn payload(&self) -> Value {
        let mut mounts: Vec<Value> = Vec::new();

        mounts
            .extend(self.volumes.iter().map(|mount| json!({ "Type": "volume", "Source": mount.volume, "Target": mount.target, "ReadOnly": mount.read_only })));
        mounts.extend(self.binds.iter().map(|mount| json!({ "Type": "bind", "Source": mount.source, "Target": mount.target, "ReadOnly": mount.read_only })));
        mounts.extend(
            self.tmpfs
                .iter()
                .map(|mount| json!({ "Type": "tmpfs", "Target": mount.target, "TmpfsOptions": { "SizeBytes": mount.size_bytes, "Mode": mount.mode } })),
        );

        let mut host = Map::new();

        host.insert("AutoRemove".to_owned(), json!(self.auto_remove));
        host.insert("NetworkMode".to_owned(), json!(if self.network_mode == NetworkMode::Host { "host" } else { "bridge" }));

        if !self.cap_drop.is_empty() {
            host.insert("CapDrop".to_owned(), json!(self.cap_drop));
        }

        if !self.security_opt.is_empty() {
            host.insert("SecurityOpt".to_owned(), json!(self.security_opt));
        }

        if self.pids_limit > 0 {
            host.insert("PidsLimit".to_owned(), json!(self.pids_limit));
        }

        if let Some(memory) = self.memory_bytes {
            host.insert("Memory".to_owned(), json!(memory));
        }

        if let Some(shares) = self.cpu_shares {
            host.insert("CpuShares".to_owned(), json!(shares));
        }

        host.insert(spec_keys::LOG_CONFIG.to_owned(), log_config());
        host.insert("Mounts".to_owned(), Value::Array(mounts));

        let mut payload = Map::new();

        payload.insert(spec_keys::IMAGE.to_owned(), json!(self.image));
        payload.insert(spec_keys::ENV.to_owned(), json!(self.env));
        payload.insert(spec_keys::LABELS.to_owned(), json!(self.labels));

        if let Some(cmd) = &self.cmd {
            payload.insert(spec_keys::CMD.to_owned(), json!(cmd));
        }

        if let Some(entrypoint) = &self.entrypoint {
            payload.insert("Entrypoint".to_owned(), json!(entrypoint));
        }

        payload.insert("Tty".to_owned(), json!(true));
        payload.insert("OpenStdin".to_owned(), json!(false));
        payload.insert("AttachStdin".to_owned(), json!(false));
        payload.insert("AttachStdout".to_owned(), json!(true));
        payload.insert("AttachStderr".to_owned(), json!(true));
        payload.insert(spec_keys::HOST_CONFIG.to_owned(), Value::Object(host));

        Value::Object(payload)
    }
}

/// A container the agent made, as its labels describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptedContainer {
    /// The Docker container id.
    pub id: String,
    /// The name without Docker's leading slash, or the short id when it has none.
    pub name: String,
    /// The slot the container runs in.
    pub slot_id: Option<String>,
    /// The allocation the container serves.
    pub allocation_id: Option<String>,
    /// The match the container runs, when it was launched with one.
    pub match_id: Option<String>,
    /// The idle time before the server shuts down, or `None` when unset or not positive.
    pub idle_shutdown_seconds: Option<i64>,
    /// The container's role, or `None` when the label is unknown.
    pub role: Option<ContainerRole>,
    /// Whether the container was running when listed.
    pub running: bool,
    /// The key its plugin dials the relay with.
    pub relay_key: Option<String>,
    /// The token it runs on.
    pub gslt: Option<String>,
}

/// Enough of a container to rebuild it on another image.
#[derive(Clone, Debug, PartialEq)]
pub struct InspectedContainer {
    /// The Docker container id.
    pub id: String,
    /// The name without Docker's leading slash.
    pub name: String,
    /// The id of the image the container runs.
    pub image_id: String,
    /// The reference it was created from.
    pub image: String,
    /// Docker's `Config` object for the container.
    pub config: Value,
    /// Docker's `HostConfig` object, or an empty object when Docker returned none.
    pub host_config: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageSummary {
    pub id: String,
    pub repo_tags: Vec<String>,
}

/// What an image gives every container made of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageDefaults {
    pub env: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

/// The self-update helper on the host, and the image it swaps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfUpdateHelper {
    /// The helper container's name.
    pub name: String,
    /// From the replacement spec it was given; `None` when that cannot be read.
    pub image: Option<String>,
    /// Whether the helper still runs, which means its swap has not finished.
    pub running: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateKind {
    pub id: String,
    pub mode: Cs2UpdateMode,
    pub automatic: bool,
}

/// A container that holds the maintenance guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunningUpdate {
    /// A CS2 update; `kind` is `None` when a label is missing or invalid.
    Cs2 { name: String, kind: Option<UpdateKind> },
    /// An agent swap's helper still proving its replacement.
    AgentSwap { name: String },
}

/// Lets the agent's components run against a fake Docker.
#[async_trait]
pub trait DockerPort: Send + Sync {
    async fn require_supported_engine(&self) -> Result<String, DockerError>;
    async fn start_container(&self, spec: &StartContainerSpec, hooks: ContainerHooks) -> Result<String, DockerError>;
    /// # Errors
    ///
    /// Fails when the container's output cannot be attached; its exit, which `until` decides the
    /// moment of, still reaches `hooks`.
    async fn follow_container(&self, id: &str, hooks: ContainerHooks, until: WaitCondition) -> Result<(), DockerError>;
    async fn stop_container(&self, id: &str, timeout_seconds: i64) -> Result<(), DockerError>;
    async fn remove_container(&self, id: &str, force: bool) -> Result<(), DockerError>;
    async fn list_ankor_containers(&self) -> Result<Vec<AdoptedContainer>, DockerError>;
    async fn sweep_helper_containers(&self) -> Result<Vec<String>, DockerError>;
    async fn self_update_helper(&self) -> Result<Option<SelfUpdateHelper>, DockerError>;
    async fn clear_self_update_helpers(&self) -> Result<(), DockerError>;
    async fn running_overlay_image_ids(&self) -> Result<Vec<String>, DockerError>;
    async fn running_update_container(&self) -> Result<Option<RunningUpdate>, DockerError>;
    async fn image_id(&self, image: &str) -> Result<Option<String>, DockerError>;
    async fn image_defaults(&self, id: &str) -> Result<ImageDefaults, DockerError>;
    async fn running_agent_containers(&self) -> Result<Vec<String>, DockerError>;
    async fn inspect_agent_container(&self) -> Result<Option<InspectedContainer>, DockerError>;
    async fn run_detached(&self, name: &str, spec: &Value) -> Result<String, DockerError>;
    async fn list_project_images(&self) -> Result<Vec<ImageSummary>, DockerError>;
    async fn images_in_use(&self) -> Result<Vec<String>, DockerError>;
    async fn remove_image(&self, id: &str) -> Result<(), DockerError>;
    async fn pull_image(&self, image: &str) -> Result<(), DockerError>;
    /// Whether the local image was pulled at the registry's current digest for `image`. False when
    /// either side cannot be read within `within`.
    async fn registry_has(&self, image: &str, image_id: &str, within: Duration) -> bool;
    async fn repo_digests(&self, image_id: &str) -> Result<Vec<String>, DockerError>;
    /// Unpacks `path` into `target` as `<target>/<last component of path>`. The container is never
    /// started, and it is removed whether or not the copy worked.
    async fn copy_from_image(&self, spec: &StartContainerSpec, path: &str, target: &std::path::Path) -> Result<(), DockerError>;
}

pub struct DockerService {
    api: Arc<dyn DockerApiPort>,
    log: Logger,
}

impl DockerService {
    #[must_use]
    pub fn new(api: impl DockerApiPort + 'static, log: Logger) -> Self {
        Self { api: Arc::new(api), log }
    }

    /// A create that got no answer may still make its container, which would keep the name from
    /// the next create, so the name is removed.
    async fn create(&self, name: &str, spec: &Value) -> Result<String, DockerError> {
        let created = self.api.create_container(name, spec).await;

        if matches!(created, Err(DockerError::NoAnswer(_))) {
            self.discard(name, name).await;
        }

        created
    }

    async fn discard(&self, id: &str, name: &str) {
        if let Err(error) = self.remove_container(id, true).await {
            self.log.warn(format!("could not remove container {name}: {error}"));
        }
    }

    fn label_filter(label: &str) -> Filters {
        Filters::from([("label".to_owned(), vec![label.to_owned()])])
    }

    async fn list_self_update_helpers(&self) -> Result<Vec<ListedContainer>, DockerError> {
        let filter = Self::label_filter(&role_filter(ContainerRole::SelfUpdate));

        self.api.list_containers(true, Some(&filter)).await
    }

    async fn remove_leftovers(&self, listed: Vec<ListedContainer>) -> Vec<String> {
        let mut removed = Vec::new();

        for info in listed {
            if ![container_state::CREATED, container_state::EXITED, container_state::DEAD].contains(&info.state.as_str()) {
                continue;
            }

            let name = name_of(&info);
            let role = label(&info, labels::ROLE).unwrap_or_default();

            match self.remove_container(&info.id, true).await {
                Ok(()) => {
                    self.log.info(format!("removed leftover {role} container {name} ({})", info.state));
                    removed.push(name);
                }
                Err(error) => self.log.warn(format!("could not remove leftover {role} container {name}: {error}")),
            }
        }

        removed
    }

    /// `ended` is dropped once the output has ended and its last line is passed on, or the attach failed.
    async fn pipe(&self, id: &str, hooks: &ContainerHooks, ended: oneshot::Sender<()>) -> Result<(), DockerError> {
        let mut stream = self.api.attach_container(id).await?;
        let on_line = Arc::clone(&hooks.on_line);
        let log = self.log.clone();
        let short = id.chars().take(12).collect::<String>();

        tokio::spawn(async move {
            let _ended = ended;
            let mut pending: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; 16 * 1024];

            loop {
                match stream.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(read) => {
                        pending.extend_from_slice(chunk.get(..read).unwrap_or_default());

                        if let Some(newline) = pending.iter().rposition(|&byte| byte == b'\n') {
                            let complete: Vec<u8> = pending.drain(..=newline).collect();

                            for line in split_log_chunk(&String::from_utf8_lossy(&complete)) {
                                on_line(line);
                            }
                        } else if pending.len() > MAX_PENDING_LINE {
                            pending.drain(..pending.len() - KEPT_PENDING_LINE);
                        }
                    }
                    // A broken stream's unfinished line is dropped, because only an ended stream completes it.
                    Err(error) => {
                        log.warn(format!("log stream error on {short}: {error}"));
                        return;
                    }
                }
            }

            // A container's last line need not end in a newline.
            for line in split_log_chunk(&String::from_utf8_lossy(&pending)) {
                on_line(line);
            }
        });

        Ok(())
    }

    /// A failed wait cannot establish that the container stopped, so it is retried with a growing
    /// delay. The exit waits for `output_ended`, so every output line comes before it.
    fn watch_exit(&self, id: &str, hooks: &ContainerHooks, registered: Option<PendingExit>, output_ended: oneshot::Receiver<()>, until: WaitCondition) {
        let api = Arc::clone(&self.api);
        let log = self.log.clone();
        let id = id.to_owned();
        let on_exit = Arc::clone(&hooks.on_exit);

        tokio::spawn(async move {
            let mut retry = EXIT_WATCH_MIN;
            let mut registered = registered;

            let code = loop {
                let pending = match registered.take() {
                    Some(pending) => Ok(pending),
                    None => api.wait_container(&id, until).await,
                };
                let waited = match pending {
                    Ok(pending) => pending.await,
                    Err(error) => Err(error),
                };

                match waited {
                    Ok(code) => break code,
                    Err(error) if error.has_status(&[StatusCode::NOT_FOUND]) => break None,
                    Err(error) => {
                        log.warn(format!("container exit is unconfirmed for {id}: {error}"));
                        tokio::time::sleep(retry).await;
                        retry = (retry * 2).min(EXIT_WATCH_MAX);
                    }
                }
            };

            let _ = tokio::time::timeout(OUTPUT_END_WAIT, output_ended).await;
            on_exit(code);
        });
    }
}

fn label(info: &ListedContainer, key: &str) -> Option<String> {
    info.labels.as_ref().and_then(|labels| labels.get(key)).cloned()
}

/// Docker lists and inspects names with a leading slash.
fn docker_name(name: &str) -> String {
    name.trim_start_matches('/').to_owned()
}

/// A container without a name goes by its short id.
fn name_of(info: &ListedContainer) -> String {
    info.names.first().map_or_else(|| info.id.chars().take(12).collect(), |name| docker_name(name))
}

fn positive_int(value: Option<&str>) -> Option<i64> {
    value.and_then(|value| value.parse::<i64>().ok()).filter(|&value| value > 0)
}

fn spec_image(spec: Option<&str>) -> Option<String> {
    serde_json::from_str::<Value>(spec?).ok()?.get(spec_keys::IMAGE)?.as_str().map(str::to_owned)
}

#[async_trait]
impl DockerPort for DockerService {
    async fn require_supported_engine(&self) -> Result<String, DockerError> {
        match self.api.engine().await {
            Ok((version, _)) => Ok(version),
            Err(DockerError::Api { status: 400, message }) => Err(DockerError::Http(format!(
                "Docker Engine is too old for this agent ({message}); it needs Docker Engine {MIN_DOCKER_ENGINE} or newer, API {DOCKER_API_VERSION}"
            ))),
            Err(error) => Err(error),
        }
    }

    async fn start_container(&self, spec: &StartContainerSpec, hooks: ContainerHooks) -> Result<String, DockerError> {
        let id = self.create(&spec.name, &spec.payload()).await?;
        let (ended, output_ended) = oneshot::channel();
        // Attached and waited on before the start, so no early line is missed, and a container that
        // exits and is removed at once still gives its exit code.
        let started = async {
            self.pipe(&id, &hooks, ended).await?;

            let exit = self.api.wait_container(&id, WaitCondition::NextExit).await?;

            self.api.start_container(&id).await?;
            Ok::<_, DockerError>(exit)
        };

        match started.await {
            Ok(exit) => {
                self.watch_exit(&id, &hooks, Some(exit), output_ended, WaitCondition::NotRunning);
                Ok(id)
            }
            Err(error) => {
                self.discard(&id, &spec.name).await;
                Err(error)
            }
        }
    }

    async fn follow_container(&self, id: &str, hooks: ContainerHooks, until: WaitCondition) -> Result<(), DockerError> {
        let (ended, output_ended) = oneshot::channel();

        self.watch_exit(id, &hooks, None, output_ended, until);
        self.pipe(id, &hooks, ended).await
    }

    /// Already stopped (304) and already gone (404) count as stopped.
    async fn stop_container(&self, id: &str, timeout_seconds: i64) -> Result<(), DockerError> {
        match self.api.stop_container(id, timeout_seconds).await {
            Err(error) if !error.has_status(&[StatusCode::NOT_MODIFIED, StatusCode::NOT_FOUND]) => Err(error),
            _ => Ok(()),
        }
    }

    /// Gone (404) and on its way out (409, auto-removed) count as removed.
    async fn remove_container(&self, id: &str, force: bool) -> Result<(), DockerError> {
        match self.api.remove_container(id, force, false).await {
            Err(error) if !error.has_status(&[StatusCode::NOT_FOUND, StatusCode::CONFLICT]) => Err(error),
            _ => Ok(()),
        }
    }

    async fn list_ankor_containers(&self) -> Result<Vec<AdoptedContainer>, DockerError> {
        let listed = self.api.list_containers(true, Some(&Self::label_filter(labels::ROLE))).await?;

        Ok(listed
            .iter()
            .map(|info| AdoptedContainer {
                id: info.id.clone(),
                name: name_of(info),
                slot_id: label(info, labels::SLOT),
                allocation_id: label(info, labels::ALLOCATION),
                match_id: label(info, labels::MATCH),
                relay_key: label(info, labels::RELAY_KEY),
                gslt: label(info, labels::GSLT),
                idle_shutdown_seconds: positive_int(label(info, labels::IDLE_SHUTDOWN).as_deref()),
                role: label(info, labels::ROLE).and_then(|role| ContainerRole::parse(&role)),
                running: info.state == container_state::RUNNING,
            })
            .collect())
    }

    /// Only helpers that exited or never started are removed.
    async fn sweep_helper_containers(&self) -> Result<Vec<String>, DockerError> {
        let listed = self.api.list_containers(true, Some(&Self::label_filter(labels::ROLE))).await?;
        // Docker ANDs label filters, so the roles are picked out here.
        let helpers = listed
            .into_iter()
            .filter(|info| label(info, labels::ROLE).and_then(|role| ContainerRole::parse(&role)).is_some_and(|role| SWEPT_ROLES.contains(&role)))
            .collect();

        Ok(self.remove_leftovers(helpers).await)
    }

    /// The last swap's helper is kept after it exits, as the swap's record.
    async fn self_update_helper(&self) -> Result<Option<SelfUpdateHelper>, DockerError> {
        let Some(info) = self.list_self_update_helpers().await?.into_iter().next() else {
            return Ok(None);
        };
        let raw = self.api.inspect_container(&info.id).await?;
        let spec = env_value(raw.config.get(spec_keys::ENV).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str), self_update_env::SPEC);

        Ok(Some(SelfUpdateHelper { name: name_of(&info), image: spec_image(spec), running: raw.state.is_some_and(|state| state.running) }))
    }

    /// Running helpers are kept. Clearing the rest leaves the next swap's helper as the only one.
    async fn clear_self_update_helpers(&self) -> Result<(), DockerError> {
        for info in self.list_self_update_helpers().await? {
            if info.state == container_state::RUNNING || info.state == container_state::RESTARTING {
                continue;
            }

            self.remove_container(&info.id, true).await?;
            self.log.info(format!("removed the previous swap's helper {}", name_of(&info)));
        }

        Ok(())
    }

    async fn running_overlay_image_ids(&self) -> Result<Vec<String>, DockerError> {
        let listed = self.api.list_containers(false, Some(&Self::label_filter(labels::OVERLAY))).await?;
        let ids: BTreeSet<String> = listed.iter().filter_map(|info| label(info, labels::OVERLAY)).filter(|id| !id.is_empty()).collect();

        Ok(ids.into_iter().collect())
    }

    async fn running_update_container(&self) -> Result<Option<RunningUpdate>, DockerError> {
        let role = |role: ContainerRole| Self::label_filter(&role_filter(role));
        let Some(info) = self.api.list_containers(false, Some(&role(ContainerRole::Update))).await?.into_iter().next() else {
            let helper = self.api.list_containers(false, Some(&role(ContainerRole::SelfUpdate))).await?.into_iter().next();

            return Ok(helper.map(|helper| RunningUpdate::AgentSwap { name: name_of(&helper) }));
        };
        let id = label(&info, labels::UPDATE_ID).filter(|id| safe_parse(s_update_id, json!(id)).is_ok());
        // Written and read by its wire name.
        let mode = label(&info, labels::UPDATE_MODE).and_then(|mode| from_wire_name::<Cs2UpdateMode>(&mode));
        // Written as `bool::to_string` writes it.
        let automatic = label(&info, labels::UPDATE_AUTOMATIC).and_then(|flag| flag.parse::<bool>().ok());
        let kind = match (id, mode, automatic) {
            (Some(id), Some(mode), Some(automatic)) => Some(UpdateKind { id, mode, automatic }),
            _ => None,
        };

        Ok(Some(RunningUpdate::Cs2 { name: name_of(&info), kind }))
    }

    /// `None` for an image the host lacks.
    async fn image_id(&self, image: &str) -> Result<Option<String>, DockerError> {
        match self.api.inspect_image(image).await {
            Ok(info) => match info.id.filter(|id| !id.is_empty()) {
                Some(id) => Ok(Some(id)),
                None => Err(DockerError::Http(format!("Docker returned no image id for {image}"))),
            },
            Err(error) if error.has_status(&[StatusCode::NOT_FOUND]) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn image_defaults(&self, id: &str) -> Result<ImageDefaults, DockerError> {
        let config = self.api.inspect_image(id).await?.config.unwrap_or_default();

        Ok(ImageDefaults { env: config.env.unwrap_or_default(), labels: config.labels.unwrap_or_default() })
    }

    /// This agent's container is among them.
    async fn running_agent_containers(&self) -> Result<Vec<String>, DockerError> {
        Ok(self.api.list_containers(false, Some(&Self::label_filter(&role_filter(ContainerRole::Agent)))).await?.iter().map(name_of).collect())
    }

    /// Only a running one, because the stopped, replaced container keeps the label while the helper
    /// proves its successor.
    async fn inspect_agent_container(&self) -> Result<Option<InspectedContainer>, DockerError> {
        let Some(info) = self.api.list_containers(false, Some(&Self::label_filter(&role_filter(ContainerRole::Agent)))).await?.into_iter().next() else {
            return Ok(None);
        };
        let raw = self.api.inspect_container(&info.id).await?;

        Ok(Some(InspectedContainer {
            id: raw.id,
            name: docker_name(&raw.name),
            image_id: raw.image,
            image: raw.config.get(spec_keys::IMAGE).and_then(Value::as_str).unwrap_or_default().to_owned(),
            config: raw.config,
            host_config: raw.host_config.unwrap_or_else(|| json!({})),
        }))
    }

    async fn run_detached(&self, name: &str, spec: &Value) -> Result<String, DockerError> {
        let id = self.create(name, spec).await?;

        if let Err(error) = self.api.start_container(&id).await {
            // A start that got no answer may take effect at any time. A running self-update helper
            // is mid-swap, and removing it could leave the node with no agent. A running one is
            // therefore kept, and the next swap clears one that never ran.
            if matches!(error, DockerError::NoAnswer(_)) {
                if self.api.inspect_container(&id).await.is_ok_and(|inspected| inspected.state.is_some_and(|state| state.running)) {
                    self.log.warn(format!("Docker gave no answer to starting {name}, which runs; keeping it"));
                    return Ok(id);
                }

                return Err(error);
            }

            self.discard(&id, name).await;
            return Err(error);
        }

        Ok(id)
    }

    /// Tagged or dangling, picked out by Ankor's image label.
    async fn list_project_images(&self) -> Result<Vec<ImageSummary>, DockerError> {
        let listed = self.api.list_images(Some(&Self::label_filter(IMAGE_LABEL))).await?;

        Ok(listed
            .into_iter()
            .map(|info| ImageSummary { id: info.id, repo_tags: info.repo_tags.unwrap_or_default().into_iter().filter(|tag| tag != "<none>:<none>").collect() })
            .collect())
    }

    /// Image ids any container uses, in any state, foreign ones included.
    async fn images_in_use(&self) -> Result<Vec<String>, DockerError> {
        Ok(self.api.list_containers(true, None).await?.into_iter().map(|info| info.image_id).filter(|id| !id.is_empty()).collect())
    }

    async fn remove_image(&self, id: &str) -> Result<(), DockerError> {
        match self.api.remove_image(id).await {
            Err(error) if error.status() != Some(404) => Err(error),
            _ => Ok(()),
        }
    }

    async fn pull_image(&self, image: &str) -> Result<(), DockerError> {
        self.api.pull_image(image).await
    }

    async fn registry_has(&self, image: &str, image_id: &str, within: Duration) -> bool {
        let lookup = async {
            let (digest, local) = tokio::join!(self.api.registry_digest(image), self.api.inspect_image(image_id));

            match (digest, local) {
                (Ok(digest), Ok(local)) => pulled_at(&local.repo_digests.unwrap_or_default(), &digest),
                _ => false,
            }
        };

        tokio::time::timeout(within, lookup).await.unwrap_or(false)
    }

    async fn repo_digests(&self, image_id: &str) -> Result<Vec<String>, DockerError> {
        Ok(self.api.inspect_image(image_id).await?.repo_digests.unwrap_or_default())
    }

    async fn copy_from_image(&self, spec: &StartContainerSpec, path: &str, target: &std::path::Path) -> Result<(), DockerError> {
        let id = self.create(&spec.name, &spec.payload()).await?;
        let copied = tokio::time::timeout(COPY_TIMEOUT, self.api.copy_from_container(&id, path, target)).await.unwrap_or(Err(DockerError::CopyTimedOut));

        self.discard(&id, &spec.name).await;
        copied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_leaves_out_what_is_unset() {
        let spec = StartContainerSpec {
            name: "n".to_owned(),
            image: "i".to_owned(),
            env: vec!["A=1".to_owned()],
            labels: BTreeMap::new(),
            network_mode: NetworkMode::Host,
            cap_drop: Vec::new(),
            security_opt: Vec::new(),
            pids_limit: 0,
            memory_bytes: None,
            cpu_shares: Some(26_193),
            auto_remove: true,
            volumes: Vec::new(),
            binds: vec![BindMount { source: "/a".to_owned(), target: "/b".to_owned(), read_only: true }],
            tmpfs: Vec::new(),
            cmd: None,
            entrypoint: Some(vec!["/bin/sh".to_owned()]),
        };
        let payload = spec.payload();
        let host = &payload["HostConfig"];

        assert!(host.get("CapDrop").is_none() && host.get("PidsLimit").is_none() && host.get("Memory").is_none());
        assert_eq!(host["CpuShares"], 26_193);
        assert_eq!(host["LogConfig"], json!({ "Type": "local", "Config": { "mode": "non-blocking", "max-buffer-size": "4m" } }));
        assert_eq!(host["Mounts"], json!([{ "Type": "bind", "Source": "/a", "Target": "/b", "ReadOnly": true }]));
        assert!(payload.get("Cmd").is_none());
        assert_eq!(payload["Tty"], true);
    }

    #[test]
    fn helper_specs_name_their_image() {
        assert_eq!(spec_image(Some(r#"{"Image":"ghcr.io/x:1"}"#)).as_deref(), Some("ghcr.io/x:1"));
        assert_eq!(spec_image(Some("nonsense")), None);
        assert_eq!(spec_image(None), None);
        assert_eq!(positive_int(Some("600")), Some(600));
        assert_eq!(positive_int(Some("0")), None);
    }
}
