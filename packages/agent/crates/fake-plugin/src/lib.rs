//! The Ankor plugin as the agent meets it on its relay socket, for the agent's fuzz test, which runs
//! in process on a paused clock. The conformance suite runs the plugin itself instead, from its
//! headless image. [`Plugin`] holds what the real plugin in `packages/plugin`
//! knows about its server and match, and answers the agent's frames as it does. A caller owns the
//! socket and the clock: it sends the frames each method returns, and decides when the map loads,
//! players join and the match moves on.
//!
//! The rules cite the C# they follow, so a change to the plugin points here.

use std::time::Duration;

use ankor_protocol::constants::{PROTOCOL_HEADER, PROTOCOL_VERSION, agent_errors};
use ankor_protocol::helpers::from_wire_name;
use ankor_protocol::types::{MatchPhase, PlayingSide, SidesMode};
use serde_json::{Map, Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

/// Whether the engine has the backup of a round, by its one-based number.
pub type Backups = Box<dyn Fn(u64) -> bool + Send + Sync>;

/// What the plugin does with a frame from the agent.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    /// Sends these frames back, in order: the answer, after any event the request caused.
    Reply(Vec<Value>),
    /// Drops the socket, as the plugin does when the frame breaks its stream, and reconnects.
    Drop,
    /// Nothing goes back: an acknowledgement, or a frame the plugin cannot read.
    Nothing,
}

/// A game server's plugin: its status, its match and the events not yet acknowledged.
#[allow(clippy::struct_excessive_bools, reason = "each flag mirrors one field of the plugin's match state")]
pub struct Plugin {
    status: Map<String, Value>,
    /// The launch's match, as the agent wrote it to `match.json`. `None` without one, or after
    /// [`Plugin::reject_config`].
    game: Option<Value>,
    map_loaded: bool,
    /// `start_match` was accepted and its countdown runs (`MatchState.StartCountdown`). A second
    /// start meanwhile is accepted and changes nothing.
    counting_down: bool,
    /// The match is going live and waits for its round start, still in warmup (`_goingLive`).
    going_live: bool,
    /// The side swap at halftime is pending (`SidesSwapPending`).
    halftime: bool,
    /// Events sent and not yet acknowledged, by seq, which a reconnect sends again.
    unacked: Vec<(u64, Value)>,
    next_seq: u64,
    backups: Backups,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin").field("status", &self.status).field("next_seq", &self.next_seq).finish_non_exhaustive()
    }
}

impl Plugin {
    /// A plugin that has just loaded on `map_name`, with the launch's match if it has one: not yet
    /// ready, its match loading, and no round backups.
    #[must_use]
    pub fn new(game: Option<Value>, map_name: &str) -> Self {
        let status = json!({
            "map_name": map_name,
            "match": game.as_ref().map(|game| match_status(game, MatchPhase::Loading)),
            "players": [],
            "config_error": null,
            "ready": false,
            "server_steam_id": null,
        });

        Self {
            status: status.as_object().cloned().unwrap_or_default(),
            game,
            map_loaded: false,
            counting_down: false,
            going_live: false,
            halftime: false,
            unacked: Vec::new(),
            next_seq: 0,
            backups: Box::new(|_| false),
        }
    }

    /// Reads round backups through `backups`, as the engine's files on the server's disk.
    #[must_use]
    pub fn with_backups(mut self, backups: Backups) -> Self {
        self.backups = backups;
        self
    }

    /// The `server_status` frame, answering `request_id`, or unasked when it is `None`.
    #[must_use]
    pub fn status(&self, request_id: Option<&str>) -> Value {
        let mut frame = self.status.clone();

        frame.insert("type".to_owned(), json!("server_status"));
        frame.insert("request_id".to_owned(), json!(request_id));
        Value::Object(frame)
    }

    /// Whether the match sends gameplay events, which only `send_statistics` turns on.
    #[must_use]
    pub fn statistics(&self) -> bool {
        self.game.as_ref().is_some_and(|game| game.get("send_statistics") == Some(&json!(true)))
    }

