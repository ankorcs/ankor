//! Everything the node logs, kept so the controller can ask for it again: the agent's lines, each game's
//! output and each CS2 install or update's output, one source each. A source's lines reach the controller
//! only while the controller holds that source's lease; the index of sources while it holds any.

use crate::log_markers::printable;
use crate::sync::lock;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use ankor_protocol::constants::{
    AGENT_LOG_LINES, ENDED_SERVER_LOGS, ENDED_UPDATE_LOGS, LOG_BATCH_MAX_BYTES, LOG_INDEX_REFRESH, LOG_LINE_MAX_CHARS, SERVER_LOG_LINES, WATCH_LEASE,
    agent_budgets,
};
use ankor_protocol::helpers::{Rate, TokenBucket, log_source_key};
use ankor_protocol::types::{
    AgentLogBatch, AgentLogIndex, AgentLogLevel, AgentToControllerMessage, LogLine, LogSource, LogSourceAgent, LogSourceCs2Update, LogSourceServer,
    LogSourceSummary,
};
use indexmap::IndexMap;
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::failure::truncate;
use crate::logger::{Entry, Level};

/// New lines wait this long for company, so a chatty game sends batches rather than a frame per line.
const FLUSH: Duration = Duration::from_millis(250);

/// The controller's buckets start full on every connection and ours carries over; the rest absorbs clock
/// differences.
const BUDGET_SHARE: f64 = 0.75;

/// Milliseconds since the epoch.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[must_use]
pub fn wall_clock() -> Clock {
    Arc::new(|| jiff::Timestamp::now().as_millisecond())
}

/// Where reported frames go: the controller connection.
pub trait Transport: Send + Sync {
    /// Whether a frame would reach the controller; nothing is sent while it would not.
    fn connected(&self) -> bool;
    fn send(&self, frame: AgentToControllerMessage);
}

/// Lines logged before the connection exists are kept, and sent once it does.
struct Detached;

impl Transport for Detached {
    fn connected(&self) -> bool {
        false
    }

    fn send(&self, _: AgentToControllerMessage) {}
}

struct Ring {
    lines: VecDeque<LogLine>,
    capacity: usize,
}

impl Ring {
    fn new(capacity: u64) -> Self {
        Self { lines: VecDeque::new(), capacity: usize::try_from(capacity).unwrap_or(usize::MAX) }
    }

    fn push(&mut self, line: LogLine) {
        self.lines.push_back(line);

        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
    }
}

fn json_bytes<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

#[allow(clippy::cast_precision_loss, reason = "the budgets are small whole numbers")]
fn share(burst: u64, per_second: u64) -> Rate {
    Rate { burst: burst as f64 * BUDGET_SHARE, per_second: per_second as f64 * BUDGET_SHARE }
}

fn agent() -> LogSource {
    LogSource::Agent(LogSourceAgent {})
}

fn server(allocation_id: &str) -> LogSource {
    LogSource::Server(LogSourceServer { allocation_id: allocation_id.to_owned() })
}

fn update(update_id: &str) -> LogSource {
    LogSource::Cs2Update(LogSourceCs2Update { update_id: update_id.to_owned() })
}

fn level(level: Level) -> AgentLogLevel {
    match level {
        Level::Error => AgentLogLevel::Error,
        Level::Warn => AgentLogLevel::Warn,
        // The logger sends the sink nothing below info.
        _ => AgentLogLevel::Info,
    }
}

fn limit(count: u64) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}

struct Lease {
    source: LogSource,
    until: i64,
}

struct Pending {
    source: LogSource,
    lines: Vec<LogLine>,
}

struct Timer {
    id: u64,
    task: JoinHandle<()>,
}

#[derive(Clone, Copy)]
enum TimerKind {
    Flush,
    Refresh,
}

