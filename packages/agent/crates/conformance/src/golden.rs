//! The expected transcript of each scenario, kept in the repository as `golden/<scenario>.json`.
//! A run compares its normalised transcript with the golden one; an update run writes it instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::diff;

/// Where the golden transcripts live: `golden/` beside this crate's manifest.
#[must_use]
pub fn default_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("golden")
}

/// How a transcript compared with its golden one.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Match,
    /// The rendered differences, the golden version as `-` and this run's as `+`.
    Differs(String),
    /// No golden transcript yet; an update run records one.
    Missing,
    /// An update run wrote this run's transcript as the golden one.
    Updated,
}

/// The golden file for `scenario` in `dir`.
#[must_use]
pub fn path(dir: &Path, scenario: &str) -> PathBuf {
    dir.join(format!("{scenario}.json"))
}

/// The transcript as its golden file holds it: pretty JSON with a final newline, so a change
/// reviews as a line diff.
pub fn render(transcript: &BTreeMap<String, Vec<Value>>) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(transcript)?))
}

/// Compares `transcript` with the golden one for `scenario`, or, with `update`, writes it as the
/// golden one.
pub fn check(dir: &Path, scenario: &str, transcript: &BTreeMap<String, Vec<Value>>, update: bool) -> Result<Verdict> {
    let file = path(dir, scenario);

    if update {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::write(&file, render(transcript)?).with_context(|| format!("write {}", file.display()))?;

        return Ok(Verdict::Updated);
    }

    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Verdict::Missing),
        Err(error) => return Err(error).with_context(|| format!("read {}", file.display())),
    };
    let golden: BTreeMap<String, Vec<Value>> = serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))?;
    let report = diff::compare("golden", &golden, "run", transcript);

    Ok(if report.is_empty() { Verdict::Match } else { Verdict::Differs(report) })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a test fails by crashing")]

    use serde_json::json;

    use super::*;

    fn transcript(value: i64) -> BTreeMap<String, Vec<Value>> {
        BTreeMap::from([("controller".to_owned(), vec![json!({ "a": value })])])
    }

    #[test]
    fn reports_a_missing_golden_then_records_one_an_update_run_writes() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(check(dir.path(), "launch", &transcript(1), false).unwrap(), Verdict::Missing);
        assert_eq!(check(dir.path(), "launch", &transcript(1), true).unwrap(), Verdict::Updated);
        assert_eq!(check(dir.path(), "launch", &transcript(1), false).unwrap(), Verdict::Match);
        assert!(std::fs::read_to_string(path(dir.path(), "launch")).unwrap().ends_with("]\n}\n"));
    }

    #[test]
    fn a_changed_run_differs_from_its_golden_with_the_golden_side_first() {
        let dir = tempfile::tempdir().unwrap();

        check(dir.path(), "launch", &transcript(1), true).unwrap();

        let Verdict::Differs(report) = check(dir.path(), "launch", &transcript(2), false).unwrap() else { panic!("expected a difference") };

        assert!(report.contains("=== controller: --- golden +++ run"), "{report}");
        assert!(report.contains("-     \"a\": 1"), "{report}");
        assert!(report.contains("+     \"a\": 2"), "{report}");
    }
}
