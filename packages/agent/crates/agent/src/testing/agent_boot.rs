//! The agent as `run_agent` wires it, on a fake Docker host against a [`ControllerPeer`]. The node
//! has one slot, installed at build 19000, which Steam calls the newest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ankor_protocol::constants::{PROTOCOL_HEADER, PROTOCOL_VERSION};
use ankor_protocol::types::AgentToControllerMessage;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::controller_peer::{ControllerPeer, PeerOptions};
use super::{FakeDocker, lock, plugin_message, until};
use crate::agent::{AgentHost, RunningAgent, run_agent};
use crate::container_env::env_value;
use crate::container_spec::{ContainerRole, labels};
use crate::contract::constants::ankor_env;
use crate::contract::types::AgentToPluginMessage;
use crate::cs2_auto_update::{SteamAnswer, SteamFetch};
use crate::cs2_install::{CS2_APP_ID, INSTALLED_BUILD_KEY, installed_record_path};
use crate::docker::{DockerPort, InspectedContainer, StartContainerSpec};
use crate::host_layout::{DATA_VOLUME, INSTALL_DIR, OVERLAY_ADDONS};
use crate::logger::{Entry, Logger};
use crate::node_log::{NodeLog, wall_clock};
use crate::overlay::{COMPLETE_MARKER, overlay_root};
use crate::public_ip::PublicIpResolver;
use crate::registry_tags::{Method, RegistryAnswer, RegistryError, RegistryFetch};

pub const TOKEN: &str = "runagenttoken0000000000000000001";
pub const GSLT_A: &str = super::fixtures::GSLT_A;

#[must_use]
pub fn agent_image_id() -> String {
    format!("sha256:{}", "a".repeat(64))
}

#[must_use]
pub fn installed_json() -> String {
    installed_record_path()
}

/// # Panics
///
/// When it does not mount it.
#[must_use]
pub fn volume_target(spec: &StartContainerSpec) -> String {
    spec.volumes.iter().find(|mount| mount.volume == DATA_VOLUME).map(|mount| mount.target.clone()).expect("a container on the game volume")
}

/// Where `container_path`, under the game volume mounted at `target`, lands in `volume`, the host
/// folder standing in for it.
///
/// # Panics
///
/// When the path is not on the game volume.
#[must_use]
pub fn on_volume(volume: &Path, container_path: &str, target: &str) -> PathBuf {
    let rest = container_path.strip_prefix(target).expect("a path on the game volume");

    PathBuf::from(format!("{}{rest}", volume.display()))
}

/// Writes the version file as the image's update mode does once steamcmd is done.
///
/// # Panics
///
/// When the file cannot be written.
pub fn write_installed(file: &Path, build_id: &str) {
    std::fs::create_dir_all(file.parent().expect("a folder")).expect("the folder");
    let record = json!({ INSTALLED_BUILD_KEY: build_id, "installed_at": "2026-10-01T10:00:00Z" });

    std::fs::write(file, record.to_string()).expect("the version file");
}

/// The newest public CS2 build, as Steam answers it and the update downloads it.
#[derive(Clone)]
pub struct Steam(pub Arc<Mutex<String>>);

#[async_trait]
impl SteamFetch for Steam {
    async fn fetch(&self, _url: &str) -> Result<SteamAnswer, crate::cs2_auto_update::Cs2AutoUpdateError> {
        let build = lock(&self.0).clone();
        let body = json!({ "status": "success", "data": { CS2_APP_ID.to_string(): { "depots": { "branches": { "public": { "buildid": build } } } } } });

        Ok(SteamAnswer { status: 200, body: body.to_string().into_bytes() })
    }
}

struct FixedAddress;

#[async_trait]
impl PublicIpResolver for FixedAddress {
    async fn resolve(&self) -> Result<String, crate::public_ip::PublicIpError> {
        Ok(json!({ "ip": "203.0.113.7", "country": "DK" }).to_string())
    }
}

/// A registry that has nothing.
pub struct NoRegistry;

