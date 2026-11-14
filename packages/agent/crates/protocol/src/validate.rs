//! The parse model the generated validators in [`crate::schemas`] run on. A schema parses a
//! [`Payload`], then runs its checks; an issue without `cont: Some(true)` stops the checks after it.
//! Messages that do not depend on the input are generated as constants.

use std::fmt::Write as _;

use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathKey {
    Key(String),
    Index(usize),
}

impl PathKey {
    #[must_use]
    pub fn key(text: &str) -> Self {
        Self::Key(text.to_owned())
    }
}

impl std::fmt::Display for PathKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key(key) => f.write_str(key),
            Self::Index(index) => write!(f, "{index}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    InvalidType,
    InvalidValue,
    TooBig,
    TooSmall,
    InvalidFormat,
    InvalidUnion,
    Custom,
}

impl Code {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidType => "invalid_type",
            Self::InvalidValue => "invalid_value",
            Self::TooBig => "too_big",
            Self::TooSmall => "too_small",
            Self::InvalidFormat => "invalid_format",
            Self::InvalidUnion => "invalid_union",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub code: Code,
    pub path: Vec<PathKey>,
    pub message: String,
    /// `Some(true)` lets later checks run, anything else stops them.
    pub cont: Option<bool>,
}

impl Issue {
    #[must_use]
    pub fn new(code: Code, message: impl Into<String>, cont: Option<bool>) -> Self {
        Self { code, path: Vec::new(), message: message.into(), cont }
    }
}

#[derive(Debug)]
pub struct Payload {
    /// `None` for an absent key.
    pub value: Option<Value>,
    pub issues: Vec<Issue>,
    pub aborted: bool,
}

impl Payload {
    #[must_use]
    pub fn new(value: Option<Value>) -> Self {
        Self { value, issues: Vec::new(), aborted: false }
    }

    pub fn push(&mut self, issue: Issue) {
        self.issues.push(issue);
    }

    /// Moves a nested payload's issues here under `key`.
    pub fn absorb(&mut self, key: &PathKey, child: &mut Payload) {
        for mut issue in child.issues.drain(..) {
            issue.path.insert(0, key.clone());
            self.issues.push(issue);
        }
    }
}

/// Whether an issue from `start` on stops later checks.
#[must_use]
pub fn aborted(payload: &Payload, start: usize) -> bool {
    payload.aborted || payload.issues.iter().skip(start).any(|issue| issue.cont != Some(true))
}

/// Whether an issue from `start` on was raised with `cont: Some(false)`.
#[must_use]
pub fn explicitly_aborted(payload: &Payload, start: usize) -> bool {
    payload.aborted || payload.issues.iter().skip(start).any(|issue| issue.cont == Some(false))
}

