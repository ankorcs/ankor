//! Turns a raw transcript into one that every run of a scenario produces alike: values that differ
//! from run to run become placeholders, entries whose number depends on timing are dropped or
//! collapsed, and entries are sorted: the controller's within each step, the others whole.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Map, Number, Value, json};

use crate::scenario::ALLOCATIONS;
use crate::transcript::{channels, controller_entry};

/// Normalises every channel of a transcript.
pub fn transcript(channels: &BTreeMap<String, Vec<Value>>) -> BTreeMap<String, Vec<Value>> {
    let mut out = BTreeMap::new();
    for (channel, entries) in channels {
        let mut entries: Vec<Value> = entries.iter().filter(|entry| keep(channel, entry)).map(|entry| entry_value(channel, entry)).collect();
        // These two read entries in the order they arrived, so they run before the sort.
        if channel == channels::CONTROLLER {
            entries = collapse_progress(entries);
        }
        if channel.starts_with(channels::PLUGIN_PREFIX) {
            entries = repeated_polls(&entries);
        }
        // The order within a step depends on how the agent's tasks interleave, so it is not
        // compared. The controller channel keeps its steps in order, between their markers, so it
        // reads as the scenario. The other channels are sorted whole.
        if channel == channels::CONTROLLER {
            sort_within_steps(&mut entries);
        } else {
            entries.sort_by_key(Value::to_string);
        }
        out.insert(channel.clone(), entries);
    }
    number_images(&out)
}

/// The agent reads a plugin's status on a timer, so how many reads a scenario sees depends on how
/// long it ran. The reads are not kept, and an answer that repeats the status before it, unchanged,
/// is dropped too.
fn repeated_polls(entries: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for entry in entries {
        let poll = entry["relay_frame"]["type"] == "get_server_status" && entry["relay_frame"]["request_id"].is_null();
        if poll {
            continue;
        }
        let status = entry["plugin_frame"]["type"] == "server_status" && entry["plugin_frame"]["request_id"].is_null();
        if status && out.last() == Some(entry) {
            continue;
        }
        out.push(entry.clone());
    }
    out
}

/// Image ids become `<image-N>`, numbered in the order they first appear: a rebuilt image gets a
/// new id, as Docker builds are not reproducible, while whether two ids are one image still shows.
fn number_images(channels: &BTreeMap<String, Vec<Value>>) -> BTreeMap<String, Vec<Value>> {
    let Ok(text) = serde_json::to_string(channels) else { return channels.clone() };
    let mut numbers: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut rest = text.as_str();
    // Split after `sha256` and its separator by characters, so text that follows it with a
    // character of several bytes cannot cut one in half.
    while let Some(at) = rest.find("sha256") {
        let (head, tail) = rest.split_at(at + "sha256".len());
        let separator = tail.chars().next().filter(|c| matches!(c, ':' | '_'));
        let (before, from) = match separator {
            Some(c) => (&rest[..head.len() + c.len_utf8()], &tail[c.len_utf8()..]),
            None => (head, tail),
        };
        out.push_str(before);
        let id: String = from.chars().take(64).collect();
        if separator.is_some() && id.len() == 64 && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
            let number = numbers.iter().position(|known| *known == id).unwrap_or_else(|| {
                numbers.push(id.clone());
                numbers.len() - 1
            });
            let _ = write!(out, "<image-{}>", number + 1);
            rest = from.get(64..).unwrap_or_default();
        } else {
            rest = from;
        }
    }
    out.push_str(rest);
    serde_json::from_str(&out).unwrap_or_else(|_| channels.clone())
}

/// Sorts the entries between each pair of step markers, leaving the markers in place.
fn sort_within_steps(entries: &mut [Value]) {
    for step in entries.split_mut(|entry| entry.get(controller_entry::AFTER_STEP).is_some()) {
        step.sort_by_key(Value::to_string);
    }
}

/// Entries whose presence depends on timers rather than on what the scenario did.
fn keep(channel: &str, entry: &Value) -> bool {
    if channel == channels::CONTROLLER {
        let frame = &entry[controller_entry::AGENT];
        // Resource and log batches follow watches no scenario starts.
        if matches!(frame["type"].as_str(), Some("resource_batch" | "log_batch" | "log_index")) {
            return false;
        }
    }
    true
}

