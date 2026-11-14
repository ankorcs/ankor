//! The budget each agent message spends from, checked on a valid message of each type from
//! `corpus.jsonl`, so a renamed type fails here instead of moving to another budget unnoticed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, reason = "a test fails by crashing")]

use std::collections::BTreeMap;

use ankor_protocol::helpers::{BudgetName, frame_budget};
use ankor_protocol::types::AgentToControllerMessage;
use serde_json::Value;

/// One valid message of each type the agent sends, by its `type`, from the inputs the contract accepted.
fn samples() -> BTreeMap<String, AgentToControllerMessage> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus.jsonl");
    let corpus = std::fs::read_to_string(path).expect("tests/corpus.jsonl is missing: run `npm run corpus` in codegen/");
    let mut samples = BTreeMap::new();

    for line in corpus.lines() {
        let case: Value = serde_json::from_str(line).unwrap();
        let Some(output) = case.get("ok") else { continue };
        let Ok(message) = serde_json::from_value::<AgentToControllerMessage>(output.clone()) else { continue };
        let Some(tag) = output.get("type").and_then(Value::as_str) else { continue };

        samples.entry(tag.to_owned()).or_insert(message);
    }

    samples
}

#[test]
fn agent_messages_spend_from_their_budget() {
    let samples = samples();

    for (tag, budget) in [
        ("log_batch", BudgetName::LogFrames),
        ("log_index", BudgetName::LogFrames),
        ("resource_batch", BudgetName::Resources),
        ("cs2_update_progress", BudgetName::Cs2UpdateProgress),
        ("demo_progress", BudgetName::DemoProgress),
        ("phase_change", BudgetName::MatchReports),
        ("score", BudgetName::MatchReports),
        ("backup", BudgetName::MatchReports),
        ("reply", BudgetName::StrayReplies),
        ("slot_status", BudgetName::State),
    ] {
        let message = samples.get(tag).unwrap_or_else(|| panic!("no valid {tag} message in the corpus"));

        assert_eq!(frame_budget(message), budget, "{tag}");
    }
}
