//! The contract's caps as the agent meets them: how many slots, demos and characters a frame
//! takes, the largest frames either side can send, and the token bucket the controller spends them from.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::agent_reporting::{ChangeReporter, ExitReporter, SlotFeed, update_feed};
use ankor_agent::contract::constants::MAX_FRAME_BYTES;
use ankor_agent::contract::schemas::s_plugin_event_frame;
use ankor_agent::failure::{AgentFailure, agent_error};
use ankor_agent::logger::{Entry, Level};
use ankor_agent::node_log::{NodeLog, Transport};
use ankor_agent::slot_manager::SlotEvent;
use ankor_agent::testing::fixtures::{MATCH_ID, agent_config, launch_config, match_config};
use ankor_agent::testing::slots::{Reported, Slots, launch_server, make_slots};
use ankor_protocol::constants::{
    AGENT_ERROR_DETAIL_MAX_CHARS, AGENT_FRAME_MAX_BYTES, AGENT_UPDATE_DETAIL_MAX_CHARS, CONSOLE_COMMAND_MAX_CHARS, CONTROLLER_FRAME_MAX_BYTES,
    CS2_UPDATE_DETAIL_MAX_CHARS, DEMO_FILE_NAME_MAX_CHARS, DEMO_UPLOAD_ERROR_MAX_CHARS, EVENT_BACKLOG_REASON_MAX_CHARS, MAX_BACKUP_FILE_CHARS, MAX_MEMORY_MB,
    MAX_REPORTED_BACKLOGS, MAX_REPORTED_DEMOS, MAX_SLOTS, NODE_UNAVAILABLE_DETAIL_MAX_CHARS, SERVER_EXIT_DETAIL_MAX_CHARS,
};
use ankor_protocol::constants::{AGENT_PING_INTERVAL, CS2_UPDATE_MESSAGE_MAX_CHARS, LOG_LINE_MAX_CHARS, SERVER_LOG_LINES, agent_budgets};
use ankor_protocol::helpers::{BudgetName, Rate, TokenBucket, budget_rate, frame_budget};
use ankor_protocol::schemas::{s_agent_config_schema, s_agent_to_controller_message, s_controller_to_agent_message};
use ankor_protocol::types::{
    AgentCs2UpdateProgress, AgentErrorCode, AgentSlotStatus, AgentToControllerMessage, LogSource, LogSourceAgent, LogSourceCs2Update, LogSourceServer,
};
use ankor_protocol::validate::{Payload, safe_parse};
use futures_util::FutureExt as _;
use serde_json::{Value, json};

const UPDATE_ID: &str = "00000000-0000-4000-8000-00000000a0a0";
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

fn n(value: u64) -> usize {
    usize::try_from(value).unwrap()
}

fn passes(schema: fn(&mut Payload), value: &Value) -> bool {
    safe_parse(schema, value.clone()).is_ok()
}

fn slot_ids(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("s{index:02}")).collect()
}

fn hello(slots: usize, demos: usize) -> Value {
    let ids = slot_ids(slots);

    json!({
        "type": "hello", "agent": { "version": "1.0.0", "image": "ghcr.io/example/ankor-agent:1", "image_id": format!("sha256:{}", "a".repeat(64)) },
        "host": { "cpu_count": 4, "os_name": null, "kernel": "6.12.48" }, "public_ip": "203.0.113.7", "country": "DK",
        "config": {
            "slots": ids.iter().enumerate().map(|(index, id)| json!({ "id": id, "port": 20000 + 2 * index, "tv_port": 20001 + 2 * index, "gslt_hint": "AAAAAA0001" })).collect::<Vec<_>>(),
            "pinned_ip": null, "controller": { "host": "cs.example.com", "tls": true, "socket_path": "/ws/agent", "events_path": "/api/node/events" }, "auto_update": true, "demo_upload": null, "memory_mb": null,
        },
        "slot_status": ids.iter().map(|id| json!({ "slot_id": id, "state": "idle", "allocation_id": null, "match_id": null, "plugin_online": false, "gslt_state": "ok", "reserved": false, "server_steam_id": null })).collect::<Vec<_>>(),
        "cs2": { "build_id": null, "latest_build_id": "19001", "image": "ghcr.io/example/ankor-cs2:1", "image_id": null, "update": null, "image_update": null },
        "unavailable": null,
        "demos": (0..demos).map(|index| json!({ "slot_id": "s00", "file_name": format!("demo-{index}.dem"), "size_bytes": 1, "modified_at": index, "recording": false, "uploading": false, "upload_error": null })).collect::<Vec<_>>(),
        "server_exits": [],
        "event_backlog": { "servers": [], "server_count": 0, "event_count": 0, "reason": null },
        "agent_update": null,
    })
}