/// The "received" part of an `invalid_type` message.
#[must_use]
pub fn received_type(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

/// `message` replaces the default message.
#[must_use]
pub fn invalid_type(expected: &str, input: Option<&Value>, message: Option<&str>) -> Issue {
    let message = message.map_or_else(|| format!("Invalid input: expected {expected}, received {}", received_type(input)), str::to_owned);

    Issue::new(Code::InvalidType, message, None)
}

/// Equality of the primitives a literal or enum can hold. Numbers compare by value, so `1` and `1.0`
/// are equal.
#[must_use]
#[allow(clippy::float_cmp, reason = "JSON numbers compare by exact value, as the contract does")]
pub fn same(value: Option<&Value>, literal: &Value) -> bool {
    match (value, literal) {
        (Some(Value::Number(a)), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Some(a @ (Value::Null | Value::Bool(_) | Value::String(_))), b) => a == b,
        _ => false,
    }
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(f64::NAN)
}

/// Messages a length check needs for each kind of value it can meet.
pub struct LengthMessages {
    pub string: &'static str,
    pub array: &'static str,
}

/// Runs on a string (counting code points) or an array, even after an earlier issue, unless one
/// was raised with `cont: Some(false)`.
fn check_length(payload: &mut Payload, bound: usize, max: bool, messages: &LengthMessages, abort: bool) {
    if explicitly_aborted(payload, 0) {
        return;
    }

    let (count, message) = match &payload.value {
        Some(Value::String(text)) => (text.chars().count(), messages.string),
        Some(Value::Array(items)) => (items.len(), messages.array),
        _ => return,
    };

    if (max && count <= bound) || (!max && count >= bound) {
        return;
    }

    payload.push(Issue::new(if max { Code::TooBig } else { Code::TooSmall }, message, Some(!abort)));
}

pub fn check_max_length(payload: &mut Payload, maximum: usize, messages: &LengthMessages, abort: bool) {
    check_length(payload, maximum, true, messages, abort);
}

pub fn check_min_length(payload: &mut Payload, minimum: usize, messages: &LengthMessages, abort: bool) {
    check_length(payload, minimum, false, messages, abort);
}

/// An integer within ±(2^53 − 1).
pub fn check_safeint(payload: &mut Payload, not_int: &'static str, too_big: &'static str, too_small: &'static str, abort: bool) {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

    let input = number(payload.value.as_ref());

    if !(input.is_finite() && input.fract() == 0.0) {
        payload.push(Issue::new(Code::InvalidType, not_int, Some(false)));
    } else if input.abs() > MAX_SAFE {
        let (code, message) = if input.is_sign_positive() { (Code::TooBig, too_big) } else { (Code::TooSmall, too_small) };

        payload.push(Issue::new(code, message, Some(!abort)));
    }
}

/// A lower bound raises `too_small`, an upper bound `too_big`.
pub fn check_bound(payload: &mut Payload, bound: f64, inclusive: bool, lower: bool, message: &'static str, abort: bool) {
    let value = number(payload.value.as_ref());
    let ok = match (lower, inclusive) {
        (true, true) => value >= bound,
        (true, false) => value > bound,
        (false, true) => value <= bound,
        (false, false) => value < bound,
    };

    if !ok {
        payload.push(Issue::new(if lower { Code::TooSmall } else { Code::TooBig }, message, Some(!abort)));
    }
}

pub fn check_pattern(payload: &mut Payload, pattern: &fancy_regex::Regex, message: &'static str, abort: bool) {
    let text = match &payload.value {
        Some(Value::String(text)) => text.as_str(),
        _ => "",
    };

    if !pattern.is_match(text).unwrap_or(false) {
        payload.push(Issue::new(Code::InvalidFormat, message, Some(!abort)));
    }
}

/// A URL the WHATWG parser accepts. With `http`, it must start with `http://` or `https://`. The
/// value becomes the trimmed text without tabs and line breaks.
pub fn check_url(payload: &mut Payload, http: bool, message: &'static str, abort: bool) {
    let trimmed = match &payload.value {
        Some(Value::String(text)) => text.trim().to_owned(),
        _ => String::new(),
    };
    let lower = trimmed.chars().take(8).collect::<String>().to_ascii_lowercase();
    let prefixed = !http || lower.starts_with("http://") || lower.starts_with("https://");

    if !prefixed {
        payload.push(Issue::new(Code::InvalidFormat, message, Some(!abort)));
        return;
    }

    // With `http`, the prefix already makes the scheme `http` or `https`.
    if url::Url::parse(&trimmed).is_err() {
        payload.push(Issue::new(Code::InvalidFormat, message, Some(!abort)));
        return;
    }

    payload.value = Some(Value::String(trimmed.replace(['\t', '\n', '\r'], "")));
}

/// Where a hand-written refinement reports its issues.
pub struct RefineCtx<'a> {
    payload: &'a mut Payload,
}

impl<'a> RefineCtx<'a> {
    pub fn new(payload: &'a mut Payload) -> Self {
        Self { payload }
    }

    pub fn add_issue(&mut self, path: Vec<PathKey>, message: impl Into<String>) {
        let mut issue = Issue::new(Code::Custom, message, Some(true));

        issue.path = path;
        self.payload.push(issue);
    }
}

