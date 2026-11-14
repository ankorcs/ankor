//! Checks that span fields, which the generated validators call by name. Each runs on a value
//! whose fields have the right types, though they may break other checks.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::constants::node_unavailable;
use crate::constants::{AGENT_MIN_SLOT_PORT, EVENT_RELAY_PORT, MAX_HEALTH, MAX_PORT, MAX_REPORTED_BACKLOGS, MAX_SERVER_PLAYERS, SLOT_ID_PATTERN};
use crate::helpers::from_wire_name;
use crate::types::{GameMode, SlotState};
use crate::validate::{PathKey, RefineCtx};

const SLOT_ID_MESSAGE: &str = "slot id must be 1-32 of [A-Za-z0-9_-] (it becomes part of a container name)";
const DUPLICATE_GSLT_MESSAGE: &str = "each slot needs its own token";
/// The game modes that a match plays in: Counter-Strike's own, whose rounds end.
const MATCH_GAME_MODES: [GameMode; 3] = [GameMode::Casual, GameMode::Competitive, GameMode::Wingman];

fn num(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(f64::NAN)
}

fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn is_null(value: &Value, key: &str) -> bool {
    matches!(value.get(key), Some(Value::Null))
}

#[allow(clippy::cast_precision_loss, reason = "list lengths are far below 2^53")]
fn count(items: &[Value]) -> f64 {
    items.len() as f64
}

fn path(steps: &[PathKey]) -> Vec<PathKey> {
    steps.to_vec()
}

fn key(name: &str) -> PathKey {
    PathKey::key(name)
}

fn backlog(value: &Value, ctx: &mut RefineCtx) {
    let servers = items(value, "servers");
    let server_count = num(value, "server_count");

    #[allow(clippy::cast_precision_loss, clippy::float_cmp, reason = "JSON numbers compare by exact value; counts are far below 2^53")]
    if count(servers) != server_count.min(MAX_REPORTED_BACKLOGS as f64) {
        ctx.add_issue(path(&[key("servers")]), format!("lists the oldest {MAX_REPORTED_BACKLOGS} of server_count"));
    }

    if num(value, "event_count") < servers.iter().map(|server| num(server, "pending")).sum::<f64>() {
        ctx.add_issue(path(&[key("event_count")]), "counts at least the listed servers' events");
    }

    if (server_count == 0.0) != is_null(value, "reason") {
        ctx.add_issue(path(&[key("reason")]), "null exactly when nothing is stuck");
    }
}

pub fn agent_event_backlog(value: &Value, ctx: &mut RefineCtx) {
    backlog(value, ctx);
}

pub fn event_backlog(value: &Value, ctx: &mut RefineCtx) {
    backlog(value, ctx);
}

fn slot(value: &Value, ctx: &mut RefineCtx) {
    let idle = value.get("state").and_then(Value::as_str).and_then(from_wire_name::<SlotState>) == Some(SlotState::Idle);
    let unheld = if idle { !is_null(value, "allocation_id") || !is_null(value, "match_id") } else { is_null(value, "allocation_id") };

    if unheld {
        ctx.add_issue(Vec::new(), "a held slot names its allocation, an idle one none");
    }

    if !idle && value.get("reserved") == Some(&Value::Bool(true)) {
        ctx.add_issue(Vec::new(), "only an idle slot is reserved");
    }

    if idle && !is_null(value, "server_steam_id") {
        ctx.add_issue(Vec::new(), "an idle slot names no server_steam_id");
    }
}

pub fn agent_slot_status(value: &Value, ctx: &mut RefineCtx) {
    slot(value, ctx);
}

pub fn slot_status(value: &Value, ctx: &mut RefineCtx) {
    slot(value, ctx);
}

pub fn agent_hello(value: &Value, ctx: &mut RefineCtx) {
    let slots: HashSet<&Value> = items(value.get("config").unwrap_or(&Value::Null), "slots").iter().filter_map(|slot| slot.get("id")).collect();
    let mut seen = HashSet::new();

    for (index, status) in items(value, "slot_status").iter().enumerate() {
        let id = status.get("slot_id").unwrap_or(&Value::Null);

        if !slots.contains(id) || seen.contains(id) {
            ctx.add_issue(vec![key("slot_status"), PathKey::Index(index), key("slot_id")], "slot_status names each configured slot once");
        }

        seen.insert(id);
    }

    if seen.len() != slots.len() {
        ctx.add_issue(path(&[key("slot_status")]), "slot_status names every configured slot");
    }
}