mod contract_caps {
    use super::*;

    #[test]
    fn takes_up_to_max_slots_slots_and_their_statuses_in_a_hello_and_no_more() {
        assert!(passes(s_agent_to_controller_message, &hello(n(MAX_SLOTS), 0)));
        assert!(!passes(s_agent_to_controller_message, &hello(n(MAX_SLOTS) + 1, 0)));
    }

    #[test]
    fn takes_up_to_max_reported_demos_demos_in_a_hello_and_no_more() {
        assert!(passes(s_agent_to_controller_message, &hello(1, n(MAX_REPORTED_DEMOS))));
        assert!(!passes(s_agent_to_controller_message, &hello(1, n(MAX_REPORTED_DEMOS) + 1)));
    }

    #[test]
    fn caps_set_config_at_max_slots_slots() {
        let request = |count: usize| {
            json!({
                "type": "set_config", "id": "r1", "config": {
                    "pinned_ip": null, "controller": { "host": "cs.example.com", "tls": true, "socket_path": "/ws/agent", "events_path": "/api/node/events" }, "auto_update": true, "demo_upload": null, "memory_mb": null,
                    "slots": slot_ids(count).iter().enumerate().map(|(index, id)| json!({ "id": id, "port": 20000 + 2 * index, "tv_port": 20001 + 2 * index })).collect::<Vec<_>>(),
                },
            })
        };

        assert!(passes(s_controller_to_agent_message, &request(n(MAX_SLOTS))));
        assert!(!passes(s_controller_to_agent_message, &request(n(MAX_SLOTS) + 1)));
    }

    #[test]
    fn refuses_an_agent_yml_with_more_than_max_slots_slots() {
        let yml = |count: usize| {
            json!({
                "token": "0123456789abcdef0123456789abcdef", "controller": { "host": "cs.example.com", "tls": true },
                "cs2_image": "ghcr.io/example/ankor-cs2:1", "agent_image": "ghcr.io/example/ankor-agent:1",
                "slots": slot_ids(count).iter().enumerate().map(|(index, id)| json!({ "id": id, "port": 20000 + 2 * index, "tv_port": 20001 + 2 * index, "gslt": format!("TOKEN{index:08}") })).collect::<Vec<_>>(),
            })
        };

        assert!(passes(s_agent_config_schema, &yml(n(MAX_SLOTS))));
        assert!(!passes(s_agent_config_schema, &yml(n(MAX_SLOTS) + 1)));
    }

    #[test]
    fn cuts_a_refusals_detail_and_an_updates_progress_message_to_what_the_contract_takes() {
        let long = "x".repeat(5_000);
        let plain: ankor_agent::failure::OpError = long.clone().into();
        let failure: ankor_agent::failure::OpError = AgentFailure { code: AgentErrorCode::Failed, detail: long.clone() }.into();

        for error in [plain, failure] {
            let reply = json!({ "type": "reply", "reply_to": "r1", "request": "start_server", "ok": false, "error": agent_error(&error) });

            assert_eq!(reply["error"]["detail"].as_str().unwrap().chars().count(), n(AGENT_ERROR_DETAIL_MAX_CHARS));
            assert!(passes(s_agent_to_controller_message, &reply));
        }

        let pushed: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let record = Arc::clone(&pushed);
        let feed = update_feed(Arc::new(move |frame| record.lock().unwrap().push(frame)), Arc::new(|_| {}));

        (feed.on_progress)(AgentCs2UpdateProgress { update_id: UPDATE_ID.to_owned(), percent: Some(50.0), message: long });

        let pushed: Vec<Value> = pushed.lock().unwrap().iter().map(|frame| serde_json::to_value(frame).unwrap()).collect();

        assert_eq!(
            pushed,
            [json!({ "type": "cs2_update_progress", "update_id": UPDATE_ID, "percent": 50.0, "message": "x".repeat(n(CS2_UPDATE_MESSAGE_MAX_CHARS)) })]
        );
        assert!(passes(s_agent_to_controller_message, &pushed[0]));
    }

