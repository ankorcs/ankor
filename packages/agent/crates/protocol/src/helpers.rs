//! Functions both ends of the wire share.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::constants::{AGENT_LOG_LINES, AGENT_RECONNECT_JITTER, PROTOCOL_VERSION, SERVER_LOG_LINES, agent_budgets};
use crate::types::{AgentLogBatch, AgentToControllerMessage, GsltState, LogLine, LogSource, SlotState};
use crate::validate::{Issue, Payload, format_issues, safe_parse};

const GSLT_HINT_CHARS: usize = 10;

/// The `Authorization` value a node presents to its controller, on the socket and on every upload.
#[must_use]
pub fn node_authorization(token: &str) -> String {
    format!("Bearer {token}")
}

/// A contract enum's value as it goes on the wire, such as `node_connected` for
/// `CloseReason::NodeConnected`: its serde name, so it follows the contract. Empty for a value that
/// does not serialise to a string, which a contract enum always does.
#[must_use]
pub fn wire_name<T: Serialize>(value: T) -> String {
    serde_json::to_value(value).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}

/// The contract enum value a wire string names, or `None` for a string the contract does not define.
#[must_use]
pub fn from_wire_name<T: DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(Value::String(name.to_owned())).ok()
}

/// Game servers a node with `cpu_count` CPUs runs at once: one per CPU, keeping one for the host.
#[must_use]
pub fn game_server_budget(cpu_count: usize) -> usize {
    cpu_count.saturating_sub(1).max(1)
}

/// The part of a token that may leave the agent.
#[must_use]
pub fn gslt_hint(token: &str) -> String {
    let skip = token.chars().count().saturating_sub(GSLT_HINT_CHARS);

    token.chars().skip(skip).collect()
}

/// Indexes of tokens that an earlier slot already has. Missing tokens are skipped.
#[must_use]
pub fn duplicate_gslts(tokens: &[Option<&str>]) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();

    tokens.iter().enumerate().filter_map(|(index, token)| token.filter(|token| !seen.insert(*token)).map(|_| index)).collect()
}

/// The hostname of a `host[:port]`, without the brackets of an IPv6 literal, or `None` for a value
/// that is not a host with an optional port.
#[must_use]
pub fn hostname_of(host: &str) -> Option<String> {
    if host.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '?' | '#')) {
        return None;
    }

    // Without `/`, `\`, `?` and `#` the URL has no path, query or fragment; only credentials remain.
    let url = url::Url::parse(&format!("http://{host}")).ok()?;

    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }

    url.host_str().filter(|name| !name.is_empty()).map(|name| name.trim_start_matches('[').trim_end_matches(']').to_owned())
}

/// Whether a `controller.host` is valid: a host with an optional port.
#[must_use]
pub fn is_controller_host(host: &str) -> bool {
    hostname_of(host).is_some()
}

fn local_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, ..] = address.octets();

    address.is_loopback() || address.is_private() || address.is_link_local() || (a == 100 && (64..128).contains(&b))
}

