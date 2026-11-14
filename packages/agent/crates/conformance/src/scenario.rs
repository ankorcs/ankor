//! What a scenario can do: configure and boot the node, talk to the agent as the controller, drive the fake
//! plugins and take snapshots of the node.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use ankor_protocol::types::{AgentReply, AgentToControllerMessage, Side, SidesMode, SlotState};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::controller::{Controller, DEMO_PATH};
use crate::sandbox::{CONTROLLER_HOST, CS2_IMAGE, Sandbox};
use crate::stubs::Stubs;
use crate::transcript::{Recorder, channels, controller_entry};

/// A frame the agent sent, read through the contract's type, so a scenario waits on a message kind
/// rather than on its `type` text. `None` for a frame that does not parse.
#[must_use]
pub fn agent_message(frame: &Value) -> Option<AgentToControllerMessage> {
    serde_json::from_value(frame.clone()).ok()
}

/// Whether `frame` reports the slot of `allocation` running.
#[must_use]
pub fn slot_runs(frame: &Value, allocation: &str) -> bool {
    matches!(agent_message(frame), Some(AgentToControllerMessage::SlotStatus(status)) if status.allocation_id.as_deref() == Some(allocation) && status.state == SlotState::Running)
}

/// Whether `frame` is a reply to the request `id`.
#[must_use]
pub fn replies_to(frame: &Value, id: &str) -> bool {
    matches!(agent_message(frame), Some(AgentToControllerMessage::Reply(_))) && frame["reply_to"] == id
}

/// The relay's acknowledgement of a plugin's events, as the agent sends it to the plugin.
pub const RELAY_ACK: &str = "ack";

/// The last event that `report`, from a game server, says the relay acknowledged; `None` for any
/// other report.
#[must_use]
pub fn acked_to(report: &Value) -> Option<u64> {
    let frame = &report["relay_frame"];
    if frame["type"] == RELAY_ACK { frame["to_seq"].as_u64() } else { None }
}

/// Whether `reply` reports its request succeeded.
#[must_use]
pub fn succeeded(reply: &Value) -> bool {
    matches!(agent_message(reply), Some(AgentToControllerMessage::Reply(AgentReply::Success(_))))
}

/// The node token every scenario's `agent.yml` carries.
pub const NODE_TOKEN: &str = "conformance0node0token0000000000000000000000000000000000000000001";
/// A slot token the stubbed Steam accepts.
pub const GOOD_GSLT: &str = "GOODTOKEN000000000000000000000001";
/// A second slot token the stubbed Steam accepts.
pub const OTHER_GSLT: &str = "GOODTOKEN000000000000000000000002";
/// A slot token the stubbed Steam refuses.
pub const REFUSED_GSLT: &str = "REFUSEDTOKEN00000000000000000001";
/// The allocations scenarios launch, in order of use.
pub const ALLOCATIONS: &[&str] = &["11111111-1111-4111-8111-111111111111", "22222222-2222-4222-8222-222222222222", "33333333-3333-4333-8333-333333333333"];
/// The demo upload header scenarios configure.
pub const DEMO_HEADER: &str = "X-Conformance-Key";

/// How long a step may take before the scenario fails.
pub const STEP_TIMEOUT: Duration = Duration::from_mins(1);
/// How long a wait for quiet may last; a node that keeps talking is compared as it is.
const SETTLE_LIMIT: Duration = Duration::from_secs(20);
/// How long nothing may happen before a step counts as finished.
const QUIET: Duration = Duration::from_millis(2500);

/// A slot in `agent.yml`.
#[derive(Clone)]
pub struct Slot {
    pub id: &'static str,
    pub port: u16,
    pub tv_port: u16,
    pub gslt: &'static str,
}

/// The node a scenario starts from.
#[derive(Clone)]
pub struct NodeConfig {
    pub slots: Vec<Slot>,
    pub auto_update: bool,
    pub demo_upload: bool,
    pub pinned_ip: Option<&'static str>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self { slots: vec![Slot { id: "s1", port: 27015, tv_port: 27020, gslt: GOOD_GSLT }], auto_update: true, demo_upload: false, pinned_ip: None }
    }
}

/// The demo upload URL, on the fake controller.
pub fn demo_url() -> String {
    format!("http://{CONTROLLER_HOST}{DEMO_PATH}")
}

