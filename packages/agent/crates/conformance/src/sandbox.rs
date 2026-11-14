//! The Docker host the agent runs on: a Docker-in-Docker container, so that the node's paths
//! (`/srv/ankor`, the Docker socket) are the same for the agent and the daemon, and so that nothing
//! the agent does reaches the workstation's own containers.
//!
//! The sandbox and the suite share a Docker network with no way out. The suite joins it under the
//! names of every host it fakes, so the agent and its game servers reach the fakes by name, and
//! nothing else at all.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::stubs::STUB_HOSTS;
use crate::transcript::{Recorder, channels};

/// The label the agent marks its containers with, and the roles the sandbox looks for. The suite
/// sees the agent from outside, so it keeps its own copy of these, as `node-setup.sh` does.
const ROLE_LABEL: &str = "ankor.role";
const AGENT_ROLE: &str = "agent";
const UPDATE_ROLE: &str = "update";

/// The Docker-in-Docker container.
pub const DIND_NAME: &str = "ankor-conformance-dind";
/// Pinned, as the checks' image pins the Docker CLI that drives it.
pub const DIND_IMAGE: &str = "docker:29.9-dind";
/// The network the sandbox and the suite share, which reaches nothing else.
const NETWORK: &str = "ankor-conformance";
/// The fake controller's host name, as `agent.yml` names it.
pub const CONTROLLER_HOST: &str = "controller.conformance.test";
macro_rules! data_dir {
    () => {
        "/srv/ankor"
    };
}
/// The node's data directory, the same path on the host and in the agent.
pub const DATA_DIR: &str = data_dir!();
/// The game volume the agent mounts into its game and probe containers.
pub const GAME_VOLUME: &str = "ankor_cs2_data";
macro_rules! install_dir {
    () => {
        "/home/steam/cs2-dedicated"
    };
}
/// Where the fake cs2 image keeps the install, as the agent mounts the game volume into it.
pub const INSTALL_DIR: &str = install_dir!();
macro_rules! game_ankor_dir {
    () => {
        "game/csgo/ankor"
    };
}
/// The directory in the install where the agent leaves a server's match and the game records.
const GAME_ANKOR_DIR: &str = game_ankor_dir!();
/// Where the suite writes what a fake update does, inside the update container. The update container
/// runs on the bridge network, which the sandbox keeps from the fake controller, so the suite reaches it
/// through `docker exec` instead.
pub const UPDATE_INSTRUCTION: &str = "/tmp/conformance-update.json";

macro_rules! cs2_app_id {
    () => {
        "730"
    };
}
/// CS2's Steam app id.
pub const CS2_APP_ID: &str = cs2_app_id!();
/// Steam's app manifest, relative to the install.
pub const APP_MANIFEST: &str = concat!("steamapps/appmanifest_", cs2_app_id!(), ".acf");
/// The image's state in the install, such as the record of the installed build, relative to the install.
pub const ANKOR_STATE_DIR: &str = ".ankor";
/// The user and group the game runs as, which owns the install and the data directory.
const STEAM_OWNER: &str = "1000:1000";

/// Steam's app manifest for a full install of `build`, as the agent reads it.
#[must_use]
pub fn app_manifest(build: &str) -> String {
    format!("\"AppState\"\n{{\n\t\"appid\"\t\t\"{CS2_APP_ID}\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"buildid\"\t\t\"{build}\"\n}}\n")
}

/// The node's `agent.yml`, in the data directory.
const AGENT_YML: &str = concat!(data_dir!(), "/agent.yml");
/// The Docker socket, in the sandbox and on the workstation.
const DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// The name of the agent container, as `node-setup.sh` names it.
pub const AGENT_CONTAINER: &str = "ankor-agent";
/// Where the test CA sits in the sandbox and in the agent container.
pub const CA_DIR: &str = "/conformance";
/// The cs2 image reference `agent.yml` names; the fake image is tagged as it in the sandbox.
pub const CS2_IMAGE: &str = "ghcr.io/ankorcs/ankor-cs2:1";
/// The agent image reference `agent.yml` names; the agent under test is tagged as it.
pub const AGENT_IMAGE: &str = "ghcr.io/ankorcs/ankor-agent:0";