    #[must_use]
    pub fn phase(&self) -> Option<MatchPhase> {
        from_wire_name(self.status.get("match")?.get("phase")?.as_str()?)
    }

    #[must_use]
    pub fn ready(&self) -> bool {
        self.status.get("ready") == Some(&json!(true))
    }

    #[must_use]
    pub fn map_loaded(&self) -> bool {
        self.map_loaded
    }

    /// The countdown of an accepted `start_match` runs, until [`Plugin::begin_match`].
    #[must_use]
    pub fn counting_down(&self) -> bool {
        self.counting_down
    }

    /// Team 1's and team 2's score.
    #[must_use]
    pub fn score(&self) -> (u64, u64) {
        let read = |key: &str| self.status.get("match").and_then(|game| game.get(key)).and_then(Value::as_u64).unwrap_or(0);

        (read("team1_score"), read("team2_score"))
    }

    /// Every event sent has been acknowledged, which the plugin waits for before `quitting`.
    #[must_use]
    pub fn all_acked(&self) -> bool {
        self.unacked.is_empty()
    }

    /// The frames for a socket that just opened: the status, unasked, and every event not yet
    /// acknowledged (`WsClient.cs`, `AgentLink.cs`).
    #[must_use]
    pub fn connected(&self) -> Vec<Value> {
        let mut frames = vec![self.status(None)];

        frames.extend(self.unacked.iter().map(|(_, frame)| frame.clone()));
        frames
    }

    /// A map loaded: the launch's, a workshop map, or one a `changelevel` asked for. The match
    /// reports `map_start` unless it ended, and waits in `loading` until [`Plugin::enter_warmup`],
    /// which the plugin runs a second later (`LoadedMatch.OnMapLoad`, `GameplayCallbacks.MapLoad`).
    pub fn load_map(&mut self) -> Vec<Value> {
        self.map_loaded = true;

        // An ended match ignores the reload altogether (`GameplayCallbacks.MapLoad`).
        if self.game.is_none() || self.phase() == Some(MatchPhase::PostLive) {
            return Vec::new();
        }

        let map = self.status.get("map_name").cloned().unwrap_or(Value::Null);
        let mut frames: Vec<Value> = self
            .emit(&json!({ "type": "map_start", "timestamp": 1_700_000_000_000_u64, "tick": 0, "plugin_version": "0.0.0-fake", "framework_version": "0.0.0-fake", "server_version": "0.0.0-fake", "map_name": map }))
            .into_iter()
            .collect();

        frames.extend(self.set_phase(MatchPhase::Loading));
        frames
    }

    /// Warmup starts over: no countdown, no swap pending, and the score back at 0:0
    /// (`MatchState.EnterWarmup`).
    pub fn enter_warmup(&mut self) -> Vec<Value> {
        self.counting_down = false;
        self.halftime = false;

        let mut frames: Vec<Value> = self.set_phase(MatchPhase::Warmup).into_iter().collect();

        frames.extend(self.set_score(0, 0));
        frames
    }

    /// The countdown ended: a knife match starts its knife round, and any other goes live, still in
    /// warmup until its round starts (`MatchState.BeginMatch`, `GoLive`).
    pub fn begin_match(&mut self, knife: bool) -> Vec<Value> {
        self.counting_down = false;

        if knife {
            return self.set_phase(MatchPhase::Knife).into_iter().collect();
        }

        self.going_live = true;
        Vec::new()
    }

    /// Halftime: team 1 changes sides now, and the swap stays pending until [`Plugin::end_halftime`]
    /// (`MatchState.SwapSides`).
    pub fn swap_sides(&mut self) -> Vec<Value> {
        let Some(game) = self.status.get_mut("match").and_then(Value::as_object_mut) else {
            return Vec::new();
        };
        let (team1, team2) = (game.get("team2_side").cloned().unwrap_or(Value::Null), game.get("team1_side").cloned().unwrap_or(Value::Null));
        let message = format!("halftime: team 1 is now {}", team1.as_str().unwrap_or_default());

        game.insert("team1_side".to_owned(), team1);
        game.insert("team2_side".to_owned(), team2);
        self.halftime = true;
        self.emit(&self.log(&message)).into_iter().collect()
    }

