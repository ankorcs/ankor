//! A careless controller against the agent as `run_agent` wires it: random sequences of valid,
//! malformed, repeated and oversized frames, game servers that crash, quit and lose their plugin,
//! and a socket the controller drops at will. The clock is paused once the agent has said hello, so
//! minutes of deadlines pass in milliseconds; the sockets keep running between steps.
//!
//! Each game loads a plugin that answers as `ankor-fake-plugin` models the real one. It reconnects
//! with backoff, turns ready late or never, and may refuse its match config or hang. Players join
//! a ready server, and a started match plays its rounds, sends events and quits when it ends.
//!
//! Each run checks that the agent stays up, answers a request at most once and, on the socket still
//! open, at least once, never runs two games in one slot, and ends up reporting what Docker runs.
//!
//! `FUZZ_RUNS` sets how many runs there are (default 8), and `FUZZ_SEED` replays one run's actions,
//! whose seed a failure prints. The agent's tasks still interleave as they will, so a replay may
//! take another path. `FUZZ_DEBUG` prints the requests owed a reply by step, the replies that
//! failed and the agent's log. `ANKOR_LOG_LEVEL=debug` adds the agent's debug lines:
//!
//! ```sh
//! FUZZ_RUNS=200 scripts/run.sh cargo nextest run -p ankor-agent --test fuzz --no-capture
//! FUZZ_DEBUG=1 ANKOR_LOG_LEVEL=debug FUZZ_SEED=1234 scripts/run.sh cargo nextest run -p ankor-agent --test fuzz --no-capture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
#![allow(clippy::too_many_lines, reason = "one run reads as one story, from boot to the last check")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::container_spec::ContainerRole;
use ankor_agent::contract::backup_file_name;
use ankor_agent::contract::constants::{BACKUP_DIR, DEMO_DIR};
use ankor_agent::controller_connection::Signal;
use ankor_agent::testing::agent_boot::{BootOptions, Booted, boot, installed_json, on_volume, plugin_url, volume_target, write_installed};
use ankor_agent::testing::fixtures::{launch_config, match_config};
use ankor_agent::testing::{FakeDocker, until};
use ankor_fake_plugin::{Backoff, Inbound, Plugin};
use ankor_protocol::constants::{REMEMBERED_ALLOCATIONS, official_images};
use ankor_protocol::types::AgentToControllerMessage;
use ankor_protocol::types::MatchPhase;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

/// Slots the node starts with, enough for many servers to start, run and stop across each other.
const SLOTS: usize = 20;

/// The allocations the requests aim at: a few more than the slots, so launches compete for them.
fn allocations() -> Vec<String> {
    (1..=SLOTS + 4).map(|n| format!("0199a0b4-5c00-7000-8000-{:012x}", 0xf000 + n)).collect()
}

/// The node's slots, `s01` up to `s20`, and `s21`, which only a `set_config` adds.
fn slot_id(n: usize) -> String {
    format!("s{n:02}")
}

/// Slot `n` on its own ports, with its own token.
fn slot(n: usize) -> Value {
    let port = 27015 + 10 * (n - 1);

    json!({ "id": slot_id(n), "port": port, "tv_port": port + 1, "gslt": format!("AAAAAAAAAAAAAAAAAAAAAAAAAAAA{n:04}") })
}

/// The requests about one running game, which the agent passes to its plugin or reads it for.
const GAME_REQUESTS: [&str; 5] = ["stop_server", "get_server_status", "console_command", "restore_round", "start_match"];
const NOTICES: [&str; 3] = ["check_cs2_build", "watch_resources", "watch_logs"];
/// The demo `delete_demo` names, which `WriteDemo` puts in a game's folder.
const DEMO_FILE: &str = "20260922-183000_match-1_de_mirage.dem";
/// Past every stop grace period and the start timeout.
const QUIET: Duration = Duration::from_secs(320);

/// `SplitMix64`: small, and one seed replays a run exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// One index by weight.
    fn weighted(&mut self, weights: &[usize]) -> usize {
        let mut roll = self.below(weights.iter().sum());

        weights
            .iter()
            .position(|&weight| {
                roll < weight || {
                    roll -= weight;
                    false
                }
            })
            .unwrap()
    }

    fn text(&mut self, max: usize) -> String {
        let alphabet: Vec<char> = "abcXYZ019 _-;\"'{}[]\\\u{0}\u{7f}é🙂".chars().collect();

        (0..self.below(max + 1)).map(|_| *self.pick(&alphabet)).collect()
    }

    fn json(&mut self, depth: usize) -> Value {
        match self.below(if depth == 0 { 4 } else { 6 }) {
            0 => Value::Null,
            1 => json!(self.below(2) == 0),
            2 => json!(i64::try_from(self.next() >> 1).unwrap() * if self.below(2) == 0 { 1 } else { -1 }),
            3 => json!(self.text(12)),
            4 => Value::Array((0..self.below(4)).map(|_| self.json(depth - 1)).collect()),
            _ => Value::Object((0..self.below(4)).map(|_| (self.text(6), self.json(depth - 1))).collect()),
        }
    }
}

/// A request the contract accepts, without its id.
fn valid_request(rng: &mut Rng) -> Value {
    let allocation = rng.pick(&allocations()).clone();
    // Now and then a slot the node does not have.
    let slot = if rng.below(10) == 0 { "s99".to_owned() } else { slot_id(1 + rng.below(SLOTS)) };

    match rng.weighted(&[8, 1, 1, 1, 1, 1, 1, 1, 2, 4, 1, 1, 2]) {
        0 => json!({ "type": "start_server", "allocation_id": allocation, "launch": launch_config(&json!({})) }),
        1 => json!({ "type": "stop_server", "allocation_id": allocation }),
        2 => json!({ "type": "get_server_status", "allocation_id": allocation }),
        3 => json!({ "type": "console_command", "allocation_id": allocation, "command": rng.text(40) }),
        // Mostly the first rounds, which WriteBackup gives backups.
        4 => json!({ "type": "restore_round", "allocation_id": allocation, "round": 1 + if rng.below(4) == 0 { rng.below(40) } else { rng.below(5) } }),
        5 => json!({ "type": "reserve_slot", "slot_id": slot }),
        6 => json!({ "type": "delete_demo", "slot_id": slot, "file_name": DEMO_FILE }),
        7 => json!({ "type": "update_cs2", "mode": rng.pick(&["update", "verify", "reinstall"]) }),
        9 => json!({ "type": "start_match", "allocation_id": allocation }),
        // Official images, someone else's, and each kind's image sent as the other.
        10 => json!({ "type": "update_agent", "pull": rng.below(2) == 0, "image": rng.pick(&candidate_images()) }),
        11 => json!({ "type": "update_cs2_image", "pull": rng.below(2) == 0, "image": rng.pick(&candidate_images()) }),
        12 => json!({ "type": "set_config", "config": { "slots": slot_layout(rng) } }),
        _ => json!({ "type": "set_config", "config": rng.pick(&[
            json!({ "demo_upload": null }),
            json!({ "auto_update": false }),
            json!({ "pinned_ip": null }),
            json!({ "memory_mb": 4096 }),
            json!({ "memory_mb": null }),
        ]) }),
    }
}

/// The images update requests ask for: each official one, by tag and by digest, someone else's,
/// and a look-alike. Each kind's official image is foreign to the other kind.
fn candidate_images() -> [String; 6] {
    [
        format!("{}:0.8", official_images::AGENT),
        format!("{}@sha256:{}", official_images::AGENT, "c".repeat(64)),
        format!("{}:0.6", official_images::CS2),
        format!("{}-x:0.6", official_images::CS2),
        "ghcr.io/example/ankor-agent:0.8".to_owned(),
        "ankor-cs2:next".to_owned(),
    ]
}