    #[test]
    fn fits_the_largest_hello_the_contract_allows_in_half_a_frame() {
        let control = "\u{1}";
        let slots = n(MAX_SLOTS);
        let ids: Vec<String> = (0..slots).map(|index| format!("{}{index:03}", "s".repeat(29))).collect();
        let allocation = |index: usize| format!("11111111-1111-4111-8111-{index:012}");
        let image = format!("r{}", "x".repeat(254));
        let backlogs = n(MAX_REPORTED_BACKLOGS);
        let largest = json!({
            "type": "hello",
            "agent": { "version": control.repeat(64), "image": image, "image_id": format!("sha256:{}", "a".repeat(64)) },
            "host": { "cpu_count": MAX_SAFE_INTEGER, "os_name": control.repeat(256), "kernel": control.repeat(256) },
            "public_ip": "255.255.255.255", "country": "DK",
            "config": {
                "slots": ids.iter().enumerate().map(|(index, id)| json!({ "id": id, "port": 10000 + 2 * index, "tv_port": 10001 + 2 * index, "gslt_hint": "ABCDEFGHIJ" })).collect::<Vec<_>>(),
                "pinned_ip": "255.255.255.255",
                "controller": { "host": format!("{}.{}.{}.{}:65535", "a".repeat(63), "b".repeat(63), "c".repeat(63), "d".repeat(57)), "tls": true, "socket_path": "/ws/agent", "events_path": "/api/node/events" },
                "auto_update": false,
                "demo_upload": { "url": format!("https://{}.example.com/{}", "u".repeat(60), "p".repeat(170)), "header": { "name": "H".repeat(64), "value_sha256": "f".repeat(64) }, "limit_mbit": 10_000 },
                "memory_mb": MAX_MEMORY_MB,
            },
            "slot_status": ids.iter().enumerate().map(|(index, id)| json!({
                "slot_id": id, "state": "running", "allocation_id": allocation(index), "match_id": allocation(index), "plugin_online": true, "gslt_state": "unchecked", "reserved": false, "server_steam_id": "85568392936840254",
            })).collect::<Vec<_>>(),
            "cs2": {
                "build_id": "9".repeat(20), "latest_build_id": "9".repeat(20), "image": image, "image_id": format!("sha256:{}", "a".repeat(64)),
                "update": { "id": UPDATE_ID, "mode": "reinstall", "automatic": false, "outcome": "overlay_failed", "detail": control.repeat(n(CS2_UPDATE_DETAIL_MAX_CHARS)) },
                "image_update": { "image": image, "step": null, "outcome": "pull_failed", "detail": control.repeat(n(CS2_UPDATE_DETAIL_MAX_CHARS)) },
            },
            "unavailable": { "code": "event_storage", "operation": null, "detail": control.repeat(n(NODE_UNAVAILABLE_DETAIL_MAX_CHARS)) },
            "demos": (0..n(MAX_REPORTED_DEMOS)).map(|index| json!({
                "slot_id": ids[index % slots], "file_name": format!("{index:f>width$}.dem", width = n(DEMO_FILE_NAME_MAX_CHARS) - 4),
                "size_bytes": MAX_SAFE_INTEGER, "modified_at": -MAX_SAFE_INTEGER, "recording": true, "uploading": true,
                "upload_error": control.repeat(n(DEMO_UPLOAD_ERROR_MAX_CHARS)),
            })).collect::<Vec<_>>(),
            "server_exits": ids.iter().enumerate().map(|(index, id)| json!({
                "slot_id": id, "allocation_id": allocation(slots + index), "reason": "config_rejected", "exit_code": null, "detail": control.repeat(n(SERVER_EXIT_DETAIL_MAX_CHARS)),
            })).collect::<Vec<_>>(),
            "event_backlog": {
                "servers": (0..backlogs).map(|index| json!({ "allocation_id": allocation(index), "pending": MAX_SAFE_INTEGER / i64::try_from(backlogs).unwrap() })).collect::<Vec<_>>(),
                "server_count": MAX_SAFE_INTEGER, "event_count": MAX_SAFE_INTEGER, "reason": control.repeat(n(EVENT_BACKLOG_REASON_MAX_CHARS)),
            },
            "agent_update": { "image": image, "step": null, "outcome": "failed", "detail": control.repeat(n(AGENT_UPDATE_DETAIL_MAX_CHARS)) },
        });
        let parsed = safe_parse(s_agent_to_controller_message, largest.clone()).unwrap();
        let bytes = largest.to_string().len();

        assert!(bytes < n(AGENT_FRAME_MAX_BYTES) / 2, "{bytes}");
        assert_eq!(parsed.to_string().len(), bytes);
    }
}