/// The `docker` CLI against one daemon. The sandbox's daemon listens only on its Unix socket, so
/// the suite runs the sandbox's own CLI through `docker exec`; no daemon API is open on a port.
#[derive(Clone)]
pub struct Docker {
    sandboxed: bool,
}

impl Docker {
    /// The workstation's daemon.
    pub fn outer() -> Self {
        Self { sandboxed: false }
    }

    fn inner() -> Self {
        Self { sandboxed: true }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("docker");
        if self.sandboxed {
            command.args(["exec", "-i", DIND_NAME, "docker"]);
        }
        command.args(args).kill_on_drop(true);
        command
    }

    /// Runs `docker args` with the file at `path` on stdin, as `docker load` reads an image.
    pub async fn run_with_file(&self, args: &[&str], path: &Path) -> Result<String> {
        let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let output = self.command(args).stdin(Stdio::from(file)).output().await.with_context(|| format!("docker {}", args.join(" ")))?;
        if !output.status.success() {
            bail!("docker {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs `docker args`, returning stdout; fails with stderr when it exits non-zero.
    pub async fn run(&self, args: &[&str]) -> Result<String> {
        let output = self.command(args).stdin(Stdio::null()).output().await.with_context(|| format!("docker {}", args.join(" ")))?;
        if !output.status.success() {
            bail!("docker {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Like [`Docker::run`], with `input` on stdin.
    pub async fn run_with_input(&self, args: &[&str], input: &[u8]) -> Result<String> {
        let mut child = self.command(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input).await?;
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            bail!("docker {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Whether `docker args` exits zero, ignoring its output.
    pub async fn succeeds(&self, args: &[&str]) -> bool {
        self.command(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().await.is_ok_and(|status| status.success())
    }
}

/// Runs a shell script in the sandbox container, as root.
async fn sandbox_sh(script: &str) -> Result<String> {
    Docker::outer().run(&["exec", DIND_NAME, "sh", "-c", script]).await
}

/// The images the suite needs on the workstation's daemon.
pub struct Images {
    pub fake_cs2: String,
    pub agent: String,
}

/// The running sandbox.
pub struct Sandbox {
    pub docker: Docker,
    work_dir: PathBuf,
    loaded_agent: Option<String>,
    events: Option<Child>,
}

impl Sandbox {
    /// Starts a fresh sandbox on a network of its own, which this suite's container joins under the
    /// names of the hosts it fakes. The suite runs in a container (`scripts/conformance.sh`).
    pub async fn start(images: &Images, ca_pem: &str, work_dir: &Path) -> Result<Self> {
        let outer = Docker::outer();
        let suite = std::fs::read_to_string("/etc/hostname").context("the suite runs in a container; run it with scripts/conformance.sh")?;
        let suite = suite.trim();
        // A run that crashed left its sandbox, and its volume with the loaded images.
        let _ = outer.run(&["rm", "-f", "-v", DIND_NAME]).await;
        let _ = outer.run(&["network", "disconnect", "-f", NETWORK, suite]).await;
        let _ = outer.run(&["network", "rm", NETWORK]).await;
        outer.run(&["network", "create", "--internal", NETWORK]).await.context("create the sandbox's network")?;
        let mut join = vec!["network", "connect"];
        for host in STUB_HOSTS.iter().chain([&CONTROLLER_HOST]) {
            join.extend(["--alias", host]);
        }
        join.extend([NETWORK, suite]);
        outer.run(&join).await.context("join the sandbox's network")?;
        outer
            .run(&[
                "run",
                "--pull=never",
                "-d",
                "--name",
                DIND_NAME,
                "--privileged",
                "--network",
                NETWORK,
                DIND_IMAGE,
                // Named, so the image's entrypoint does not add its defaults, which open the daemon
                // on tcp://0.0.0.0:2375 without TLS; the suite reaches it only through `docker exec`.
                "dockerd",
                &format!("--host=unix://{DOCKER_SOCKET}"),
                // The daemon's info lines (shims, network joins) say nothing about the scenario;
                // steps.log does.
                "--log-level=warn",
            ])
            .await
            .with_context(|| format!("start the Docker-in-Docker sandbox; pull {DIND_IMAGE} first"))?;
        let docker = Docker::inner();
        let mut ready = false;
        for _ in 0..120 {
            if docker.succeeds(&["version"]).await {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !ready {
            bail!("the sandbox daemon did not come up within a minute");
        }

        Docker::outer()
            .run_with_input(
                &["exec", "-i", DIND_NAME, "sh", "-c", &format!("mkdir -p {CA_DIR} && cat > {CA_DIR}/ca.pem && chmod 644 {CA_DIR}/ca.pem")],
                ca_pem.as_bytes(),
            )
            .await?;

        let sandbox = Self { docker, work_dir: work_dir.to_owned(), loaded_agent: None, events: None };
        let tar = sandbox.save(&images.fake_cs2).await?;
        sandbox.docker.run_with_file(&["load"], &tar).await.context("load the fake cs2 image")?;
        sandbox.docker.run(&["tag", &images.fake_cs2, CS2_IMAGE]).await?;
        Ok(sandbox)
    }

    /// Saves `image` from the workstation's daemon to a tarball in the work directory.
    async fn save(&self, image: &str) -> Result<PathBuf> {
        let file = self.work_dir.join(format!("{}.tar", image.replace(['/', ':'], "_")));
        Docker::outer().run(&["save", "-o", &file.to_string_lossy(), image]).await.with_context(|| format!("save {image}; build it first"))?;
        Ok(file)
    }

    /// Makes `image` the only agent image in the sandbox, tagged as [`AGENT_IMAGE`].
    pub async fn use_agent_image(&mut self, image: &str) -> Result<()> {
        if self.loaded_agent.as_deref() != Some(image) {
            let listed = self.docker.run(&["images", "--filter", "label=ankor.image=agent", "--format", "{{.ID}}"]).await?;
            for id in listed.lines().filter(|line| !line.is_empty()) {
                let _ = self.docker.run(&["rmi", "-f", id]).await;
            }
            let tar = self.save(image).await?;
            self.docker.run_with_file(&["load"], &tar).await.with_context(|| format!("load {image}"))?;
            self.loaded_agent = Some(image.to_owned());
        }
        self.docker.run(&["tag", image, AGENT_IMAGE]).await?;
        Ok(())
    }

    /// Removes every container and volume, and lays out a fresh node: the data directory with
    /// `agent_yml`, and a game volume holding an installed build.
    pub async fn reset_node(&self, agent_yml: &str) -> Result<()> {
        self.stop_events().await;
        let containers = self.docker.run(&["ps", "-aq"]).await?;
        let ids: Vec<&str> = containers.lines().filter(|line| !line.is_empty()).collect();
        if !ids.is_empty() {
            let mut args = vec!["rm", "-f", "-v"];
            args.extend(ids);
            self.docker.run(&args).await?;
        }
        let volumes = self.docker.run(&["volume", "ls", "-q"]).await?;
        for volume in volumes.lines().filter(|line| !line.is_empty()) {
            self.docker.run(&["volume", "rm", "-f", volume]).await?;
        }
        self.docker.run(&["volume", "create", GAME_VOLUME]).await?;
        let seed = format!(
            "set -e
             cd {INSTALL_DIR}
             mkdir -p steamapps {ANKOR_STATE_DIR} game/csgo/addons game/csgo/cfg/ankor game/csgo/cfg/node {GAME_ANKOR_DIR} game/bin/linuxsteamrt64/steamapps
             printf '// replaced per slot by the agent\\n' > game/csgo/cfg/ankor_launch.cfg
             printf '%s' '{manifest}' > {APP_MANIFEST}
             chown -R {STEAM_OWNER} .",
            manifest = app_manifest(crate::stubs::PUBLIC_BUILD)
        );
        let volume = format!("{GAME_VOLUME}:{INSTALL_DIR}");
        self.docker
            .run(&["run", "--rm", "--pull=never", "--network", "none", "--user", "0", "--entrypoint", "/bin/sh", "-v", &volume, CS2_IMAGE, "-c", &seed])
            .await?;
        let dirs = ["", "/slots", "/cfg", "/swiftlys2", "/swiftlys2/plugins", "/swiftlys2/configs", "/swiftlys2/configs/plugins"]
            .map(|dir| format!("{DATA_DIR}{dir}"))
            .join(" ");
        sandbox_sh(&format!(
            "set -e
             rm -rf {DATA_DIR}
             install -d -m 0755 {dirs}
             chown {STEAM_OWNER} {dirs}"
        ))
        .await?;
        Docker::outer()
            .run_with_input(
                &["exec", "-i", DIND_NAME, "sh", "-c", &format!("cat > {AGENT_YML} && chown {STEAM_OWNER} {AGENT_YML} && chmod 600 {AGENT_YML}")],
                agent_yml.as_bytes(),
            )
            .await?;
        Ok(())
    }

    /// Follows the sandbox daemon's container events into `rec`, until the next reset.
    pub async fn follow_events(&mut self, rec: Recorder) -> Result<()> {
        self.stop_events().await;
        let mut child =
            self.docker.command(&["events", "--filter", "type=container", "--format", "{{json .}}"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let stdout = child.stdout.take().context("events stdout")?;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(summary) = summarise_event(&event) {
                    rec.record(channels::DOCKER, summary);
                }
            }
        });
        self.events = Some(child);
        // `docker events` reports only what happens after it subscribed.
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(())
    }

    async fn stop_events(&self) {
        if let Some(id) = self.events.as_ref().and_then(Child::id) {
            let _ = Command::new("kill").arg(id.to_string()).status().await;
        }
    }

    /// The node's data directory: every path with its type, mode, owner and size, and the
    /// contents of its small text files.
    pub async fn files(&self) -> Result<Value> {
        let listing = sandbox_sh(&format!(
            "cd {DATA_DIR} && find . -mindepth 1 | sort | while read -r path; do
               stat -c '%F|%a|%u:%g|%s|%n' \"$path\"
               case \"$path\" in *.sqlite*|*.dem|*.dll|*.so) continue;; esac
               if [ -f \"$path\" ] && [ \"$(stat -c %s \"$path\")\" -le 16384 ]; then printf 'CONTENT|'; base64 -w0 \"$path\"; echo; fi
             done"
        ))
        .await?;
        let mut files = Map::new();
        let mut last: Option<String> = None;
        for line in listing.lines() {
            if let Some(encoded) = line.strip_prefix("CONTENT|") {
                let content = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap_or_default();
                if let Some(entry) = last.as_ref().and_then(|path| files.get_mut(path)) {
                    entry["content"] = json!(String::from_utf8_lossy(&content));
                }
                continue;
            }
            let parts: Vec<&str> = line.splitn(5, '|').collect();
            let [kind, mode, owner, size, path] = parts.as_slice() else { continue };
            let path = path.trim_start_matches("./").to_owned();
            let mut entry = json!({ "type": kind, "mode": mode, "owner": owner });
            if kind.starts_with("regular") {
                entry["size"] = json!(size.parse::<u64>().unwrap_or(0));
            }
            files.insert(path.clone(), entry);
            last = Some(path);
        }
        Ok(Value::Object(files))
    }

    /// Every container the agent created, as `docker inspect` describes the parts the agent decides.
    /// Writes `instruction` into the running update container, where the fake cs2 image's update
    /// mode waits for it, once the agent has started that container.
    pub async fn instruct_update(&self, instruction: &Value) -> Result<()> {
        for _ in 0..120 {
            let ids = self.docker.run(&["ps", "-q", "--filter", &format!("label={ROLE_LABEL}={UPDATE_ROLE}"), "--filter", "status=running"]).await?;
            if let Some(id) = ids.lines().find(|line| !line.is_empty()) {
                let write = format!("cat > {UPDATE_INSTRUCTION}.tmp && mv {UPDATE_INSTRUCTION}.tmp {UPDATE_INSTRUCTION}");
                self.docker.run_with_input(&["exec", "-i", id, "sh", "-c", &write], instruction.to_string().as_bytes()).await?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        bail!("no update container ran within 30 s")
    }

    pub async fn containers(&self) -> Result<Value> {
        let ids = self.docker.run(&["ps", "-a", "--filter", &format!("label={ROLE_LABEL}"), "--format", "{{.ID}}"]).await?;
        let mut containers = Vec::new();
        for id in ids.lines().filter(|line| !line.is_empty()) {
            let inspected: Value = serde_json::from_str(&self.docker.run(&["inspect", id]).await?)?;
            let Some(container) = inspected.get(0) else { continue };
            if container.pointer(&format!("/Config/Labels/{ROLE_LABEL}")).and_then(Value::as_str) == Some(AGENT_ROLE) {
                continue;
            }
            containers.push(summarise_container(container));
        }
        containers.sort_by_key(|container| container["name"].as_str().unwrap_or_default().to_owned());
        Ok(Value::Array(containers))
    }

    /// Starts the agent under test with `node-setup.sh`'s run command, plus the test CA.
    pub async fn start_agent(&self) -> Result<()> {
        let data = format!("{DATA_DIR}:{DATA_DIR}");
        let install = format!("{GAME_VOLUME}:/cs2-install:ro");
        let ca = format!("{CA_DIR}:{CA_DIR}:ro");
        let ssl_ca = format!("SSL_CERT_FILE={CA_DIR}/ca.pem");
        self.docker
            .run(&[
                "run",
                "--pull=never",
                "-d",
                "--name",
                AGENT_CONTAINER,
                "--label",
                &format!("{ROLE_LABEL}={AGENT_ROLE}"),
                "--cpu-shares",
                "2598",
                "--restart",
                "unless-stopped",
                "--log-driver",
                "local",
                "--log-opt",
                "mode=non-blocking",
                "--log-opt",
                "max-buffer-size=4m",
                "--network",
                "host",
                "-v",
                &format!("{DOCKER_SOCKET}:{DOCKER_SOCKET}"),
                "-v",
                "/proc/meminfo:/host/proc/meminfo:ro",
                "-v",
                "/sys/fs/cgroup:/host/sys/fs/cgroup:ro",
                "-v",
                "/etc/os-release:/host/etc/os-release:ro",
                "-v",
                &data,
                "-v",
                &install,
                "-v",
                &ca,
                "-e",
                &ssl_ca,
                AGENT_IMAGE,
            ])
            .await
            .context("start the agent")?;
        Ok(())
    }

    /// Stops the agent with SIGTERM, keeps its log in `log_file` and removes its container.
    pub async fn stop_agent(&self, log_file: &Path) -> Result<()> {
        let _ = self.docker.run(&["stop", "-t", "20", AGENT_CONTAINER]).await;
        let logs = self.docker.command(&["logs", AGENT_CONTAINER]).output().await?;
        let mut text = String::from_utf8_lossy(&logs.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&logs.stderr));
        let mut existing = tokio::fs::read_to_string(log_file).await.unwrap_or_default();
        existing.push_str(&text);
        existing.push_str("---- agent stopped ----\n");
        tokio::fs::write(log_file, existing).await?;
        self.docker.run(&["rm", "-f", AGENT_CONTAINER]).await?;
        Ok(())
    }

    /// Removes the sandbox container and everything in it.
    pub async fn remove(mut self) {
        self.stop_events().await;
        self.events = None;
        let outer = Docker::outer();
        let _ = outer.run(&["rm", "-f", "-v", DIND_NAME]).await;
        if let Ok(suite) = std::fs::read_to_string("/etc/hostname") {
            let _ = outer.run(&["network", "disconnect", "-f", NETWORK, suite.trim()]).await;
        }
        let _ = outer.run(&["network", "rm", NETWORK]).await;
    }
}

fn summarise_event(event: &Value) -> Option<Value> {
    let action = event.get("Action").and_then(Value::as_str)?;
    let wanted = ["create", "start", "die", "destroy", "kill", "stop"];
    let action_name = action.split(':').next().unwrap_or(action);
    if !wanted.contains(&action_name) {
        return None;
    }
    let attributes = event.pointer("/Actor/Attributes")?;
    if attributes.get(ROLE_LABEL).and_then(Value::as_str) == Some(AGENT_ROLE) || attributes.get(ROLE_LABEL).is_none() {
        return None;
    }
    Some(json!({
        "action": action_name,
        "name": attributes.get("name"),
        "image": attributes.get("image"),
        "role": attributes.get(ROLE_LABEL),
        "slot": attributes.get("ankor.slot"),
        "allocation": attributes.get("ankor.allocation"),
        "exit_code": attributes.get("exitCode"),
        "signal": attributes.get("signal"),
    }))
}

fn summarise_container(container: &Value) -> Value {
    let pick = |pointer: &str| container.pointer(pointer).cloned().unwrap_or(Value::Null);
    let mut env: Vec<Value> = container.pointer("/Config/Env").and_then(Value::as_array).cloned().unwrap_or_default();
    env.sort_by_key(|value| value.as_str().unwrap_or_default().to_owned());
    let mut mounts: Vec<Value> = container
        .get("Mounts")
        .and_then(Value::as_array)
        .map(|mounts| {
            mounts
                .iter()
                .map(|mount| json!({ "type": mount.get("Type"), "name": mount.get("Name"), "source": mount.get("Source"), "destination": mount.get("Destination"), "rw": mount.get("RW") }))
                .collect()
        })
        .unwrap_or_default();
    mounts.sort_by_key(|mount| mount["destination"].as_str().unwrap_or_default().to_owned());
    json!({
        "name": pick("/Name"),
        "image": pick("/Config/Image"),
        "user": pick("/Config/User"),
        "entrypoint": pick("/Config/Entrypoint"),
        "cmd": pick("/Config/Cmd"),
        "env": env,
        "labels": pick("/Config/Labels"),
        "stop_signal": pick("/Config/StopSignal"),
        "stop_timeout": pick("/Config/StopTimeout"),
        "network_mode": pick("/HostConfig/NetworkMode"),
        "binds": pick("/HostConfig/Binds"),
        "host_mounts": pick("/HostConfig/Mounts"),
        "tmpfs": pick("/HostConfig/Tmpfs"),
        "cpu_shares": pick("/HostConfig/CpuShares"),
        "memory": pick("/HostConfig/Memory"),
        "pids_limit": pick("/HostConfig/PidsLimit"),
        "cap_drop": pick("/HostConfig/CapDrop"),
        "cap_add": pick("/HostConfig/CapAdd"),
        "security_opt": pick("/HostConfig/SecurityOpt"),
        "readonly_rootfs": pick("/HostConfig/ReadonlyRootfs"),
        "restart_policy": pick("/HostConfig/RestartPolicy"),
        "auto_remove": pick("/HostConfig/AutoRemove"),
        "log_config": pick("/HostConfig/LogConfig"),
        "init": pick("/HostConfig/Init"),
        "ulimits": pick("/HostConfig/Ulimits"),
        "mounts": mounts,
        "state": pick("/State/Status"),
        "exit_code": pick("/State/ExitCode"),
    })
}