    pub fn end_halftime(&mut self) {
        self.halftime = false;
    }

    /// A match log line as the plugin emits it.
    #[must_use]
    pub fn log(&self, message: &str) -> Value {
        json!({ "type": "log", "timestamp": 1_700_000_000_000_u64 + self.next_seq, "tick": self.next_seq, "level": "info", "message": message })
    }

    /// The server is logged on to Steam with its map loaded. The status goes out unasked when this
    /// turns it ready, and `ready` never turns back (`ServerReadiness.cs`).
    pub fn turn_ready(&mut self) -> Option<Value> {
        if self.ready() {
            return None;
        }

        self.status.insert("ready".to_owned(), json!(true));
        Some(self.status(None))
    }

    /// The match moved to `phase`, which the plugin reports, when it changed, whether or not it
    /// sends statistics. Any phase set ends a going-live that has not reached its round start
    /// (`MatchState.Phase`).
    pub fn set_phase(&mut self, phase: MatchPhase) -> Option<Value> {
        let changed = self.phase() != Some(phase);
        let game = self.status.get_mut("match").and_then(Value::as_object_mut)?;

        game.insert("phase".to_owned(), json!(phase));
        self.going_live = false;
        changed.then(|| json!({ "type": "phase_change", "phase": phase }))
    }

    /// The scoreboard changed, which the plugin reports when it did (`MatchState.SetScore`).
    pub fn set_score(&mut self, team1: u64, team2: u64) -> Option<Value> {
        let changed = self.score() != (team1, team2);
        let game = self.status.get_mut("match").and_then(Value::as_object_mut)?;

        game.insert("team1_score".to_owned(), json!(team1));
        game.insert("team2_score".to_owned(), json!(team2));
        changed.then(|| json!({ "type": "score", "team1_score": team1, "team2_score": team2 }))
    }

    /// A rostered player reached the round restored from `round`'s backup: the match is live again
    /// on that round's score, which `match_restored` reports (`MatchState.RestoreRound`).
    pub fn restored(&mut self, round: u64, team1: u64, team2: u64) -> Vec<Value> {
        let side = self.status.get("match").and_then(|game| game.get("team1_side")).cloned().unwrap_or(Value::Null);
        let mut frames: Vec<Value> = self.set_phase(MatchPhase::Live).into_iter().collect();

        frames.extend(self.set_score(team1, team2));
        frames.extend(self.emit(&json!({
            "type": "match_restored", "timestamp": 1_700_000_000_000_u64 + self.next_seq, "tick": self.next_seq,
            "round": round, "team1_side": side, "team1_score": team1, "team2_score": team2,
        })));
        frames
    }

    /// A player connected, on `side`. The status shows them from the agent's next read, because
    /// the plugin sends no status for it.
    pub fn join(&mut self, steam_id: &str, name: &str, side: &str) {
        if let Some(players) = self.status.get_mut("players").and_then(Value::as_array_mut) {
            players.retain(|player| player.get("steam_id").and_then(Value::as_str) != Some(steam_id));
            players.push(json!({ "steam_id": steam_id, "name": name, "side": side }));
        }
    }

    /// A gameplay event, kept until it is acknowledged. `None` for a match that sends no
    /// statistics, or after it ended, when events no longer leave the plugin (`MatchState.Emit`).
    pub fn emit(&mut self, event: &Value) -> Option<Value> {
        if !self.statistics() || self.phase() == Some(MatchPhase::PostLive) {
            return None;
        }

        let seq = self.next_seq;
        let frame = json!({ "type": "event", "seq": seq, "event": event });

        self.next_seq += 1;
        self.unacked.push((seq, frame.clone()));
        Some(frame)
    }