/// Whether a node that reaches the controller at `host` should encrypt by default: everywhere
/// except `localhost` and the literals of a local network, for which no authority issues
/// certificates.
#[must_use]
pub fn controller_tls_by_default(host: &str) -> bool {
    let Some(hostname) = hostname_of(host) else {
        return true;
    };

    if hostname.eq_ignore_ascii_case("localhost") {
        return false;
    }

    match hostname.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => !local_ipv4(address),
        Ok(IpAddr::V6(address)) => {
            let first = address.segments()[0];
            let local = address.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80;

            !(local || address.to_ipv4_mapped().is_some_and(local_ipv4))
        }
        Err(_) => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginKind {
    Http,
    Ws,
}

/// The controller's origin for `host`, as `http(s)://` or `ws(s)://`.
#[must_use]
pub fn controller_origin(host: &str, tls: bool, kind: OriginKind) -> String {
    let scheme = match (kind, tls) {
        (OriginKind::Http, true) => "https",
        (OriginKind::Http, false) => "http",
        (OriginKind::Ws, true) => "wss",
        (OriginKind::Ws, false) => "ws",
    };

    format!("{scheme}://{host}")
}

/// The agent socket's URL.
#[must_use]
pub fn agent_ws_url(host: &str, tls: bool, path: &str) -> String {
    format!("{}{path}", controller_origin(host, tls, OriginKind::Ws))
}

/// Orders build ids numerically; an unknown build sorts lowest.
#[must_use]
pub fn compare_build_ids(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    let number = |id: Option<&str>| id.map(|id| id.parse::<u128>().unwrap_or(0));

    number(a).cmp(&number(b))
}

/// Whether a node is behind the newest public CS2 build. False while either build is unknown.
#[must_use]
pub fn cs2_behind(installed: Option<&str>, latest: Option<&str>) -> bool {
    installed.is_some() && latest.is_some() && compare_build_ids(installed, latest).is_lt()
}

/// Whether a launch may use a slot: idle, unreserved, and with a token Steam has not refused.
#[must_use]
pub fn slot_takes_launch(state: SlotState, gslt_state: GsltState, reserved: bool) -> bool {
    state == SlotState::Idle && !reserved && gslt_state != GsltState::Invalid
}

/// The key log sources are matched on: `agent`, `cs2_update:<id>` or `server:<id>`.
#[must_use]
pub fn log_source_key(source: &LogSource) -> String {
    match source {
        LogSource::Agent(_) => "agent".to_owned(),
        LogSource::Cs2Update(update) => format!("cs2_update:{}", update.update_id),
        LogSource::Server(server) => format!("server:{}", server.allocation_id),
    }
}

/// Lines the agent keeps of a log source, and the controller after it.
#[must_use]
pub fn log_source_lines(source: &LogSource) -> usize {
    let lines = if matches!(source, LogSource::Agent(_)) { AGENT_LOG_LINES } else { SERVER_LOG_LINES };

    usize::try_from(lines).unwrap_or(usize::MAX)
}

/// A source's lines with one `log_batch` of it applied, kept to what the agent keeps.
#[must_use]
pub fn with_log_batch(lines: &[LogLine], batch: &AgentLogBatch) -> Vec<LogLine> {
    let mut held: Vec<LogLine> = if batch.reset { Vec::new() } else { lines.to_vec() };

    held.extend(batch.lines.iter().cloned());

    let excess = held.len().saturating_sub(log_source_lines(&batch.source));

    held.split_off(excess)
}

/// The key demo frames are matched on.
#[must_use]
pub fn demo_key(slot_id: &str, file_name: &str) -> String {
    format!("{slot_id}/{file_name}")
}

/// A budget's burst and refill rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    pub burst: f64,
    pub per_second: f64,
}

/// A burst that refills at a steady rate.
#[derive(Clone, Debug)]
pub struct TokenBucket {
    rate: Rate,
    tokens: f64,
    at_ms: f64,
}

impl TokenBucket {
    #[must_use]
    pub fn new(rate: Rate, now_ms: f64) -> Self {
        Self { rate, tokens: rate.burst, at_ms: now_ms }
    }

    /// Spends `amount` and returns true, or spends nothing and returns false.
    pub fn take(&mut self, amount: f64, now_ms: f64) -> bool {
        self.refill(now_ms);

        if self.tokens < amount {
            return false;
        }

        self.tokens -= amount;
        true
    }

    pub fn available(&mut self, now_ms: f64) -> f64 {
        self.refill(now_ms);
        self.tokens
    }

    /// Changes the rate. A larger burst is available immediately.
    pub fn resize(&mut self, rate: Rate, now_ms: f64) {
        self.refill(now_ms);
        self.tokens = (self.tokens + rate.burst - self.rate.burst).clamp(0.0, rate.burst);
        self.rate = rate;
    }

    fn refill(&mut self, now_ms: f64) {
        self.tokens = self.rate.burst.min(self.tokens + (now_ms - self.at_ms).max(0.0) / 1000.0 * self.rate.per_second);
        self.at_ms = now_ms;
    }
}

/// A node's rate: `base`, plus `per_slot` for every slot the node has.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    pub base: Rate,
    pub per_slot: Rate,
}

/// The budgets the controller holds an agent's socket messages to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BudgetName {
    Bytes,
    State,
    LogFrames,
    LogBytes,
    Resources,
    Cs2UpdateProgress,
    DemoProgress,
    MatchReports,
    StrayReplies,
    Pings,
}

#[allow(clippy::cast_precision_loss, reason = "the budgets are small whole numbers")]
fn rate(burst: u64, per_second: u64) -> Rate {
    Rate { burst: burst as f64, per_second: per_second as f64 }
}