#[test]
fn takes_an_event_backlog_reason_of_up_to_its_limit_and_no_more() {
    let backlog = |length: usize| json!({ "type": "event_backlog", "servers": [{ "allocation_id": "00000000-0000-4000-8000-000000000001", "pending": 3 }], "server_count": 1, "event_count": 3, "reason": "x".repeat(length) });

    assert!(passes(s_agent_to_controller_message, &backlog(n(EVENT_BACKLOG_REASON_MAX_CHARS))));
    assert!(!passes(s_agent_to_controller_message, &backlog(n(EVENT_BACKLOG_REASON_MAX_CHARS) + 1)));
}

#[test]
fn takes_a_console_command_of_up_to_its_limit_and_no_more() {
    let command = |length: usize| json!({ "type": "console_command", "id": "r1", "allocation_id": "00000000-0000-4000-8000-000000000001", "command": "x".repeat(length) });

    assert!(passes(s_controller_to_agent_message, &command(n(CONSOLE_COMMAND_MAX_CHARS))));
    assert!(!passes(s_controller_to_agent_message, &command(n(CONSOLE_COMMAND_MAX_CHARS) + 1)));
}

mod round_backups {
    use super::*;

    /// A `round_backup` whose file is the longest the contract allows, made of `unit`.
    fn backup_frame(unit: &str) -> Value {
        json!({
            "type": "event", "allocation_id": "0199a0b4-5c00-7000-8000-0000000000a1", "seq": MAX_SAFE_INTEGER,
            "event": {
                "type": "round_backup", "match_id": MATCH_ID, "timestamp": jiff::Timestamp::now().as_millisecond(), "tick": MAX_SAFE_INTEGER,
                "round": 40, "team1_side": "ct", "team1_score": 20, "team2_score": 19,
                "rules": { "max_rounds": 30, "overtime": true, "overtime_max_rounds": 60, "friendly_fire": false },
                "file": unit.repeat(n(MAX_BACKUP_FILE_CHARS) / unit.chars().count()), "roster": match_config(&json!({}))["roster"],
            },
        })
    }

    // Tabs, newlines and quotes take two bytes in JSON, a player's name up to three a character.
    #[test]
    fn passes_the_relay_and_fits_a_batch_when_its_file_is_all_tabs_and_quotes_or_three_byte_characters() {
        for unit in ["\t\"", "\u{20ac}"] {
            let frame = backup_frame(unit);

            assert!(passes(s_plugin_event_frame, &frame), "{unit:?}");
            assert!(frame.to_string().len() <= n(MAX_FRAME_BYTES), "{unit:?}");
        }
    }