    /// The match config did not decode: no match, and `message` in every status.
    pub fn reject_config(&mut self, message: &str) {
        self.game = None;
        self.counting_down = false;
        self.going_live = false;
        self.status.insert("match".to_owned(), Value::Null);
        self.status.insert("config_error".to_owned(), json!(message));
    }

    /// The plugin's answer to one frame from the agent, as `AgentLink.HandleInbound` and
    /// `WsClient.cs` give it.
    pub fn answer(&mut self, frame: &Value) -> Inbound {
        let Some(kind) = frame.get("type").and_then(Value::as_str) else {
            return Inbound::Nothing;
        };
        let request_id = frame.get("request_id").and_then(Value::as_str).map(str::to_owned);
        // A command without its id does not parse, and the plugin only logs it.
        let command = |error: Option<&str>| request_id.as_ref().map(|id| json!({ "type": "command_result", "request_id": id, "error": error }));

        match kind {
            "get_server_status" => match frame.get("request_id") {
                None | Some(Value::Null) => Inbound::Reply(vec![self.status(None)]),
                Some(Value::String(id)) => Inbound::Reply(vec![self.status(Some(id))]),
                Some(_) => Inbound::Nothing,
            },
            // Answered before the command runs, so it never fails; `command` defaults to empty
            // (`AgentLink.cs`, `AgentToPlugin.cs`).
            "exec" => command(None).map_or(Inbound::Nothing, |result| Inbound::Reply(vec![result])),
            "start_match" => {
                if request_id.is_none() {
                    return Inbound::Nothing;
                }

                let refusal = self.start_refusal();
                let mut frames = Vec::new();

                if refusal.is_none() {
                    frames.extend(self.emit(&self.log("start_match from the controller")));
                    self.counting_down = true;
                }

                frames.extend(command(refusal));
                Inbound::Reply(frames)
            }
            // `round` is an int, so a larger one does not parse (`AgentToPlugin.cs`).
            "restore_round" => match frame.get("round").and_then(Value::as_u64).filter(|round| i32::try_from(*round).is_ok()) {
                Some(round) => command(self.restore_refusal(round)).map_or(Inbound::Nothing, |result| Inbound::Reply(vec![result])),
                None => Inbound::Nothing,
            },
            "ack" => match frame.get("to_seq").and_then(Value::as_u64) {
                Some(to) => self.ack(to),
                None => Inbound::Nothing,
            },
            // A type the plugin does not know, refused once it can name the request.
            _ => command(Some(agent_errors::UNSUPPORTED_REQUEST)).map_or(Inbound::Nothing, |result| Inbound::Reply(vec![result])),
        }
    }

    /// Discards the events up to `to`. An ack past the last event sent breaks the stream, and the
    /// plugin drops the socket (`WsClient.cs`).
    fn ack(&mut self, to: u64) -> Inbound {
        if to >= self.next_seq {
            return Inbound::Drop;
        }

        self.unacked.retain(|(seq, _)| *seq > to);
        Inbound::Nothing
    }

    /// Why the plugin refuses `start_match` now (`MatchState.StartFromController`, `ReadyingUp`).
    fn start_refusal(&self) -> Option<&'static str> {
        let Some(game) = &self.game else {
            return Some(agent_errors::NO_MATCH);
        };

        if self.phase() != Some(MatchPhase::Warmup) || self.going_live {
            // A countdown already running is no reason to refuse.
            return Some(agent_errors::NOT_IN_WARMUP);
        }

        let present: Vec<&str> =
            self.status.get("players").and_then(Value::as_array).into_iter().flatten().filter_map(|player| player.get("steam_id")?.as_str()).collect();
        let on_team = |team: &str| {
            game.pointer(&format!("/roster/{team}/players"))
                .and_then(Value::as_array)
                .is_some_and(|players| players.iter().any(|id| id.as_str().is_some_and(|id| present.contains(&id))))
        };

