//! Replays `contract_corpus.jsonl`, which `codegen/corpus.mjs` writes with the contract's own verdict on
//! each input, and requires the same verdict here: the same output, or the same issues in order.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]

use std::fmt::Write as _;

use ankor_agent::contract::schemas::by_name;
use ankor_protocol::validate::{PathKey, safe_parse};
use serde_json::Value;

/// Numbers compare by value, since `1` and `1.0` write differently but are the same number.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b)),
        (Value::Object(a), Value::Object(b)) => a.len() == b.len() && a.iter().zip(b).all(|((ka, va), (kb, vb))| ka == kb && same(va, vb)),
        _ => a == b,
    }
}

fn path_json(path: &[PathKey]) -> Value {
    Value::Array(
        path.iter()
            .map(|key| match key {
                PathKey::Key(key) => Value::String(key.clone()),
                PathKey::Index(index) => Value::from(*index),
            })
            .collect(),
    )
}

#[test]
fn validators_agree_with_the_contract() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/contract_corpus.jsonl");
    let corpus = std::fs::read_to_string(path).expect("tests/contract_corpus.jsonl is missing: run `npm run corpus` in codegen/");
    let mut failures = String::new();
    let mut cases = 0;
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    let mut shown = std::collections::HashSet::<String>::new();

    for line in corpus.lines() {
        let case: Value = serde_json::from_str(line).expect("corpus line");
        let name = case["schema"].as_str().expect("schema name");
        let run = by_name(name).unwrap_or_else(|| panic!("no validator for {name}"));
        let result = safe_parse(run, case["input"].clone());

        cases += 1;

        let agrees = match (&result, case.get("ok"), case.get("issues")) {
            (Ok(output), Some(expected), _) => same(output, expected),
            (Err(issues), _, Some(Value::Array(expected))) => {
                issues.len() == expected.len()
                    && issues.iter().zip(expected).all(|(issue, expected)| {
                        issue.code.as_str() == expected["code"] && issue.message == expected["message"] && same(&path_json(&issue.path), &expected["path"])
                    })
            }
            _ => false,
        };

        if !agrees {
            *counts.entry(name.to_owned()).or_insert(0) += 1;
        }

        if !agrees && !shown.contains(name) {
            shown.insert(name.to_owned());
            let got = match &result {
                Ok(output) => format!("ok {output}"),
                Err(issues) => issues.iter().map(|i| format!("[{} {} {}]", i.code.as_str(), path_json(&i.path), i.message)).collect::<Vec<_>>().join(" "),
            };
            let first_difference = match (&result, case.get("issues")) {
                (Err(issues), Some(Value::Array(expected))) => (0..issues.len().max(expected.len()))
                    .find(|&i| match (issues.get(i), expected.get(i)) {
                        (Some(issue), Some(expected)) => {
                            !(issue.code.as_str() == expected["code"]
                                && issue.message == expected["message"]
                                && same(&path_json(&issue.path), &expected["path"]))
                        }
                        _ => true,
                    })
                    .map(|i| {
                        format!(
                            "issue {i}: expected {} got {:?}",
                            expected.get(i).map_or("nothing".to_owned(), Value::to_string),
                            issues.get(i).map(|issue| (issue.code.as_str(), path_json(&issue.path).to_string(), &issue.message))
                        )
                    }),
                _ => None,
            };
            let _ = writeln!(failures, "  first difference: {}", first_difference.unwrap_or_default());
            let expected = case.get("ok").map(|ok| format!("ok {ok}")).or_else(|| case.get("issues").map(Value::to_string)).unwrap_or_default();

            let cut = |text: String| text.chars().take(400).collect::<String>();
            let _ = writeln!(failures, "{name}\n  input:    {}\n  expected: {}\n  got:      {}", cut(case["input"].to_string()), cut(expected), cut(got));
        }
    }

    assert!(failures.is_empty(), "{cases} cases, {} disagree: {counts:?}\n{failures}", counts.values().sum::<usize>());
}