    #[test]
    fn carries_text_tabs_and_line_breaks_and_no_other_control_character() {
        assert!(passes(s_plugin_event_frame, &backup_frame("\t\n\r\"x\u{20ac}")));

        for control in ['\u{1}', '\u{1b}', '\u{7f}', '\u{85}'] {
            let issues = safe_parse(s_plugin_event_frame, backup_frame(&format!("x{control}"))).unwrap_err();

            assert!(issues.iter().any(|issue| issue.message.contains("control characters")), "{control:?}: {issues:?}");
        }
    }

    #[test]
    fn leaves_room_in_one_frame_from_the_controller_for_a_launch_carrying_it_as_a_restore_will() {
        let file = backup_frame("\t\"")["event"]["file"].clone();
        let mut launch = serde_json::to_value(launch_config(&json!({ "commands": vec!["\u{20ac}".repeat(256); 128] }))).unwrap();

        launch["file"] = file;

        let frame = json!({ "type": "start_server", "id": "00000000-0000-4000-8000-000000000002", "allocation_id": "00000000-0000-4000-8000-000000000003", "launch": launch });

        assert!(frame.to_string().len() <= n(CONTROLLER_FRAME_MAX_BYTES));
    }
}

#[test]
fn a_token_bucket_spends_its_burst_refills_at_its_rate_and_gives_a_grown_budget_its_extra_burst_at_once() {
    let mut bucket = TokenBucket::new(Rate { burst: 3.0, per_second: 2.0 }, 0.0);

    assert_eq!([1, 2, 3, 4].map(|_| bucket.take(1.0, 0.0)), [true, true, true, false]);
    assert!(!bucket.take(1.0, 499.0));
    assert!(bucket.take(1.0, 500.0));
    assert!(bucket.take(1.0, 10_000.0));
    assert!(!bucket.take(3.0, 10_000.0));
    bucket.resize(Rate { burst: 10.0, per_second: 2.0 }, 10_000.0);
    assert!(bucket.take(9.0, 10_000.0));
    assert!(!bucket.take(1.0, 10_000.0));
}

/// The controller's buckets for one connection of a node with `slots` slots, on a clock the test moves.
struct Relay {
    buckets: Mutex<HashMap<BudgetName, TokenBucket>>,
    over: Mutex<Vec<String>>,
    /// Every frame received, with its bytes.
    received: Mutex<Vec<(AgentToControllerMessage, usize)>>,
    now: Arc<AtomicI64>,
}

impl Relay {
    fn new(slots: usize) -> Arc<Self> {
        let buckets = BudgetName::ALL.iter().map(|name| (*name, TokenBucket::new(budget_rate(name.budget(), slots), 0.0))).collect();

        Arc::new(Self { buckets: Mutex::new(buckets), over: Mutex::default(), received: Mutex::default(), now: Arc::default() })
    }

    fn set_now(&self, at: i64) {
        self.now.store(at, Ordering::SeqCst);
    }

    /// The frames received whose message `matches`, and their bytes.
    fn received(&self, matches: impl Fn(&AgentToControllerMessage) -> bool) -> (usize, usize) {
        self.received.lock().unwrap().iter().filter(|(message, _)| matches(message)).fold((0, 0), |(frames, total), (_, bytes)| (frames + 1, total + bytes))
    }

    fn over(&self) -> Vec<String> {
        self.over.lock().unwrap().clone()
    }

    #[allow(clippy::cast_precision_loss, reason = "a frame's bytes and a test clock")]
    fn receive(&self, frame: &AgentToControllerMessage) {
        let text = serde_json::to_string(frame).unwrap();
        let bytes = text.len();
        // For the line about a frame over its budget only.
        let kind = serde_json::from_str::<Value>(&text).unwrap()["type"].as_str().unwrap().to_owned();
        let budget = frame_budget(frame);
        let now = self.now.load(Ordering::SeqCst) as f64;
        let mut buckets = self.buckets.lock().unwrap();
        let mut spent = vec![buckets.get_mut(&BudgetName::Bytes).unwrap().take(bytes as f64, now)];

        self.received.lock().unwrap().push((frame.clone(), bytes));

        if budget == BudgetName::LogFrames {
            spent.push(buckets.get_mut(&BudgetName::LogFrames).unwrap().take(1.0, now));
            spent.push(buckets.get_mut(&BudgetName::LogBytes).unwrap().take(bytes as f64, now));
        } else if budget != BudgetName::StrayReplies {
            spent.push(buckets.get_mut(&budget).unwrap().take(1.0, now));
        }

        if spent.contains(&false) {
            self.over.lock().unwrap().push(format!("{kind} at {now} ms"));
        }

        assert!(bytes <= n(AGENT_FRAME_MAX_BYTES));
    }
}

