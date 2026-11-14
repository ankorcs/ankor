//! Compares two normalised transcripts channel by channel and renders the differences.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::Value;
use similar::{ChangeTag, TextDiff};

/// The differences between the transcripts of `left` and `right`, one section per channel that
/// differs. Empty when they match.
pub fn compare(left_name: &str, left: &BTreeMap<String, Vec<Value>>, right_name: &str, right: &BTreeMap<String, Vec<Value>>) -> String {
    let channels: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
    let mut report = String::new();
    for channel in channels {
        let empty = Vec::new();
        let a = render(left.get(channel).unwrap_or(&empty));
        let b = render(right.get(channel).unwrap_or(&empty));
        if a == b {
            continue;
        }
        let _ = writeln!(report, "=== {channel}: --- {left_name} +++ {right_name}");
        let diff = TextDiff::from_lines(&a, &b);
        for group in diff.grouped_ops(3) {
            for op in group {
                for change in diff.iter_changes(&op) {
                    let sign = match change.tag() {
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                        ChangeTag::Equal => ' ',
                    };
                    let _ = write!(report, "{sign} {change}");
                    if change.missing_newline() {
                        report.push('\n');
                    }
                }
            }
            report.push_str("  ...\n");
        }
    }
    report
}

/// Each entry under a marker line, its JSON indented beneath, so a diff lines up whole entries.
fn render(entries: &[Value]) -> String {
    let mut out = String::new();
    for entry in entries {
        let pretty = serde_json::to_string_pretty(entry).unwrap_or_default();
        out.push_str("·\n");
        for line in pretty.lines() {
            let _ = writeln!(out, "  {line}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn equal_transcripts_have_no_report() {
        let a = BTreeMap::from([("controller".to_owned(), vec![json!({ "a": 1 })])]);
        assert!(compare("golden", &a, "run", &a).is_empty());
    }

    #[test]
    fn a_difference_names_its_channel() {
        let a = BTreeMap::from([("controller".to_owned(), vec![json!({ "a": 1 })])]);
        let b = BTreeMap::from([("controller".to_owned(), vec![json!({ "a": 2 })])]);
        let report = compare("golden", &a, "run", &b);
        assert!(report.contains("=== controller"));
        assert!(report.contains("-     \"a\": 1"));
    }
}