pub fn event_batch(value: &Value, ctx: &mut RefineCtx) {
    let frames = items(value, "frames");
    let Some(first) = frames.first() else {
        return;
    };
    let start = num(first, "seq");

    #[allow(clippy::cast_precision_loss, clippy::float_cmp, reason = "JSON numbers compare by exact value; counts are far below 2^53")]
    for (index, frame) in frames.iter().enumerate() {
        if num(frame, "seq") != start + index as f64 {
            ctx.add_issue(vec![key("frames"), PathKey::Index(index)], "A batch must contain one contiguous sequence");
        }
    }
}

pub fn node_unavailable(value: &Value, ctx: &mut RefineCtx) {
    if (value.get("code").and_then(Value::as_str) == Some(node_unavailable::MAINTENANCE)) == is_null(value, "operation") {
        ctx.add_issue(path(&[key("operation")]), "names the operation exactly when the code is maintenance");
    }
}

/// Dotted-quad IPv4, each part 0 to 255 without a leading zero.
pub fn ipv4(value: &Value) -> bool {
    let text = value.as_str().unwrap_or("");
    let parts: Vec<&str> = text.split('.').collect();

    parts.len() == 4
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 3
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && !(part.len() > 1 && part.starts_with('0'))
                && part.parse::<u16>().is_ok_and(|octet| octet <= 255)
        })
}

/// A demo upload header name, in any case, other than one the agent sets itself or one that frames
/// the request.
pub fn demo_upload_header_name_1(value: &Value) -> bool {
    value.as_str().is_none_or(|name| !crate::constants::RESERVED_DEMO_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
}

/// A demo upload URL without a user name or password, which go in its header.
pub fn demo_upload_url_2(value: &Value) -> bool {
    value.as_str().and_then(|text| url::Url::parse(text).ok()).is_none_or(|url| url.username().is_empty() && url.password().is_none_or(str::is_empty))
}

/// A host with an optional port, without scheme, credentials, path, query or fragment.
pub fn controller_host(value: &Value) -> bool {
    crate::helpers::is_controller_host(value.as_str().unwrap_or(""))
}

#[allow(clippy::cast_precision_loss, reason = "a small constant")]
pub fn knife_end(value: &Value) -> bool {
    let max = MAX_HEALTH as f64;

    num(value, "team1_health") <= max * num(value, "team1_alive") && num(value, "team2_health") <= max * num(value, "team2_alive")
}

/// The winner is the score leader of a `played` map, and null for a tie or any other end.
#[allow(clippy::float_cmp, reason = "JSON numbers compare by exact value, as the contract does")]
pub fn map_end(value: &Value) -> bool {
    let winner = value.get("winner_team").unwrap_or(&Value::Null);
    let (team1, team2) = (num(value, "team1_score"), num(value, "team2_score"));
    let played = value.get("reason").and_then(Value::as_str) == Some("played");

    let leader = match team1.partial_cmp(&team2) {
        Some(std::cmp::Ordering::Greater) if played => Some(1.0),
        Some(std::cmp::Ordering::Less) if played => Some(2.0),
        _ => None,
    };

    match winner.as_f64() {
        None => winner.is_null() && leader.is_none(),
        Some(team) => leader == Some(team),
    }
}

/// `teams` is set, at 0-0, for a `ready_timeout` alone.
#[allow(clippy::float_cmp, reason = "JSON numbers compare by exact value, as the contract does")]
pub fn map_end_1(value: &Value) -> bool {
    let has_teams = !is_null(value, "teams");

    if value.get("reason").and_then(Value::as_str) == Some("ready_timeout") {
        has_teams && num(value, "team1_score") == 0.0 && num(value, "team2_score") == 0.0
    } else {
        !has_teams
    }
}

pub fn player_death(value: &Value) -> bool {
    let Some(assister) = value.get("assister").filter(|assister| !assister.is_null()) else {
        return true;
    };
    let id = assister.get("steam_id");
    let attacker = value.get("attacker").and_then(|attacker| attacker.get("steam_id"));

    id != value.get("victim").and_then(|victim| victim.get("steam_id")) && id != attacker
}

pub fn player_hurt(value: &Value) -> bool {
    is_null(value, "health_before") || num(value, "health_after") <= num(value, "health_before")
}

/// Text without NUL. Lone surrogates cannot reach here, as JSON with one fails to parse.
pub fn clean_text(value: &Value) -> bool {
    !value.as_str().unwrap_or("").contains('\0')
}

/// A weapon with an index has no fallback name.
pub fn weapon(value: &Value) -> bool {
    is_null(value, "id") || is_null(value, "fallback")
}

pub fn roster(value: &Value, ctx: &mut RefineCtx) {
    let team = |name: &str| items(value.get(name).unwrap_or(&Value::Null), "players").iter();
    let ids: Vec<&Value> = team("team1").chain(team("team2")).chain(items(value, "spectators")).collect();

    if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        ctx.add_issue(Vec::new(), "a SteamID appears more than once in the roster");
    }

    #[allow(clippy::cast_possible_truncation, reason = "a small constant")]
    if ids.len() > MAX_SERVER_PLAYERS as usize {
        ctx.add_issue(Vec::new(), format!("a roster holds at most {MAX_SERVER_PLAYERS} players and spectators"));
    }
}