/// A slot layout for `set_config`: most of the slots, now and then `s21`, each on its own ports, with
/// a token sent or left out to keep the slot's own.
fn slot_layout(rng: &mut Rng) -> Value {
    let mut layout = Vec::new();

    for n in 1..=SLOTS + 1 {
        if (n <= SLOTS && rng.below(8) == 0) || (n > SLOTS && rng.below(3) != 0) {
            continue;
        }

        let mut entry = slot(n);

        // Now and then a slot moves to another slot's ports, which the agent must refuse.
        if rng.below(16) == 0 {
            entry["port"] = json!(27015);
            entry["tv_port"] = json!(27016);
        }

        if rng.below(2) == 0 {
            entry.as_object_mut().unwrap().remove("gslt");
        }

        layout.push(entry);
    }

    Value::Array(layout)
}

/// The code an update request must be refused with: `image_not_allowed` for an image outside its
/// kind's official repository, as the node runs only official ones. None for anything else.
fn expected_refusal(frame: &Value) -> Option<&'static str> {
    let official = match frame["type"].as_str()? {
        "update_agent" => official_images::AGENT,
        "update_cs2_image" => official_images::CS2,
        _ => return None,
    };
    let image = frame["image"].as_str()?;
    let keys: BTreeSet<&str> = frame.as_object()?.keys().map(String::as_str).collect();

    // A bent frame fails validation first, even one that kept its keys but not its image.
    if keys != BTreeSet::from(["type", "image", "pull", "id"]) || !frame["pull"].is_boolean() || !candidate_images().iter().any(|candidate| candidate == image)
    {
        return None;
    }

    let repository = image.split('@').next().unwrap_or(image);
    let repository = match repository.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => name,
        _ => repository,
    };

    (repository != official).then_some("image_not_allowed")
}

/// A frame without an id, which the controller sends as it pleases.
fn notice(rng: &mut Rng) -> Value {
    match *rng.pick(&NOTICES) {
        "check_cs2_build" => json!({ "type": "check_cs2_build" }),
        "watch_resources" => json!({ "type": "watch_resources", "replay": rng.below(2) == 0 }),
        // The agent's log, a game's, or an update's that may not exist.
        _ => json!({ "type": "watch_logs", "replay_id": null, "source": match rng.below(3) {
            0 => json!({ "kind": "agent" }),
            1 => json!({ "kind": "server", "allocation_id": rng.pick(&allocations()) }),
            _ => json!({ "kind": "cs2_update", "update_id": "0199a0b4-5c00-7000-8000-0000000000aa" }),
        } }),
    }
}

/// A valid request bent out of shape: a field dropped, retyped or added.
fn bent_request(rng: &mut Rng) -> Value {
    let mut frame = valid_request(rng);
    let object = frame.as_object_mut().unwrap();
    let keys: Vec<String> = object.keys().filter(|key| *key != "type").cloned().collect();
    let key = keys.get(rng.below(keys.len().max(1))).cloned();

    match (rng.below(3), key) {
        (0, Some(key)) => drop(object.remove(&key)),
        (1, Some(key)) => drop(object.insert(key, rng.json(2))),
        (_, _) => drop(object.insert(format!("extra_{}", rng.below(7)), rng.json(2))),
    }

    frame
}

#[derive(Debug)]
enum Action {
    Request(Value),
    SameIdTwice(Value),
    Burst(Vec<Value>),
    Raw(String),
    Binary(usize),
    Oversized(usize),
    Welcome,
    Advance(u64),
    Crash(usize, Option<i64>),
    DropLink(usize),
    WriteBackup(usize, usize),
    WriteDemo(usize),
    PauseLink(u64),
    GameLog(usize, usize),
    WatchGame(usize),
    NewBuild(bool),
    DropSocket,
}

fn action(rng: &mut Rng) -> Action {
    match rng.weighted(&[6, 1, 4, 2, 2, 1, 1, 1, 5, 1, 2, 1, 1, 1, 1, 3, 2, 1, 1]) {
        0 => Action::Request(valid_request(rng)),
        1 => Action::SameIdTwice(valid_request(rng)),
        2 => Action::Burst(if rng.below(2) == 0 {
            let frame = valid_request(rng);
            let times = if rng.below(2) == 0 { 2 + rng.below(9) } else { 50 + rng.below(51) };

            vec![frame; times]
        } else {
            (0..2 + rng.below(11))
                .map(|_| match rng.weighted(&[3, 2, 1]) {
                    0 => valid_request(rng),
                    1 => notice(rng),
                    _ => bent_request(rng),
                })
                .collect()
        }),
        3 => Action::Raw(notice(rng).to_string()),
        4 => Action::Request(bent_request(rng)),
        5 => Action::Raw(match rng.below(3) {
            0 => rng.json(3).to_string(),
            1 => rng.text(64),
            _ => (*rng.pick(&["null", "[]", "{\"id\":\"x\"}", "{\"type\":\"reply\"}"])).to_owned(),
        }),
        6 => Action::Binary(1 + rng.below(4096)),
        7 => Action::Oversized(*rng.pick(&[1, 5, 20]) * 1024 * 1024),
        8 => Action::Advance(*rng.pick(&[10, 500, 2_000, 10_000, 30_000, 70_000, 310_000])),
        9 => Action::Crash(rng.below(64), *rng.pick(&[Some(0), Some(1), Some(137), None])),
        10 | 11 => Action::DropLink(rng.below(64)),
        12 => Action::WriteBackup(rng.below(64), 1 + rng.below(5)),
        13 => Action::WriteDemo(rng.below(64)),
        14 => Action::PauseLink(*rng.pick(&[2_000, 20_000, 60_000, 120_000])),
        15 => Action::GameLog(rng.below(64), 1 + rng.below(40)),
        16 => Action::WatchGame(rng.below(64)),
        // Steam publishes a newer build, and the controller may tell the agent to look.
        17 => Action::NewBuild(rng.below(2) == 0),
        _ => match rng.below(2) {
            0 => Action::Welcome,
            _ => Action::DropSocket,
        },
    }
}

/// The action's kind and size, for a failure's account of the run.
fn short(action: &Action) -> String {
    match action {
        Action::Request(frame) | Action::SameIdTwice(frame) => frame["type"].as_str().unwrap_or("?").to_owned(),
        Action::Burst(frames) => format!("burst {}x{}", frames.len(), frames.first().and_then(|frame| frame["type"].as_str()).unwrap_or("?")),
        other => format!("{other:?}").chars().take(30).collect(),
    }
}

/// Ends the games whose plugin ran `quit`, gives each new game its plugin, and ends the plugins of
/// games that are gone.
fn tend(booted: &Booted, links: &mut BTreeMap<String, Link>, ended: &mut BTreeSet<String>, seed: u64) {
    for (id, link) in links.iter() {
        if let Some(code) = *link.quit.lock().unwrap()
            && ended.insert(id.clone())
        {
            end_game(&booted.docker, id, Some(code));
        }
    }

    let live = live_games(&booted.docker, ended);

    for game in &live {
        if !links.contains_key(&game.id) {
            let mut draw = Rng(seed ^ u64::try_from(game.index).unwrap().wrapping_mul(0x9e37_79b9));
            let backups = booted.data_dir.path().join("slots").join(&game.slot).join(BACKUP_DIR);
            let link = link_plugin(plugin_url(&booted.docker, &game.allocation), behaviour(&mut draw), backups, draw.next());

            links.insert(game.id.clone(), link);
        }
    }

    links.retain(|id, link| {
        let alive = live.iter().any(|game| &game.id == id);

        if !alive {
            link.task.abort();
        }

        alive
    });
}

