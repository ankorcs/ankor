//! Refinements whose edge the corpus does not reach, each held to the contract's verdict.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, reason = "a test fails by crashing")]

use ankor_protocol::schemas::by_name;
use ankor_protocol::validate::safe_parse;
use serde_json::{Value, json};

/// The messages of the issues the schema reports for `value`, or none when it accepts it.
fn issues(schema: &str, value: Value) -> Vec<String> {
    safe_parse(by_name(schema).unwrap(), value).err().unwrap_or_default().into_iter().map(|issue| issue.message).collect()
}

#[test]
fn a_demo_upload_header_cannot_name_one_the_agent_sets_in_any_case() {
    for name in ["Content-Type", "content-length", "Ankor-Match-Id"] {
        assert_eq!(issues("DemoUploadHeader", json!({ "name": name, "value": "x" })), ["the agent sets this header itself"], "{name}");
    }

    assert!(issues("DemoUploadHeader", json!({ "name": "Authorization", "value": "x" })).is_empty());
}

#[test]
fn a_demo_upload_url_carries_no_user_name_or_password() {
    for url in ["https://user@up.example.com/demos", "https://:secret@up.example.com/demos", "https://user:secret@up.example.com/demos"] {
        assert_eq!(issues("DemoUploadUrl", json!(url)), ["credentials go in the header, not the URL"], "{url}");
    }

    assert!(issues("DemoUploadUrl", json!("https://up.example.com/demos")).is_empty());
}

#[test]
fn a_ready_timeout_ends_the_map_with_teams_at_nil_nil() {
    let map_end = |team1_score: u32, team2_score: u32| {
        let side = json!({ "connected": 1, "ready": false });
        let teams = json!({ "team1": side, "team2": side, "spectators": { "connected": 0, "ready": true } });

        json!({
            "type": "map_end",
            "timestamp": 1_700_000_000_000_i64,
            "tick": 0,
            "reason": "ready_timeout",
            "winner_team": null,
            "team1_score": team1_score,
            "team2_score": team2_score,
            "teams": teams,
        })
    };
    let refused = ["teams must be set, at 0-0, exactly for a ready_timeout"];

    assert!(issues("MapEnd", map_end(0, 0)).is_empty());
    assert_eq!(issues("MapEnd", map_end(1, 0)), refused);
    assert_eq!(issues("MapEnd", map_end(0, 1)), refused);
}
