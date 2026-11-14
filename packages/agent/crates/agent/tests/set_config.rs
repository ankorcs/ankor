//! `set_config`: the controller edits a node (its ports, its tokens, its pinned connect address, or the
//! address it reaches the controller at), and the agent has to end up with all of it in three places: its
//! live slot map, the `hello` it sends next, and the `agent.yml` it will boot from, without disturbing a
//! server that is already running. The persistence half runs against a real temporary `agent.yml`,
//! because what is worth testing is that the rest of the file survives: an operator edits it too.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::agent_reporting::{HelloInput, build_hello, reported_config};
use ankor_agent::config::{ConfigPatch, load_agent_config, patch_config_document, persist_config_patch};
use ankor_agent::controller_connection::HandlerError;
use ankor_agent::gslt_keep_alive::GsltSlotsPort;
use ankor_agent::testing::fixtures::{GSLT_A, GSLT_B, agent_config, launch_config};
use ankor_agent::testing::handler::{Handler, make_handler};
use ankor_agent::testing::slots::{Slots, launch_server, make_slots};
use ankor_agent::testing::until;
use ankor_protocol::config::{AgentConfig, DemoUpload};
use ankor_protocol::helpers::gslt_hint;
use ankor_protocol::schemas::{s_agent_config_schema, s_controller_set_config, s_node_settings_edit__slots};
use ankor_protocol::types::{
    AgentErrorCode, AgentInfo, AgentToControllerMessage, ConfiguredSlot, ControllerFields, Cs2State, DemoUploadHeader, EventBacklog, GsltState, HostInfo,
    SlotState,
};
use ankor_protocol::validate::safe_parse;
use serde_json::{Value, json};

const ORIGINAL: &str = "# hand-written comment that must survive
token: 0123456789abcdef0123456789abcdef
controller:
  host: cs.example.com
  tls: true
cs2_image: ghcr.io/example/ankor-cs2:latest
agent_image: ghcr.io/example/ankor-agent:latest

# how many servers this box runs
slots:
  - id: a
    port: 27015
    tv_port: 27016
    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001

# a host that runs other things it cares about
memory_mb: 6144
";

const GSLT_C: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCC0003";
/// The address the fixture config already has, so most requests change nothing about it.
const SAME_HOST: &str = "cs.example.com";
const CURRENT_PUBLIC_IP: &str = "203.0.113.7";

fn slot(id: &str, port: i64, tv_port: i64, gslt: &str) -> ConfiguredSlot {
    ConfiguredSlot { id: id.to_owned(), port, tv_port, gslt: gslt.to_owned() }
}

fn next() -> Vec<ConfiguredSlot> {
    vec![slot("a", 27015, 27016, GSLT_A), slot("b", 27017, 27018, GSLT_B)]
}

/// The slots and pinned address the fixture config already runs, so a `set_config` built from them
/// is the one the controller sends on every `hello`: nothing about it changed.
fn current_slots() -> Vec<ConfiguredSlot> {
    vec![slot("a", 27015, 27020, GSLT_A), slot("b", 27016, 27021, GSLT_B)]
}

/// The slots as the controller asks for them, keeping each slot's token.
fn keep(slots: &[ConfiguredSlot]) -> Vec<Value> {
    slots.iter().map(|slot| json!({ "id": slot.id, "port": slot.port, "tv_port": slot.tv_port })).collect()
}

fn with_token(edit: &Value, gslt: &str) -> Value {
    let mut edit = edit.clone();

    edit["gslt"] = json!(gslt);
    edit
}

fn same_controller() -> Value {
    json!({ "host": SAME_HOST, "tls": true, "socket_path": "/ws/agent", "events_path": "/api/node/events" })
}