struct State {
    /// Timestamps lines and checks leases.
    now: Clock,
    /// The log itself, which a timer calls back into once it fires.
    this: Weak<Inner>,
    /// Where timers run; `None` until a runtime is known.
    runtime: Option<Handle>,
    /// The agent's lines.
    agent: Ring,
    /// By game, in the order their output began.
    games: IndexMap<String, Ring>,
    /// Games that ended, oldest first, also before any output; past `ENDED_SERVER_LOGS` their output goes.
    ended: Vec<String>,
    /// Games whose output went, as many as `ended` holds. A late line from one is dropped, as a
    /// buffer opened for it would never end.
    gone: VecDeque<String>,
    /// By update id, in the order they started.
    updates: IndexMap<String, Ring>,
    /// The update whose output arrives now.
    current: Option<String>,
    /// Updates that ended, oldest first; past `ENDED_UPDATE_LOGS` their output goes.
    ended_updates: Vec<String>,
    /// Per source, by its key: until when the controller holds it.
    leases: IndexMap<String, Lease>,
    /// New lines of watched sources, by key, until the next flush.
    pending: IndexMap<String, Pending>,
    /// The index changed since it was last sent.
    index_dirty: bool,
    /// Sends pending lines and a dirty index after `FLUSH`.
    timer: Option<Timer>,
    /// Brings counts and last lines up to date once lines have arrived since the index went out.
    refresh: Option<Timer>,
    /// The id of the last timer scheduled, so a superseded timer that fires does nothing.
    timers: u64,
    /// The frame budget, a share of the controller's.
    frames: TokenBucket,
    /// The byte budget, a share of the controller's.
    bytes: TokenBucket,
    /// Where frames go; [`Detached`] until [`NodeLog::attach`].
    transport: Arc<dyn Transport>,
    /// Frames to send once the lock is released, so a send that logs a line cannot deadlock.
    outbox: Vec<AgentToControllerMessage>,
    /// Once cancelled, no flush or refresh timer runs.
    stop: CancellationToken,
}