#[allow(clippy::float_cmp, reason = "JSON numbers compare by exact value, as the contract does")]
pub fn round_buy_end(value: &Value) -> bool {
    let team_equipment = |team: f64| {
        items(value, "players")
            .iter()
            .filter(|sample| sample.get("player").and_then(|player| player.get("team")).and_then(Value::as_f64) == Some(team))
            .map(|sample| num(sample, "equipment_value"))
            .sum::<f64>()
    };

    num(value, "team1_equipment_value") == team_equipment(1.0) && num(value, "team2_equipment_value") == team_equipment(2.0)
}

/// Every sample names a different player.
pub fn unique_players(value: &Value) -> bool {
    let players = value.as_array().map_or(&[][..], Vec::as_slice);
    let ids: HashSet<Option<&Value>> = players.iter().map(|sample| sample.get("player").and_then(|player| player.get("steam_id"))).collect();

    ids.len() == players.len()
}

#[allow(clippy::float_cmp, reason = "JSON numbers compare by exact value, as the contract does")]
pub fn weapon_shots(value: &Value) -> bool {
    num(value, "shots") == count(items(value, "ticks"))
}

pub fn round_shots(value: &Value, ctx: &mut RefineCtx) {
    if !unique_players(value.get("players").unwrap_or(&Value::Null)) {
        ctx.add_issue(path(&[key("players")]), "each player is listed once");
    }

    for (index, player) in items(value, "players").iter().enumerate() {
        let weapons = items(player, "weapons");
        let at = || vec![key("players"), PathKey::Index(index), key("weapons")];

        #[allow(clippy::cast_precision_loss, reason = "MAX_ROUND_SHOTS is exact as an f64")]
        if weapons.iter().map(|shots| num(shots, "shots")).sum::<f64>() > crate::constants::MAX_ROUND_SHOTS as f64 {
            ctx.add_issue(at(), "a player fires at most MAX_ROUND_SHOTS shots");
        }

        let names: HashSet<(Option<&Value>, Option<&Value>)> = weapons
            .iter()
            .map(|shots| {
                let weapon = shots.get("weapon");

                (weapon.and_then(|weapon| weapon.get("id")), weapon.and_then(|weapon| weapon.get("fallback")))
            })
            .collect();

        if names.len() != weapons.len() {
            ctx.add_issue(at(), "each weapon is listed once");
        }
    }
}

pub fn server_launch_config(value: &Value, ctx: &mut RefineCtx) {
    if is_null(value, "workshop_id") && is_null(value, "map_name") {
        ctx.add_issue(path(&[key("map_name")]), "a launch without a workshop id names its stock map");
    }

    let Some(game) = value.get("match").filter(|game| !game.is_null()) else {
        return;
    };

    if !is_null(value, "workshop_id") {
        ctx.add_issue(path(&[key("workshop_id")]), "a match plays a stock map");
    }

    let roster = game.get("roster").unwrap_or(&Value::Null);
    let players = |team: &str| count(items(roster.get(team).unwrap_or(&Value::Null), "players"));

    if players("team1") + players("team2") + count(items(roster, "spectators")) > num(value, "max_players") {
        ctx.add_issue(path(&[key("max_players")]), "the roster, players and spectators, must fit in `max_players`");
    }

    let match_mode = value.get("game_mode").and_then(Value::as_str).and_then(from_wire_name::<GameMode>).is_some_and(|mode| MATCH_GAME_MODES.contains(&mode));

    if !match_mode {
        ctx.add_issue(path(&[key("game_mode")]), "a match needs `casual`, `competitive` or `wingman`");
    }

    if game.get("ready_mode").and_then(Value::as_str) == Some("external") && !is_null(game, "ready_timeout_seconds") {
        ctx.add_issue(
            vec![key("match"), key("ready_timeout_seconds")],
            "`ready_timeout_seconds` must be null with an `external` ready-up, because the controller starts the match",
        );
    }
}