fn yaml(text: &str) -> Value {
    yaml_serde::from_str(text).unwrap()
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn allocation() -> String {
    format!("0199a0b4-5c00-7000-8000-{:012x}", NEXT_ID.fetch_add(1, Ordering::SeqCst))
}

fn code_of(error: &HandlerError) -> AgentErrorCode {
    error.refusal().unwrap_or_else(|| panic!("no agent failure: {error}")).code
}

fn temp_config(dir: &Path) -> PathBuf {
    let file = dir.join("agent.yml");

    std::fs::write(&file, ORIGINAL).unwrap();
    file
}

fn read(file: &Path) -> String {
    std::fs::read_to_string(file).unwrap()
}

fn hello(config: &AgentConfig, slot_status: Vec<ankor_protocol::types::SlotStatus>) -> Value {
    let message = build_hello(HelloInput {
        agent: AgentInfo { version: "0.1.0".to_owned(), image: config.agent_image.clone(), image_id: format!("sha256:{}", "a".repeat(64)) },
        host: HostInfo { cpu_count: 8, os_name: None, kernel: "6.12.48".to_owned() },
        public_ip: Some(CURRENT_PUBLIC_IP.to_owned()),
        country: Some("DK".to_owned()),
        config: config.clone(),
        slot_status,
        cs2: Cs2State { build_id: None, latest_build_id: None, image: config.cs2_image.clone(), image_id: None, update: None, image_update: None },
        unavailable: None,
        demos: Vec::new(),
        server_exits: Vec::new(),
        event_backlog: EventBacklog { servers: Vec::new(), server_count: 0, event_count: 0, reason: None },
        agent_update: None,
    });
    let AgentToControllerMessage::Hello(hello) = message else { panic!("a hello") };

    serde_json::to_value(hello).unwrap()
}

mod the_set_config_slot_layout {
    use super::*;

    fn layout_error(slots: &Value) -> Option<String> {
        safe_parse(s_node_settings_edit__slots, slots.clone()).err().map(|issues| issues[0].message.clone())
    }

    #[test]
    fn takes_a_list_the_controller_could_have_sent_the_empty_one_too_and_refuses_duplicate_ids_and_a_reused_port() {
        let edit = keep(&next());

        assert_eq!(layout_error(&json!(edit)), None);
        assert_eq!(layout_error(&json!([])), None);
        assert!(layout_error(&json!([edit[0], edit[0]])).unwrap().contains("duplicate slot id"));
        assert!(layout_error(&json!(keep(&[slot("a", 27015, 27016, ""), slot("b", 27016, 27018, "")]))).unwrap().contains("more than one slot"));
    }

    #[test]
    fn refuses_an_id_that_could_not_be_part_of_a_container_name() {
        assert!(layout_error(&json!(keep(&[slot("a/b", 27015, 27016, "")]))).is_some());
    }

    #[test]
    fn takes_a_new_token_or_none_per_slot_and_refuses_null_a_malformed_token_and_one_token_on_two_slots() {
        let edit = keep(&next());

        assert_eq!(layout_error(&json!([with_token(&edit[0], GSLT_C), edit[1]])), None);
        assert!(layout_error(&json!([with_token(&edit[0], "short")])).unwrap().contains("10 to 64 letters and digits"));
        assert!(layout_error(&json!([with_token(&edit[0], GSLT_C), with_token(&edit[1], GSLT_C)])).unwrap().contains("its own token"));

        let mut null = edit[0].clone();

        null["gslt"] = Value::Null;
        assert!(layout_error(&json!([null])).is_some());
    }

    /// The rule is the contract's and takes a port floor, and this side's floor is 1: the controller refuses
    /// anything below 1024 because a browser is where a person picks a port, while `agent.yml` is
    /// also edited by hand on the host and dockerd publishes any port.
    #[test]
    fn holds_a_port_to_the_whole_port_range_not_the_controllers_1024_floor() {
        assert_eq!(layout_error(&json!(keep(&[slot("a", 80, 443, "")]))), None);
        assert!(layout_error(&json!(keep(&[slot("a", 0, 27016, "")]))).is_some());
        assert!(layout_error(&json!(keep(&[slot("a", 27015, 70000, "")]))).is_some());
    }
}

mod patch_config_document {
    use super::*;

    fn patch(source: &str, patch: &ConfigPatch) -> String {
        patch_config_document(Path::new("agent.yml"), source, patch).unwrap()
    }

    fn slots_patch() -> ConfigPatch {
        ConfigPatch { slots: Some(next()), ..ConfigPatch::default() }
    }

    fn controller(host: &str, tls: bool, socket_path: &str, events_path: &str) -> ControllerFields {
        ControllerFields { host: host.to_owned(), tls, socket_path: socket_path.to_owned(), events_path: events_path.to_owned() }
    }

    #[test]
    fn replaces_the_slots_and_leaves_every_other_key_and_comment_alone() {
        let out = patch(ORIGINAL, &slots_patch());

        for kept in [
            "# hand-written comment that must survive",
            "# how many servers this box runs",
            "# a host that runs other things it cares about",
            "memory_mb: 6144",
            "token: 0123456789abcdef0123456789abcdef",
        ] {
            assert!(out.contains(kept), "{kept}");
        }

        let parsed = yaml(&out);

        assert_eq!(parsed["memory_mb"], 6144);
        assert_eq!(
            parsed["slots"],
            json!([{ "id": "a", "port": 27015, "tv_port": 27016, "gslt": GSLT_A }, { "id": "b", "port": 27017, "tv_port": 27018, "gslt": GSLT_B }])
        );
    }

    #[test]
    fn writes_each_optional_key_only_when_it_is_given() {
        let cases = [
            (
                ConfigPatch { controller: Some(controller("127.0.0.1:3000", false, "/ws/agent", "/api/node/events")), ..slots_patch() },
                json!({ "host": "127.0.0.1:3000", "tls": false }),
            ),
            (
                ConfigPatch { controller: Some(controller("127.0.0.1:3000", false, "/ankor/agent", "/ankor/events")), ..slots_patch() },
                json!({ "host": "127.0.0.1:3000", "tls": false, "socket_path": "/ankor/agent", "events_path": "/ankor/events" }),
            ),
        ];

        for (patched, expected) in cases {
            assert_eq!(yaml(&patch(ORIGINAL, &patched))["controller"], expected);
        }

        assert_eq!(yaml(&patch(ORIGINAL, &ConfigPatch { pinned_ip: Some(Some("203.0.113.10".to_owned())), ..slots_patch() }))["pinned_ip"], "203.0.113.10");

        // Absent leaves the lines an operator is reading exactly as they were, and says nothing
        // about an address the file never pinned.
        let untouched = patch(ORIGINAL, &slots_patch());

        assert!(untouched.contains("  host: cs.example.com"));
        assert!(yaml(&untouched).get("pinned_ip").is_none());

        // Cleared is not absent: the controller said "resolve your own address again".
        let pinned = patch(ORIGINAL, &ConfigPatch { pinned_ip: Some(Some("203.0.113.10".to_owned())), ..slots_patch() });

        assert!(yaml(&patch(&pinned, &ConfigPatch { pinned_ip: Some(None), ..ConfigPatch::default() })).get("pinned_ip").is_none());
    }

    #[test]
    fn writes_the_demo_upload_target_and_takes_the_key_back_out_when_it_is_cleared() {
        let header = DemoUploadHeader { name: "X-Demo-Key".to_owned(), value: "k".to_owned() };
        let with_header = patch(
            ORIGINAL,
            &ConfigPatch {
                demo_upload: Some(Some(DemoUpload { url: "https://demos.example.com/put".to_owned(), header: Some(header), limit_mbit: None })),
                ..slots_patch()
            },
        );

        assert_eq!(yaml(&with_header)["demo_upload"], json!({ "url": "https://demos.example.com/put", "header": { "name": "X-Demo-Key", "value": "k" } }));

        let no_header = patch(
            &with_header,
            &ConfigPatch {
                demo_upload: Some(Some(DemoUpload { url: "https://demos.example.com/put".to_owned(), header: None, limit_mbit: None })),
                ..slots_patch()
            },
        );

        assert_eq!(yaml(&no_header)["demo_upload"], json!({ "url": "https://demos.example.com/put" }));
        // Uploads off is the key's absence, which is also what a fresh file looks like.
        assert!(yaml(&patch(&with_header, &ConfigPatch { demo_upload: Some(None), ..slots_patch() })).get("demo_upload").is_none());
        assert!(safe_parse(s_agent_config_schema, yaml(&with_header)).is_ok());
    }

    #[test]
    fn writes_auto_update_only_when_it_is_off_and_takes_the_key_back_out_when_it_is_on() {
        let held = patch(ORIGINAL, &ConfigPatch { auto_update: Some(false), ..ConfigPatch::default() });

        assert!(held.contains("auto_update: false"));
        assert_eq!(safe_parse(s_agent_config_schema, yaml(&held)).unwrap()["auto_update"], false);
        assert!(yaml(&patch(&held, &ConfigPatch { auto_update: Some(true), ..ConfigPatch::default() })).get("auto_update").is_none());
    }

    #[test]
    fn writes_the_memory_cap_as_a_number_and_takes_the_key_out_for_none() {
        let capped = patch(ORIGINAL, &ConfigPatch { memory_mb: Some(Some(6144)), ..ConfigPatch::default() });

        assert!(capped.contains("memory_mb: 6144"), "{capped}");
        assert_eq!(safe_parse(s_agent_config_schema, yaml(&capped)).unwrap()["memory_mb"], 6144);
        assert!(yaml(&patch(&capped, &ConfigPatch { memory_mb: Some(None), ..ConfigPatch::default() })).get("memory_mb").is_none());
    }

    #[test]
    fn leaves_a_document_the_agent_can_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let file = temp_config(dir.path());

        persist_config_patch(&file, &ConfigPatch { pinned_ip: Some(Some("203.0.113.10".to_owned())), ..slots_patch() }).unwrap();

        let config = load_agent_config(&file, |_| {}).unwrap();

        assert_eq!(config.slots, next());
        assert_eq!(config.pinned_ip.as_deref(), Some("203.0.113.10"));
        assert!(safe_parse(s_agent_config_schema, yaml(&read(&file))).is_ok());
    }
}