impl NodeConfig {
    /// The node's `agent.yml`, as a controller renders it.
    pub fn agent_yml(&self) -> String {
        use std::fmt::Write as _;
        let mut yml = format!(
            "# agent.yml for an Ankor node.\ntoken: {NODE_TOKEN}\ncontroller:\n  host: {CONTROLLER_HOST}\n  tls: false\ncs2_image: {CS2_IMAGE}\nagent_image: {}\n",
            crate::sandbox::AGENT_IMAGE
        );
        if let Some(ip) = self.pinned_ip {
            let _ = writeln!(yml, "pinned_ip: {ip}");
        }
        if !self.auto_update {
            yml.push_str("auto_update: false\n");
        }
        if self.slots.is_empty() {
            yml.push_str("slots: []\n");
        } else {
            yml.push_str("slots:\n");
            for slot in &self.slots {
                let _ = write!(yml, "  - id: {}\n    port: {}\n    tv_port: {}\n    gslt: {}\n", slot.id, slot.port, slot.tv_port, slot.gslt);
            }
        }
        if self.demo_upload {
            let _ = write!(yml, "demo_upload:\n  url: {}\n  header:\n    name: {DEMO_HEADER}\n    value: conformance-secret\n", demo_url());
        }
        yml
    }
}

/// A launch with no match, on `de_dust2`.
pub fn plain_launch() -> Value {
    json!({
        "hostname": "Conformance",
        "steam_web_api_key": null,
        "game_password": null,
        "chat_tag": { "text": "Ankor", "color": "green" },
        "game_mode": "competitive",
        "max_players": 10,
        "workshop_id": null,
        "map_name": "de_dust2",
        "gotv": { "password": null, "delay_seconds": 0 },
        "idle_shutdown_seconds": null,
        "exec": [],
        "commands": [],
        "match": null,
    })
}

/// One run of one scenario against one agent.
pub struct Ctx<'a> {
    pub rec: Recorder,
    pub controller: Controller,
    pub stubs: Stubs,
    pub sandbox: &'a mut Sandbox,
    pub log_file: PathBuf,
    requests: usize,
    /// Where each allocation's launch began, which `ready` looks from.
    launched: std::collections::HashMap<String, usize>,
    /// The last step that is not a detail of another, which names the controller entries since the
    /// previous marker.
    last_step: std::sync::Mutex<String>,
    /// How many entries were recorded when the last step marker went in.
    marked: std::sync::Mutex<usize>,
}

impl<'a> Ctx<'a> {
    pub fn new(rec: Recorder, controller: Controller, stubs: Stubs, sandbox: &'a mut Sandbox, log_file: PathBuf) -> Self {
        let _ = std::fs::remove_file(log_file.with_file_name("steps.log"));
        Self {
            rec,
            controller,
            stubs,
            sandbox,
            log_file,
            requests: 0,
            launched: std::collections::HashMap::new(),
            last_step: std::sync::Mutex::default(),
            marked: std::sync::Mutex::default(),
        }
    }

