//! Uploads spooled events to the controller, oldest server first. A failure that every server would meet,
//! such as no answer or a refused node, backs off the whole node. A failure of one server's batch backs off
//! that server only. The spool is the queue.

use crate::sync::lock;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::constants::{
    EVENT_BACKLOG_REASON_MAX_CHARS, EVENT_RETRY_MAX, EVENT_RETRY_MIN, EVENT_UPLOAD_TIMEOUT, MAX_REPORTED_BACKLOGS, PROTOCOL_HEADER, PROTOCOL_VERSION,
};
use ankor_protocol::helpers::{OriginKind, ParseError, backoff_delay, controller_origin, node_authorization, parse_json, serialize_json};
use ankor_protocol::schemas::{s_event_batch, s_event_batch_receipt, s_event_batch_rejection};
use ankor_protocol::types::{EventBacklog, EventBatch, EventBatchReceipt, EventBatchRejection, EventFrame, StuckServer};
use async_trait::async_trait;
use reqwest::StatusCode;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::event_spool::{SpoolBacklog, SpoolError};
use crate::failure::truncate;
use crate::http_body::is_success;
use crate::logger::Logger;
use crate::spool_client::SpoolPort;

const TICK: Duration = Duration::from_secs(1);
const RETENTION_INTERVAL: Duration = Duration::from_hours(1);

/// The backlog while the last upload worked.
#[must_use]
pub fn nothing_stuck() -> EventBacklog {
    EventBacklog { servers: Vec::new(), server_count: 0, event_count: 0, reason: None }
}

/// Read on every pass, so `set_config` can move a running node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventsTarget {
    pub host: String,
    pub tls: bool,
    pub events_path: String,
}