/// `frame`, or, three times in four, the same request about a running game: a random pick of the
/// run's allocations seldom names one, and the request then never reaches a plugin.
fn aimed(frame: &Value, running: &[String], n: usize) -> Value {
    let mut frame = frame.clone();

    // Only a request that names one of the run's allocations, so a bent one stays bent.
    if GAME_REQUESTS.contains(&frame["type"].as_str().unwrap_or_default())
        && frame["allocation_id"].as_str().is_some_and(|allocation| allocations().iter().any(|known| known == allocation))
        && !n.is_multiple_of(4)
        && let Some(allocation) = running.get(n % running.len().max(1))
    {
        frame["allocation_id"] = json!(allocation);
    }

    frame
}

/// A game container as Docker runs it, with the slot and allocation it was started for.
struct Game {
    index: usize,
    id: String,
    slot: String,
    allocation: String,
}

/// The game containers still running: started, and neither stopped nor ended by the run.
fn live_games(docker: &FakeDocker, ended: &BTreeSet<String>) -> Vec<Game> {
    let stopped: BTreeSet<String> = docker.stopped.lock().unwrap().iter().map(|(id, _)| id.clone()).collect();

    docker
        .started
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, started)| started.spec.labels.get("ankor.role").map(String::as_str) == Some("game"))
        .filter(|(_, started)| !stopped.contains(&started.id) && !ended.contains(&started.id))
        .map(|(index, started)| Game {
            index,
            id: started.id.clone(),
            slot: started.spec.labels.get("ankor.slot").cloned().unwrap_or_default(),
            allocation: started.spec.labels.get("ankor.allocation").cloned().unwrap_or_default(),
        })
        .collect()
}

/// Makes a game exit, as Docker tells the agent that started it.
fn end_game(docker: &FakeDocker, id: &str, code: Option<i64>) {
    let hooks = docker.started.lock().unwrap().iter().find(|started| started.id == id).map(|started| started.hooks.clone());

    if let Some(hooks) = hooks {
        (hooks.on_exit)(code);
    }
}

/// The update containers still running, by id.
fn running_updates(docker: &FakeDocker, ended: &BTreeSet<String>) -> Vec<String> {
    let stopped: BTreeSet<String> = docker.stopped.lock().unwrap().iter().map(|(id, _)| id.clone()).collect();

    docker
        .started
        .lock()
        .unwrap()
        .iter()
        .filter(|started| started.spec.labels.get("ankor.role").map(String::as_str) == Some("update"))
        .filter(|started| !stopped.contains(&started.id) && !ended.contains(&started.id))
        .filter(|started| !docker.removed.lock().unwrap().contains(&started.id))
        .map(|started| started.id.clone())
        .collect()
}

/// Ends each update container 10 to 15 seconds after it started: it writes the newest public build
/// to the install's version file, as steamcmd does, and exits.
fn finish_updates(booted: &Booted, updates: &mut BTreeMap<String, Option<tokio::time::Instant>>, ends: &mut BTreeMap<String, usize>, rng: &mut Rng) {
    let now = tokio::time::Instant::now();
    let started: Vec<(String, ankor_agent::docker::StartContainerSpec, ankor_agent::docker::ContainerHooks)> = booted
        .docker
        .started
        .lock()
        .unwrap()
        .iter()
        .filter(|started| started.spec.labels.get("ankor.role").map(String::as_str) == Some("update"))
        .map(|started| (started.id.clone(), started.spec.clone(), started.hooks.clone()))
        .collect();

    for (id, spec, hooks) in started {
        let due = updates.entry(id.clone()).or_insert_with(|| Some(now + Duration::from_millis(10_000 + u64::try_from(rng.below(5_001)).unwrap())));

        if due.is_some_and(|due| due <= now) {
            let latest = booted.steam.0.lock().unwrap().clone();

            *due = None;
            // Every container started before this one ends counts as started during the update.
            ends.insert(id.clone(), booted.docker.started.lock().unwrap().len());
            write_installed(&on_volume(booted.volume.path(), &installed_json(), &volume_target(&spec)), &latest);
            (hooks.on_exit)(Some(0));
        }
    }
}

/// Why a launch sent now must be refused, by what the agent last reported: a CS2 game update
/// running, or a build behind the newest while it keeps itself up to date. Other maintenance, such
/// as a cs2 image switch, need not stop launches, as each game runs on an overlay of its own.
fn launch_refusal(booted: &Booted) -> Option<&'static str> {
    let report = booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_) | AgentToControllerMessage::NodeStatus(_))).last().cloned()?;
    let auto_update =
        booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_) | AgentToControllerMessage::Reply(_))).iter().rev().find_map(|frame| {
            frame["config"]["auto_update"].as_bool().or_else(|| (frame["request"] == "set_config").then(|| frame["data"]["auto_update"].as_bool()).flatten())
        })?;

    if report["unavailable"]["code"] == "maintenance" && report["unavailable"]["operation"] == "cs2_update" {
        return Some("a CS2 update");
    }

    let (build, latest) = (report["cs2"]["build_id"].as_str()?, report["cs2"]["latest_build_id"].as_str()?);

    (auto_update && build.parse::<u64>().ok()? < latest.parse::<u64>().ok()?).then_some("cs2 behind")
}

/// How a game's plugin behaves. Every one answers the agent as `ankor-fake-plugin` models the real
/// plugin; these decide what happens on the server around it.
#[derive(Clone, Copy, Debug)]
enum Behaviour {
    /// The plugin did not load, so it never connects.
    Absent,
    /// Its game thread hangs: it connects, then sends and answers nothing, so reads time out. Its
    /// socket's own thread still drops the socket on an ack past its events.
    Hung,
    /// The match config did not decode: no match, a `config_error`, and `no_match` to commands.
    Rejected,
    /// Logs on and loads the map after this long, or never, past the start timeout. Then players
    /// join, and the match runs once the controller starts it.
    Ready(Option<Duration>),
}

fn behaviour(rng: &mut Rng) -> Behaviour {
    // Most servers are ready within ten seconds, faster than a real one, so that matches start in a
    // run's few minutes. Some take longer than the start timeout allows.
    let ready = |rng: &mut Rng, within: usize| Behaviour::Ready(Some(Duration::from_millis(u64::try_from(rng.below(within)).unwrap())));

    match rng.weighted(&[1, 2, 1, 2, 8, 2]) {
        0 => Behaviour::Absent,
        1 => Behaviour::Hung,
        2 => Behaviour::Rejected,
        3 => Behaviour::Ready(None),
        4 => ready(rng, 10_000),
        _ => ready(rng, 90_000),
    }
}

/// A game's plugin on its own task. `drop` makes it lose its socket, as a stalled network does, and
/// `quit` holds the exit code once the plugin ran `quit` after its match.
struct Link {
    task: tokio::task::JoinHandle<()>,
    drop: Arc<tokio::sync::Notify>,
    quit: Arc<Mutex<Option<i64>>>,
}

/// The players of the launch's roster, `match_config`'s one a team.
const ROSTER: [(&str, &str, &str); 2] = [("76561198000000001", "alpha", "ct"), ("76561198000000002", "bravo", "t")];