    /// Says what the scenario does, on the terminal and in `steps.log` beside the agent's log.
    pub fn step(&self, line: &str) {
        use std::io::Write as _;

        println!("    {line}");
        // Indented lines are details of the step above them.
        if !line.starts_with(' ') {
            line.clone_into(&mut self.last_step.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(self.log_file.with_file_name("steps.log")) {
            let _ = writeln!(file, "{line}");
        }
    }

    /// Where the next wait starts looking.
    pub fn mark(&self) -> usize {
        self.rec.len()
    }

    /// Starts the agent and lets it boot: its hello is welcomed, then Steam's newest build and
    /// the token logons are let through one after the other, each after the node went quiet, so
    /// their reports come in the same order for every agent.
    pub async fn boot(&mut self) -> Result<()> {
        self.stubs.steamcmd.close();
        self.stubs.steam.close();
        let mark = self.mark();
        self.step("start the agent");
        self.sandbox.start_agent().await?;
        self.wait_agent(mark, |frame| matches!(agent_message(frame), Some(AgentToControllerMessage::Hello(_)))).await.context("the agent never said hello")?;
        self.step("the agent said hello; let Steam's build and token answers through");
        self.settle().await;
        self.stubs.steamcmd.open();
        self.settle().await;
        self.stubs.steam.open();
        self.settle().await;
        Ok(())
    }

    /// Stops the agent as `docker stop` does; its game servers keep running.
    pub async fn stop_agent(&mut self) -> Result<()> {
        self.step("stop the agent");
        self.sandbox.stop_agent(&self.log_file).await?;
        self.settle().await;
        Ok(())
    }

    /// Waits until nothing new reached any fake for a moment, then marks the controller entries
    /// since the last marker as the current step's. Normalisation sorts the entries between two
    /// markers, whose order depends on how the agent's tasks interleave.
    pub async fn settle(&self) {
        self.rec
            .settle(&[channels::CONTROLLER, channels::EVENTS, channels::DEMOS, channels::PLUGIN_PREFIX, channels::STEAM, channels::DOCKER], QUIET, SETTLE_LIMIT)
            .await;
        let mut marked = self.marked.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.rec.entries().iter().skip(*marked).any(|entry| entry.channel == channels::CONTROLLER) {
            let step = self.last_step.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            self.rec.record(channels::CONTROLLER, json!({ (controller_entry::AFTER_STEP): step }));
            *marked = self.rec.len();
        }
    }

    /// The first frame from the agent after `mark` that `matches`.
    pub async fn wait_agent(&self, mark: usize, matches: impl Fn(&Value) -> bool) -> Result<Value> {
        let (_, entry) = self.rec.wait_for(mark, channels::CONTROLLER, STEP_TIMEOUT, |entry| entry.get(controller_entry::AGENT).is_some_and(&matches)).await?;
        Ok(entry[controller_entry::AGENT].clone())
    }

    /// The first entry on `channel` after `mark` that `matches`.
    pub async fn wait_on(&self, mark: usize, channel: &str, matches: impl Fn(&Value) -> bool) -> Result<Value> {
        Ok(self.rec.wait_for(mark, channel, STEP_TIMEOUT, matches).await?.1)
    }

    /// The id of the next request the controller sends: `req-1`, `req-2` and so on, the same in every run.
    fn next_request_id(&mut self) -> String {
        self.requests += 1;
        format!("req-{}", self.requests)
    }

    /// Sends a request with the next request id and waits for its reply, then for quiet.
    pub async fn request(&mut self, mut frame: Value) -> Result<Value> {
        let id = self.next_request_id();
        frame["id"] = json!(id);
        let target = ["allocation_id", "slot_id", "mode"].iter().find_map(|key| frame[*key].as_str()).map(|value| format!(" {value}")).unwrap_or_default();
        self.step(&format!("controller → {}{target}", frame["type"].as_str().unwrap_or("?")));
        let mark = self.mark();
        self.controller.send(frame)?;
        let reply = self.wait_agent(mark, |frame| replies_to(frame, &id)).await.with_context(|| format!("no reply to {id}"))?;
        self.step_reply("", &reply);
        self.settle().await;
        Ok(reply)
    }

    /// Sends every request before waiting for any reply, as a controller that retries without
    /// waiting does, and returns the replies in the order the requests went.
    pub async fn requests_together(&mut self, frames: Vec<Value>) -> Result<Vec<Value>> {
        let mark = self.mark();
        let mut ids = Vec::new();
        for mut frame in frames {
            let id = self.next_request_id();
            frame["id"] = json!(id);
            let target = frame["allocation_id"].as_str().map(|value| format!(" {value}")).unwrap_or_default();
            self.step(&format!("controller → {}{target} (without waiting)", frame["type"].as_str().unwrap_or("?")));
            self.controller.send(frame)?;
            ids.push(id);
        }
        let mut replies = Vec::new();
        for id in ids {
            let reply = self.wait_agent(mark, |frame| replies_to(frame, &id)).await.with_context(|| format!("no reply to {id}"))?;
            self.step_reply(&format!("{id}: "), &reply);
            replies.push(reply);
        }
        self.settle().await;
        Ok(replies)
    }

    /// Logs whether `reply` succeeded, after `label`.
    fn step_reply(&self, label: &str, reply: &Value) {
        if succeeded(reply) {
            self.step(&format!("  ← {label}ok"));
        } else {
            self.step(&format!("  ← {label}refused: {}", reply["error"]["code"].as_str().unwrap_or("?")));
        }
    }

    /// Launches `launch` as `allocation`, then has its plugin dial the relay once the reply is in.
    pub async fn launch(&mut self, allocation: &str, launch: Value) -> Result<Value> {
        let mark = self.mark();
        self.launched.insert(allocation.to_owned(), mark);
        let reply = self.request(json!({ "type": "start_server", "allocation_id": allocation, "launch": launch })).await?;
        if succeeded(&reply) {
            let channel = channels::plugin(allocation);
            self.wait_on(mark, &channel, |report| report["started"] == true).await.context("the game server never started")?;
            self.step("  the game server started");
            let mark = self.mark();
            self.plugin(allocation, json!({ "op": "connect" }))?;
            self.wait_on(mark, &channel, |report| report["relay"] == "connected").await.context("the plugin never reached the relay")?;
            self.step("  its plugin reached the relay");
            self.settle().await;
        }
        Ok(reply)
    }

    /// Waits for the slot of `allocation` to run: its plugin reports the server ready once the map
    /// loaded and Steam logged it on, which the server does as its plugin loads.
    pub async fn ready(&mut self, allocation: &str) -> Result<()> {
        // A server launched some other way is looked for from the start of the scenario, which
        // launches each allocation once.
        let mark = self.launched.get(allocation).copied().unwrap_or(0);
        self.wait_agent(mark, |frame| slot_runs(frame, allocation)).await?;
        self.step("  the slot runs");
        self.settle().await;
        Ok(())
    }

    /// Tells the update the agent started how it ends: `{"op": "install", "build": …}` or
    /// `{"op": "fail", "code": …}`, where `code` is what steamcmd exits with; the entrypoint then
    /// dies with 1, as the real one does.
    pub async fn finish_update(&self, instruction: Value) -> Result<()> {
        self.step(&format!("the update container: {instruction}"));
        self.sandbox.instruct_update(&instruction).await
    }

    /// Sends an instruction to a game server.
    pub fn plugin(&self, allocation: &str, op: Value) -> Result<()> {
        let mut detail = String::new();
        for (key, value) in op.as_object().into_iter().flatten().filter(|(key, _)| *key != "op") {
            let _ = write!(detail, " {key}={value}");
        }
        let detail: String = detail.chars().take(120).collect();
        let short: String = allocation.chars().take(8).collect();
        self.step(&format!("plugin {short}: {}{detail}", op["op"].as_str().unwrap_or("?")));
        self.controller.plugin(allocation, op)
    }

    /// Has a player join the server of `allocation` on `side`, as the engine reports one, and waits
    /// until the agent acknowledged and uploaded the events the plugin sent for it.
    pub async fn join(&mut self, allocation: &str, steam_id: &str, name: &str, side: Side) -> Result<()> {
        let mark = self.mark();
        self.plugin(allocation, json!({ "op": "join", "steam_id": steam_id, "name": name, "side": side }))?;
        self.settle().await;
        self.events_delivered(allocation, mark).await
    }

    /// Waits until the agent acknowledged and uploaded the newest event that the plugin of
    /// `allocation` sent since `mark`, if it sent one.
    pub async fn events_delivered(&self, allocation: &str, mark: usize) -> Result<()> {
        let channel = channels::plugin(allocation);
        let newest = self
            .rec
            .entries()
            .iter()
            .skip(mark)
            .filter(|entry| entry.channel == channel && entry.value["plugin_frame"]["type"] == "event")
            .filter_map(|entry| entry.value["plugin_frame"]["seq"].as_u64())
            .max();
        let Some(seq) = newest else { return Ok(()) };
        self.wait_on(mark, &channel, |report| acked_to(report) >= Some(seq)).await.context("the events were never acknowledged")?;
        self.wait_on(mark, channels::EVENTS, |post| post["body"]["frames"].as_array().is_some_and(|frames| frames.iter().any(|frame| frame["seq"] == seq)))
            .await
            .context("the events were never uploaded")?;
        self.step(&format!("  events up to {seq} acknowledged and uploaded"));
        self.settle().await;
        Ok(())
    }

    /// Records the node's containers and data directory under `label`.
    pub async fn snapshot(&self, label: &str) -> Result<()> {
        self.step(&format!("snapshot {label}"));
        let containers = self.sandbox.containers().await?;
        let files = self.sandbox.files().await?;
        self.rec.record(channels::snapshot(label, channels::SNAPSHOT_CONTAINERS), containers);
        self.rec.record(channels::snapshot(label, channels::SNAPSHOT_FILES), files);
        Ok(())
    }

    /// Fails the scenario unless `reply` succeeded.
    pub fn expect_ok(reply: &Value) -> Result<()> {
        if !succeeded(reply) {
            bail!("the request failed: {reply}");
        }
        Ok(())
    }
}

/// The match of [`match_launch`].
pub const MATCH_ID: &str = "0199a0b4-5c00-7000-8000-00000000c0f0";
/// The one player of each team of [`match_launch`].
pub const TEAM1_PLAYER: &str = "76561198000000001";
pub const TEAM2_PLAYER: &str = "76561198000000002";

/// A launch with a match of one player a team, on `de_dust2`. `statistics` decides whether the
/// plugin sends gameplay events; its control messages go either way.
pub fn match_launch(statistics: bool, record_demo: bool) -> Value {
    let mut launch = plain_launch();
    launch["match"] = json!({
        "match_id": MATCH_ID,
        "send_statistics": statistics,
        "roster": {
            "team1": { "name": "Alpha", "players": [TEAM1_PLAYER] },
            "team2": { "name": "Bravo", "players": [TEAM2_PLAYER] },
            "spectators": [],
        },
        "sides": SidesMode::Team1Ct,
        "record_demo": record_demo,
    });
    launch
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a test fails by crashing")]

    use super::*;

    /// The fixtures are what a controller would send, so the contract accepts each of them.
    #[test]
    fn the_launches_and_events_pass_the_contract() {
        use ankor_protocol::schemas::s_server_launch_config;
        use ankor_protocol::validate::{format_issues, safe_parse};

        for launch in [plain_launch(), match_launch(false, false), match_launch(true, true)] {
            if let Err(issues) = safe_parse(s_server_launch_config, launch) {
                panic!("{}", format_issues(&issues));
            }
        }
    }
}