/// Whether an object property may be absent, and whether its schema fills in a value when it is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OptIn {
    Required,
    Optional,
    Defaulted,
}

const MISSING_KEY: &str = "Invalid input: expected nonoptional, received undefined";

/// Parses `input[key]` with `run` into `out`, reporting its issues under `key`.
pub fn property(
    p: &mut Payload,
    out: &mut Map<String, Value>,
    input: &Map<String, Value>,
    key: &str,
    run: fn(&mut Payload),
    optin: OptIn,
    optout_optional: bool,
) {
    let present = input.contains_key(key);
    let mut result = Payload::new(input.get(key).cloned());

    run(&mut result);

    if !present && optout_optional && optin == OptIn::Optional {
        return;
    }

    let failed = !result.issues.is_empty();

    if failed {
        if optin != OptIn::Required && optout_optional && !present {
            return;
        }

        p.absorb(&PathKey::key(key), &mut result);
    }

    if !present && optin == OptIn::Required {
        if !failed {
            let mut issue = Issue::new(Code::InvalidType, MISSING_KEY, None);

            issue.path = vec![PathKey::key(key)];
            p.push(issue);
        }

        return;
    }

    if let Some(value) = result.value {
        out.insert(key.to_owned(), value);
    }
}

/// Copies the properties a loose object does not declare.
pub fn extra_properties(out: &mut Map<String, Value>, input: &Map<String, Value>, declared: &[&str]) {
    for (key, value) in input {
        if key != "__proto__" && !declared.contains(&key.as_str()) {
            out.insert(key.clone(), value.clone());
        }
    }
}

/// Parses each item with `run`, reporting its issues under its index.
pub fn elements(p: &mut Payload, items: Vec<Value>, run: fn(&mut Payload)) -> Vec<Value> {
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let mut result = Payload::new(Some(item));

            run(&mut result);
            p.absorb(&PathKey::Index(index), &mut result);
            result.value.unwrap_or(Value::Null)
        })
        .collect()
}

/// The first option that parses without issues wins. Failing that, a single option whose issues
/// all let checks continue reports those; otherwise the union reports one issue.
pub fn union(p: &mut Payload, options: &[fn(&mut Payload)], message: &'static str) {
    let mut results = Vec::with_capacity(options.len());

    for run in options {
        let mut result = Payload::new(p.value.clone());

        run(&mut result);

        if result.issues.is_empty() {
            *p = result;
            return;
        }

        results.push(result);
    }

    let mut open: Vec<usize> = results.iter().enumerate().filter(|(_, result)| !aborted(result, 0)).map(|(index, _)| index).collect();

    if open.len() == 1 {
        *p = results.swap_remove(open.remove(0));
        return;
    }

    p.push(Issue::new(Code::InvalidUnion, message, None));
}

/// Parses the value with both sides and merges what they make of it.
pub fn intersection(p: &mut Payload, left: fn(&mut Payload), right: fn(&mut Payload)) {
    let mut a = Payload::new(p.value.clone());
    let mut b = Payload::new(p.value.clone());

    left(&mut a);
    right(&mut b);

    p.issues.append(&mut a.issues);
    p.issues.append(&mut b.issues);

    match (a.value, b.value) {
        (Some(a), Some(b)) => match merge(a, b) {
            Some(merged) => p.value = Some(merged),
            None => p.push(Issue::new(Code::Custom, "the two sides of an intersection disagree", None)),
        },
        (a, b) => p.value = a.or(b),
    }
}

/// Both sides' keys, the second side's after; values both sides hold must merge too.
fn merge(a: Value, b: Value) -> Option<Value> {
    match (a, b) {
        (Value::Object(mut left), Value::Object(right)) => {
            for (key, value) in right {
                let merged = match left.get_mut(&key) {
                    Some(existing) => merge(existing.take(), value)?,
                    None => value,
                };

                left.insert(key, merged);
            }

            left.shift_remove("__proto__");
            Some(Value::Object(left))
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            left.into_iter().zip(right).map(|(a, b)| merge(a, b)).collect::<Option<Vec<_>>>().map(Value::Array)
        }
        (a, b) if same(Some(&a), &b) => Some(a),
        _ => None,
    }
}