/// What the server does next, by when.
#[derive(Debug)]
enum Due {
    Dial,
    /// A map loads: the launch's, or a `changelevel` in warmup.
    MapLoad,
    /// Warmup starts, a second after the map loaded.
    Warmup,
    /// Steam logged the server on.
    Ready,
    Join(usize),
    /// The countdown of an accepted `start_match` ended.
    Begin,
    Phase(MatchPhase),
    Round,
    /// A `restore_round` the plugin accepted reached its round, from the backup of that round.
    Restored(u64),
    /// The plugin kicked the players after the match, and quits once its events are acknowledged.
    Quit,
}

/// The plugin's countdown between an accepted `start_match` and the knife round.
const COUNTDOWN: Duration = Duration::from_secs(10);

/// Writes the engine's backup of `round`, one-based, as it does when the round starts.
fn write_backup(backups: &Path, round: u64) {
    std::fs::create_dir_all(backups).unwrap();
    std::fs::write(backups.join(backup_file_name(u32::try_from(round - 1).unwrap())), "\"backup\"\n{\n}\n").unwrap();
}

/// A game's plugin and server. The plugin dials the relay within five seconds of the game's start
/// and keeps dialling it, with the real plugin's backoff, until the game ends; the game runs on
/// whether or not it has a socket. The map loads, warmup starts a second later, and Steam logs the
/// server on around then; one server in six changes level once in warmup. The rostered players join
/// a ready server over 10 seconds, though in one game in four only team 1's does. A started match
/// runs its knife round and then one to eight rounds, swapping sides at halftime and writing each
/// round's backup as the engine does, and ends with `quit` once the agent acknowledged its events.
fn link_plugin(url: String, behaviour: Behaviour, backups: PathBuf, seed: u64) -> Link {
    let drop = Arc::new(tokio::sync::Notify::new());
    let quit: Arc<Mutex<Option<i64>>> = Arc::default();
    let (dropped, quitting) = (Arc::clone(&drop), Arc::clone(&quit));
    let task = tokio::spawn(async move {
        if matches!(behaviour, Behaviour::Absent) {
            return;
        }

        let mut rng = Rng(seed);
        let start = tokio::time::Instant::now();
        let after = |rng: &mut Rng, within_ms: usize| tokio::time::Instant::now() + Duration::from_millis(u64::try_from(rng.below(within_ms)).unwrap());
        let written = backups.clone();
        let mut plugin = Plugin::new(Some(match_config(&json!({}))), "de_mirage").with_backups(Box::new(move |round| {
            round.checked_sub(1).and_then(|index| u32::try_from(index).ok()).is_some_and(|index| written.join(backup_file_name(index)).exists())
        }));
        let hung = matches!(behaviour, Behaviour::Hung);
        let rounds = 1 + u64::try_from(rng.below(8)).unwrap();
        let mut changes_level = rng.below(6) == 0;
        let mut played = 0;
        let mut due: Vec<(tokio::time::Instant, Due)> = vec![(after(&mut rng, 5_000), Due::Dial)];
        let mut socket: Option<ankor_fake_plugin::Socket> = None;
        let mut opened = start;
        let mut acked = false;
        let mut backoff = Backoff::default();

        match behaviour {
            Behaviour::Rejected => {
                plugin.reject_config("match config: the roster does not decode");
                due.push((after(&mut rng, 20_000), Due::MapLoad));
            }
            Behaviour::Ready(Some(delay)) => due.push((start + delay, Due::MapLoad)),
            _ => {}
        }

        loop {
            due.sort_by_key(|(at, _)| *at);

            let next = due.first().map(|(at, _)| *at);
            let mut frames: Vec<Value> = Vec::new();
            let mut lost = false;

            tokio::select! {
                () = dropped.notified(), if socket.is_some() => lost = true,
                () = async { match next { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {
                    let (_, what) = due.remove(0);
                    let now = tokio::time::Instant::now();

                    match what {
                        Due::Dial => match ankor_fake_plugin::connect(&url).await {
                            Ok(opening) => {
                                socket = Some(opening);
                                opened = tokio::time::Instant::now();
                                acked = false;

                                if !hung {
                                    frames = plugin.connected();
                                }
                            }
                            Err(_) => due.push((now + backoff.after(None, false) + Duration::from_millis(u64::try_from(rng.below(100)).unwrap()), Due::Dial)),
                        },
                        Due::MapLoad => {
                            let first = !plugin.map_loaded();

                            frames = plugin.load_map();
                            due.push((now + Duration::from_secs(1), Due::Warmup));

                            if first {
                                due.push((after(&mut rng, 2_000), Due::Ready));
                            }
                        }
                        Due::Warmup => {
                            frames = plugin.enter_warmup();

                            if std::mem::take(&mut changes_level) {
                                due.push((after(&mut rng, 5_000), Due::MapLoad));
                            }
                        }
                        Due::Ready => {
                            frames.extend(plugin.turn_ready());

                            let joining = if rng.below(4) == 0 { 1 } else { ROSTER.len() };

                            for player in 0..joining {
                                due.push((after(&mut rng, 10_000), Due::Join(player)));
                            }
                        }
                        Due::Join(player) => {
                            let (steam_id, name, side) = ROSTER[player];

                            plugin.join(steam_id, name, side);
                            frames.extend(plugin.emit(&json!({
                                "type": "player_connect", "timestamp": 1_700_000_000_000_u64, "tick": 0,
                                "player": { "steam_id": steam_id, "name": name, "side": "spec", "team": player + 1 }, "rejected": false,
                            })));
                        }
                        // A level change in warmup cancels the countdown, so only a running one begins.
                        Due::Begin if plugin.counting_down() => {
                            frames = plugin.begin_match(true);
                            due.push((after(&mut rng, 30_000), Due::Phase(MatchPhase::SideChoice)));
                        }
                        Due::Begin => {}
                        Due::Phase(phase) => {
                            frames.extend(plugin.set_phase(phase));

                            match phase {
                                MatchPhase::SideChoice => due.push((after(&mut rng, 20_000), Due::Phase(MatchPhase::Live))),
                                MatchPhase::Live => {
                                    write_backup(&backups, 1);
                                    due.push((after(&mut rng, 60_000), Due::Round));
                                }
                                MatchPhase::PostLive => due.push((now + Duration::from_secs(60), Due::Quit)),
                                _ => {}
                            }
                        }
                        // A round that a restore or the match's end overtook does not score.
                        Due::Round if plugin.phase() != Some(MatchPhase::Live) => {}
                        Due::Round => {
                            let (team1, team2) = plugin.score();
                            let (team1, team2) = if rng.below(2) == 0 { (team1 + 1, team2) } else { (team1, team2 + 1) };

                            played += 1;
                            plugin.end_halftime();
                            let over = plugin.log(&format!("round {played} over"));
                            frames.extend(plugin.emit(&over));
                            frames.extend(plugin.set_score(team1, team2));

                            if played < rounds {
                                if played == rounds.div_ceil(2) {
                                    frames.extend(plugin.swap_sides());
                                }

                                write_backup(&backups, played + 1);
                                due.push((after(&mut rng, 60_000), Due::Round));
                            } else {
                                let winner = match team1.cmp(&team2) {
                                    std::cmp::Ordering::Greater => json!(1),
                                    std::cmp::Ordering::Less => json!(2),
                                    std::cmp::Ordering::Equal => Value::Null,
                                };

                                frames.extend(plugin.emit(&json!({ "type": "map_end", "timestamp": 1_700_000_000_000_u64, "tick": played, "reason": "played", "winner_team": winner, "team1_score": team1, "team2_score": team2, "teams": null })));
                                frames.extend(plugin.set_phase(MatchPhase::PostLive));
                                due.push((now + Duration::from_secs(60), Due::Quit));
                            }
                        }
                        Due::Restored(round) => {
                            // The score the backup holds, split as the rounds before it went.
                            let before = round - 1;

                            played = before;
                            frames = plugin.restored(round, before.div_ceil(2), before / 2);
                            due.retain(|(_, what)| !matches!(what, Due::Round));
                            due.push((after(&mut rng, 60_000), Due::Round));
                        }
                        Due::Quit if plugin.all_acked() => {
                            if let Some(open) = socket.as_mut() {
                                let _ = open.send(Message::Text(json!({ "type": "quitting" }).to_string().into())).await;
                            }

                            *quitting.lock().unwrap() = Some(0);
                            return;
                        }
                        Due::Quit => due.push((now + Duration::from_secs(1), Due::Quit)),
                    }
                }
                message = async { match socket.as_mut() { Some(open) => open.next().await, None => std::future::pending().await } } => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            let frame = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);

                            // A hung game thread answers nothing, but the socket's thread still reads acks.
                            if !hung || frame["type"] == "ack" {
                                let counting = plugin.counting_down();

                                match plugin.answer(&frame) {
                                    // One game tick later.
                                    Inbound::Reply(replies) => {
                                        tokio::time::sleep(Duration::from_millis(16)).await;
                                        frames.extend(replies);
                                    }
                                    Inbound::Drop => lost = true,
                                    Inbound::Nothing => acked |= frame["type"] == "ack",
                                }

                                if !counting && plugin.counting_down() {
                                    due.push((tokio::time::Instant::now() + COUNTDOWN, Due::Begin));
                                }

                                let accepted = frames.iter().any(|reply| reply["type"] == "command_result" && reply["error"].is_null());

                                if frame["type"] == "restore_round" && accepted {
                                    let _ = plugin.set_phase(MatchPhase::PendingRestore);
                                    due.push((after(&mut rng, 5_000), Due::Restored(frame["round"].as_u64().unwrap())));
                                }
                            }
                        }
                        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                        // A binary frame, a close or a broken socket.
                        _ => lost = true,
                    }
                }
            }

            if let Some(open) = socket.as_mut() {
                for frame in &frames {
                    if !lost && open.send(Message::Text(frame.to_string().into())).await.is_err() {
                        lost = true;
                    }
                }
            }

            if lost && socket.take().is_some() {
                let wait = backoff.after(Some(opened.elapsed()), acked) + Duration::from_millis(u64::try_from(rng.below(100)).unwrap());

                due.push((tokio::time::Instant::now() + wait, Due::Dial));
            }
        }
    });

    Link { task, drop, quit }
}