/// Consecutive `demo_progress` frames of one demo collapse into the last, because how many an
/// upload sends depends on how fast the link is. Frames of different demos stay apart.
fn collapse_progress(entries: Vec<Value>) -> Vec<Value> {
    let demo = |entry: &Value| {
        (entry[controller_entry::AGENT]["type"] == "demo_progress")
            .then(|| (entry[controller_entry::AGENT]["slot_id"].clone(), entry[controller_entry::AGENT]["file_name"].clone()))
    };
    let mut out: Vec<Value> = Vec::new();
    for entry in entries {
        if demo(&entry).is_some() && out.last().and_then(demo) == demo(&entry) {
            out.pop();
        }
        out.push(entry);
    }
    out
}

fn entry_value(channel: &str, entry: &Value) -> Value {
    let mut value = entry.clone();
    if channels::is_snapshot(channel, channels::SNAPSHOT_CONTAINERS) {
        value = containers(&value);
    }
    if channels::is_snapshot(channel, channels::SNAPSHOT_FILES) {
        value = files(&value);
    }
    if channel == channels::DOCKER {
        value["name"] = json!(value["name"].as_str().map(container_name));
    }
    scrub(&value, "")
}

fn containers(value: &Value) -> Value {
    let Some(list) = value.as_array() else { return value.clone() };
    let mut out: Vec<Value> = list
        .iter()
        .map(|container| {
            let mut container = container.clone();
            container["name"] = json!(container["name"].as_str().map(container_name));
            container
        })
        .collect();
    out.sort_by_key(|container| container["name"].as_str().unwrap_or_default().to_owned());
    Value::Array(out)
}

fn files(value: &Value) -> Value {
    let Some(map) = value.as_object() else { return value.clone() };
    let mut out = Map::new();
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for key in keys {
        let mut entry = map[key].clone();
        let name = path_placeholders(key);
        // The spool's size depends on SQLite's page use and checkpoints.
        if name.contains("streams.sqlite")
            && let Some(object) = entry.as_object_mut()
        {
            object.remove("size");
        }
        out.insert(name, entry);
    }
    Value::Object(out)
}

/// `/ankor-s1-game-1a2b3c` → `/ankor-s1-game-<id>`: the short id is random.
fn container_name(name: &str) -> String {
    let trimmed = name.trim_start_matches('/');
    match trimmed.rsplit_once('-') {
        Some((stem, tail)) if tail.len() == 6 && tail.chars().all(|c| c.is_ascii_hexdigit()) => format!("{stem}-<id>"),
        _ => trimmed.to_owned(),
    }
}

fn path_placeholders(path: &str) -> String {
    path.split('/')
        .map(|part| {
            if part.len() > 20 && std::path::Path::new(part).extension().is_some_and(|extension| extension == "tmp") {
                "<tmp>".to_owned()
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Keys whose values are clock readings or durations.
const TIME_KEYS: &[&str] = &["timestamp", "at", "since", "started_at", "ended_at", "uptime_seconds", "recorded_at", "updated_at", "checked_at", "attempted_at"];

fn scrub(value: &Value, key: &str) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for name in keys {
                out.insert(name.clone(), scrub(&map[name], name));
            }
            agent_identity(&mut out);
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|item| scrub(item, key)).collect()),
        Value::Number(number) => number_value(number, key),
        Value::String(text) => string_value(text, key),
        other => other.clone(),
    }
}

/// The agent's own build changes with every release, which no scenario is about.
fn agent_identity(object: &mut Map<String, Value>) {
    if object.contains_key("version") && object.contains_key("image_id") && object.contains_key("image") {
        object.insert("version".to_owned(), json!("<agent-version>"));
        object.insert("image_id".to_owned(), json!("<agent-image-id>"));
    }
}

/// The hello's `host` fields that come from the machine running the suite, not from the agent.
const HOST_CPU_COUNT: &str = "cpu_count";
const HOST_KERNEL: &str = "kernel";
/// The plugin's build in its `map_start` event.
const PLUGIN_VERSION: &str = "plugin_version";