impl Transport for Relay {
    fn connected(&self) -> bool {
        true
    }

    fn send(&self, frame: AgentToControllerMessage) {
        self.receive(&frame);
    }
}

mod our_agent_within_the_budgets {
    use super::*;

    /// Feeds the manager's events, in order, into the feed.
    fn pump(base: &Slots, feed: &SlotFeed, seen: &mut usize) {
        let events = base.events();

        for event in &events[*seen..] {
            let event = match event.clone() {
                Reported::ServerExited(exit, _) => SlotEvent::ServerExited(exit),
                Reported::SlotState(status) => SlotEvent::SlotState(status),
                Reported::ContainerLog { allocation_id, line } => SlotEvent::ContainerLog { allocation_id, line },
                Reported::SlotIdle(slot_id) => SlotEvent::SlotIdle { slot_id, allocation_id: None },
                Reported::SlotRemoved(slot_id) => SlotEvent::SlotRemoved(slot_id),
                Reported::TokenUsed(token) => SlotEvent::TokenUsed(token),
            };

            feed.handle(&event);
        }

        *seen = events.len();
    }

    #[test]
    #[allow(clippy::cast_precision_loss, reason = "small whole numbers")]
    fn pings_well_within_the_ping_budget() {
        assert!(AGENT_PING_INTERVAL.as_secs_f64() > 10.0 / agent_budgets::pings::base::PER_SECOND as f64);
    }

    #[tokio::test]
    async fn stays_within_them_when_every_slot_of_a_max_slots_slot_node_fills_at_once_and_empties_on_the_next_poll() {
        let relay = Relay::new(n(MAX_SLOTS));
        let clock = Arc::clone(&relay.now);
        let node_log = NodeLog::new(Arc::new(move || clock.load(Ordering::SeqCst)), tokio_util::sync::CancellationToken::new());
        let ids = slot_ids(n(MAX_SLOTS));
        let (to_reports, to_exits) = (Arc::clone(&relay), Arc::clone(&relay));
        let reports = ChangeReporter::new(
            |frame: &AgentSlotStatus| frame.slot_id.clone(),
            Arc::new(move |frame| {
                to_reports.receive(&AgentToControllerMessage::SlotStatus(frame));
                async { true }.boxed()
            }),
        );
        let exits = ExitReporter::new(
            Arc::new(move |frame| {
                to_exits.receive(&AgentToControllerMessage::ServerExited(frame));
                async { true }.boxed()
            }),
            None,
            ankor_agent::logger::Logger::null(),
        );
        let slots: Vec<Value> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| json!({ "id": id, "port": 20000 + 2 * index, "tv_port": 20001 + 2 * index, "gslt": format!("TOKEN{index:08}") }))
            .collect();
        let base = make_slots(agent_config(&json!({ "slots": slots })));
        let feed = SlotFeed::new(Arc::new(reports.clone()), exits, node_log);
        let mut seen = 0;
        let players = |slot: usize| (0..10).map(|index| format!("7656119800000{:04}", slot * 10 + index)).collect::<Vec<_>>();
        let status = |steam_ids: &[String]| -> ankor_agent::contract::types::ServerStatus {
            serde_json::from_value(json!({
                "type": "server_status", "request_id": null, "map_name": "de_mirage", "match": null, "config_error": null, "ready": true, "server_steam_id": null,
                "players": steam_ids.iter().map(|steam_id| json!({ "steam_id": steam_id, "name": steam_id, "side": "spec" })).collect::<Vec<_>>(),
            }))
            .unwrap()
        };

