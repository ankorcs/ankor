//! What one agent did during one scenario, in the order the fakes saw it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

/// One observation: a channel names where it was seen, the value is what was seen.
#[derive(Debug, Clone)]
pub struct Entry {
    pub channel: String,
    pub value: Value,
}

/// The keys of an entry on the `controller` channel: the side that sent the frame, or how the agent's
/// socket ended.
pub mod controller_entry {
    /// A text frame from the agent.
    pub const AGENT: &str = "agent";
    /// A frame the controller sent.
    pub const CONTROLLER: &str = "controller";
    /// A binary frame from the agent, recorded by its length.
    pub const AGENT_BINARY: &str = "agent_binary";
    /// The agent closed the socket.
    pub const AGENT_CLOSED: &str = "agent_closed";
    /// The socket ended without a close.
    pub const AGENT_GONE: &str = "agent_gone";
    /// Not a frame: the scenario's step that the entries since the previous marker belong to.
    pub const AFTER_STEP: &str = "after_step";
}

/// The recorder's channels: what each fake records under, and what the golden transcripts are keyed by.
pub mod channels {
    /// The agent socket, both ways.
    pub const CONTROLLER: &str = "controller";
    /// Event uploads.
    pub const EVENTS: &str = "events";
    /// Demo uploads.
    pub const DEMOS: &str = "demos";
    /// Docker's container events.
    pub const DOCKER: &str = "docker";
    /// The Steam stub's logons.
    pub const STEAM: &str = "steam";
    /// Every request the stubbed outside services answer.
    pub const STUBS: &str = "stubs";
    /// Requests to the controller that no route of the fake serves.
    pub const CONTROLLER_HTTP: &str = "controller_http";
    /// What each game server reports, one channel per allocation; see [`plugin`].
    pub const PLUGIN_PREFIX: &str = "plugin/";
    /// The node's state at a scenario's named moment, one channel per label and kind; see [`snapshot`].
    pub const SNAPSHOT_PREFIX: &str = "snapshot/";
    /// A snapshot's containers.
    pub const SNAPSHOT_CONTAINERS: &str = "containers";
    /// A snapshot's files in the data directory.
    pub const SNAPSHOT_FILES: &str = "files";

    /// The channel the game server of `allocation` reports on.
    #[must_use]
    pub fn plugin(allocation: &str) -> String {
        format!("{PLUGIN_PREFIX}{allocation}")
    }

    /// The channel of the `kind` part of the snapshot `label`, such as [`SNAPSHOT_CONTAINERS`].
    #[must_use]
    pub fn snapshot(label: &str, kind: &str) -> String {
        format!("{SNAPSHOT_PREFIX}{label}/{kind}")
    }

    /// Whether `channel` holds the `kind` part of a snapshot, as [`snapshot`] names it.
    #[must_use]
    pub fn is_snapshot(channel: &str, kind: &str) -> bool {
        channel.strip_prefix(SNAPSHOT_PREFIX).and_then(|rest| rest.rsplit_once('/')).is_some_and(|(_, part)| part == kind)
    }
}

/// Collects entries from every fake; scenarios wait on it.
#[derive(Clone)]
pub struct Recorder {
    entries: Arc<Mutex<Vec<Entry>>>,
    changed: watch::Sender<usize>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub fn new() -> Self {
        let (changed, _) = watch::channel(0);
        Self { entries: Arc::new(Mutex::new(Vec::new())), changed }
    }

    pub fn record(&self, channel: impl Into<String>, value: Value) {
        let count = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            entries.push(Entry { channel: channel.into(), value });
            entries.len()
        };
        self.changed.send_replace(count);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Clears everything recorded so far, for a new scenario run.
    pub fn clear(&self) {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.changed.send_replace(0);
    }

    /// The first entry at or after `from` on `channel` that `matches`, waiting up to `timeout`.
    /// Returns its index and value.
    pub async fn wait_for(&self, from: usize, channel: &str, timeout: Duration, matches: impl Fn(&Value) -> bool) -> Result<(usize, Value)> {
        let deadline = Instant::now() + timeout;
        let mut changed = self.changed.subscribe();
        loop {
            {
                let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
                for (index, entry) in entries.iter().enumerate().skip(from) {
                    if entry.channel == channel && matches(&entry.value) {
                        return Ok((index, entry.value.clone()));
                    }
                }
            }
            if tokio::time::timeout_at(deadline, changed.changed()).await.is_err() {
                bail!("timed out after {timeout:?} waiting on channel {channel}");
            }
        }
    }

    /// Waits until nothing new has been recorded on `channels` (all channels when empty) for `quiet`,
    /// giving up after `limit`.
    pub async fn settle(&self, channels: &[&str], quiet: Duration, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut seen = self.count_on(channels);
        let mut last_change = Instant::now();
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let now = self.count_on(channels);
            if now != seen {
                seen = now;
                last_change = Instant::now();
            } else if last_change.elapsed() >= quiet {
                return;
            }
        }
    }

    fn count_on(&self, channels: &[&str]) -> usize {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.iter().filter(|entry| channels.is_empty() || channels.iter().any(|channel| entry.channel.starts_with(channel))).count()
    }
}

/// A finished transcript: entries grouped by channel, in the order they were recorded.
pub fn by_channel(entries: &[Entry]) -> BTreeMap<String, Vec<Value>> {
    let mut channels: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for entry in entries {
        channels.entry(entry.channel.clone()).or_default().push(entry.value.clone());
    }
    channels
}

/// The transcript as one JSON document, for the files the runner writes.
pub fn to_json(channels: &BTreeMap<String, Vec<Value>>) -> Value {
    json!(channels)
}