struct Inner {
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct NodeLog {
    inner: Arc<Inner>,
}

/// A log on the wall clock that no shutdown stops, for a part that is never attached.
impl Default for NodeLog {
    fn default() -> Self {
        Self::new(wall_clock(), CancellationToken::new())
    }
}

impl NodeLog {
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "milliseconds since the epoch stay far below 2^53")]
    /// A log whose flush and refresh timers stop once `stop` is cancelled.
    pub fn new(now: Clock, stop: CancellationToken) -> Self {
        let at = now() as f64;
        let inner = Arc::new_cyclic(|this| Inner {
            state: Mutex::new(State {
                this: this.clone(),
                runtime: Handle::try_current().ok(),
                agent: Ring::new(AGENT_LOG_LINES),
                games: IndexMap::new(),
                ended: Vec::new(),
                gone: VecDeque::new(),
                updates: IndexMap::new(),
                current: None,
                ended_updates: Vec::new(),
                leases: IndexMap::new(),
                pending: IndexMap::new(),
                index_dirty: false,
                timer: None,
                refresh: None,
                timers: 0,
                frames: TokenBucket::new(share(agent_budgets::log_frames::base::BURST, agent_budgets::log_frames::base::PER_SECOND), at),
                bytes: TokenBucket::new(share(agent_budgets::log_bytes::base::BURST, agent_budgets::log_bytes::base::PER_SECOND), at),
                transport: Arc::new(Detached),
                outbox: Vec::new(),
                stop,
                now,
            }),
        });

        Self { inner }
    }

    /// Runs `change` on the state, then sends what it queued once the lock is released.
    fn with(&self, change: impl FnOnce(&mut State)) {
        let (transport, frames) = {
            let mut state = lock(&self.inner.state);

            change(&mut state);
            (Arc::clone(&state.transport), std::mem::take(&mut state.outbox))
        };

        for frame in frames {
            transport.send(frame);
        }
    }

    /// Sends from here on; timers run on the runtime this is called on.
    pub fn attach(&self, transport: Arc<dyn Transport>) {
        let mut state = lock(&self.inner.state);

        state.transport = transport;

        if let Ok(runtime) = Handle::try_current() {
            state.runtime = Some(runtime);
        }
    }

    pub fn agent_line(&self, logged: &Entry) {
        // An agent line can quote what it read from outside, such as a frame it dropped.
        let line = LogLine { at: logged.at_ms, level: Some(level(logged.level)), line: truncate(&printable(&logged.message), LOG_LINE_MAX_CHARS) };

        self.with(|state| state.add_agent(line));
    }

    pub fn game_line(&self, allocation_id: &str, text: &str) {
        self.with(|state| {
            if state.gone.iter().any(|gone| gone == allocation_id) {
                return;
            }

            let line = LogLine { at: (state.now)(), level: None, line: truncate(text, LOG_LINE_MAX_CHARS) };

            state.games.entry(allocation_id.to_owned()).or_insert_with(|| Ring::new(SERVER_LOG_LINES));
            state.add(&server(allocation_id), line);
        });
    }

    /// An update's output goes to a source of its own from here; one still running has ended.
    pub fn update_started(&self, id: &str) {
        self.with(|state| {
            state.update_ended();
            state.current = Some(id.to_owned());
            state.updates.insert(id.to_owned(), Ring::new(SERVER_LOG_LINES));
            state.changed();
        });
    }

    /// A line after its update ended, which the exit watch can report before the last output, joins
    /// the last update. Every update opens its source before its first line.
    pub fn update_line(&self, text: &str) {
        self.with(|state| {
            let Some(id) = state.current.clone().or_else(|| state.updates.keys().last().cloned()) else {
                return;
            };
            let line = LogLine { at: (state.now)(), level: None, line: truncate(text, LOG_LINE_MAX_CHARS) };

            state.add(&update(&id), line);
        });
    }

    /// The running update ended: its output stays among the last few ended ones.
    pub fn update_ended(&self) {
        self.with(State::update_ended);
    }

    /// The game's slot went idle: its output stays among the last few ended ones.
    pub fn game_ended(&self, allocation_id: &str) {
        self.with(|state| {
            if state.ended.iter().any(|ended| ended == allocation_id) {
                return;
            }

            state.ended.push(allocation_id.to_owned());
            state.changed();

            while state.ended.len() > limit(ENDED_SERVER_LOGS) {
                let oldest = state.ended.remove(0);

                state.games.shift_remove(&oldest);
                state.pending.shift_remove(&log_source_key(&server(&oldest)));
                state.gone.push_back(oldest);

                if state.gone.len() > limit(ENDED_SERVER_LOGS) {
                    state.gone.pop_front();
                }
            }
        });
    }

    /// Renews the source's lease; a `replay_id` replays the source to the one that asked, and a lease
    /// that starts without one to every watcher.
    pub fn watch(&self, source: &LogSource, replay_id: Option<&str>) {
        self.with(|state| {
            let key = log_source_key(source);
            let started = !state.watched(&key);
            let until = (state.now)().saturating_add(i64::try_from(WATCH_LEASE.as_millis()).unwrap_or(i64::MAX));

            state.leases.insert(key, Lease { source: source.clone(), until });

            if started || replay_id.is_some() {
                state.flush(false);
                state.send_index();
                state.replay(source, replay_id);
            }
        });
    }

    /// A new connection holds nothing: every source whose lease runs gets everything kept.
    pub fn resend(&self) {
        self.with(|state| {
            let watched = state.watched_sources();

            if watched.is_empty() {
                return;
            }

            state.flush(false);
            state.send_index();

            for source in &watched {
                state.replay(source, None);
            }
        });
    }

    fn fire(&self, kind: TimerKind, id: u64) {
        self.with(|state| {
            if state.stop.is_cancelled() {
                return;
            }

            let slot = match kind {
                TimerKind::Flush => &mut state.timer,
                TimerKind::Refresh => &mut state.refresh,
            };

            if slot.as_ref().is_none_or(|timer| timer.id != id) {
                return;
            }

            *slot = None;

            match kind {
                TimerKind::Flush => state.flush(true),
                TimerKind::Refresh => state.changed(),
            }
        });
    }
}

impl State {
    #[allow(clippy::cast_precision_loss, reason = "milliseconds since the epoch stay far below 2^53")]
    fn now_ms(&self) -> f64 {
        (self.now)() as f64
    }