mod the_set_config_request {
    use super::*;

    struct Setup {
        base: Slots,
        handler: Handler,
        file: PathBuf,
        controller_addresses: Arc<Mutex<Vec<ControllerFields>>>,
        polled: Arc<AtomicUsize>,
        _dir: tempfile::TempDir,
    }

    impl Setup {
        async fn request(&self, frame: Value) -> Result<Value, HandlerError> {
            let result = self.handler.request(frame).await;

            // Work owed after the reply runs as soon as the call has answered.
            settle().await;
            result
        }

        async fn set(&self, config: Value) -> Result<Value, HandlerError> {
            self.request(json!({ "type": "set_config", "id": "r1", "config": config })).await
        }

        fn config(&self) -> AgentConfig {
            self.base.config.lock().unwrap().clone()
        }

        fn checked(&self) -> Vec<String> {
            self.handler.tokens.checked.lock().unwrap().clone()
        }
    }

    async fn settle() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    fn setup(overrides: &Value) -> Setup {
        let base = make_slots(agent_config(overrides));
        let dir = tempfile::tempdir().unwrap();
        let file = temp_config(dir.path());
        let controller_addresses: Arc<Mutex<Vec<ControllerFields>>> = Arc::default();
        let polled = Arc::new(AtomicUsize::new(0));
        let (addresses, config, poll, path) = (Arc::clone(&controller_addresses), Arc::clone(&base.config), Arc::clone(&polled), file.clone());
        let handler = make_handler(&base, move |deps| {
            deps.config = deps.config.at(path);
            deps.redial = Arc::new(move || addresses.lock().unwrap().push(config.lock().unwrap().controller.clone()));
            deps.poll_images = Arc::new(move || {
                poll.fetch_add(1, Ordering::SeqCst);
            });
        });

        Setup { base, handler, file, controller_addresses, polled, _dir: dir }
    }