        // At most, the welcome after a reconnect reports every slot.
        for status in base.slots.statuses() {
            reports.report(AgentSlotStatus {
                slot_id: status.slot_id,
                state: status.state,
                allocation_id: status.allocation_id,
                match_id: status.match_id,
                plugin_online: status.plugin_online,
                gslt_state: status.gslt_state,
                reserved: status.reserved,
                server_steam_id: status.server_steam_id.clone(),
            });
        }

        let allocations: Vec<String> = (0..ids.len()).map(|index| format!("55555555-5555-4555-8555-{index:012}")).collect();

        for allocation_id in &allocations {
            launch_server(&base.slots, launch_config(&json!({})), allocation_id).await.unwrap();
            pump(&base, &feed, &mut seen);
        }

        for (index, allocation_id) in allocations.iter().enumerate() {
            base.slots.plugin_online_changed(allocation_id, true);
            base.slots.plugin_status(allocation_id, &status(&[]));
            base.slots.plugin_status(allocation_id, &status(&players(index)));
            pump(&base, &feed, &mut seen);
        }

        relay.set_now(30_000);

        for allocation_id in &allocations {
            base.slots.plugin_status(allocation_id, &status(&[]));
            pump(&base, &feed, &mut seen);
        }

        assert_eq!(relay.over(), Vec::<String>::new());
        // Start, linked, running: three a slot, and the welcome's one; players stay on the node.
        assert_eq!(relay.received(|message| matches!(message, AgentToControllerMessage::SlotStatus(_))).0, n(MAX_SLOTS) * 3 + n(MAX_SLOTS));
    }

    #[tokio::test(start_paused = true)]
    async fn stays_within_them_with_full_sources_of_the_longest_lines_several_watched_at_once_and_replayed_again_and_again() {
        let relay = Relay::new(1);
        let clock = Arc::clone(&relay.now);
        let node_log = NodeLog::new(Arc::new(move || clock.load(Ordering::SeqCst)), tokio_util::sync::CancellationToken::new());
        let games: Vec<String> = (0..n(MAX_SLOTS) + 10).map(|index| format!("66666666-6666-4666-8666-{index:012}")).collect();
        let longest = |index: usize| format!("{index} {}", "\u{1}".repeat(n(LOG_LINE_MAX_CHARS)));
        let update_id = UPDATE_ID.to_owned();

        node_log.attach(Arc::clone(&relay) as Arc<dyn Transport>);

        for allocation_id in &games {
            for index in 0..n(SERVER_LOG_LINES) {
                node_log.game_line(allocation_id, &longest(index));
            }
        }

        node_log.update_started(&update_id);

        for index in 0..n(SERVER_LOG_LINES) {
            node_log.update_line(&longest(index));
            node_log.agent_line(&Entry { at_ms: i64::try_from(index).unwrap(), level: Level::Info, message: longest(index) });
        }

        // Four admins on four tabs, each switching to another game's tab every second for two minutes,
        // while the four newest prints twelve of the longest lines a second. A one-slot node has the least room.
        let mut watched: Vec<String> = games[..4].to_vec();

        node_log.watch(&LogSource::Agent(LogSourceAgent {}), None);
        node_log.watch(&LogSource::Cs2Update(LogSourceCs2Update { update_id }), None);

        for tick in 1..=480 {
            relay.set_now(tick * 250);

            if tick % 4 == 0 {
                let next = games[usize::try_from(tick / 4).unwrap() % games.len()].clone();

                watched.push(next.clone());
                node_log.watch(&LogSource::Server(LogSourceServer { allocation_id: next }), Some(&format!("replay-{tick}")));
            }

            for allocation_id in &watched[watched.len() - 4..] {
                for index in 0..3 {
                    node_log.game_line(allocation_id, &longest(index));
                }
            }

            tokio::time::sleep(Duration::from_millis(250)).await;

            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        }

        assert_eq!(relay.over(), Vec::<String>::new());
        assert!(relay.received(|message| matches!(message, AgentToControllerMessage::LogBatch(_))).1 > n(agent_budgets::log_bytes::base::BURST));
        assert!(relay.received(|message| matches!(message, AgentToControllerMessage::LogIndex(_))).0 > 100);
    }
}