/// # Errors
///
/// Returns the issues in the order they were found.
pub fn safe_parse(run: fn(&mut Payload), value: Value) -> Result<Value, Vec<Issue>> {
    let mut payload = Payload::new(Some(value));

    run(&mut payload);

    if payload.issues.is_empty() { Ok(payload.value.unwrap_or(Value::Null)) } else { Err(payload.issues) }
}

/// Issues as the agent reports them to the controller: `path: message`, joined with `; `.
#[must_use]
pub fn format_issues(issues: &[Issue]) -> String {
    let mut out = String::new();

    for (index, issue) in issues.iter().enumerate() {
        if index > 0 {
            out.push_str("; ");
        }

        for (step, key) in issue.path.iter().enumerate() {
            if step > 0 {
                out.push('.');
            }

            let _ = write!(out, "{key}");
        }

        let _ = write!(out, ": {}", issue.message);
    }

    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn checked(value: Value, check: impl FnOnce(&mut Payload)) -> Vec<(Code, Option<bool>)> {
        let mut payload = Payload::new(Some(value));

        check(&mut payload);
        payload.issues.iter().map(|issue| (issue.code, issue.cont)).collect()
    }

    #[test]
    fn safe_integers_stop_at_two_to_the_53() {
        let safeint = |value: Value| checked(value, |p| check_safeint(p, "int", "big", "small", false));

        assert_eq!(safeint(json!(9_007_199_254_740_991_i64)), []);
        assert_eq!(safeint(json!(-9_007_199_254_740_991_i64)), []);
        assert_eq!(safeint(json!(9_007_199_254_740_993_i64)), [(Code::TooBig, Some(true))]);
        assert_eq!(safeint(json!(-9_007_199_254_740_993_i64)), [(Code::TooSmall, Some(true))]);
        assert_eq!(safeint(json!(1.5)), [(Code::InvalidType, Some(false))]);
    }

    #[test]
    fn bounds_include_or_exclude_their_edge() {
        let bound = |value: f64, inclusive: bool, lower: bool| checked(json!(value), |p| check_bound(p, 10.0, inclusive, lower, "bound", true));

        for (value, inclusive, lower, ok) in [
            (9.0, false, false, true),
            (10.0, false, false, false),
            (11.0, false, false, false),
            (10.0, true, false, true),
            (11.0, true, false, false),
            (11.0, false, true, true),
            (10.0, false, true, false),
            (10.0, true, true, true),
            (9.0, true, true, false),
        ] {
            let expected = if ok { vec![] } else { vec![(if lower { Code::TooSmall } else { Code::TooBig }, Some(false))] };

            assert_eq!(bound(value, inclusive, lower), expected, "{value} inclusive {inclusive} lower {lower}");
        }
    }

    #[test]
    fn urls_are_trimmed_and_need_http_when_asked() {
        let url = |text: &str, http: bool, abort: bool| {
            let mut payload = Payload::new(Some(json!(text)));

            check_url(&mut payload, http, "url", abort);
            (payload.issues.iter().map(|issue| issue.cont).collect::<Vec<_>>(), payload.value)
        };

        assert_eq!(url(" https://up.example.com/a\tb ", true, true), (vec![], Some(json!("https://up.example.com/ab"))));
        assert_eq!(url("ftp://up.example.com", false, true), (vec![], Some(json!("ftp://up.example.com"))));
        assert_eq!(url("ftp://up.example.com", true, true).0, [Some(false)]);
        assert_eq!(url("http://", true, true).0, [Some(false)]);
        assert_eq!(url("http://", true, false).0, [Some(true)]);
        assert_eq!(url("not a url", false, false).0, [Some(true)]);
    }

    fn fill(p: &mut Payload) {
        if p.value.is_none() {
            p.value = Some(json!(5));
        }
    }

    fn need(p: &mut Payload) {
        if p.value.is_none() {
            p.push(Issue::new(Code::InvalidType, "needed", None));
        }
    }

    fn parse_property(key: &str, run: fn(&mut Payload), optin: OptIn, optout_optional: bool) -> (Map<String, Value>, String) {
        let input = json!({ "present": 1 });
        let mut p = Payload::new(None);
        let mut out = Map::new();

        property(&mut p, &mut out, input.as_object().unwrap_or(&Map::new()), key, run, optin, optout_optional);
        (out, format_issues(&p.issues))
    }

    #[test]
    fn absent_properties_default_or_drop_by_opt_in() {
        let defaulted = parse_property("absent", fill, OptIn::Defaulted, true);
        assert_eq!(defaulted, (json!({ "absent": 5 }).as_object().cloned().unwrap_or_default(), String::new()));

        assert_eq!(parse_property("absent", fill, OptIn::Optional, true), (Map::new(), String::new()));
        assert_eq!(parse_property("absent", need, OptIn::Defaulted, true), (Map::new(), String::new()));
        assert_eq!(parse_property("absent", need, OptIn::Defaulted, false), (Map::new(), "absent: needed".to_owned()));
        assert_eq!(parse_property("absent", fill, OptIn::Required, true), (Map::new(), format!("absent: {MISSING_KEY}")));
        assert_eq!(parse_property("present", need, OptIn::Required, true), (json!({ "present": 1 }).as_object().cloned().unwrap_or_default(), String::new()));
    }

    #[test]
    fn loose_objects_copy_only_undeclared_keys() {
        let input = json!({ "declared": 1, "extra": 2, "__proto__": 3 });
        let mut out = Map::new();

        extra_properties(&mut out, input.as_object().unwrap_or(&Map::new()), &["declared"]);
        assert_eq!(Value::Object(out), json!({ "extra": 2 }));
    }

    fn intersect(a: Value, b: Value) -> (Option<Value>, usize) {
        thread_local! {
            static SIDES: std::cell::RefCell<(Value, Value)> = const { std::cell::RefCell::new((Value::Null, Value::Null)) };
        }

        fn left(p: &mut Payload) {
            p.value = Some(SIDES.with(|sides| sides.borrow().0.clone()));
        }

        fn right(p: &mut Payload) {
            p.value = Some(SIDES.with(|sides| sides.borrow().1.clone()));
        }

        SIDES.with(|sides| *sides.borrow_mut() = (a, b));

        let mut p = Payload::new(Some(Value::Null));

        intersection(&mut p, left, right);
        (p.value, p.issues.len())
    }

    #[test]
    fn intersections_merge_or_disagree() {
        assert_eq!(
            intersect(json!({ "a": 1, "both": { "x": 1 }, "__proto__": 0 }), json!({ "b": 2, "both": { "y": 2 } })),
            (Some(json!({ "a": 1, "both": { "x": 1, "y": 2 }, "b": 2 })), 0),
        );
        assert_eq!(intersect(json!([1, 2]), json!([1, 2])), (Some(json!([1, 2])), 0));
        assert_eq!(intersect(json!([1, 2]), json!([1])).1, 1);
        assert_eq!(intersect(json!([1, 2]), json!([1, 3])).1, 1);
        assert_eq!(intersect(json!(1), json!(1.0)), (Some(json!(1)), 0));
        assert_eq!(intersect(json!(1), json!(2)).1, 1);
        assert_eq!(intersect(json!("a"), json!({ "a": 1 })).1, 1);
    }

    #[test]
    fn issues_read_as_dotted_paths() {
        let mut nested = Issue::new(Code::TooBig, "too big", None);
        nested.path = vec![PathKey::key("slots"), PathKey::Index(0), PathKey::key("port")];
        let mut second = Issue::new(Code::Custom, "named twice", None);
        second.path = vec![PathKey::key("slot_status")];
        let root = Issue::new(Code::InvalidType, "expected object", None);

        assert_eq!(format_issues(&[nested, second, root]), "slots.0.port: too big; slot_status: named twice; : expected object");
        assert_eq!(format_issues(&[]), "");
    }
}