/// Text without control characters, which would end a console line.
pub fn cfg_text(value: &Value) -> bool {
    !value.as_str().unwrap_or("").chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
}

/// Ids match the slot-id pattern and differ; each port lies in `min_port..=MAX_PORT`, avoids the
/// event relay and differs from every other.
#[allow(clippy::cast_precision_loss, clippy::float_cmp, reason = "ports compare by exact value and are far below 2^53")]
fn slot_layout_issues(ctx: &mut RefineCtx, slots: &[Value], min_port: u64, prefix: &[PathKey]) {
    let mut ids = HashSet::new();
    let mut ports: Vec<f64> = Vec::new();

    for (index, slot) in slots.iter().enumerate() {
        let at = |field: &str| [prefix, &[PathKey::Index(index), key(field)]].concat();

        if let Some(id) = slot.get("id") {
            let text = id.as_str().unwrap_or("");

            if !SLOT_ID_PATTERN.is_match(text).unwrap_or(false) {
                ctx.add_issue(at("id"), SLOT_ID_MESSAGE);
            } else if ids.contains(text) {
                ctx.add_issue(at("id"), format!("duplicate slot id \"{text}\""));
            }

            ids.insert(text);
        }

        for field in ["port", "tv_port"] {
            let port = num(slot, field);

            if port.fract() != 0.0 || port < min_port as f64 || port > MAX_PORT as f64 {
                ctx.add_issue(at(field), format!("port {port} must be an integer between {min_port} and {MAX_PORT}"));
                continue;
            }

            if port == EVENT_RELAY_PORT as f64 {
                ctx.add_issue(at(field), format!("port {port} is reserved for the event relay"));
            }

            if ports.contains(&port) {
                ctx.add_issue(at(field), format!("port {port} is used by more than one slot"));
            }

            ports.push(port);
        }
    }
}

/// An issue on each slot whose token an earlier slot already has.
fn gslt_issues(ctx: &mut RefineCtx, slots: &[Value], prefix: &[PathKey]) {
    let mut seen = HashSet::new();

    for (index, slot) in slots.iter().enumerate() {
        let Some(token) = slot.get("gslt") else {
            continue;
        };

        if !seen.insert(token) {
            ctx.add_issue([prefix, &[PathKey::Index(index), key("gslt")]].concat(), DUPLICATE_GSLT_MESSAGE);
        }
    }
}

pub fn agent_config(value: &Value, ctx: &mut RefineCtx) {
    let slots = items(value, "slots");
    let prefix = [key("slots")];

    slot_layout_issues(ctx, slots, AGENT_MIN_SLOT_PORT, &prefix);
    gslt_issues(ctx, slots, &prefix);
}

pub fn slot_layout(value: &Value, ctx: &mut RefineCtx) {
    slot_layout_issues(ctx, value.as_array().map_or(&[], Vec::as_slice), AGENT_MIN_SLOT_PORT, &[]);
}

pub fn slot_edit_layout(value: &Value, ctx: &mut RefineCtx) {
    let slots = value.as_array().map_or(&[][..], Vec::as_slice);

    slot_layout_issues(ctx, slots, AGENT_MIN_SLOT_PORT, &[]);
    gslt_issues(ctx, slots, &[]);
}

/// Fills `pinned_ip` and `demo_upload` with null when absent, and `demo_upload`'s optional fields.
pub fn agent_config_output(value: Value) -> Value {
    let Value::Object(mut config) = value else {
        return value;
    };
    let pinned = config.get("pinned_ip").cloned().unwrap_or(Value::Null);
    let upload = match config.get("demo_upload") {
        None => Value::Null,
        Some(upload) => {
            let mut fields = Map::new();

            fields.insert("url".to_owned(), upload.get("url").cloned().unwrap_or(Value::Null));
            fields.insert("header".to_owned(), upload.get("header").cloned().unwrap_or(Value::Null));
            fields.insert("limit_mbit".to_owned(), upload.get("limit_mbit").cloned().unwrap_or(Value::Null));
            Value::Object(fields)
        }
    };

    config.insert("pinned_ip".to_owned(), pinned);
    config.insert("demo_upload".to_owned(), upload);
    Value::Object(config)
}
