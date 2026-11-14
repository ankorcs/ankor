//! `agent.yml` and the config in memory stay one: changes that run at the same time, on several
//! threads, each reach both, and none writes over another's edit.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::Arc;

use ankor_agent::config::load_agent_config;
use ankor_agent::docker::InspectedContainer;
use ankor_agent::testing::fixtures::agent_config;
use ankor_agent::testing::handler::make_handler;
use ankor_agent::testing::slots::make_slots;
use serde_json::json;

const ROUNDS: usize = 200;

const AGENT_YML: &str = "token: 0123456789abcdef0123456789abcdef
controller:
  host: cs.example.com
  tls: true
cs2_image: ghcr.io/example/ankor-cs2:latest
agent_image: ghcr.io/example/ankor-agent:latest
official_images_only: false
slots:
  - id: a
    port: 27015
    tv_port: 27020
    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001
  - id: b
    port: 27016
    tv_port: 27021
    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0002
";

fn own_container() -> InspectedContainer {
    InspectedContainer {
        id: "agent-container".to_owned(),
        name: "ankor-agent".to_owned(),
        image_id: "sha256:image".to_owned(),
        image: "ghcr.io/example/ankor-agent:latest".to_owned(),
        config: json!({}),
        host_config: json!({}),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_config_and_an_agent_image_change_at_the_same_time_both_reach_memory_and_agent_yml() {
    for round in 0..ROUNDS {
        let base = make_slots(agent_config(&json!({})));

        // Every reference resolves to the running image, so the update keeps the tag and writes it.
        *base.docker.agent_container.lock().unwrap() = Some(own_container());

        let handler = Arc::new(make_handler(&base, |_| {}));

        std::fs::write(&handler.config_path, AGENT_YML).unwrap();

        let ip = format!("198.51.100.{}", round % 250 + 1);
        let image = format!("ghcr.io/example/ankor-agent:v{round}");
        let setting = {
            let (handler, ip) = (Arc::clone(&handler), ip.clone());

            tokio::spawn(async move { handler.request(json!({ "type": "set_config", "id": "r1", "config": { "pinned_ip": ip } })).await })
        };
        let updating = {
            let (handler, image) = (Arc::clone(&handler), image.clone());

            tokio::spawn(async move { handler.request(json!({ "type": "update_agent", "id": "r2", "image": image, "pull": false })).await })
        };

        setting.await.unwrap().unwrap();
        updating.await.unwrap().unwrap();

        let memory = base.config.lock().unwrap().clone();
        let file = load_agent_config(&handler.config_path, |_| {}).unwrap();

        assert_eq!((memory.pinned_ip.as_deref(), memory.agent_image.as_str()), (Some(ip.as_str()), image.as_str()), "memory, round {round}");
        assert_eq!((file.pinned_ip.as_deref(), file.agent_image.as_str()), (Some(ip.as_str()), image.as_str()), "agent.yml, round {round}");
    }
}