fn number_value(number: &Number, key: &str) -> Value {
    if key == HOST_CPU_COUNT {
        return json!("<cpu-count>");
    }
    let is_time = TIME_KEYS.contains(&key) || key.ends_with("_at") || key.ends_with("_ms") || key.ends_with("At");
    if is_time && number.as_f64().is_some_and(|float| float > 1.0e12) {
        return json!("<epoch-ms>");
    }
    Value::Number(number.clone())
}

/// The label that holds a game's relay key, which the agent makes at random for each launch.
const RELAY_KEY_LABEL: &str = "ankor.relay-key";
/// Where the relay key stands in the URL the agent hands a plugin.
const RELAY_KEY_PARAM: &str = "key=";

fn string_value(text: &str, key: &str) -> Value {
    if key.ends_with("_at") && looks_like_date(text) {
        return json!("<date>");
    }
    if key == RELAY_KEY_LABEL {
        return json!("<relay-key>");
    }
    if key == HOST_KERNEL {
        return json!("<kernel>");
    }
    // The plugin's build, in its map_start event, changes with every release of the plugin.
    if key == PLUGIN_VERSION {
        return json!("<plugin-version>");
    }
    let mut out = replace_relay_key(text);
    out = replace_hex_runs(&out, 64, "<hex64>");
    out = replace_uuids(&out);
    json!(out)
}

fn looks_like_date(text: &str) -> bool {
    text.len() >= 19 && text.as_bytes().get(4) == Some(&b'-') && text.as_bytes().get(10) == Some(&b'T')
}

/// `...&key=0a1b2c3d4e5f6a7b` → `...&key=<relay-key>`: the key is random for each launch. It
/// matches the first `key=`, as no recorded text carries another parameter ending in `key=`.
fn replace_relay_key(text: &str) -> String {
    let Some(at) = text.find(RELAY_KEY_PARAM).map(|found| found + RELAY_KEY_PARAM.len()) else {
        return text.to_owned();
    };
    let end = text[at..].find(|c: char| !c.is_ascii_hexdigit()).map_or(text.len(), |length| at + length);

    format!("{}<relay-key>{}", &text[..at], &text[end..])
}