/// Moves the clock until the agent has a socket again; its backoff tops out at 30 seconds.
async fn reconnected(booted: &Booted, accepted: usize) {
    for _ in 0..120 {
        if booted.peer.accepted() > accepted && booted.peer.connected() {
            settle(Duration::from_secs(2)).await;
            return;
        }

        settle(Duration::from_secs(1)).await;
    }

    panic!("the agent did not dial the controller again within two minutes");
}

/// Moves the paused clock, and lets the agent's tasks and sockets catch up.
async fn settle(by: Duration) {
    tokio::time::advance(by).await;

    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// Panics on any thread of the run, which a spawned task's panic would otherwise hide.
fn panic_log() -> Arc<Mutex<Vec<String>>> {
    let panics: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&panics);
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        seen.lock().unwrap().push(info.to_string());
        previous(info);
    }));

    panics
}

async fn run(seed: u64, panics: &Mutex<Vec<String>>) {
    let mut rng = Rng(seed);
    let actions: Vec<Action> = (0..5 + rng.below(56)).map(|_| action(&mut rng)).collect();
    let slots = Value::Array((1..=SLOTS).map(slot).collect());
    let images = (Some(format!("{}:0.6", official_images::CS2)), Some(format!("{}:0.8", official_images::AGENT)));
    let booted = boot(BootOptions { slots: Some(slots), images, official_images_only: Some(true), ..BootOptions::default() }).await;

    until("the hello", || booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).len() == 1).await;

    // A CS2 update runs until the run ends it, 10 to 15 seconds in, as steamcmd takes a while.
    booted.docker.helper_runs.lock().unwrap().remove(&ContainerRole::Update);
    tokio::time::pause();

    // The socket each request went out on, by id: only one still open is owed its reply.
    let mut sent_on: BTreeMap<String, Option<usize>> = BTreeMap::new();
    let mut sent_type: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let mut expected: BTreeMap<String, &'static str> = BTreeMap::new();
    let mut sent_frames: BTreeMap<String, Value> = BTreeMap::new();
    // Each game's console lines by allocation, the allocations any watch_logs named, and each replay
    // asked for: its id, the game, the lines printed by then, the socket and whether a pause came after.
    let mut printed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut watched: BTreeSet<String> = BTreeSet::new();
    let mut replays: Vec<(String, String, usize, Option<usize>)> = Vec::new();
    let mut paused = false;
    let mut most_live = 0;
    // When each running update container finishes, on the paused clock.
    let mut updates: BTreeMap<String, Option<tokio::time::Instant>> = BTreeMap::new();
    // Where each finished update ended in Docker's start order.
    let mut update_ends: BTreeMap<String, usize> = BTreeMap::new();
    // Single start_server requests the agent must refuse: sent while it reported maintenance, or a
    // build behind the newest with auto_update on.
    let mut must_refuse: BTreeMap<String, (&'static str, usize)> = BTreeMap::new();
    let ids = std::cell::Cell::new(0_u64);
    let next_id = || {
        ids.set(ids.get() + 1);
        format!("fuzz-{}", ids.get())
    };
    let mut ended: BTreeSet<String> = BTreeSet::new();
    let mut links: BTreeMap<String, Link> = BTreeMap::new();
    let mut doubled = false;
    let socket = |booted: &Booted| booted.peer.connected().then(|| booted.peer.accepted());

    for (step, act) in actions.iter().enumerate() {
        let running: Vec<String> = live_games(&booted.docker, &ended).into_iter().map(|game| game.allocation).collect();
        let mut send = |frame: &Value, id: Option<String>| {
            let id = id.unwrap_or_else(next_id);
            let mut frame = frame.clone();

            frame["id"] = json!(id);

            if let Some(code) = expected_refusal(&frame) {
                expected.insert(id.clone(), code);
            }

            sent_type.insert(id.clone(), (frame["type"].as_str().unwrap_or("?").to_owned(), step));
            sent_frames.insert(id.clone(), frame.clone());
            sent_on.insert(id, socket(&booted));
            booted.peer.send_json(&frame);
        };

        match act {
            Action::Request(frame) => {
                if frame["type"] == "start_server" {
                    let id = next_id();

                    if let Some(why) = launch_refusal(&booted) {
                        must_refuse.insert(id.clone(), (why, booted.peer.received().len()));
                    }

                    send(frame, Some(id));
                } else {
                    send(&aimed(frame, &running, step), None);
                }
            }
            Action::SameIdTwice(frame) => {
                let id = next_id();

                doubled = true;
                send(frame, Some(id.clone()));
                send(frame, Some(id));
            }
            Action::Burst(frames) => {
                for (n, frame) in frames.iter().enumerate() {
                    if NOTICES.contains(&frame["type"].as_str().unwrap_or_default()) {
                        if let Some(allocation) = frame["source"]["allocation_id"].as_str() {
                            watched.insert(allocation.to_owned());
                        }

                        booted.peer.send_json(frame);
                    } else {
                        send(&aimed(frame, &running, step + n), None);
                    }
                }
            }
            Action::Raw(text) => {
                if let Some(allocation) =
                    serde_json::from_str::<Value>(text).ok().and_then(|frame| frame["source"]["allocation_id"].as_str().map(str::to_owned))
                {
                    watched.insert(allocation);
                }

                booted.peer.send(text);
            }
            Action::Binary(bytes) => booted.peer.send_binary(vec![0x7b; *bytes]),
            // A request with one field far past anything the contract allows.
            Action::Oversized(bytes) => send(&json!({ "type": "console_command", "allocation_id": allocations()[0], "command": "x".repeat(*bytes) }), None),
            Action::Welcome => booted.peer.send_json(&json!({ "type": "welcome" })),
            Action::Advance(ms) => settle(Duration::from_millis(*ms)).await,
            Action::Crash(pick, code) => {
                let live = live_games(&booted.docker, &ended);

                if let Some(game) = live.get(pick % live.len().max(1)) {
                    ended.insert(game.id.clone());
                    end_game(&booted.docker, &game.id, *code);
                }
            }
            // The engine's backup of a round of a running game, which restore_round may then load.
            Action::WriteBackup(pick, round) => {
                let live = live_games(&booted.docker, &ended);

                if let Some(game) = live.get(pick % live.len().max(1)) {
                    // The slot's folder under the data folder, as the agent lays it out.
                    let dir = booted.data_dir.path().join("slots").join(&game.slot).join(BACKUP_DIR);

                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join(backup_file_name(u32::try_from(round - 1).unwrap())), "\"backup\"\n{\n}\n").unwrap();
                }
            }
            Action::DropLink(pick) => {
                let linked: Vec<String> = links.keys().cloned().collect();

                if let Some(id) = linked.get(pick % linked.len().max(1)) {
                    // Only a plugin waiting on its socket now loses it; no drop is kept for a later one.
                    links[id].drop.notify_waiters();
                }
            }
            // A demo the game recorded, which delete_demo may then remove.
            Action::WriteDemo(pick) => {
                let live = live_games(&booted.docker, &ended);

                if let Some(game) = live.get(pick % live.len().max(1)) {
                    let dir = booted.data_dir.path().join("slots").join(&game.slot).join(DEMO_DIR).join(&game.allocation);

                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join(DEMO_FILE), b"HL2DEMO\0").unwrap();
                }
            }
            // A game prints to its console, which the agent keeps as the game's log. Now and then a
            // line runs past the longest the agent keeps.
            Action::GameLog(pick, count) => {
                let live = live_games(&booted.docker, &ended);

                if let Some(game) = live.get(pick % live.len().max(1)) {
                    let lines = printed.entry(game.allocation.clone()).or_default();

                    for _ in 0..*count {
                        let mut line = format!("{} line {}", game.id, lines.len());

                        if lines.len() % 17 == 16 {
                            line.push_str(&"x".repeat(2_500));
                        }

                        booted.docker.line(game.index, &line);
                        lines.push(line);
                    }
                }
            }
            // The controller opens a game's log, asking for a replay of the lines kept so far.
            Action::WatchGame(pick) => {
                let games: Vec<String> = printed.keys().cloned().collect();

                if let Some(allocation) = games.get(pick % games.len().max(1)) {
                    let replay_id = format!("replay-{step}");

                    watched.insert(allocation.clone());
                    replays.push((replay_id.clone(), allocation.clone(), printed[allocation].len(), socket(&booted)));
                    booted
                        .peer
                        .send_json(&json!({ "type": "watch_logs", "source": { "kind": "server", "allocation_id": allocation }, "replay_id": replay_id }));
                }
            }
            Action::NewBuild(tell) => {
                let next = booted.steam.0.lock().unwrap().parse::<u64>().unwrap() + 1;

                *booted.steam.0.lock().unwrap() = next.to_string();

                if *tell {
                    booted.peer.send_json(&json!({ "type": "check_cs2_build" }));
                }
            }
            // A half-open link: the controller stops reading, as a stalled network does, then catches up.
            Action::PauseLink(ms) => {
                let accepted = booted.peer.accepted();

                paused = true;
                booted.peer.pause();
                settle(Duration::from_millis(*ms)).await;
                booted.peer.resume();
                settle(Duration::ZERO).await;

                if !booted.peer.connected() {
                    reconnected(&booted, accepted).await;
                }
            }
            Action::DropSocket => {
                let accepted = booted.peer.accepted();

                booted.peer.terminate();
                reconnected(&booted, accepted).await;
            }
        }

        settle(Duration::ZERO).await;
        finish_updates(&booted, &mut updates, &mut update_ends, &mut rng);
        settle(Duration::ZERO).await;
        assert!(panics.lock().unwrap().is_empty(), "seed {seed}, step {step} ({act:?}): the agent panicked");

        // The agent updates CS2 only on an idle node, and launches nothing while an update runs.
        let updating = running_updates(&booted.docker, &ended);
        let live = live_games(&booted.docker, &ended);

        assert!(updating.is_empty() || live.is_empty(), "seed {seed}, step {step} ({act:?}): {} game(s) run beside the CS2 update {updating:?}", live.len());

        // No game started at any moment between an update's start and its end, whether or not it
        // still runs: the start order Docker recorded is complete, where the samples above are not.
        let order: Vec<(String, String)> = booted
            .docker
            .started
            .lock()
            .unwrap()
            .iter()
            .map(|started| (started.id.clone(), started.spec.labels.get("ankor.role").cloned().unwrap_or_default()))
            .collect();

        for (start, (id, _)) in order.iter().enumerate().filter(|(_, (_, role))| role == "update") {
            let end = update_ends.get(id).copied().unwrap_or(order.len());
            let during: Vec<&String> = order.get(start + 1..end).unwrap_or_default().iter().filter(|(_, role)| role == "game").map(|(game, _)| game).collect();

            assert!(during.is_empty(), "seed {seed}, step {step} ({act:?}): game(s) {during:?} started while the CS2 update {id} ran");
        }

        tend(&booted, &mut links, &mut ended, seed);

        // Docker never runs two games in one slot.
        let mut per_slot: BTreeMap<String, usize> = BTreeMap::new();

        for game in live_games(&booted.docker, &ended) {
            *per_slot.entry(game.slot).or_default() += 1;
        }

        most_live = most_live.max(per_slot.values().sum::<usize>());

        if per_slot.values().any(|&count| count > 1) {
            for game in live_games(&booted.docker, &ended) {
                eprintln!("live {} slot {} allocation {}", game.id, game.slot, game.allocation);
            }

            eprintln!("removed {:?}", booted.docker.removed.lock().unwrap());
            eprintln!("steps {:?}", actions.iter().map(short).collect::<Vec<_>>());

            for line in booted.lines.lock().unwrap().iter().filter(|line| !line.contains("no backup of round")) {
                eprintln!("agent: {line}");
            }
        }

        assert!(per_slot.values().all(|&count| count <= 1), "seed {seed}, step {step} ({act:?}): two games in one slot: {per_slot:?}");
    }

    // The controller stops misbehaving.
    // In steps, so the games' plugins move their matches on as time passes.
    for _ in 0..QUIET.as_secs() / 10 {
        settle(Duration::from_secs(10)).await;
        tend(&booted, &mut links, &mut ended, seed);
    }

    // Every frame the agent sent is one the contract accepts.
    let refused = booted.peer.refused();

    assert!(refused.is_empty(), "seed {seed}: the agent sent frames the contract refuses:\n{}", refused.join("\n---\n"));

    if !booted.peer.connected() {
        reconnected(&booted, booted.peer.accepted()).await;
    }

    // Replies by the request they answer, read from every frame in one pass: the run sends hundreds
    // of requests, and reading the frames once per request would outlast the waits below.
    let reply_counts = || {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();

        for reply in booted.frames(|message| matches!(message, AgentToControllerMessage::Reply(_))) {
            *counts.entry(reply["reply_to"].as_str().unwrap_or_default().to_owned()).or_default() += 1;
        }

        counts
    };
    let open = socket(&booted);

    if std::env::var("FUZZ_DEBUG").is_ok() {
        for reply in booted.frames(|message| matches!(message, AgentToControllerMessage::Reply(_))) {
            if reply["ok"] == false && reply["error"]["code"] == "failed" {
                let id = reply["reply_to"].as_str().unwrap_or_default();

                eprintln!("failed {id} {:?}: {}", sent_type.get(id), reply["error"]["detail"]);
            }
        }

        let counts = reply_counts();
        let replies = |id: &str| counts.get(id).copied().unwrap_or_default();
        let mut by_step: Vec<(usize, String, String, usize)> = sent_on
            .iter()
            .filter(|(_, sent)| **sent == open)
            .map(|(id, _)| {
                let (kind, step) = sent_type.get(id).cloned().unwrap_or_default();
                (step, id.clone(), kind, replies(id))
            })
            .collect();

        by_step.sort();

        for (step, id, kind, count) in by_step {
            eprintln!("step {step:>2} {id:<9} {kind:<18} replies {count}");
        }

        eprintln!("open {open:?} accepted {} disconnects {:?} closes {:?}", booted.peer.accepted(), booted.peer.disconnects(), booted.peer.closes());

        for line in booted.lines.lock().unwrap().iter().filter(|line| !line.contains("no backup of round")) {
            eprintln!("agent: {line}");
        }
    }

    // A paused clock lets an idle runtime move time on instead of waiting for the sockets, so the
    // replies owed on the open socket get real time to arrive, on a running clock, before they count.
    tokio::time::resume();

    let owed: Vec<&String> = sent_on.iter().filter(|(_, sent)| sent.is_some() && **sent == open).map(|(id, _)| id).collect();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    while std::time::Instant::now() < deadline && {
        let counts = reply_counts();

        owed.iter().any(|id| !counts.contains_key(id.as_str()))
    } {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    tokio::time::pause();

    let counts = reply_counts();
    let replies = |id: &str| counts.get(id).copied().unwrap_or_default();

    // A launch is refused as reused only for an allocation started on this node before, and not once
    // the node took on as many newer launches as it remembers. Requests count in the order they went
    // out, so a launch whose reply was lost with its socket still counts as started; a newer launch
    // counts once its reply showed the node took it on.
    let order = |id: &str| id.strip_prefix("fuzz-").and_then(|number| number.parse::<u64>().ok()).unwrap_or(u64::MAX);
    let starts: Vec<(u64, String)> = sent_frames
        .iter()
        .filter(|(_, frame)| frame["type"] == "start_server")
        .filter_map(|(id, frame)| Some((order(id), frame["allocation_id"].as_str()?.to_owned())))
        .collect();
    let all_replies = booted.frames(|message| matches!(message, AgentToControllerMessage::Reply(_)));
    let taken_on: Vec<u64> = all_replies
        .iter()
        .filter(|reply| sent_frames.get(reply["reply_to"].as_str().unwrap_or_default()).is_some_and(|frame| frame["type"] == "start_server"))
        .filter(|reply| reply["ok"] == true || reply["error"]["code"] == "failed")
        .map(|reply| order(reply["reply_to"].as_str().unwrap_or_default()))
        .collect();

    for reply in all_replies.iter().filter(|reply| reply["error"]["code"] == "allocation_reused") {
        let id = reply["reply_to"].as_str().unwrap_or_default();
        let refused = order(id);
        let Some(allocation) = sent_frames.get(id).and_then(|frame| frame["allocation_id"].as_str()) else { continue };
        let earlier = starts.iter().filter(|(number, start)| *number < refused && start == allocation).map(|(number, _)| *number).max();

        let Some(last_start) = earlier else {
            panic!("seed {seed}: {id} refused {allocation} as reused, which was never started on this node");
        };

        let newer = taken_on.iter().filter(|&&number| number > last_start && number < refused).count();

        assert!(
            newer < usize::try_from(REMEMBERED_ALLOCATIONS).unwrap(),
            "seed {seed}: {id} refused {allocation} as reused after {newer} newer launches, past the {REMEMBERED_ALLOCATIONS} the node remembers"
        );
    }

    // Replays owed on the open socket get real time to arrive too.
    let owed_replays: Vec<&String> = replays
        .iter()
        .filter(|(_, _, before, sent_on_socket)| *before > 0 && !paused && sent_on_socket.is_some() && *sent_on_socket == open)
        .map(|(replay_id, ..)| replay_id)
        .collect();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    while std::time::Instant::now() < deadline
        && owed_replays.iter().any(|replay_id| {
            !booted.frames(|message| matches!(message, AgentToControllerMessage::LogBatch(_))).iter().any(|batch| batch["replay_id"] == replay_id.as_str())
        })
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // A game's log holds only what that game printed, a long line cut to its start, and the agent
    // sends a game's lines only to a controller that watched them.
    let batches: Vec<Value> = booted.frames(|message| matches!(message, AgentToControllerMessage::LogBatch(_)));

    for batch in batches.iter().filter(|batch| batch["source"]["kind"] == "server") {
        let allocation = batch["source"]["allocation_id"].as_str().unwrap_or_default();
        let lines = printed.get(allocation).map(Vec::as_slice).unwrap_or_default();

        if batch["lines"].as_array().is_some_and(|sent| !sent.is_empty()) {
            assert!(watched.contains(allocation), "seed {seed}: the agent sent the log of {allocation}, which no watch_logs named");
        }

        for line in batch["lines"].as_array().unwrap() {
            let text = line["line"].as_str().unwrap_or_default();

            assert!(
                lines.iter().any(|printed| printed.starts_with(text) && !text.is_empty()),
                "seed {seed}: {allocation}'s log has a line it never printed: {text:.80}"
            );
        }
    }

    // A replay starts with reset, and holds the game's lines in order without a gap: the newest it
    // kept, its oldest left out when the log budget runs short.
    for (replay_id, allocation, before, sent_on_socket) in &replays {
        let frames: Vec<&Value> = batches.iter().filter(|batch| batch["replay_id"] == replay_id.as_str()).collect();
        let lines = &printed[allocation];

        if let Some(first) = frames.first() {
            assert_eq!(first["reset"], true, "seed {seed}: {replay_id} of {allocation} does not start with reset");
        } else if *before > 0 && !paused && sent_on_socket.is_some() && *sent_on_socket == open {
            panic!("seed {seed}: no replay {replay_id} of {allocation}, which had printed {before} lines, on the socket still open");
        }

        let replayed: Vec<&str> =
            frames.iter().flat_map(|frame| frame["lines"].as_array().unwrap().iter().map(|line| line["line"].as_str().unwrap_or_default())).collect();

        if let Some(first) = replayed.first() {
            let start = lines.iter().position(|line| line.starts_with(first)).unwrap();

            for (offset, line) in replayed.iter().enumerate() {
                assert!(
                    lines.get(start + offset).is_some_and(|printed| printed.starts_with(line)),
                    "seed {seed}: {replay_id} of {allocation} skips or reorders lines at {offset}: {line:.80}"
                );
            }
        }
    }

    // A launch sent while the agent reported a CS2 game update running, or a build behind the newest
    // with auto_update on, did not start.
    // A report that cleared the reason, sent before the reply, excuses it: frames on a socket keep
    // their order, so the agent changed its mind before it took the launch on.
    let received = booted.peer.received();

    for (id, (why, seen)) in &must_refuse {
        let Some(at) = received.iter().position(|frame| frame["type"] == "reply" && frame["reply_to"] == id.as_str()) else { continue };
        let cleared = received.get(*seen..at).unwrap_or_default().iter().any(|frame| {
            let report = frame["type"] == "hello" || frame["type"] == "node_status";
            let off = frame["type"] == "reply" && frame["request"] == "set_config" && frame["data"]["auto_update"] == false;
            let caught_up = frame["cs2"]["build_id"].is_string() && frame["cs2"]["build_id"] == frame["cs2"]["latest_build_id"];

            // Turning auto_update off lifts the outdated build's refusal, and only that one.
            match *why {
                "a CS2 update" => report && frame["unavailable"]["operation"] != "cs2_update",
                _ => off || (report && caught_up),
            }
        });

        assert!(cleared || received[at]["ok"] != true, "seed {seed}: {id} started a server while the agent reported {why}");

        if cleared && received[at]["ok"] == true {
            let excuse = received[*seen..at].iter().rev().find(|frame| frame["type"] == "node_status" || frame["type"] == "hello").cloned().unwrap_or_default();

            eprintln!(
                "excused {seed} {id} ({why}): cleared by {} unavailable {} build {} latest {}",
                excuse["type"], excuse["unavailable"], excuse["cs2"]["build_id"], excuse["cs2"]["latest_build_id"]
            );
        }
    }

    // An update to an image outside the official repositories is refused as such.
    for (id, code) in &expected {
        for reply in booted.frames(|message| matches!(message, AgentToControllerMessage::Reply(_))).iter().filter(|reply| reply["reply_to"] == id.as_str()) {
            assert_eq!(
                reply["error"]["code"],
                json!(code),
                "seed {seed}: {id} ({:?}) asked for an image outside the official repositories; reply {reply}",
                sent_type.get(id)
            );
        }
    }

    for (id, sent) in &sent_on {
        let count = replies(id);

        assert!(count <= if doubled { 2 } else { 1 }, "seed {seed}: {count} replies to {id}");

        if sent.is_some() && *sent == open {
            assert!(
                count >= 1,
                "seed {seed}: no reply to {id} ({:?}), sent on the socket still open; steps: {:?}",
                sent_type.get(id),
                actions.iter().map(short).collect::<Vec<_>>()
            );
        }
    }

    // A fresh hello is the agent's whole account of the node: it must match what Docker runs. A
    // launch marks its slot starting before it writes the allocation and the game's files, which
    // take real time that the paused clock does not wait for, so a launch still preparing them is
    // given real time to reach Docker first.
    for _ in 0..40 {
        let accepted = booted.peer.accepted();

        booted.peer.terminate();
        reconnected(&booted, accepted).await;

        let running: BTreeSet<String> = live_games(&booted.docker, &ended).into_iter().map(|game| game.allocation).collect();
        let hello = booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).last().cloned().unwrap();
        let preparing = hello["slot_status"]
            .as_array()
            .unwrap()
            .iter()
            .any(|status| status["state"] == "starting" && !running.contains(status["allocation_id"].as_str().unwrap_or_default()));

        if !preparing {
            break;
        }

        tokio::time::resume();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::pause();
    }

    let hello = booted.frames(|message| matches!(message, AgentToControllerMessage::Hello(_))).last().cloned().unwrap();
    let configured: Vec<String> = hello["config"]["slots"].as_array().unwrap().iter().map(|slot| slot["id"].as_str().unwrap().to_owned()).collect();

    // A game that ended just before the hello is gone from Docker while its exit still waits in the
    // agent's Docker event stream, so the hello can name it. The agent reports the slot again once it
    // reads the exit, which real time lets it do; a report still wrong after that fails below.
    for _ in 0..40 {
        let live = live_games(&booted.docker, &ended);
        let stale = configured.iter().any(|slot| {
            let status = booted.peer.status(slot);
            match live.iter().find(|game| &game.slot == slot) {
                None => status.is_none_or(|status| status["state"] != "idle"),
                Some(game) => status.is_none_or(|status| status["allocation_id"] != game.allocation.as_str()),
            }
        });

        if !stale {
            break;
        }

        tokio::time::resume();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::pause();
    }

    let live = live_games(&booted.docker, &ended);

    // A game can only run in a slot the node still has.
    assert!(live.iter().all(|game| configured.contains(&game.slot)), "seed {seed}: a game runs in a slot the node no longer has");

    for slot in &configured {
        let status = booted.peer.status(slot);
        let running: Vec<&Game> = live.iter().filter(|game| &game.slot == slot).collect();

        match running.first() {
            None => assert_eq!(status.as_ref().map(|status| status["state"].clone()), Some(json!("idle")), "seed {seed}: slot {slot} runs no game"),
            Some(game) => {
                assert_eq!(status.as_ref().map(|status| status["allocation_id"].clone()), Some(json!(game.allocation)), "seed {seed}: slot {slot}");
            }
        }
    }

    assert!(panics.lock().unwrap().is_empty(), "seed {seed}: the agent panicked");

    let codes =
        booted.frames(|message| matches!(message, AgentToControllerMessage::Reply(_))).iter().fold(BTreeMap::<String, usize>::new(), |mut codes, reply| {
            let code = if reply["ok"] == true { "ok".to_owned() } else { reply["error"]["code"].as_str().unwrap_or("failed").to_owned() };

            *codes.entry(code).or_default() += 1;
            codes
        });

    let answered_replays = replays.iter().filter(|(replay_id, ..)| batches.iter().any(|batch| batch["replay_id"] == replay_id.as_str())).count();

    eprintln!(
        "seed {seed}: up to {most_live} games at once, {} lines printed, {answered_replays} of {} replays answered, {} log batches",
        printed.values().map(Vec::len).sum::<usize>(),
        replays.len(),
        batches.len()
    );
    eprintln!(
        "seed {seed}: {} actions, {} games, {} sockets, replies {codes:?}",
        actions.len(),
        booted.docker.started.lock().unwrap().len(),
        booted.peer.accepted()
    );

    for link in links.into_values() {
        link.task.abort();
    }

    let _ = booted.agent.shutdown(Signal::Sigterm).await;
    // The next run boots on a running clock, and pauses it again.
    tokio::time::resume();
}

#[tokio::test]
async fn stays_up_answers_each_request_once_and_ends_up_telling_the_controller_the_truth() {
    let panics = panic_log();
    let seeds: Vec<u64> = match std::env::var("FUZZ_SEED") {
        Ok(seed) => vec![seed.parse().expect("FUZZ_SEED is a number")],
        Err(_) => {
            let runs: u64 = std::env::var("FUZZ_RUNS").ok().and_then(|runs| runs.parse().ok()).unwrap_or(8);
            let first = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

            (0..runs).map(|run| first.wrapping_mul(1_000).wrapping_add(run)).collect()
        }
    };

    for seed in seeds {
        eprintln!("seed {seed}: starting");
        run(seed, &panics).await;
    }
}