        (!(on_team("team1") && on_team("team2"))).then_some(agent_errors::TEAM_EMPTY)
    }

    /// Why the plugin refuses `restore_round` now, in the order `RoundBackups.Restore` and then
    /// `MatchState.RestoreRound` check. Round 0 names a backup file that never exists.
    fn restore_refusal(&self, round: u64) -> Option<&'static str> {
        if self.game.is_none() {
            Some(agent_errors::NO_MATCH)
        } else if round == 0 || !(self.backups)(round) {
            Some(agent_errors::NO_BACKUP)
        } else if self.phase() != Some(MatchPhase::Live) {
            Some(agent_errors::NOT_LIVE)
        } else if self.halftime {
            Some(agent_errors::AT_HALFTIME)
        } else {
            None
        }
    }
}

/// The match part of a status, as the plugin reads it off its match state.
fn match_status(game: &Value, phase: MatchPhase) -> Value {
    let team1_t = game.get("sides").and_then(Value::as_str).and_then(from_wire_name::<SidesMode>) == Some(SidesMode::Team1T);
    let (team1, team2) = if team1_t { (PlayingSide::T, PlayingSide::Ct) } else { (PlayingSide::Ct, PlayingSide::T) };

    json!({ "match_id": game.get("match_id").cloned().unwrap_or(Value::Null), "phase": phase, "team1_side": team1, "team2_side": team2, "team1_score": 0, "team2_score": 0 })
}

/// The plugin's wait before it dials the relay again: from 100 ms, doubled after each attempt up
/// to 5 s, and back to 100 ms after a socket that lasted 10 s or saw an ack (`WsClient.cs`). The
/// caller adds the plugin's 0 to 100 ms of jitter.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: Self::FIRST }
    }
}

impl Backoff {
    pub const FIRST: Duration = Duration::from_millis(100);
    pub const MAX: Duration = Duration::from_secs(5);
    /// A socket open this long starts the backoff over.
    pub const STABLE: Duration = Duration::from_secs(10);

    /// The wait after a socket that was open for `open`, or that never opened, and whether the
    /// socket saw an ack.
    pub fn after(&mut self, open: Option<Duration>, acked: bool) -> Duration {
        if acked || open.is_some_and(|open| open >= Self::STABLE) {
            self.next = Self::FIRST;
        }

        let wait = self.next;

        self.next = (self.next * 2).min(Self::MAX);
        wait
    }
}