    fn current_edit() -> Vec<Value> {
        keep(&current_slots())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_edits_at_once_both_land_in_memory_and_on_disk() {
        for _ in 0..20 {
            let setup = Arc::new(setup(&json!({})));
            let (one, two) = (Arc::clone(&setup), Arc::clone(&setup));
            let pin = tokio::spawn(async move { one.set(json!({ "pinned_ip": "203.0.113.10" })).await });
            let off = tokio::spawn(async move { two.set(json!({ "auto_update": false })).await });

            pin.await.unwrap().unwrap();
            off.await.unwrap().unwrap();

            let config = setup.config();
            let disk = load_agent_config(&setup.file, |_| {}).unwrap();

            assert_eq!((config.pinned_ip.as_deref(), config.auto_update), (Some("203.0.113.10"), false));
            assert_eq!((disk.pinned_ip.as_deref(), disk.auto_update), (Some("203.0.113.10"), false));
        }
    }

    #[tokio::test]
    async fn turns_automatic_updates_off_and_on_keeps_it_through_edits_that_leave_it_out_and_catches_up_once_it_is_back_on() {
        let setup = setup(&json!({}));

        assert_eq!(setup.set(json!({ "auto_update": false })).await.unwrap()["auto_update"], false);
        assert!(!setup.config().auto_update);
        assert_eq!(yaml(&read(&setup.file))["auto_update"], false);

        assert_eq!(setup.set(json!({ "pinned_ip": "198.51.100.4" })).await.unwrap()["auto_update"], false);
        assert_eq!(*setup.handler.public_build.catch_ups.lock().unwrap(), 0);

        assert_eq!(setup.set(json!({ "auto_update": true })).await.unwrap()["auto_update"], true);
        assert!(setup.config().auto_update);
        assert!(yaml(&read(&setup.file)).get("auto_update").is_none());
        assert_eq!((*setup.handler.public_build.catch_ups.lock().unwrap(), setup.polled.load(Ordering::SeqCst)), (1, 1));

        // Already on: nothing changes, so nothing is reconsidered.
        setup.set(json!({ "auto_update": true })).await.unwrap();
        assert_eq!((*setup.handler.public_build.catch_ups.lock().unwrap(), setup.polled.load(Ordering::SeqCst)), (1, 1));
    }

    #[tokio::test]
    async fn reserves_a_slot_until_the_set_config_that_gives_it_a_new_token_and_no_launch_takes_it_meanwhile() {
        let setup = setup(&json!({}));
        let allocation_id = allocation();
        let edit: Vec<Value> = current_edit().into_iter().map(|slot| if slot["id"] == "a" { with_token(&slot, GSLT_C) } else { slot }).collect();

        assert_eq!(setup.request(json!({ "type": "reserve_slot", "id": "r1", "slot_id": "a" })).await.unwrap(), json!({ "slot_id": "a" }));
        assert_eq!(
            setup.request(json!({ "type": "start_server", "id": "r2", "allocation_id": allocation_id, "launch": launch_config(&json!({})) })).await.unwrap(),
            json!({ "allocation_id": allocation_id, "slot_id": "b" })
        );
        setup.set(json!({ "pinned_ip": CURRENT_PUBLIC_IP, "demo_upload": null, "slots": edit, "controller": same_controller() })).await.unwrap();

        let status = setup.base.status("a");

        assert_eq!((status.state, status.reserved, status.gslt_state), (SlotState::Idle, false, GsltState::Unchecked));
    }

    #[tokio::test]
    async fn refuses_to_reserve_a_slot_holding_a_server() {
        let setup = setup(&json!({}));

        setup.request(json!({ "type": "start_server", "id": "r1", "allocation_id": allocation(), "launch": launch_config(&json!({})) })).await.unwrap();
        assert_eq!(code_of(&setup.request(json!({ "type": "reserve_slot", "id": "r2", "slot_id": "a" })).await.unwrap_err()), AgentErrorCode::SlotHeld);
    }

    mod a_demo_upload_header_sent_without_its_value {
        use super::*;

        fn current() -> Value {
            json!({ "url": "https://demos.example.com/put", "header": { "name": "X-Demo-Key", "value": "s3cr3t" } })
        }

        #[tokio::test]
        async fn keeps_the_value_the_node_sends_also_when_the_hellos_own_demo_upload_comes_back_changed() {
            let setup = setup(&json!({ "demo_upload": current() }));
            let reported = hello(&setup.config(), Vec::new())["config"]["demo_upload"].clone();

            assert_eq!(reported["header"]["name"], "X-Demo-Key");
            assert!(regex::Regex::new("^[0-9a-f]{64}$").unwrap().is_match(reported["header"]["value_sha256"].as_str().unwrap()));

            let mut edit = reported.clone();

            edit["url"] = json!("https://demos.example.com/v2");
            edit["limit_mbit"] = json!(40);

            let frame = safe_parse(s_controller_set_config, json!({ "type": "set_config", "id": "r1", "config": { "demo_upload": edit } })).unwrap();

            setup.request(frame).await.unwrap();

            let expected = json!({ "url": "https://demos.example.com/v2", "header": current()["header"], "limit_mbit": 40 });

            assert_eq!(serde_json::to_value(setup.config().demo_upload).unwrap(), expected);
            assert_eq!(yaml(&read(&setup.file))["demo_upload"], expected);
        }

        #[tokio::test]
        async fn is_refused_on_a_node_without_uploads_one_uploading_without_a_header_or_one_sending_another_header_saving_nothing() {
            let uploads = [
                Value::Null,
                json!({ "url": current()["url"] }),
                json!({ "url": current()["url"], "header": { "name": "Authorization", "value": "Bearer x" } }),
            ];

            for upload in uploads {
                let setup = setup(&if upload.is_null() { json!({}) } else { json!({ "demo_upload": upload }) });
                let before = read(&setup.file);
                let kept = setup.config().demo_upload;
                let error = setup
                    .set(
                        json!({ "controller": same_controller(), "demo_upload": { "url": current()["url"], "header": { "name": "X-Demo-Key" }, "limit_mbit": null } }),
                    )
                    .await
                    .unwrap_err();

                assert_eq!(code_of(&error), AgentErrorCode::DemoHeaderValueRequired);
                assert!(error.to_string().contains("X-Demo-Key"));
                assert_eq!(setup.config().demo_upload, kept);
                assert_eq!(read(&setup.file), before);
            }
        }
    }

    #[tokio::test]
    async fn persists_a_new_demo_upload_target_clears_it_again_and_rescans_for_demos_on_every_edit_that_changes_something() {
        let setup = setup(&json!({}));
        let target = json!({ "url": "https://demos.example.com/put", "header": { "name": "X-Demo-Key", "value": "k" }, "limit_mbit": null });
        let edit = |upload: Value| json!({ "slots": current_edit(), "pinned_ip": CURRENT_PUBLIC_IP, "controller": same_controller(), "demo_upload": upload });

        setup.set(edit(target.clone())).await.unwrap();
        assert_eq!(serde_json::to_value(setup.config().demo_upload).unwrap(), target);
        assert_eq!(yaml(&read(&setup.file))["demo_upload"]["header"], target["header"]);
        // Demos the node kept while it had nowhere to send them go now.
        assert_eq!(*setup.handler.demos.scans.lock().unwrap(), 1);

        // The same target again changes nothing: the file stays as it was, and nothing is scanned.
        let before = read(&setup.file);

        setup.set(edit(target)).await.unwrap();
        assert_eq!(read(&setup.file), before);
        assert_eq!(*setup.handler.demos.scans.lock().unwrap(), 1);

        setup.set(edit(Value::Null)).await.unwrap();
        assert_eq!(setup.config().demo_upload, None);
        assert!(yaml(&read(&setup.file)).get("demo_upload").is_none());
        assert_eq!(*setup.handler.demos.scans.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn applies_the_list_persists_it_and_answers_with_it_as_the_next_hello_reports_it() {
        let setup = setup(&json!({}));
        let reply = setup.set(json!({ "pinned_ip": null, "slots": keep(&next()), "controller": same_controller(), "demo_upload": null })).await.unwrap();

        assert_eq!(reply, serde_json::to_value(reported_config(&setup.config())).unwrap());
        assert_eq!(setup.base.slots.statuses().iter().map(|status| status.slot_id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        // The same config every launch and every hello reads its ports from.
        assert_eq!(setup.config().slots, next());
        assert_eq!(yaml(&read(&setup.file))["slots"], serde_json::to_value(next()).unwrap());

        // And the next hello carries them, so the controller and the node agree without a restart.
        let hello = hello(&setup.config(), setup.base.slots.statuses());
        let expected: Vec<Value> =
            next().iter().map(|slot| json!({ "id": slot.id, "port": slot.port, "tv_port": slot.tv_port, "gslt_hint": gslt_hint(&slot.gslt) })).collect();

        assert_eq!(hello["config"]["slots"], json!(expected));
    }

    #[tokio::test]
    async fn leaves_the_running_layout_alone_when_agent_yml_cannot_be_written() {
        let base = make_slots(agent_config(&json!({})));
        let before = base.config.lock().unwrap().slots.clone();
        let mut grown = keep(&before);

        grown.push(json!({ "id": "c", "port": 27040, "tv_port": 27041, "gslt": GSLT_C }));

        let handler = make_handler(&base, |deps| {
            deps.config =
                deps.config.with_persist(Arc::new(|_, _| Err(ankor_agent::config::ConfigError::Io(std::io::Error::other("EROFS: read-only file system")))));
        });
        let error = handler
            .request(
                json!({ "type": "set_config", "id": "r1", "config": { "pinned_ip": null, "slots": grown, "controller": same_controller(), "demo_upload": null } }),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("EROFS"), "{error}");
        // The next hello's statuses must name exactly the slots it reports, or the controller refuses it.
        assert_eq!(base.config.lock().unwrap().slots, before);
        assert_eq!(
            base.slots.statuses().iter().map(|status| status.slot_id.clone()).collect::<Vec<_>>(),
            before.iter().map(|slot| slot.id.clone()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn drops_a_slot_that_is_gone_and_keeps_the_ones_that_stay() {
        let setup = setup(&json!({}));

        setup.set(json!({ "pinned_ip": null, "slots": [keep(&next())[0]], "controller": same_controller(), "demo_upload": null })).await.unwrap();
        assert_eq!(setup.base.slots.statuses().iter().map(|status| status.slot_id.as_str()).collect::<Vec<_>>(), ["a"]);
    }

    #[tokio::test]
    async fn removes_every_slot_leaving_a_node_that_reports_none_and_boots_with_none() {
        let setup = setup(&json!({}));

        setup.set(json!({ "pinned_ip": null, "slots": [], "controller": same_controller(), "demo_upload": null })).await.unwrap();
        assert!(setup.base.slots.statuses().is_empty());
        assert!(setup.config().slots.is_empty());
        assert!(load_agent_config(&setup.file, |_| {}).unwrap().slots.is_empty());
    }

    #[tokio::test]
    async fn refuses_changes_to_occupied_slots_without_saving_or_queueing_them() {
        let setup = setup(&json!({}));
        let allocation_id = allocation();
        let change =
            json!({ "pinned_ip": null, "controller": same_controller(), "demo_upload": null, "slots": [{ "id": "a", "port": 28000, "tv_port": 28001 }] });

        launch_server(&setup.base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();
        assert_eq!(code_of(&setup.set(change.clone()).await.unwrap_err()), AgentErrorCode::SlotHeld);
        assert_eq!(setup.config().slots, current_slots());
        assert_eq!(read(&setup.file), ORIGINAL);
        setup.base.slots.stop_allocation(&allocation_id).await.unwrap();
        assert_eq!(setup.config().slots, current_slots());
        assert!(setup.handler.pushed.lock().unwrap().is_empty());
        setup.set(change).await.unwrap();
        assert_eq!(setup.config().slots, [slot("a", 28000, 28001, GSLT_A)]);
    }

    #[tokio::test]
    async fn names_every_occupied_slot_a_refused_edit_would_change() {
        let setup = setup(&json!({}));

        launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();
        launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();

        let error = setup
            .set(json!({ "pinned_ip": null, "controller": same_controller(), "demo_upload": null, "slots": [{ "id": "a", "port": 28000, "tv_port": 28001 }, { "id": "b", "port": 28002, "tv_port": 28003 }] }))
            .await
            .unwrap_err();

        assert_eq!(code_of(&error), AgentErrorCode::SlotHeld);
        assert!(error.to_string().contains("a, b"), "{error}");
    }

    #[tokio::test]
    async fn applies_demo_and_idle_slot_changes_while_an_occupied_slot_stays_unchanged() {
        let setup = setup(&json!({}));

        launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();

        let next = vec![current_slots()[0].clone(), slot("b", 28000, 28001, GSLT_B), slot("c", 28002, 28003, GSLT_C)];
        let demo_upload = json!({ "url": "https://demos.example/upload", "header": null, "limit_mbit": null });
        let edit: Vec<Value> = next.iter().map(|slot| serde_json::to_value(slot).unwrap()).collect();
        let reply = setup.set(json!({ "pinned_ip": null, "controller": same_controller(), "demo_upload": demo_upload, "slots": edit })).await.unwrap();

        assert_eq!(serde_json::to_value(setup.config().demo_upload).unwrap(), demo_upload);
        assert_eq!(setup.config().slots, next);
        assert_eq!(reply, serde_json::to_value(reported_config(&setup.config())).unwrap());
        assert_eq!(
            setup.base.slots.statuses().iter().map(|status| (status.slot_id.as_str(), status.state)).collect::<Vec<_>>(),
            [("a", SlotState::Starting), ("b", SlotState::Idle), ("c", SlotState::Idle)]
        );
    }

    #[test]
    fn refuses_a_list_that_would_not_survive_the_next_boot_before_writing_anything() {
        let frame = json!({ "type": "set_config", "id": "r1", "config": {
            "pinned_ip": null,
            "slots": [{ "id": "a", "port": 27015, "tv_port": 27016, "gslt": GSLT_A }, { "id": "a", "port": 27017, "tv_port": 27018, "gslt": GSLT_C }],
            "controller": same_controller(), "demo_upload": null,
        } });
        let issues = safe_parse(s_controller_set_config, frame).unwrap_err();

        assert!(issues.iter().any(|issue| issue.message.contains("duplicate slot id")));
    }

    #[tokio::test]
    async fn clears_the_pinned_address() {
        let setup = setup(&json!({ "pinned_ip": "198.51.100.7" }));

        setup.set(json!({ "slots": keep(&next()), "pinned_ip": null, "controller": same_controller(), "demo_upload": null })).await.unwrap();
        assert_eq!(setup.config().pinned_ip, None);
        assert_eq!(safe_parse(s_agent_config_schema, yaml(&read(&setup.file))).unwrap()["pinned_ip"], Value::Null);
    }

    #[tokio::test]
    async fn persists_a_new_controller_address_updates_the_live_config_answers_with_it_and_redials_once() {
        let setup = setup(&json!({}));
        let controller = json!({ "host": "127.0.0.1:3000", "tls": false, "socket_path": "/ws/agent", "events_path": "/api/node/events" });
        let reply = setup.set(json!({ "pinned_ip": null, "slots": keep(&next()), "controller": controller, "demo_upload": null })).await.unwrap();

        assert_eq!((reply["controller"]["host"].clone(), reply["controller"]["tls"].clone()), (json!("127.0.0.1:3000"), json!(false)));
        assert_eq!(yaml(&read(&setup.file))["controller"], json!({ "host": "127.0.0.1:3000", "tls": false }));
        // The live config too: the next connect and every plugin URL read it.
        assert_eq!((setup.config().controller.host.as_str(), setup.config().controller.tls), ("127.0.0.1:3000", false));
        assert_eq!(serde_json::to_value(setup.controller_addresses.lock().unwrap().clone()).unwrap(), json!([controller]));
        // Still a file the agent can boot from.
        assert!(safe_parse(s_agent_config_schema, yaml(&read(&setup.file))).is_ok());
    }

    #[tokio::test]
    async fn keeps_every_setting_the_edit_leaves_out_and_changes_slots_on_a_busy_node_dialling_its_own_address() {
        let setup = setup(&json!({ "pinned_ip": "203.0.113.7" }));
        let before = setup.config();
        let mut edit = current_edit();

        edit.push(json!({ "id": "c", "port": 28002, "tv_port": 28003, "gslt": GSLT_C }));
        launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();
        setup.set(json!({ "slots": edit })).await.unwrap();

        let after = setup.config();

        assert_eq!((&after.pinned_ip, &after.controller, &after.demo_upload), (&before.pinned_ip, &before.controller, &before.demo_upload));
        assert_eq!(after.slots.iter().map(|slot| slot.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(setup.controller_addresses.lock().unwrap().is_empty());
        assert_eq!(yaml(&read(&setup.file))["controller"]["host"], SAME_HOST);
    }

    #[tokio::test]
    async fn takes_null_as_a_value_and_a_missing_field_as_none() {
        let setup = setup(&json!({ "pinned_ip": "203.0.113.7" }));

        setup.set(json!({})).await.unwrap();
        assert_eq!(setup.config().pinned_ip.as_deref(), Some("203.0.113.7"));
        setup.set(json!({ "pinned_ip": null })).await.unwrap();
        assert_eq!(setup.config().pinned_ip, None);
    }

    #[tokio::test]
    async fn says_nothing_about_the_address_when_it_did_not_change() {
        let setup = setup(&json!({}));

        setup.set(json!({ "pinned_ip": null, "slots": keep(&next()), "controller": same_controller(), "demo_upload": null })).await.unwrap();
        // An explicit edit with the same address does not reconnect.
        assert!(setup.controller_addresses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn redials_when_only_the_encryption_flag_moves_the_host_staying_as_it_is() {
        let setup = setup(&json!({ "controller": { "host": "controller.example", "tls": true } }));
        let controller = json!({ "host": "controller.example", "tls": false, "socket_path": "/ws/agent", "events_path": "/api/node/events" });

        setup.set(json!({ "slots": current_edit(), "pinned_ip": CURRENT_PUBLIC_IP, "controller": controller, "demo_upload": null })).await.unwrap();
        assert_eq!(serde_json::to_value(setup.controller_addresses.lock().unwrap().clone()).unwrap(), json!([controller]));
        assert!(!setup.config().controller.tls);
        assert_eq!(yaml(&read(&setup.file))["controller"], json!({ "host": "controller.example", "tls": false }));
    }

    #[tokio::test]
    async fn does_write_and_redial_when_the_address_names_a_different_host() {
        let setup = setup(&json!({ "controller": { "host": "controller.example", "tls": true } }));
        let controller = json!({ "host": "other.example", "tls": true, "socket_path": "/ws/agent", "events_path": "/api/node/events" });

        setup.set(json!({ "slots": current_edit(), "pinned_ip": CURRENT_PUBLIC_IP, "controller": controller, "demo_upload": null })).await.unwrap();
        assert_eq!(serde_json::to_value(setup.controller_addresses.lock().unwrap().clone()).unwrap(), json!([controller]));
        assert_eq!(setup.config().controller.host, "other.example");
        assert_eq!(yaml(&read(&setup.file))["controller"], json!({ "host": "other.example", "tls": true }));
    }

    #[tokio::test]
    async fn redials_and_writes_the_paths_when_only_they_move_and_takes_them_back_out_at_their_defaults() {
        let setup = setup(&json!({ "controller": { "host": "controller.example", "tls": true } }));
        let controller =
            |socket_path: &str, events_path: &str| json!({ "host": "controller.example", "tls": true, "socket_path": socket_path, "events_path": events_path });
        let edit = |socket_path: &str, events_path: &str| json!({ "slots": current_edit(), "pinned_ip": CURRENT_PUBLIC_IP, "controller": controller(socket_path, events_path), "demo_upload": null });

        setup.set(edit("/ankor/agent", "/api/node/events")).await.unwrap();
        setup.set(edit("/ankor/agent", "/ankor/events")).await.unwrap();
        assert_eq!((setup.config().controller.socket_path.as_str(), setup.config().controller.events_path.as_str()), ("/ankor/agent", "/ankor/events"));
        assert_eq!(yaml(&read(&setup.file))["controller"]["socket_path"], "/ankor/agent");
        assert_eq!(yaml(&read(&setup.file))["controller"]["events_path"], "/ankor/events");

        setup.set(edit("/ws/agent", "/api/node/events")).await.unwrap();
        assert_eq!(
            serde_json::to_value(setup.controller_addresses.lock().unwrap().clone()).unwrap(),
            json!([controller("/ankor/agent", "/api/node/events"), controller("/ankor/agent", "/ankor/events"), controller("/ws/agent", "/api/node/events")])
        );
        assert_eq!(yaml(&read(&setup.file))["controller"], json!({ "host": "controller.example", "tls": true }));
    }

    #[tokio::test]
    async fn still_writes_when_only_the_pinned_address_changed_and_answers_with_it() {
        let setup = setup(&json!({}));
        let reply =
            setup.set(json!({ "slots": current_edit(), "pinned_ip": "198.51.100.4", "controller": same_controller(), "demo_upload": null })).await.unwrap();

        assert_eq!(yaml(&read(&setup.file))["pinned_ip"], "198.51.100.4");
        assert_eq!(reply["pinned_ip"], "198.51.100.4");
    }

    #[tokio::test]
    async fn refuses_a_controller_address_change_while_any_slot_is_occupied() {
        let setup = setup(&json!({}));

        launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();

        let error = setup
            .set(json!({ "pinned_ip": null, "slots": current_edit(), "controller": { "host": "10.0.0.5:3000", "tls": false, "socket_path": "/ws/agent", "events_path": "/api/node/events" }, "demo_upload": null }))
            .await
            .unwrap_err();

        assert_eq!(code_of(&error), AgentErrorCode::SlotsHeld);
        assert!(!error.to_string().trim().is_empty());
        assert_eq!(setup.config().controller.host, SAME_HOST);
        assert!(setup.controller_addresses.lock().unwrap().is_empty());
        assert_eq!(read(&setup.file), ORIGINAL);
    }

    #[tokio::test]
    async fn refuses_a_socket_or_events_path_change_while_any_slot_is_occupied() {
        for (socket_path, events_path) in [("/ankor/agent", "/api/node/events"), ("/ws/agent", "/ankor/events")] {
            let setup = setup(&json!({}));

            launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();

            let controller = json!({ "host": SAME_HOST, "tls": true, "socket_path": socket_path, "events_path": events_path });

            assert_eq!(
                code_of(&setup.set(json!({ "pinned_ip": null, "slots": current_edit(), "controller": controller, "demo_upload": null })).await.unwrap_err()),
                AgentErrorCode::SlotsHeld
            );
            assert_eq!((setup.config().controller.socket_path.as_str(), setup.config().controller.events_path.as_str()), ("/ws/agent", "/api/node/events"));
            assert!(setup.controller_addresses.lock().unwrap().is_empty());
            assert_eq!(read(&setup.file), ORIGINAL);
        }
    }

    #[tokio::test]
    async fn refuses_moving_an_occupied_slot_to_another_port_or_gotv_port_or_removing_it_each_on_its_own() {
        let setup = setup(&json!({}));
        let edit = current_edit();
        let mut moved = edit[0].clone();
        let mut tv_moved = edit[0].clone();

        moved["port"] = json!(28000);
        tv_moved["tv_port"] = json!(28001);
        launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();

        for slots in [json!([moved, edit[1]]), json!([tv_moved, edit[1]]), json!([edit[1]])] {
            let error =
                setup.set(json!({ "pinned_ip": CURRENT_PUBLIC_IP, "controller": same_controller(), "demo_upload": null, "slots": slots })).await.unwrap_err();

            assert_eq!(code_of(&error), AgentErrorCode::SlotHeld);
        }

        assert_eq!(setup.config().slots, current_slots());
        assert_eq!(read(&setup.file), ORIGINAL);
    }

    #[tokio::test]
    async fn keeps_the_old_controller_address_when_a_slot_edit_is_refused() {
        let setup = setup(&json!({}));

        launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();

        let error = setup
            .set(json!({
                "pinned_ip": null, "slots": [{ "id": "a", "port": 27100, "tv_port": 27101 }],
                "controller": { "host": "10.0.0.5:3000", "tls": false, "socket_path": "/ws/agent", "events_path": "/api/node/events" }, "demo_upload": null,
            }))
            .await
            .unwrap_err();

        assert_eq!(code_of(&error), AgentErrorCode::SlotHeld);
        assert_eq!(setup.config().controller.host, SAME_HOST);
        assert!(setup.controller_addresses.lock().unwrap().is_empty());
    }

    mod slot_tokens {
        use super::*;

        fn tokens(slots: &[Value]) -> Value {
            json!({ "pinned_ip": CURRENT_PUBLIC_IP, "controller": same_controller(), "demo_upload": null, "slots": slots })
        }

        #[tokio::test]
        async fn replaces_the_token_of_a_slot_that_runs_a_server_the_server_keeps_its_own_the_slot_checks_the_new_one() {
            let setup = setup(&json!({}));
            let edit = current_edit();

            launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap();
            setup.base.slots.set_gslt_state("a", GSLT_A, GsltState::Ok);
            setup.set(tokens(&[with_token(&edit[0], GSLT_C), edit[1].clone()])).await.unwrap();

            assert_eq!(setup.config().slots.iter().map(|slot| slot.gslt.as_str()).collect::<Vec<_>>(), [GSLT_C, GSLT_B]);
            assert_eq!(yaml(&read(&setup.file))["slots"][0]["gslt"], GSLT_C);
            assert_eq!(yaml(&read(&setup.file))["slots"][1]["gslt"], GSLT_B);

            let status = setup.base.status("a");

            assert_eq!((status.state, status.gslt_state), (SlotState::Starting, GsltState::Unchecked));
            assert_eq!(setup.base.slots.running_tokens().into_iter().collect::<Vec<_>>(), [GSLT_A]);
            assert_eq!(setup.checked(), ["a"]);
        }

        #[tokio::test]
        async fn starts_a_server_preparing_when_its_slots_token_is_replaced_on_the_token_it_was_picked_with() {
            let setup = setup(&json!({}));
            let edit = current_edit();

            setup.base.overlay.close();

            let slots = setup.base.slots.clone();
            let started = tokio::spawn(async move { launch_server(&slots, launch_config(&json!({})), &allocation()).await });

            // The slot is picked once the spool says the allocation is new.
            until("the slot picked", || setup.base.status("a").state == SlotState::Starting).await;
            setup.set(tokens(&[with_token(&edit[0], GSLT_C), edit[1].clone()])).await.unwrap();
            setup.base.overlay.gate.add_permits(1 << 20);
            started.await.unwrap().unwrap();

            assert!(setup.base.docker.specs()[0].env.contains(&format!("SRCDS_TOKEN={GSLT_A}")));
            assert_eq!(setup.base.slots.running_tokens().into_iter().collect::<Vec<_>>(), [GSLT_A]);
        }

        #[tokio::test]
        async fn gives_an_invalid_slot_its_games_back_once_its_new_token_checks_out() {
            let setup = setup(&json!({ "slots": [{ "id": "a", "port": 27015, "tv_port": 27020, "gslt": GSLT_A }] }));

            setup.base.slots.set_gslt_state("a", GSLT_A, GsltState::Invalid);

            let refused = launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap_err().into();

            assert_eq!(code_of(&refused), AgentErrorCode::NoFreeSlot);
            setup.set(tokens(&[json!({ "id": "a", "port": 27015, "tv_port": 27020, "gslt": GSLT_C })])).await.unwrap();
            assert_eq!(setup.base.status("a").gslt_state, GsltState::Unchecked);
            setup.base.slots.set_gslt_state("a", GSLT_C, GsltState::Ok);
            assert_eq!(launch_server(&setup.base.slots, launch_config(&json!({})), &allocation()).await.unwrap().slot_id, "a");
        }

        #[tokio::test]
        async fn refuses_to_hand_a_running_servers_token_to_another_slot_swapped_or_replaced_until_that_server_ends() {
            let setup = setup(&json!({}));
            let edit = current_edit();
            let allocation_id = allocation();

            launch_server(&setup.base.slots, launch_config(&json!({})), &allocation_id).await.unwrap();

            for slots in [[with_token(&edit[0], GSLT_B), with_token(&edit[1], GSLT_A)], [with_token(&edit[0], GSLT_C), with_token(&edit[1], GSLT_A)]] {
                assert_eq!(code_of(&setup.set(tokens(&slots)).await.unwrap_err()), AgentErrorCode::SlotHeld);
            }

            assert_eq!(setup.config().slots, current_slots());
            assert_eq!(read(&setup.file), ORIGINAL);

            setup.base.slots.stop_allocation(&allocation_id).await.unwrap();
            setup.set(tokens(&[with_token(&edit[0], GSLT_B), with_token(&edit[1], GSLT_A)])).await.unwrap();
            assert_eq!(setup.config().slots.iter().map(|slot| slot.gslt.as_str()).collect::<Vec<_>>(), [GSLT_B, GSLT_A]);
        }

        #[tokio::test]
        async fn swaps_two_slots_tokens_in_one_request() {
            let setup = setup(&json!({}));
            let edit = current_edit();

            setup.set(tokens(&[with_token(&edit[0], GSLT_B), with_token(&edit[1], GSLT_A)])).await.unwrap();
            assert_eq!(setup.config().slots.iter().map(|slot| slot.gslt.as_str()).collect::<Vec<_>>(), [GSLT_B, GSLT_A]);
            assert_eq!(setup.checked(), ["a", "b"]);
        }

        #[tokio::test]
        async fn keeps_every_token_when_the_hellos_own_config_comes_back_as_it_is() {
            let setup = setup(&json!({}));
            let reported = hello(&setup.config(), Vec::new())["config"].clone();

            assert!(reported["slots"][0].get("gslt_hint").is_some());

            let frame = safe_parse(s_controller_set_config, json!({ "type": "set_config", "id": "echo", "config": reported })).unwrap();

            setup.request(frame).await.unwrap();
            assert_eq!(setup.config().slots, current_slots());
            // Nothing changed, so nothing is written.
            assert_eq!(read(&setup.file), ORIGINAL);
            assert!(setup.checked().is_empty());
        }

        #[tokio::test]
        async fn refuses_a_token_another_slot_keeps_and_a_kept_token_for_a_slot_the_node_does_not_have() {
            let setup = setup(&json!({}));
            let edit = current_edit();
            let repeated = setup
                .set(tokens(&[edit[0].clone(), with_token(&edit[1], GSLT_A), json!({ "id": "c", "port": 27040, "tv_port": 27041, "gslt": GSLT_A })]))
                .await
                .unwrap_err();

            // Each slot that repeats another's token is named.
            assert_eq!(code_of(&repeated), AgentErrorCode::InvalidRequest);
            assert!(repeated.to_string().contains("b, c"), "{repeated}");

            let missing = setup.set(tokens(&[edit[0].clone(), edit[1].clone(), json!({ "id": "c", "port": 27040, "tv_port": 27041 })])).await.unwrap_err();

            assert_eq!(code_of(&missing), AgentErrorCode::SlotTokenRequired);
            assert!(missing.to_string().contains("slot c"), "{missing}");
            assert_eq!(setup.config().slots, current_slots());
            assert_eq!(read(&setup.file), ORIGINAL);
        }

        #[tokio::test]
        async fn checks_nothing_when_a_slot_is_sent_the_token_it_has() {
            let setup = setup(&json!({}));
            let edit = current_edit();

            setup.base.slots.set_gslt_state("a", GSLT_A, GsltState::Ok);
            setup.set(tokens(&[with_token(&edit[0], GSLT_A), edit[1].clone()])).await.unwrap();
            assert!(setup.checked().is_empty());
            assert_eq!(setup.base.status("a").gslt_state, GsltState::Ok);
        }

        #[tokio::test]
        async fn answers_with_the_tokens_hints_alone_as_the_next_hello_carries_them() {
            let setup = setup(&json!({}));
            let edit = current_edit();
            let reply = setup.set(tokens(&[with_token(&edit[0], GSLT_C), edit[1].clone()])).await.unwrap();
            let hello = hello(&setup.config(), setup.base.slots.statuses());
            let pushed = serde_json::to_value(setup.handler.pushed.lock().unwrap().clone()).unwrap();
            let sent = json!([reply, pushed, hello]).to_string();

            assert_eq!(reply, hello["config"]);
            assert_eq!(hello["config"]["slots"][0]["gslt_hint"], "CCCCCC0003");
            assert_eq!(hello["config"]["slots"][1]["gslt_hint"], "AAAAAA0002");

            for token in [GSLT_A, GSLT_B, GSLT_C] {
                assert!(!sent.contains(token), "{token}");
            }
        }
    }
}