    fn schedule(&mut self, kind: TimerKind, delay: Duration) -> Option<Timer> {
        let runtime = self.runtime.as_ref().filter(|_| !self.stop.is_cancelled())?;
        let this = self.this.clone();

        self.timers += 1;

        let id = self.timers;
        let task = runtime.spawn(async move {
            tokio::time::sleep(delay).await;

            if let Some(inner) = this.upgrade() {
                NodeLog { inner }.fire(kind, id);
            }
        });

        Some(Timer { id, task })
    }

    fn update_ended(&mut self) {
        let Some(current) = self.current.take() else {
            return;
        };

        self.ended_updates.push(current);
        self.changed();

        while self.ended_updates.len() > limit(ENDED_UPDATE_LOGS) {
            let oldest = self.ended_updates.remove(0);

            self.updates.shift_remove(&oldest);
            self.pending.shift_remove(&log_source_key(&update(&oldest)));
        }
    }

    fn cancel_flush(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.task.abort();
        }
    }

    fn watched(&mut self, key: &str) -> bool {
        let now = (self.now)();

        match self.leases.get(key) {
            Some(lease) if lease.until <= now => {
                self.leases.shift_remove(key);
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    fn watched_sources(&mut self) -> Vec<LogSource> {
        let now = (self.now)();

        self.leases.retain(|_, lease| lease.until > now);
        self.leases.values().map(|lease| lease.source.clone()).collect()
    }

    fn ring_mut(&mut self, source: &LogSource) -> Option<&mut Ring> {
        match source {
            LogSource::Agent(_) => Some(&mut self.agent),
            LogSource::Cs2Update(update) => self.updates.get_mut(&update.update_id),
            LogSource::Server(server) => self.games.get_mut(&server.allocation_id),
        }
    }

    fn ring(&self, source: &LogSource) -> Option<&Ring> {
        match source {
            LogSource::Agent(_) => Some(&self.agent),
            LogSource::Cs2Update(update) => self.updates.get(&update.update_id),
            LogSource::Server(server) => self.games.get(&server.allocation_id),
        }
    }

    fn add_agent(&mut self, line: LogLine) {
        self.add(&agent(), line);
    }

    fn add(&mut self, source: &LogSource, line: LogLine) {
        let Some(ring) = self.ring_mut(source) else {
            return;
        };
        let first = ring.lines.is_empty();

        ring.push(line.clone());

        if first {
            self.changed();
        } else if self.refresh.is_none() && !self.watched_sources().is_empty() {
            self.refresh = self.schedule(TimerKind::Refresh, LOG_INDEX_REFRESH);
        }

        let key = log_source_key(source);

        if !self.watched(&key) {
            return;
        }

        self.pending.entry(key).or_insert_with(|| Pending { source: source.clone(), lines: Vec::new() }).lines.push(line);
        self.arm();
    }

    /// The index goes out with the next flush, while any source is watched.
    fn changed(&mut self) {
        self.index_dirty = true;

        if !self.watched_sources().is_empty() {
            self.arm();
        }
    }

    fn arm(&mut self) {
        if self.timer.is_none() {
            self.timer = self.schedule(TimerKind::Flush, FLUSH);
        }
    }

    /// `index` false when the caller sends the index itself right after.
    fn flush(&mut self, index: bool) {
        self.cancel_flush();

        let pending = std::mem::take(&mut self.pending);

        if self.watched_sources().is_empty() {
            return;
        }

        if index && self.index_dirty {
            self.send_index();
        }

        for (key, Pending { source, lines }) in pending {
            if self.watched(&key) {
                self.send_lines(&source, &lines, None, false);
            }
        }
    }

    fn index(&self) -> AgentLogIndex {
        let summary = |source: LogSource, ring: &Ring, ended: bool| LogSourceSummary {
            source,
            line_count: i64::try_from(ring.lines.len()).unwrap_or(i64::MAX),
            last_line_at: ring.lines.back().map(|line| line.at),
            ended,
        };
        let mut sources = vec![summary(agent(), &self.agent, false)];

        sources.extend(self.updates.iter().map(|(id, ring)| summary(update(id), ring, self.ended_updates.contains(id))));
        sources.extend(self.games.iter().map(|(id, ring)| summary(server(id), ring, self.ended.contains(id))));

        AgentLogIndex { sources }
    }

    /// Kept dirty when the budget has no room, so the next flush tries again.
    fn send_index(&mut self) {
        if !self.transport.connected() {
            return;
        }

        let frame = AgentToControllerMessage::LogIndex(self.index());

        if self.spend(json_bytes(&frame)) {
            self.index_dirty = false;
            self.outbox.push(frame);

            // The index just sent is as fresh as the refresh would make it.
            if let Some(refresh) = self.refresh.take() {
                refresh.task.abort();
            }
        } else {
            self.index_dirty = true;
            self.arm();
        }
    }

    /// As much of the source as the budget has room for, newest kept.
    #[allow(clippy::cast_precision_loss, reason = "byte counts stay far below 2^53")]
    fn replay(&mut self, source: &LogSource, id: Option<&str>) {
        let all: Vec<LogLine> = self.ring(source).map(|ring| ring.lines.iter().cloned().collect()).unwrap_or_default();
        let now = self.now_ms();
        let envelope = json_bytes(&AgentToControllerMessage::LogBatch(AgentLogBatch {
            source: source.clone(),
            replay_id: id.map(str::to_owned),
            reset: true,
            lines: Vec::new(),
        })) as f64;
        // A frame closes once the next line would not fit, so each carries at least this much.
        let widest = LogLine { at: (self.now)(), level: Some(AgentLogLevel::Error), line: "\u{0}".repeat(limit(LOG_LINE_MAX_CHARS)) };
        let fill = LOG_BATCH_MAX_BYTES as f64 - envelope - json_bytes(&widest) as f64;
        let room = self.bytes.available(now);
        let frame_room = self.frames.available(now).floor();
        let mut start = all.len();
        let mut bytes = 0.0;

        while let Some(line) = start.checked_sub(1).and_then(|index| all.get(index)) {
            let next = bytes + json_bytes(line) as f64 + 1.0;
            let frames = (next / fill).ceil().max(1.0);

            if next + frames * envelope > room || frames > frame_room {
                break;
            }

            bytes = next;
            start -= 1;
        }

        self.send_lines(source, all.get(start..).unwrap_or_default(), id, true);
    }

    /// In as many frames as `LOG_BATCH_MAX_BYTES` needs; only the first carries `reset`. A frame the
    /// budget has no room for is dropped, with the rest.
    fn send_lines(&mut self, source: &LogSource, lines: &[LogLine], replay_id: Option<&str>, reset: bool) {
        if !self.transport.connected() {
            return;
        }

        let empty = |first: bool| AgentLogBatch { source: source.clone(), replay_id: replay_id.map(str::to_owned), reset: first && reset, lines: Vec::new() };
        let size = |frame: &AgentLogBatch| json_bytes(&AgentToControllerMessage::LogBatch(frame.clone()));
        let mut frame = empty(true);
        let mut bytes = size(&frame);

        for line in lines {
            let line_bytes = json_bytes(line) + 1;

            if !frame.lines.is_empty() && bytes + line_bytes > limit(LOG_BATCH_MAX_BYTES) {
                if !self.send_batch(std::mem::replace(&mut frame, empty(false)), bytes) {
                    return;
                }

                bytes = size(&frame);
            }

            frame.lines.push(line.clone());
            bytes += line_bytes;
        }

        // A replay of nothing still tells the controller to drop what it holds.
        if !frame.lines.is_empty() || frame.reset {
            self.send_batch(frame, bytes);
        }
    }

    fn send_batch(&mut self, frame: AgentLogBatch, bytes: usize) -> bool {
        if !self.spend(bytes) {
            return false;
        }

        self.outbox.push(AgentToControllerMessage::LogBatch(frame));
        true
    }

    #[allow(clippy::cast_precision_loss, reason = "byte counts stay far below 2^53")]
    fn spend(&mut self, bytes: usize) -> bool {
        let now = self.now_ms();

        if self.frames.available(now) < 1.0 || self.bytes.available(now) < bytes as f64 {
            return false;
        }

        self.frames.take(1.0, now);
        self.bytes.take(bytes as f64, now);
        true
    }
}