/// Container ids and other 64-digit hex strings; `sha256:` image ids stay for [`number_images`].
fn replace_hex_runs(text: &str, length: usize, placeholder: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let run = chars[index..].iter().take_while(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()).count();
        let after_sha = out.ends_with("sha256:") || out.ends_with("sha256_");
        if run == length && !after_sha {
            out.push_str(placeholder);
            index += run;
        } else if run > 0 {
            out.extend(&chars[index..index + run]);
            index += run;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

/// UUIDs the agent generates (request ids to plugins); the scenario's allocations stay.
fn replace_uuids(text: &str) -> String {
    let mut out = text.to_owned();
    let mut search_from = 0;
    while let Some(found) = find_uuid(&out, search_from) {
        let uuid = &out[found..found + 36];
        if ALLOCATIONS.contains(&uuid) {
            search_from = found + 36;
            continue;
        }
        out.replace_range(found..found + 36, "<uuid>");
        search_from = found + 6;
    }
    out
}

fn find_uuid(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let shape = |at: usize| -> bool {
        (0..36).all(|offset| {
            let Some(&byte) = bytes.get(at + offset) else { return false };
            if matches!(offset, 8 | 13 | 18 | 23) { byte == b'-' } else { byte.is_ascii_hexdigit() }
        })
    };
    (from..bytes.len().saturating_sub(35)).find(|&at| shape(at))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a test fails by crashing")]

    use super::*;

    #[test]
    fn keys_sort_and_clock_readings_become_placeholders() {
        let value: Value = serde_json::from_str(r#"{"b":1.0,"a":2,"started_at":1760000000000}"#).unwrap();
        assert_eq!(scrub(&value, "").to_string(), r#"{"a":2,"b":1.0,"started_at":"<epoch-ms>"}"#);
    }

    #[test]
    fn generated_uuids_become_placeholders_but_allocations_stay() {
        let text = format!("{} and 9f0c6a8e-3b1d-4c2e-9a7f-1b2c3d4e5f60", ALLOCATIONS[0]);
        assert_eq!(replace_uuids(&text), format!("{} and <uuid>", ALLOCATIONS[0]));
    }

    #[test]
    fn entries_are_sorted_within_each_step_and_the_steps_keep_their_order() {
        let marker = |step: &str| json!({ (controller_entry::AFTER_STEP): step });
        let frame = |kind: &str| json!({ (controller_entry::AGENT): { "type": kind } });
        let mut entries = vec![frame("b"), frame("a"), marker("one"), frame("d"), frame("c"), marker("two"), frame("a")];
        sort_within_steps(&mut entries);
        assert_eq!(entries, [frame("a"), frame("b"), marker("one"), frame("c"), frame("d"), marker("two"), frame("a")]);
    }

    #[test]
    fn progress_collapses_per_demo_and_keeps_different_demos_apart() {
        let progress =
            |file: &str, sent: u64| json!({ (controller_entry::AGENT): { "type": "demo_progress", "slot_id": "s1", "file_name": file, "sent_bytes": sent } });
        let collapsed = collapse_progress(vec![progress("a.dem", 1), progress("a.dem", 2), progress("b.dem", 1), progress("b.dem", 2)]);
        assert_eq!(collapsed, vec![progress("a.dem", 2), progress("b.dem", 2)]);
    }

    #[test]
    fn image_ids_are_numbered_in_the_order_they_first_appear() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let channels = BTreeMap::from([(
            "docker".to_owned(),
            vec![json!({ "image": format!("sha256:{b}") }), json!({ format!("overlay/sha256_{a}"): format!("sha256:{b} sha256:{a}") })],
        )]);
        let numbered = number_images(&channels);
        assert_eq!(
            numbered["docker"],
            vec![json!({ "image": "sha256:<image-1>" }), json!({ "overlay/sha256_<image-2>": "sha256:<image-1> sha256:<image-2>" })]
        );
    }

    #[test]
    fn leaves_text_after_sha256_alone_whatever_its_characters() {
        let channels = BTreeMap::from([("logs".to_owned(), vec![json!("sha256é sha256:\u{2603} sha256")])]);

        assert_eq!(number_images(&channels), channels);
    }

    #[test]
    fn container_names_lose_their_short_id() {
        assert_eq!(container_name("/ankor-s1-game-0a1b2c"), "ankor-s1-game-<id>");
        assert_eq!(
            replace_relay_key("ws://127.0.0.1:27101/ws/plugin?slot_id=s1&key=0a1b2c3d4e5f6a7b"),
            "ws://127.0.0.1:27101/ws/plugin?slot_id=s1&key=<relay-key>"
        );
        assert_eq!(string_value("0a1b2c3d4e5f6a7b", RELAY_KEY_LABEL), json!("<relay-key>"));
        assert_eq!(string_value("7.0.14-linuxkit", HOST_KERNEL), json!("<kernel>"));
        assert_eq!(number_value(&Number::from(18), HOST_CPU_COUNT), json!("<cpu-count>"));
        assert_eq!(container_name("ankor-agent"), "ankor-agent");
    }

    #[test]
    fn container_ids_become_placeholders_and_image_ids_stay() {
        let id = "a".repeat(64);
        assert_eq!(replace_hex_runs(&format!("{id} sha256:{id}"), 64, "<hex64>"), format!("<hex64> sha256:{id}"));
    }

    #[test]
    fn a_status_that_repeats_the_one_before_it_unchanged_is_dropped() {
        let ready = json!({ "plugin_frame": { "type": "server_status", "request_id": null, "ready": true } });
        let joined = json!({ "plugin_frame": { "type": "server_status", "request_id": null, "ready": true, "players": [1] } });
        let entries = vec![ready.clone(), ready.clone(), ready.clone(), joined.clone(), joined.clone(), ready.clone()];

        assert_eq!(repeated_polls(&entries), [ready.clone(), joined, ready]);
    }
}