impl BudgetName {
    pub const ALL: &'static [Self] = &[
        Self::Bytes,
        Self::State,
        Self::LogFrames,
        Self::LogBytes,
        Self::Resources,
        Self::Cs2UpdateProgress,
        Self::DemoProgress,
        Self::MatchReports,
        Self::StrayReplies,
        Self::Pings,
    ];

    /// The budget's rates, as the contract sets them.
    #[must_use]
    pub fn budget(self) -> Budget {
        macro_rules! budget {
            ($name:ident) => {
                Budget {
                    base: rate(agent_budgets::$name::base::BURST, agent_budgets::$name::base::PER_SECOND),
                    per_slot: rate(agent_budgets::$name::per_slot::BURST, agent_budgets::$name::per_slot::PER_SECOND),
                }
            };
        }

        match self {
            Self::Bytes => budget!(bytes),
            Self::State => budget!(state),
            Self::LogFrames => budget!(log_frames),
            Self::LogBytes => budget!(log_bytes),
            Self::Resources => budget!(resources),
            Self::Cs2UpdateProgress => budget!(cs2_update_progress),
            Self::DemoProgress => budget!(demo_progress),
            Self::MatchReports => budget!(match_reports),
            Self::StrayReplies => budget!(stray_replies),
            Self::Pings => budget!(pings),
        }
    }
}

/// The budget a socket message spends from. Log messages spend `LogBytes` as well, and a reply
/// spends `StrayReplies` only when it answers nothing open. The match names every message, so a new
/// one cannot fall into a budget without a decision.
#[must_use]
pub fn frame_budget(message: &AgentToControllerMessage) -> BudgetName {
    match message {
        AgentToControllerMessage::LogBatch(_) | AgentToControllerMessage::LogIndex(_) => BudgetName::LogFrames,
        AgentToControllerMessage::ResourceBatch(_) => BudgetName::Resources,
        AgentToControllerMessage::Cs2UpdateProgress(_) => BudgetName::Cs2UpdateProgress,
        AgentToControllerMessage::DemoProgress(_) => BudgetName::DemoProgress,
        AgentToControllerMessage::PhaseChange(_) | AgentToControllerMessage::Score(_) | AgentToControllerMessage::Backup(_) => BudgetName::MatchReports,
        AgentToControllerMessage::Reply(_) => BudgetName::StrayReplies,
        AgentToControllerMessage::Hello(_)
        | AgentToControllerMessage::NodeStatus(_)
        | AgentToControllerMessage::Shutdown(_)
        | AgentToControllerMessage::SlotStatus(_)
        | AgentToControllerMessage::ServerExited(_)
        | AgentToControllerMessage::Demo(_)
        | AgentToControllerMessage::DemoRemoved(_)
        | AgentToControllerMessage::EventBacklog(_) => BudgetName::State,
    }
}

/// A budget's rate for a node with `slots` slots. A count below one counts as one.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "a slot count")]
pub fn budget_rate(budget: Budget, slots: usize) -> Rate {
    let scale = slots.max(1) as f64;

    Rate { burst: budget.base.burst + budget.per_slot.burst * scale, per_second: budget.base.per_second + budget.per_slot.per_second * scale }
}

/// A protocol header value, such as `1.3`, as its major and minor. `None` for any other value.
#[must_use]
pub fn protocol_version_of(header: &str) -> Option<(u32, u32)> {
    let part = |text: &str| {
        (!text.is_empty() && text.len() <= 6 && text.bytes().all(|byte| byte.is_ascii_digit()) && (text == "0" || !text.starts_with('0')))
            .then(|| text.parse().ok())
            .flatten()
    };
    let (major, minor) = header.split_once('.')?;

    Some((part(major)?, part(minor)?))
}

/// Whether a request's protocol header names a version this build can talk to: the same major as
/// `PROTOCOL_VERSION`, at any minor.
#[must_use]
pub fn is_compatible_protocol(header: Option<&str>) -> bool {
    let (Some(peer), Some(own)) = (header.and_then(protocol_version_of), protocol_version_of(PROTOCOL_VERSION)) else {
        return false;
    };

    peer.0 == own.0
}

/// The agent's redial and retry delay: exponential backoff from `min`, capped at `max`, then
/// jittered, in whole milliseconds. `random` returns a value in `0..1`.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss, reason = "delays are positive and capped far below 2^53 ms")]
pub fn backoff_delay(attempt: u32, min: Duration, max: Duration, random: impl FnOnce() -> f64) -> Duration {
    let (min_ms, max_ms) = (min.as_millis() as f64, max.as_millis() as f64);
    let base = (min_ms * 2f64.powi(attempt.saturating_sub(1).try_into().unwrap_or(i32::MAX))).min(max_ms);
    let jitter = base * AGENT_RECONNECT_JITTER * (random() * 2.0 - 1.0);

    Duration::from_millis((base + jitter).round().max(min_ms) as u64)
}