pub type TargetFn = Arc<dyn Fn() -> EventsTarget + Send + Sync>;
pub(crate) type AttachedFn = Arc<dyn Fn() -> Vec<String> + Send + Sync>;
pub type BacklogHook = Arc<dyn Fn(EventBacklog) + Send + Sync>;
pub(crate) type RandomFn = Arc<dyn Fn() -> f64 + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostAnswer {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// The HTTP client could not be built, so no request was sent.
    #[error("no HTTP client: {0}")]
    NoClient(#[source] Arc<reqwest::Error>),
    /// The HTTP client failed to send the request or to read the answer.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// The request failed on the way, outside the HTTP client.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The spool could not be read or acknowledged.
    #[error(transparent)]
    Spool(#[from] SpoolError),
    /// A batch could not be serialized, or a stored frame or the receipt could not be parsed.
    #[error(transparent)]
    Json(#[from] ParseError),
    /// The controller answered 409 and refused the batch, with its detail.
    #[error("event backend refused the batch: {0}")]
    Refused(String),
    /// The controller answered with another status that is not a success.
    #[error("event backend returned HTTP {0}")]
    Status(u16),
    /// The request was abandoned; `moved` when that was for a new address.
    #[error("This operation was aborted")]
    Aborted { moved: bool },
}

/// A hello carries the reason too, so it is never empty.
fn failure_reason(error: &UploadError) -> String {
    let reason = truncate(&error.to_string(), EVENT_BACKLOG_REASON_MAX_CHARS);

    if reason.is_empty() { "the upload failed".to_owned() } else { reason }
}

/// `Err` means the request got no answer.
#[async_trait]
pub trait EventsPost: Send + Sync {
    async fn post(&self, url: &str, headers: &[(&str, String)], body: String) -> Result<PostAnswer, UploadError>;
}

pub struct HttpPost(Result<reqwest::Client, Arc<reqwest::Error>>);

impl HttpPost {
    #[must_use]
    pub fn new() -> Self {
        crate::init_tls();

        Self(reqwest::Client::builder().timeout(EVENT_UPLOAD_TIMEOUT).build().map_err(Arc::new))
    }
}

impl Default for HttpPost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventsPost for HttpPost {
    async fn post(&self, url: &str, headers: &[(&str, String)], body: String) -> Result<PostAnswer, UploadError> {
        let mut request = self.0.as_ref().map_err(|error| UploadError::NoClient(Arc::clone(error)))?.post(url).body(body);

        for (name, value) in headers {
            request = request.header(*name, value);
        }

        let response = request.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;

        Ok(PostAnswer { status, body })
    }
}

pub struct UploaderParts {
    /// The spool that events are read from and acknowledged in.
    pub spool: Arc<dyn SpoolPort>,
    /// The node token that authorizes each upload.
    pub token: String,
    /// Returns the controller address, read on every pass.
    pub target: TargetFn,
    /// Returns the servers whose plugin is attached, which pruning spares.
    pub attached: AttachedFn,
    /// Receives the backlog whenever a pass changes it.
    pub on_backlog: BacklogHook,
    /// Receives the warnings about retained and dropped events.
    pub log: Logger,
    /// Sends each batch to the controller.
    pub post: Arc<dyn EventsPost>,
    /// The backoff's jitter, from 0 to 1.
    pub random: RandomFn,
}

struct State {
    /// The address the uploader last followed.
    target: EventsTarget,
    /// When the node may upload again after a node-wide failure.
    retry_at: Option<Instant>,
    /// The latest failure's reason, which the backlog reports.
    last_failure: String,
    /// The backlog last passed to `on_backlog`, so only changes are reported.
    last_backlog: EventBacklog,
    /// Servers whose batches the controller refused, and when each is tried again. A server leaves when an
    /// upload of it works or it has nothing waiting.
    servers: HashMap<String, ServerRetry>,
}

struct ServerRetry {
    failures: u32,
    retry_at: Instant,
}

/// The statuses that mean the controller or the link failed, not one batch.
const NODE_WIDE_STATUSES: [StatusCode; 7] = [
    StatusCode::UNAUTHORIZED,
    StatusCode::FORBIDDEN,
    StatusCode::REQUEST_TIMEOUT,
    StatusCode::TOO_MANY_REQUESTS,
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
    StatusCode::GATEWAY_TIMEOUT,
];

/// A failure of the controller or the link would meet every server, so the whole node backs off.
fn node_wide(error: &UploadError) -> bool {
    match error {
        UploadError::Refused(_) | UploadError::Json(_) => false,
        UploadError::Status(status) => NODE_WIDE_STATUSES.iter().any(|node_wide| *status == *node_wide),
        _ => true,
    }
}

struct Inner {
    /// The spool that events are read from and acknowledged in.
    spool: Arc<dyn SpoolPort>,
    /// The node token that authorizes each upload.
    token: String,
    /// Returns the current controller address.
    target: TargetFn,
    /// Returns the servers whose plugin is attached, which pruning spares.
    attached: AttachedFn,
    /// Receives the backlog whenever a pass changes it.
    on_backlog: BacklogHook,
    /// Receives the warnings about retained and dropped events.
    log: Logger,
    /// Sends each batch to the controller.
    post: Arc<dyn EventsPost>,
    /// The backoff's jitter, from 0 to 1.
    random: RandomFn,
    /// Backoff and reporting state, shared by the loops and `backlog`.
    state: Mutex<State>,
    /// Consecutive node-wide failures, which set the node's backoff. An upload that works resets it.
    failures: AtomicU32,
    /// Ends the upload and pruning loops; a request in flight is abandoned.
    stop: CancellationToken,
    /// Set when the controller refused this node. History keeps landing in the spool, but nothing goes upstream.
    upstream_closed: CancellationToken,
    /// The loops `start` runs, which `close` waits for.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct EventUploader {
    inner: Arc<Inner>,
}

impl EventUploader {
    #[must_use]
    pub fn new(spool: Arc<dyn SpoolPort>, token: String, target: TargetFn, attached: AttachedFn, on_backlog: BacklogHook, log: Logger) -> Self {
        Self::from_parts(UploaderParts {
            spool,
            token,
            target,
            attached,
            on_backlog,
            log,
            post: Arc::new(HttpPost::new()),
            random: Arc::new(rand::random::<f64>),
        })
    }

    #[must_use]
    pub fn from_parts(parts: UploaderParts) -> Self {
        let UploaderParts { spool, token, target, attached, on_backlog, log, post, random } = parts;
        let first = target();

        Self {
            inner: Arc::new(Inner {
                spool,
                token,
                target,
                attached,
                on_backlog,
                log,
                post,
                random,
                state: Mutex::new(State { target: first, retry_at: None, last_failure: String::new(), last_backlog: nothing_stuck(), servers: HashMap::new() }),
                failures: AtomicU32::new(0),
                stop: CancellationToken::new(),
                upstream_closed: CancellationToken::new(),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    pub async fn start(&self) {
        self.prune().await;

        let uploader = self.clone();
        let uploads = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(TICK);

            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;
                    () = uploader.inner.stop.cancelled() => break,
                    _ = ticks.tick() => uploader.tick().await,
                }
            }
        });
        let pruner = self.clone();
        let first_prune = Instant::now() + RETENTION_INTERVAL;
        let pruning = tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(first_prune, RETENTION_INTERVAL);

            loop {
                tokio::select! {
                    biased;
                    () = pruner.inner.stop.cancelled() => break,
                    _ = ticks.tick() => pruner.prune().await,
                }
            }
        });

        lock(&self.inner.tasks).extend([uploads, pruning]);
    }

    /// History keeps being spooled.
    pub fn close_upstream(&self) {
        self.inner.upstream_closed.cancel();
    }

    fn halted(&self) -> bool {
        self.inner.stop.is_cancelled() || self.inner.upstream_closed.is_cancelled()
    }

    /// Empty while uploads work, even when events wait in the spool.
    ///
    /// # Errors
    ///
    /// Fails when the spool cannot be read.
    pub async fn backlog(&self) -> Result<EventBacklog, SpoolError> {
        if self.inner.failures.load(Ordering::SeqCst) == 0 && lock(&self.inner.state).servers.is_empty() {
            return Ok(nothing_stuck());
        }

        let servers = self.inner.spool.backlog().await?;

        if servers.is_empty() {
            return Ok(nothing_stuck());
        }

        let reason = lock(&self.inner.state).last_failure.clone();

        Ok(EventBacklog {
            servers: servers
                .iter()
                .take(usize::try_from(MAX_REPORTED_BACKLOGS).unwrap_or(usize::MAX))
                .map(|server| StuckServer { allocation_id: server.allocation_id.clone(), pending: server.pending })
                .collect(),
            server_count: i64::try_from(servers.len()).unwrap_or(i64::MAX),
            event_count: servers.iter().map(|server| server.pending).sum(),
            reason: Some(reason),
        })
    }

    pub async fn close(&self) {
        self.inner.stop.cancel();

        let tasks = std::mem::take(&mut *lock(&self.inner.tasks));

        for task in tasks {
            let _ = task.await;
        }
    }

    async fn tick(&self) {
        loop {
            self.follow_target();

            let waiting = lock(&self.inner.state).retry_at.is_some_and(|at| Instant::now() < at);

            // `drain` is true when a move abandoned a request, so start over at the new address.
            if self.halted() || waiting || !self.drain().await {
                return;
            }
        }
    }

    /// Backoffs earned against the old address do not apply to a new one.
    fn follow_target(&self) {
        let target = (self.inner.target)();
        let mut state = lock(&self.inner.state);

        if target == state.target {
            return;
        }

        state.target = target;
        state.retry_at = None;
        state.servers.clear();
        self.inner.failures.store(0, Ordering::SeqCst);
    }

    fn target_moved(&self) -> bool {
        (self.inner.target)() != lock(&self.inner.state).target
    }

    /// Returns `true` when a move abandoned the request in flight.
    async fn drain(&self) -> bool {
        let backlog = match self.inner.spool.backlog().await {
            Ok(backlog) => backlog,
            Err(error) => {
                self.fail_node(&error.into());
                self.report_backlog().await;
                return false;
            }
        };

        lock(&self.inner.state).servers.retain(|allocation_id, _| backlog.iter().any(|server| &server.allocation_id == allocation_id));

        // Without this set, a frame too large for a batch would bring its server back every round.
        let mut emptied = HashSet::new();
        let mut round = self.eligible(backlog, &emptied);
        let mut read_at = Instant::now();

        // One batch per server per round, so a busy server cannot hold back the others. The spool
        // is reread once a tick. A new or unblocked server then joins soon, and a large backlog
        // does not rescan the spool after every batch while the relay's commits wait behind it.
        'rounds: while !round.is_empty() {
            let mut next = Vec::new();

            for allocation_id in round {
                match self.upload_batch(&allocation_id).await {
                    // A stop ends the upload without its events delivered; it must not be reported clear.
                    Ok(_) if self.halted() => return false,
                    Ok(more) => {
                        self.inner.failures.store(0, Ordering::SeqCst);

                        if more {
                            next.push(allocation_id);
                        } else {
                            lock(&self.inner.state).servers.remove(&allocation_id);
                            emptied.insert(allocation_id);
                        }
                    }
                    Err(UploadError::Aborted { moved: true }) => return true,
                    Err(error) if node_wide(&error) => {
                        self.fail_node(&error);
                        break 'rounds;
                    }
                    Err(error) => self.fail_server(&allocation_id, &error),
                }
            }

            if next.is_empty() || read_at.elapsed() < TICK {
                round = next;
                continue;
            }

            read_at = Instant::now();
            round = match self.inner.spool.backlog().await {
                Ok(backlog) => self.eligible(backlog, &emptied),
                Err(error) => {
                    self.fail_node(&error.into());
                    break;
                }
            };
        }

        self.report_backlog().await;
        false
    }

    fn eligible(&self, backlog: Vec<SpoolBacklog>, emptied: &HashSet<String>) -> Vec<String> {
        let state = lock(&self.inner.state);
        let now = Instant::now();

        backlog
            .into_iter()
            .map(|server| server.allocation_id)
            .filter(|allocation_id| !emptied.contains(allocation_id))
            .filter(|allocation_id| state.servers.get(allocation_id).is_none_or(|retry| now >= retry.retry_at))
            .collect()
    }

    fn fail_node(&self, error: &UploadError) {
        let failures = self.inner.failures.fetch_add(1, Ordering::SeqCst) + 1;
        let delay = backoff_delay(failures, EVENT_RETRY_MIN, EVENT_RETRY_MAX, || (self.inner.random)());

        {
            let mut state = lock(&self.inner.state);

            state.last_failure = failure_reason(error);
            state.retry_at = Some(Instant::now() + delay);
        }

        if !self.halted() {
            self.inner.log.warn(format!("events retained for retry: {error}"));
        }
    }

    fn fail_server(&self, allocation_id: &str, error: &UploadError) {
        {
            let mut state = lock(&self.inner.state);
            let failures = state.servers.get(allocation_id).map_or(0, |retry| retry.failures) + 1;
            let delay = backoff_delay(failures, EVENT_RETRY_MIN, EVENT_RETRY_MAX, || (self.inner.random)());

            state.last_failure = failure_reason(error);
            state.servers.insert(allocation_id.to_owned(), ServerRetry { failures, retry_at: Instant::now() + delay });
        }

        self.inner.log.warn(format!("events of {allocation_id} retained for retry: {error}"));
    }

    /// Returns `true` when the server may have more waiting.
    async fn upload_batch(&self, allocation_id: &str) -> Result<bool, UploadError> {
        let pending = self.inner.spool.pending(allocation_id.to_owned()).await?;

        if self.halted() || pending.is_empty() {
            return Ok(false);
        }

        // The parse reads only `seq`, which the relay checked before storing, so a stored frame passes.
        let frames = pending.iter().map(|entry| serde_json::from_str::<EventFrame>(&entry.frame)).collect::<Result<_, _>>().map_err(ParseError::Json)?;
        let last = pending.last().map_or(0, |frame| frame.seq);
        let receipt = self.post_batch(&EventBatch { allocation_id: allocation_id.to_owned(), frames }).await?;

        // The controller holds what it quarantined, so resending it could only fail the same way.
        if !receipt.rejected.is_empty() {
            let reasons: Vec<String> = receipt.rejected.iter().map(|entry| format!("{}: {}", entry.seq, entry.reason)).collect();

            self.inner.log.warn(format!(
                "the controller quarantined {} event(s) of {allocation_id}, dropped here: {}",
                receipt.rejected.len(),
                reasons.join("; ")
            ));
        }

        match self.inner.spool.acknowledge(allocation_id.to_owned(), last).await {
            Ok(()) => Ok(true),
            // Retention dropped the stream while the controller took the batch. The prune logged the
            // dropped events.
            Err(SpoolError::AcknowledgedMissing) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn post_batch(&self, batch: &EventBatch) -> Result<EventBatchReceipt, UploadError> {
        // A move between two requests sends the next one to the new address.
        self.follow_target();

        let aborted = |moved| Err(UploadError::Aborted { moved });
        // Checks the address once a tick, as between requests.
        let moved = async {
            loop {
                tokio::time::sleep(TICK).await;

                if self.target_moved() {
                    return;
                }
            }
        };

        tokio::select! {
            receipt = self.request_batch(batch) => receipt,
            () = self.inner.stop.cancelled() => aborted(false),
            () = self.inner.upstream_closed.cancelled() => aborted(false),
            () = moved => aborted(true),
        }
    }

    /// A 409 means the controller could not read this batch, so only its server backs off.
    async fn request_batch(&self, batch: &EventBatch) -> Result<EventBatchReceipt, UploadError> {
        let body = serialize_json(s_event_batch, batch)?;
        let target = lock(&self.inner.state).target.clone();
        let url = format!("{}{}", controller_origin(&target.host, target.tls, OriginKind::Http), target.events_path);
        let headers = [
            ("authorization", node_authorization(&self.inner.token)),
            ("content-type", "application/json".to_owned()),
            (PROTOCOL_HEADER, PROTOCOL_VERSION.to_string()),
        ];
        let PostAnswer { status, body } = self.inner.post.post(&url, &headers, body).await?;

        if status == StatusCode::CONFLICT {
            // A proxy's own 409 page is not the controller's rejection, and reads as no detail.
            let detail =
                parse_json::<EventBatchRejection>(s_event_batch_rejection, &body).map_or_else(|_| format!("HTTP {status}"), |rejection| rejection.detail);

            return Err(UploadError::Refused(detail));
        }

        if !is_success(status) {
            return Err(UploadError::Status(status));
        }

        Ok(parse_json(s_event_batch_receipt, &body)?)
    }

    /// Reports only changes, so the controller's figures follow the backlog's growth and its end.
    async fn report_backlog(&self) {
        match self.backlog().await {
            Ok(backlog) => {
                let changed = {
                    let mut state = lock(&self.inner.state);

                    if self.halted() || backlog == state.last_backlog {
                        false
                    } else {
                        state.last_backlog = backlog.clone();
                        true
                    }
                };

                if changed {
                    (self.inner.on_backlog)(backlog);
                }
            }
            Err(error) => self.inner.log.warn(format!("event backlog count failed: {error}")),
        }
    }

    /// A server whose plugin is attached keeps its stream, because the plugin can still replay into
    /// it. The boot pass runs before the relay serves, so no plugin is attached yet.
    async fn prune(&self) {
        match self.inner.spool.prune((self.inner.attached)()).await {
            Ok(pruned) if !pruned.is_empty() => {
                let lost: Vec<String> =
                    pruned.iter().filter(|server| server.pending > 0).map(|server| format!("{} ({})", server.allocation_id, server.pending)).collect();

                if lost.is_empty() {
                    self.inner.log.info(format!("spool retention dropped {} stream(s)", pruned.len()));
                } else {
                    self.inner.log.warn(format!(
                        "spool retention dropped {} stream(s), with events that reached no controller within the retention period: {}",
                        pruned.len(),
                        lost.join(", ")
                    ));
                }
            }
            Ok(_) => {}
            Err(error) => self.inner.log.warn(format!("spool retention failed: {error}")),
        }
    }
}