pub type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Dials the relay at `url` with the protocol version header, as the plugin does.
///
/// # Errors
///
/// When the agent refuses the socket or cannot be reached.
pub async fn connect(url: &str) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = url.into_client_request()?;

    if let Ok(version) = PROTOCOL_VERSION.parse() {
        request.headers_mut().insert(PROTOCOL_HEADER, version);
    }

    Ok(tokio_tungstenite::connect_async(request).await?.0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "a test fails by crashing")]

    use super::*;

    fn game() -> Value {
        json!({
            "match_id": "0199a0b4-5c00-7000-8000-000000000001",
            "send_statistics": true,
            "sides": "knife",
            "roster": { "team1": { "players": ["76561198000000001"] }, "team2": { "players": ["76561198000000002"] }, "spectators": [] },
        })
    }

    /// The `command_result` among the frames a request got back.
    fn result(inbound: Inbound) -> Value {
        match inbound {
            Inbound::Reply(frames) => frames.into_iter().find(|frame| frame["type"] == "command_result").unwrap()["error"].clone(),
            other => panic!("no reply: {other:?}"),
        }
    }

    /// A plugin whose map loaded and whose match is in warmup.
    fn in_warmup(game: Value) -> Plugin {
        let mut plugin = Plugin::new(Some(game), "de_mirage");
        let _ = plugin.load_map();
        let _ = plugin.enter_warmup();
        plugin
    }

    #[test]
    fn starts_a_match_in_warmup_once_both_teams_have_a_player_and_accepts_it_again_during_the_countdown() {
        let mut plugin = Plugin::new(Some(game()), "de_mirage");
        let start = json!({ "type": "start_match", "request_id": "r" });

        assert!(!plugin.map_loaded());
        let _ = plugin.load_map();
        assert!(plugin.map_loaded());
        assert_eq!(result(plugin.answer(&start)), json!("not_in_warmup"));
        let _ = plugin.enter_warmup();
        assert_eq!(result(plugin.answer(&start)), json!("team_empty"));
        plugin.join("76561198000000001", "a", "ct");
        assert_eq!(result(plugin.answer(&start)), json!("team_empty"));
        plugin.join("76561198000000002", "b", "t");
        assert!(!plugin.counting_down());

        let Inbound::Reply(frames) = plugin.answer(&start) else { panic!("no reply") };
        assert_eq!(frames[0]["event"]["message"], json!("start_match from the controller"));
        assert_eq!(frames[1]["error"], Value::Null);
        assert!(plugin.counting_down());
        assert_eq!(result(plugin.answer(&start)), Value::Null);
        // Going live waits in warmup for its round start, and refuses a start meanwhile.
        let _ = plugin.begin_match(false);
        assert_eq!(result(plugin.answer(&start)), json!("not_in_warmup"));
    }

    #[test]
    fn looks_for_the_backup_before_it_asks_whether_the_match_is_live() {
        let mut plugin = in_warmup(game()).with_backups(Box::new(|round| round == 3));
        let restore = |round: u64| json!({ "type": "restore_round", "round": round, "request_id": "r" });

        assert_eq!(result(plugin.answer(&restore(0))), json!("no_backup"));
        assert_eq!(result(plugin.answer(&restore(2))), json!("no_backup"));
        assert_eq!(result(plugin.answer(&restore(3))), json!("not_live"));
        let _ = plugin.set_phase(MatchPhase::Live);
        let _ = plugin.swap_sides();
        assert_eq!(plugin.status(None)["match"]["team1_side"], json!("t"));
        assert_eq!(result(plugin.answer(&restore(3))), json!("at_halftime"));
        plugin.end_halftime();
        assert_eq!(result(plugin.answer(&restore(3))), Value::Null);
        let restored = plugin.restored(3, 2, 1);
        assert_eq!(restored[0], json!({ "type": "score", "team1_score": 2, "team2_score": 1 }));
        // `match_restored` names the side team 1 plays after the swap.
        assert_eq!(restored[1]["event"]["team1_side"], json!("t"));
        assert_eq!(plugin.answer(&restore(u64::from(u32::MAX))), Inbound::Nothing);
    }

    #[test]
    fn refuses_commands_without_a_match_and_never_refuses_exec() {
        let mut plugin = in_warmup(game());

        plugin.reject_config("bad roster");
        assert_eq!(result(plugin.answer(&json!({ "type": "start_match", "request_id": "r" }))), json!("no_match"));
        assert_eq!(result(plugin.answer(&json!({ "type": "restore_round", "round": 1, "request_id": "r" }))), json!("no_match"));
        assert_eq!(result(plugin.answer(&json!({ "type": "exec", "command": "quit", "request_id": "r" }))), Value::Null);
        assert_eq!(result(plugin.answer(&json!({ "type": "exec", "request_id": "r" }))), Value::Null);
        assert_eq!(plugin.status(None)["config_error"], json!("bad roster"));
    }

    #[test]
    fn answers_unknown_requests_and_ignores_frames_it_cannot_read() {
        let mut plugin = Plugin::new(None, "de_dust2");

        assert_eq!(result(plugin.answer(&json!({ "type": "teleport", "request_id": "r" }))), json!("unsupported_request"));
        assert_eq!(plugin.answer(&json!({ "type": "exec", "command": "status" })), Inbound::Nothing);
        assert_eq!(plugin.answer(&json!({ "type": "start_match" })), Inbound::Nothing);
        assert_eq!(plugin.answer(&json!("text")), Inbound::Nothing);
    }

    #[test]
    fn resends_unacknowledged_events_and_drops_the_socket_on_an_ack_past_them() {
        let mut plugin = Plugin::new(Some(game()), "de_mirage");
        let loaded = plugin.load_map();

        assert_eq!(loaded[0]["event"]["type"], json!("map_start"));
        // Already loading, so no phase change until warmup.
        assert_eq!(loaded.len(), 1);
        assert!(!plugin.all_acked());
        assert_eq!(plugin.enter_warmup(), [json!({ "type": "phase_change", "phase": "warmup" })]);
        let _ = plugin.emit(&json!({ "type": "log" }));
        assert_eq!(plugin.answer(&json!({ "type": "ack", "to_seq": 0 })), Inbound::Nothing);
        assert_eq!(plugin.connected().iter().filter(|frame| frame["type"] == "event").map(|frame| frame["seq"].clone()).collect::<Vec<_>>(), [json!(1)]);
        assert_eq!(plugin.answer(&json!({ "type": "ack", "to_seq": 2 })), Inbound::Drop);
        assert_eq!(plugin.answer(&json!({ "type": "ack", "to_seq": 1 })), Inbound::Nothing);
        assert!(plugin.all_acked());
    }

    #[test]
    fn answers_a_status_request_with_its_id_and_ignores_an_id_that_is_not_text() {
        let plugin = &mut Plugin::new(None, "de_dust2");
        let ask = |plugin: &mut Plugin, id: Value| plugin.answer(&json!({ "type": "get_server_status", "request_id": id }));

        assert_eq!(ask(plugin, json!("r")), Inbound::Reply(vec![plugin.status(Some("r"))]));
        assert_eq!(ask(plugin, Value::Null), Inbound::Reply(vec![plugin.status(None)]));
        assert_eq!(ask(plugin, json!(7)), Inbound::Nothing);
        assert!(format!("{plugin:?}").starts_with("Plugin { status: {"), "{plugin:?}");
    }

    #[test]
    fn sends_no_events_without_statistics_or_after_the_match() {
        let mut quiet = game();

        quiet["send_statistics"] = json!(false);
        assert_eq!(Plugin::new(Some(quiet), "de_mirage").load_map(), Vec::<Value>::new());

        let mut ended = in_warmup(game());
        let _ = ended.set_phase(MatchPhase::PostLive);
        assert_eq!(ended.emit(&json!({ "type": "log" })), None);
        // The plugin ignores a map reload once the match ended (`GameplayCallbacks.MapLoad`).
        assert_eq!(ended.load_map(), Vec::<Value>::new());
    }

    #[test]
    fn reports_phase_and_score_only_when_they_change() {
        let mut plugin = in_warmup(game());

        assert_eq!(plugin.set_phase(MatchPhase::Warmup), None);
        assert_eq!(plugin.set_score(0, 0), None);
        assert!(plugin.set_score(1, 0).is_some());
        // Warmup again puts the score back.
        assert_eq!(plugin.enter_warmup(), [json!({ "type": "score", "team1_score": 0, "team2_score": 0 })]);
    }

    #[test]
    fn turns_ready_once() {
        let mut plugin = Plugin::new(None, "de_dust2");

        assert_eq!(plugin.turn_ready().unwrap()["ready"], json!(true));
        assert_eq!(plugin.turn_ready(), None);
    }

    #[test]
    fn backs_off_to_five_seconds_and_starts_over_after_a_stable_or_acknowledged_socket() {
        let mut backoff = Backoff::default();
        let waits: Vec<Duration> = (0..8).map(|_| backoff.after(None, false)).collect();

        assert_eq!(waits.first(), Some(&Duration::from_millis(100)));
        assert_eq!(waits.last(), Some(&Duration::from_secs(5)));
        assert_eq!(backoff.after(Some(Duration::from_secs(10)), false), Duration::from_millis(100));
        let _ = backoff.after(None, false);
        assert_eq!(backoff.after(Some(Duration::from_millis(5)), true), Duration::from_millis(100));
    }
}