/// A failed parse as one indented line per issue, under its dotted path.
#[must_use]
pub fn format_config_error(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(|issue| {
            let path = if issue.path.is_empty() { "(root)".to_owned() } else { issue.path.iter().map(ToString::to_string).collect::<Vec<_>>().join(".") };

            format!("  {path}: {}", issue.message)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug)]
pub enum ParseError {
    Json(serde_json::Error),
    Invalid(Vec<Issue>),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "{error}"),
            Self::Invalid(issues) => f.write_str(&format_issues(issues)),
        }
    }
}

impl std::error::Error for ParseError {}

/// Reads JSON text, holds it to `schema`, and returns its typed form.
///
/// # Errors
///
/// Fails on text that is not JSON or a value that `schema` rejects.
pub fn parse_json<T: DeserializeOwned>(schema: fn(&mut Payload), text: &str) -> Result<T, ParseError> {
    let value: Value = serde_json::from_str(text).map_err(ParseError::Json)?;
    let parsed = safe_parse(schema, value).map_err(ParseError::Invalid)?;

    serde_json::from_value(parsed).map_err(ParseError::Json)
}

/// Holds `value` to `schema` and writes it as JSON.
///
/// # Errors
///
/// Fails when the value does not match `schema`.
pub fn serialize_json<T: Serialize>(schema: fn(&mut Payload), value: &T) -> Result<String, ParseError> {
    let value = serde_json::to_value(value).map_err(ParseError::Json)?;
    let parsed = safe_parse(schema, value).map_err(ParseError::Invalid)?;

    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_defaults_off_for_local_hosts() {
        for host in [
            "localhost:3000",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.10:3000",
            "172.20.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "[::1]",
            "[fd00::1]",
            "[fe80::1]",
            "[::ffff:10.0.0.1]",
        ] {
            assert!(!controller_tls_by_default(host), "{host}");
        }

        for host in ["cs.example.com", "8.8.8.8", "100.128.0.1", "[2001:db8::1]", "bad host"] {
            assert!(controller_tls_by_default(host), "{host}");
        }
    }

    #[test]
    fn hostname_of_rejects_more_than_a_host() {
        assert_eq!(hostname_of("cs.example.com:443").as_deref(), Some("cs.example.com"));
        assert_eq!(hostname_of("[fd00::1]:80").as_deref(), Some("fd00::1"));

        for host in ["http://x", "a/b", "a\\b", "user@x", ":secret@x", "x?y", "x#y", "", " x"] {
            assert_eq!(hostname_of(host), None, "{host}");
        }
    }

    #[test]
    fn build_ids_compare_as_numbers() {
        assert!(cs2_behind(Some("9"), Some("10")));
        assert!(!cs2_behind(Some("10"), Some("10")));
        assert!(!cs2_behind(None, Some("10")));
        assert!(cs2_behind(Some("99999999999999999998"), Some("99999999999999999999")));
    }

    #[test]
    fn backoff_grows_caps_and_jitters() {
        let delay = |attempt: u32, random: f64| backoff_delay(attempt, Duration::from_secs(1), Duration::from_secs(30), || random);

        assert_eq!(delay(1, 0.5), Duration::from_secs(1));
        assert_eq!(delay(3, 0.5), Duration::from_secs(4));
        assert_eq!(delay(20, 0.5), Duration::from_secs(30));
        assert_eq!(delay(3, 1.0), Duration::from_secs(5));
        assert_eq!(delay(1, 0.0), Duration::from_secs(1));
    }

    #[test]
    fn bucket_refills_and_resizes() {
        let mut bucket = TokenBucket::new(Rate { burst: 10.0, per_second: 2.0 }, 0.0);

        assert!(bucket.take(10.0, 0.0));
        assert!(!bucket.take(1.0, 0.0));
        assert!(bucket.take(2.0, 1000.0));
        bucket.resize(Rate { burst: 20.0, per_second: 2.0 }, 1000.0);
        assert!((bucket.available(1000.0) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn hints_and_duplicates() {
        assert_eq!(gslt_hint("ABCDEFGHIJKLMNOP"), "GHIJKLMNOP");
        assert_eq!(gslt_hint("short"), "short");
        assert_eq!(duplicate_gslts(&[Some("a"), None, Some("b"), Some("a"), None, Some("b")]), [3, 5]);
        assert_eq!(game_server_budget(0), 1);
        assert_eq!(game_server_budget(8), 7);
    }

    #[test]
    fn origins_follow_kind_and_tls() {
        assert_eq!(controller_origin("cs.example.com", true, OriginKind::Http), "https://cs.example.com");
        assert_eq!(controller_origin("cs.example.com", false, OriginKind::Http), "http://cs.example.com");
        assert_eq!(controller_origin("cs.example.com:8080", true, OriginKind::Ws), "wss://cs.example.com:8080");
        assert_eq!(controller_origin("10.0.0.2", false, OriginKind::Ws), "ws://10.0.0.2");
        assert_eq!(agent_ws_url("cs.example.com", true, "/agent"), "wss://cs.example.com/agent");
        assert_eq!(agent_ws_url("localhost:3000", false, "/agent"), "ws://localhost:3000/agent");
    }

    #[test]
    fn only_an_idle_unreserved_slot_with_a_usable_token_takes_a_launch() {
        assert!(slot_takes_launch(SlotState::Idle, GsltState::Ok, false));
        assert!(slot_takes_launch(SlotState::Idle, GsltState::Unchecked, false));
        assert!(!slot_takes_launch(SlotState::Idle, GsltState::Invalid, false));
        assert!(!slot_takes_launch(SlotState::Idle, GsltState::Ok, true));
        assert!(!slot_takes_launch(SlotState::Running, GsltState::Ok, false));
        assert!(!slot_takes_launch(SlotState::Starting, GsltState::Invalid, true));
    }

    #[test]
    fn keys_name_their_source() {
        use crate::types::{LogSourceAgent, LogSourceCs2Update, LogSourceServer};

        assert_eq!(log_source_key(&LogSource::Agent(LogSourceAgent {})), "agent");
        assert_eq!(log_source_key(&LogSource::Cs2Update(LogSourceCs2Update { update_id: "u1".to_owned() })), "cs2_update:u1");
        assert_eq!(log_source_key(&LogSource::Server(LogSourceServer { allocation_id: "a7".to_owned() })), "server:a7");
        assert_eq!(demo_key("slot-1", "match.dem"), "slot-1/match.dem");
    }

    #[test]
    fn budget_rates_add_a_share_per_slot() {
        let budget = Budget { base: Rate { burst: 2.0, per_second: 3.0 }, per_slot: Rate { burst: 5.0, per_second: 7.0 } };

        assert_eq!(budget_rate(budget, 4), Rate { burst: 22.0, per_second: 31.0 });
        assert_eq!(budget_rate(budget, 0), Rate { burst: 7.0, per_second: 10.0 });
    }

    #[test]
    fn protocol_header_must_name_a_compatible_version() {
        let (major, minor) = protocol_version_of(PROTOCOL_VERSION).unwrap();

        assert!(is_compatible_protocol(Some(PROTOCOL_VERSION)));
        assert!(!is_compatible_protocol(Some(&format!("{}.{minor}", major + 1))));
        // Another minor of the same major is compatible, 0.x included.
        assert_eq!(major, 0);
        assert!(is_compatible_protocol(Some(&format!("0.{}", minor + 1))));
        assert!(is_compatible_protocol(Some(&format!("0.{}", minor - 1))));

        for header in ["", "1", "0.5.0", "v0.5", "00.5", " 0.5", "0.x"] {
            assert!(!is_compatible_protocol(Some(header)), "{header}");
        }

        assert!(!is_compatible_protocol(None));
        assert_eq!(protocol_version_of("1.12"), Some((1, 12)));
    }

    #[test]
    fn config_errors_list_one_issue_per_line() {
        use crate::validate::{Code, PathKey};

        let mut nested = Issue::new(Code::TooSmall, "too small", None);
        nested.path = vec![PathKey::key("slots"), PathKey::Index(1), PathKey::key("port")];
        let root = Issue::new(Code::InvalidType, "expected object", None);

        assert_eq!(format_config_error(&[nested, root]), "  slots.1.port: too small\n  (root): expected object");
    }

    #[test]
    fn json_is_held_to_its_schema() {
        let schema = crate::schemas::by_name("WorkshopId").expect("WorkshopId");

        assert_eq!(serialize_json(schema, &"1388866").expect("valid").as_str(), "\"1388866\"");

        let invalid = serialize_json(schema, &"not a number").expect_err("invalid");
        let ParseError::Invalid(issues) = &invalid else { panic!("{invalid:?}") };
        assert!(!issues.is_empty());
        assert_eq!(invalid.to_string(), format_issues(issues));
        assert!(invalid.to_string().starts_with(": "), "{invalid}");

        let parsed: String = parse_json(schema, "\"1388866\"").expect("valid");
        assert_eq!(parsed, "1388866");
        assert!(matches!(parse_json::<String>(schema, "{"), Err(ParseError::Json(_))));
        assert!(!parse_json::<String>(schema, "{").expect_err("not JSON").to_string().is_empty());
    }
}
