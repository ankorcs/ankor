//! Every value the corpus records as valid reads into its schema's type and writes back unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]

use ankor_protocol::types::round_trip;
use serde_json::Value;

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b)),
        (Value::Object(a), Value::Object(b)) => a.len() == b.len() && a.iter().all(|(key, value)| b.get(key).is_some_and(|other| same(value, other))),
        _ => a == b,
    }
}

#[test]
fn valid_values_round_trip_through_their_types() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus.jsonl");
    let corpus = std::fs::read_to_string(path).expect("tests/corpus.jsonl is missing: run `npm run corpus` in codegen/");
    let mut checked = 0;
    let mut failures = Vec::new();

    for line in corpus.lines() {
        let case: Value = serde_json::from_str(line).expect("corpus line");
        let Some(valid) = case.get("ok") else { continue };
        let name = case["schema"].as_str().expect("schema name");
        let Some(result) = round_trip(name, valid.clone()) else { continue };

        checked += 1;

        match result {
            Ok(written) if same(&written, valid) => {}
            Ok(written) => failures.push(format!("{name}: wrote {written}\n  for {valid}")),
            Err(error) => failures.push(format!("{name}: {error}\n  for {valid}")),
        }
    }

    failures.truncate(10);
    assert!(failures.is_empty(), "{checked} checked:\n{}", failures.join("\n"));
    assert!(checked > 1000, "only {checked} values checked");
}