#[async_trait]
impl RegistryFetch for NoRegistry {
    async fn fetch(&self, _url: &str, _method: Method, _headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError> {
        Ok(RegistryAnswer { status: 404, headers: BTreeMap::new(), body: b"{}".to_vec() })
    }
}

/// Sets up the host before the agent boots on it.
pub type Prepare = Box<dyn FnOnce(&Arc<FakeDocker>, &Path)>;

#[derive(Default)]
pub struct BootOptions {
    /// Whether the peer welcomes each hello itself; it does when `None`.
    pub welcome: Option<bool>,
    /// Boots on a game volume with no install yet; one with build 19000 when false.
    pub uninstalled: bool,
    /// The node's slots as agent.yml lists them; one slot `a` when `None`.
    pub slots: Option<Value>,
    /// The Docker host to boot on; a fresh one when `None`.
    pub docker: Option<Arc<FakeDocker>>,
    /// The data folder of an earlier boot, as after an agent restart.
    pub data_dir: Option<Arc<tempfile::TempDir>>,
    /// `auto_update` in agent.yml; left out when `None`.
    pub auto_update: Option<bool>,
    /// `cs2_image` and `agent_image` in agent.yml; local references without a registry when `None`.
    pub images: (Option<String>, Option<String>),
    /// `official_images_only` in agent.yml; false when `None`, since the default images are local
    /// stand-ins.
    pub official_images_only: Option<bool>,
    /// The image registries; every request answers 404 when `None`.
    pub registry: Option<Arc<dyn RegistryFetch>>,
    /// Sets up the host before the agent boots on it.
    pub prepare: Option<Prepare>,
}

pub struct Booted {
    /// The agent as `run_agent` returned it.
    pub agent: RunningAgent,
    /// The fake Docker host the agent runs on.
    pub docker: Arc<FakeDocker>,
    /// The controller the agent connects to.
    pub peer: ControllerPeer,
    /// The newest public build Steam reports, which a test may change.
    pub steam: Steam,
    /// The host folder standing in for the game volume.
    pub volume: tempfile::TempDir,
    /// The agent's data folder, which a later boot can reuse.
    pub data_dir: Arc<tempfile::TempDir>,
    /// The `agent.yml` the agent reads.
    pub config_path: PathBuf,
    /// Every line the agent logged, prefix included.
    pub lines: Arc<Mutex<Vec<String>>>,
}

impl Booted {
    #[must_use]
    pub fn frames(&self, matches: impl Fn(&AgentToControllerMessage) -> bool) -> Vec<Value> {
        self.peer.received_where(matches)
    }
}

/// # Errors
///
/// Fails as `run_agent` does.
///
/// # Panics
///
/// When the fixture's files cannot be written.
pub async fn try_boot(options: BootOptions) -> Result<Booted, (crate::failure::BoxError, Arc<FakeDocker>)> {
    let peer = ControllerPeer::start(PeerOptions { token: Some(TOKEN.to_owned()), welcome: options.welcome.unwrap_or(true), ..PeerOptions::default() }).await;
    let steam = Steam(Arc::new(Mutex::new("19000".to_owned())));
    let volume = tempfile::tempdir().expect("a volume folder");
    let data_dir = options.data_dir.unwrap_or_else(|| Arc::new(tempfile::tempdir().expect("a data folder")));
    let config_path = data_dir.path().join("agent.yml");
    // `new`, not `default`, because a stopped container of the fake exits with 143 as Docker's would.
    let docker = match options.docker {
        Some(docker) => docker,
        None => FakeDocker::new(),
    };
    let mut config = json!({
        "token": TOKEN, "controller": { "host": format!("127.0.0.1:{}", peer.port), "tls": false }, "pinned_ip": "127.0.0.1",
        "cs2_image": options.images.0.unwrap_or_else(|| "ankor-cs2:test".to_owned()),
        "agent_image": options.images.1.unwrap_or_else(|| "ankor-agent:test".to_owned()),
        "slots": options.slots.unwrap_or_else(|| json!([{ "id": "a", "port": 27015, "tv_port": 27016, "gslt": GSLT_A }])),
    });

    if let Some(auto_update) = options.auto_update {
        config["auto_update"] = json!(auto_update);
    }

    config["official_images_only"] = json!(options.official_images_only.unwrap_or(false));

    std::fs::write(&config_path, config.to_string()).expect("agent.yml");
    lock(&docker.default_image_id).get_or_insert_with(|| format!("sha256:{}", "deadbeef".repeat(8)));
    *lock(&docker.agent_container) = Some(InspectedContainer {
        id: "agent-container".to_owned(),
        name: "ankor-agent".to_owned(),
        image_id: agent_image_id(),
        image: "ankor-agent:test".to_owned(),
        config: json!({}),
        host_config: json!({}),
    });

    // A successful update writes the version file with the build it downloaded.
    let (update_volume, latest) = (volume.path().to_owned(), Arc::clone(&steam.0));

    lock(&docker.helper_runs).insert(
        ContainerRole::Update,
        Arc::new(move |spec: &StartContainerSpec| {
            write_installed(&on_volume(&update_volume, &installed_json(), &volume_target(spec)), &lock(&latest));
            (Vec::new(), Some(0))
        }),
    );
    // The overlay store makes the mountpoints itself, so the copy only has to bring `addons/`.
    *lock(&docker.on_copy) = Some(Arc::new(|path: &str, target: &Path| {
        let folder = Path::new(path).file_name().map(|name| target.join(name)).unwrap_or_default();

        std::fs::create_dir_all(folder.join(OVERLAY_ADDONS)).expect("the overlay");
    }));
    if !options.uninstalled {
        write_installed(&on_volume(volume.path(), &installed_json(), INSTALL_DIR), "19000");
    }

    if let Some(prepare) = options.prepare {
        prepare(&docker, data_dir.path());
    }

    let stop = tokio_util::sync::CancellationToken::new();
    let node_log = NodeLog::new(wall_clock(), stop.child_token());
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let (sink, recorded) = (node_log.clone(), Arc::clone(&lines));
    let log = Logger::new(
        "agent",
        Some(Arc::new(move |entry: Entry| {
            lock(&recorded).push(entry.message.clone());
            sink.agent_line(&entry);
        })),
    );
    let host = AgentHost {
        stop,
        config_path: config_path.clone(),
        data_dir: data_dir.path().to_owned(),
        cs2_install: volume.path().to_owned(),
        docker: Arc::clone(&docker) as Arc<dyn DockerPort>,
        public_ip: Arc::new(FixedAddress),
        relay_port: 0,
        steam_logon: Arc::new(|_| Box::pin(async { Ok(1) })),
        steam_fetch: Arc::new(steam.clone()),
        registry_fetch: options.registry.unwrap_or_else(|| Arc::new(NoRegistry)),
        node_log,
        log,
    };
    let agent = run_agent(host).await.map_err(|error| (error, Arc::clone(&docker)))?;
    let root = overlay_root(data_dir.path());

    // Boot prepares the overlay without waiting for it. It must be done before the data folder is deleted.
    until("the overlay", || {
        std::fs::read_dir(&root).is_ok_and(|entries| entries.filter_map(Result::ok).any(|entry| entry.path().join(COMPLETE_MARKER).exists()))
    })
    .await;

    Ok(Booted { agent, docker, peer, steam, volume, data_dir, config_path, lines })
}

/// # Panics
///
/// When it does not boot.
pub async fn boot(options: BootOptions) -> Booted {
    // As the agent's entrypoint does, so ANKOR_LOG_LEVEL=debug prints the agent's debug lines.
    crate::logger::console_level_from_env();

    match try_boot(options).await {
        Ok(booted) => booted,
        Err((error, _)) => panic!("the agent did not boot: {error}"),
    }
}

/// The relay URL the game container of `allocation_id` was given.
///
/// # Panics
///
/// When no game container has it.
#[must_use]
pub fn plugin_url(docker: &FakeDocker, allocation_id: &str) -> String {
    docker
        .specs()
        .iter()
        .find(|spec| spec.labels.get(labels::ALLOCATION).map(String::as_str) == Some(allocation_id))
        .and_then(|spec| env_value(spec.env.iter().map(String::as_str), ankor_env::WS_URL).map(str::to_owned))
        .unwrap_or_else(|| panic!("no game container for {allocation_id}"))
}

type PluginSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A game's plugin, dialling the relay URL its container was given.
pub struct GamePlugin {
    sink: futures_util::stream::SplitSink<PluginSocket, tokio_tungstenite::tungstenite::Message>,
    pub messages: Arc<Mutex<Vec<Value>>>,
}

impl GamePlugin {
    /// # Panics
    ///
    /// When the relay refuses the plugin.
    pub async fn connect(docker: &FakeDocker, allocation_id: &str) -> Self {
        use futures_util::StreamExt as _;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

        let mut request = plugin_url(docker, allocation_id).into_client_request().expect("a relay URL");

        request.headers_mut().insert(PROTOCOL_HEADER, reqwest::header::HeaderValue::from_static(PROTOCOL_VERSION));

        let (socket, _) = tokio_tungstenite::connect_async(request).await.expect("the relay takes the plugin");
        let (sink, mut stream) = socket.split();
        let messages: Arc<Mutex<Vec<Value>>> = Arc::default();
        let seen = Arc::clone(&messages);

        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                if let tokio_tungstenite::tungstenite::Message::Text(text) = message
                    && let Ok(value) = serde_json::from_str(&text)
                {
                    lock(&seen).push(value);
                }
            }
        });

        Self { sink, messages }
    }

    /// # Panics
    ///
    /// When the socket is gone.
    pub async fn send(&mut self, frame: &Value) {
        use futures_util::SinkExt as _;

        self.sink.send(tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into())).await.expect("the relay socket");
    }

    /// The frames the relay sent whose message `matches`, such as
    /// `|message| matches!(message, AgentToPluginMessage::Exec(_))`.
    #[must_use]
    pub fn received_where(&self, matches: impl Fn(&AgentToPluginMessage) -> bool) -> Vec<Value> {
        lock(&self.messages).iter().filter(|frame| plugin_message(frame).is_some_and(|message| matches(&message))).cloned().collect()
    }
}
