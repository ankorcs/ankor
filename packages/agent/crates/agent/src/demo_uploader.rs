//! Uploads the demos games leave in their slots to the node's demo target, one at a time and paced
//! while games run, and lists and deletes them for the controller. The demos on disk are the queue, oldest
//! recording first. A failure holds all of them, because whatever refuses one demo's connection
//! refuses the next. Passes run on every scan trigger and return while the node-wide backoff lasts,
//! so nothing sleeps.

use crate::sync::{SharedRun, join_or_spawn, lock};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ankor_protocol::config::DemoUpload;
use ankor_protocol::constants::{
    DEMO_NAME, DEMO_OUTAGE_LIMIT, DEMO_PROGRESS_INTERVAL, DEMO_RETRY_MAX, DEMO_RETRY_MIN, DEMO_STALL_TIMEOUT, DEMO_UPLOAD_ERROR_MAX_CHARS, PROTOCOL_HEADER,
    PROTOCOL_VERSION, demo_headers,
};
use ankor_protocol::schemas::{s_allocation_id, s_demo_file_name, s_slot_id};
use ankor_protocol::types::{AgentDemoProgress, AgentErrorCode, DemoFile};
use ankor_protocol::validate::safe_parse;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt as _, Stream, StreamExt as _};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::contract::constants::DEMO_READY_SUFFIX;
use crate::failure::{AgentFailure, OpError, truncate};
use crate::host_layout::{slot_demo_dir, slots_dir};
use crate::http_body::is_success;
use crate::logger::Logger;
use crate::node_log::{Clock, wall_clock};
use crate::upload_socket::UploadSocketError;

/// A paced upload writes about this long's worth of its allowance at a time: each write leaves the
/// network card at full speed, and a short one clears the uplink's queue before a game packet
/// waits noticeably behind it.
const PACE_SLICE_MS: f64 = 5.0;

const MIN_SLICE_BYTES: usize = 1500;

const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Why an upload got no answer.
#[derive(Debug, thiserror::Error)]
pub enum DemoUploadError {
    /// Aborted, for the reason the stall timer gave.
    #[error("{0}")]
    Aborted(String),
    /// The demo could not be opened or read.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The connection to the target failed.
    #[error(transparent)]
    Socket(#[from] UploadSocketError),
}

/// The stall timer aborts an upload through it, with a reason.
#[derive(Clone, Default)]
pub struct AbortSignal {
    token: CancellationToken,
    reason: Arc<Mutex<Option<String>>>,
}

impl AbortSignal {
    pub fn abort(&self, reason: &str) {
        lock(&self.reason).get_or_insert_with(|| reason.to_owned());
        self.token.cancel();
    }

    #[must_use]
    pub fn aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Why it was aborted; empty while it was not.
    #[must_use]
    pub fn reason(&self) -> String {
        lock(&self.reason).clone().unwrap_or_default()
    }

    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

/// Bytes a second the upload may send now, read before each write; `None` for no limit.
pub(crate) type Limit = Arc<dyn Fn() -> Option<f64> + Send + Sync>;

/// Called with the total read from the file so far.
pub type OnSent = Arc<dyn Fn(u64) + Send + Sync>;

pub struct DemoRequest {
    /// The demo target's URL.
    pub url: String,
    /// In the order they are sent.
    pub headers: Vec<(String, String)>,
    /// The demo to upload.
    pub file_path: PathBuf,
    /// The demo's size when listed, sent as `content-length`.
    pub size_bytes: u64,
    /// Aborts the upload once it stalls.
    pub signal: AbortSignal,
    /// Told of progress, which also resets the stall timer.
    pub on_sent: OnSent,
    /// The upload's current rate limit.
    pub limit: Limit,
}

/// The upload of one demo, so tests can answer it.
#[async_trait]
pub trait DemoSend: Send + Sync {
    /// The target's status; `Err` is an upload that got no answer, and why.
    async fn send(&self, request: DemoRequest) -> Result<u16, DemoUploadError>;
}

pub trait PaceClock: Send + Sync {
    /// Milliseconds from any fixed point.
    fn now(&self) -> f64;
    /// `Err` when the signal aborts the wait.
    fn sleep(&self, ms: f64, signal: &AbortSignal) -> BoxFuture<'static, Result<(), DemoUploadError>>;
}

/// tokio's clock.
pub(crate) struct SystemPaceClock(Instant);

impl Default for SystemPaceClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl PaceClock for SystemPaceClock {
    fn now(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }

    fn sleep(&self, ms: f64, signal: &AbortSignal) -> BoxFuture<'static, Result<(), DemoUploadError>> {
        let signal = signal.clone();
        let wait = Duration::try_from_secs_f64(ms / 1000.0).unwrap_or_default();

        async move {
            tokio::select! {
                () = tokio::time::sleep(wait) => Ok(()),
                () = signal.cancelled() => Err(DemoUploadError::Aborted(signal.reason())),
            }
        }
        .boxed()
    }
}

struct Pace<S> {
    source: S,
    rest: Bytes,
    rate: Option<f64>,
    due: f64,
    limit: Limit,
    signal: AbortSignal,
    clock: Arc<dyn PaceClock>,
}

/// The file's bytes at no more than `limit()` a second, in slices of `PACE_SLICE_MS` at that
/// rate. The limit is read per slice, so a game starting or ending changes it mid-upload; without
/// one, chunks pass through whole.
pub fn paced<S>(source: S, limit: Limit, signal: AbortSignal, clock: Arc<dyn PaceClock>) -> impl Stream<Item = io::Result<Bytes>> + Send
where
    S: Stream<Item = io::Result<Bytes>> + Send + Unpin,
{
    let pace = Pace { source, rest: Bytes::new(), rate: None, due: 0.0, limit, signal, clock };

    futures_util::stream::unfold(pace, |mut pace| async move {
        loop {
            if pace.rest.is_empty() {
                match pace.source.next().await? {
                    Ok(chunk) => pace.rest = chunk,
                    Err(error) => return Some((Err(error), pace)),
                }

                continue;
            }

            let current = (pace.limit)();

            if current != pace.rate {
                pace.rate = current;
                pace.due = pace.clock.now();
            }

            let Some(rate) = pace.rate else {
                let all = std::mem::take(&mut pace.rest);

                return Some((Ok(all), pace));
            };
            let length = slice_length(rate).min(pace.rest.len());
            let now = pace.clock.now();

            if pace.due > now
                && let Err(reason) = pace.clock.sleep(pace.due - now, &pace.signal).await
            {
                return Some((Err(io::Error::other(reason)), pace));
            }

            #[allow(clippy::cast_precision_loss, reason = "a slice is a few megabytes at most")]
            let spent = length as f64 / rate * 1000.0;

            // A late wake-up is made up for by one slice at most; more would go out as one long burst.
            pace.due = pace.due.max(pace.clock.now() - PACE_SLICE_MS) + spent;

            let slice = pace.rest.split_to(length);

            return Some((Ok(slice), pace));
        }
    })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a rate in bytes a second, so a slice of a few megabytes at most")]
fn slice_length(rate: f64) -> usize {
    MIN_SLICE_BYTES.max((rate * PACE_SLICE_MS / 1000.0).round() as usize)
}

/// Uploads over HTTP or HTTPS, as the target's URL says, without following redirects.
pub(crate) struct HttpDemoSend;

impl HttpDemoSend {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for HttpDemoSend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DemoSend for HttpDemoSend {
    async fn send(&self, request: DemoRequest) -> Result<u16, DemoUploadError> {
        let file = open_regular(&request.file_path).await?;
        // The target may answer while the body is still going out; that answer is the result, and
        // the rest of the body stays unsent.
        let answered = CancellationToken::new();
        let mut sent = 0u64;
        let on_sent = Arc::clone(&request.on_sent);
        let read = tokio_util::io::ReaderStream::with_capacity(file, READ_CHUNK_BYTES);
        let body = paced(read, Arc::clone(&request.limit), request.signal.clone(), Arc::new(SystemPaceClock::default()))
            .map(move |chunk| {
                if let Ok(chunk) = &chunk {
                    sent += chunk.len() as u64;
                    on_sent(sent);
                }

                chunk
            })
            .take_until(answered.clone().cancelled_owned());
        let mut headers: Vec<(String, String)> = request.headers.iter().map(|(name, value)| (name.clone(), value.clone())).collect();

        headers.push(("content-length".to_owned(), request.size_bytes.to_string()));

        let call = crate::upload_socket::post(&request.url, &headers, body);
        let status = tokio::select! {
            status = call => status?,
            () = request.signal.cancelled() => return Err(DemoUploadError::Aborted(request.signal.reason())),
        };

        answered.cancel();

        Ok(status)
    }
}

pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();

    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }

    message
}

/// The demo target now, `None` while there is none; a configuration save changes it.
pub(crate) type GetTarget = Arc<dyn Fn() -> Option<DemoUpload> + Send + Sync>;
/// The allocation whose game holds the slot, `None` while it is idle.
pub(crate) type RunningAllocation = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
pub(crate) type GamesRunning = Arc<dyn Fn() -> bool + Send + Sync>;
/// Given the uploader, so whoever reports the listing can read it.
pub(crate) type OnChange = Arc<dyn Fn(&DemoUploader) + Send + Sync>;
pub(crate) type OnProgress = Arc<dyn Fn(AgentDemoProgress) + Send + Sync>;
/// The slot and the file name of a demo the target took.
pub(crate) type OnUploaded = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct DemoUploaderOptions {
    /// The agent's data directory, which holds the slots and their demos.
    pub data_dir: PathBuf,
    /// The demo target now.
    pub get_target: GetTarget,
    /// The allocation whose game holds the slot. A demo stops counting as recording once its slot goes idle.
    pub running_allocation: RunningAllocation,
    /// Whether any slot holds a game, which is when the target's upload limit applies.
    pub games_running: GamesRunning,
    /// Receives upload progress and failures.
    pub log: Logger,
    /// Uploads one demo.
    pub send: Arc<dyn DemoSend>,
    /// The wall clock for backoff and progress rates.
    pub now: Clock,
    /// The listing may differ: a scan ran, a demo was removed or uploaded, or an upload started or ended.
    pub on_change: Option<OnChange>,
    /// How far the upload is, at most once a second while it runs.
    pub on_progress: Option<OnProgress>,
    /// The target took a demo, told before the agent removes its file.
    pub on_uploaded: Option<OnUploaded>,
    /// The agent could not remove a demo it told of through `on_uploaded`, so its file stays.
    pub on_not_removed: Option<OnUploaded>,
}

impl DemoUploaderOptions {
    /// Real uploads on the system clock, logging nowhere and telling no one.
    #[must_use]
    pub fn new(data_dir: PathBuf, get_target: GetTarget, running_allocation: RunningAllocation, games_running: GamesRunning) -> Self {
        Self {
            data_dir,
            get_target,
            running_allocation,
            games_running,
            log: Logger::null(),
            send: Arc::new(HttpDemoSend::new()),
            now: wall_clock(),
            on_change: None,
            on_progress: None,
            on_uploaded: None,
            on_not_removed: None,
        }
    }
}

#[derive(Clone)]
struct FoundDemo {
    /// The slot that recorded the demo.
    slot_id: String,
    /// Where the demo is on disk.
    file_path: PathBuf,
    /// The demo's name, which starts with its recording time.
    file_name: String,
    /// The match id from the demo's name, `None` when the name carries none.
    match_id: Option<String>,
    /// The server that recorded it, from its launch's directory.
    allocation_id: String,
}

/// One launch's demo directory, named by its allocation.
struct DemoDir {
    slot_id: String,
    dir: PathBuf,
    allocation_id: String,
    names: Vec<String>,
}

/// How an upload attempt ended.
enum Outcome {
    /// Done with the demo: uploaded, gone, or refused as invalid.
    Done,
    /// The target refused this node.
    Refused(String),
    /// Why the upload needs a retry.
    Retry(String),
}

/// A pass that failed to read the demo directories.
pub(crate) type ScanError = Arc<io::Error>;

type Pass = SharedRun<Result<(), ScanError>>;

#[derive(Default)]
struct State {
    /// Demos already warned about for a name that does not match `DEMO_NAME`.
    unnamed: HashSet<PathBuf>,
    /// Demos refused as invalid by the current target, or uploaded but not removable. A new target
    /// starts it over.
    left: HashSet<PathBuf>,
    /// The demos waiting when the target refused this node or had failed for a day. Any other demo,
    /// or a new target, retries them all.
    outage: Option<HashSet<PathBuf>>,
    /// The demo uploading now.
    current: Option<PathBuf>,
    /// Demos a `delete_demo` is removing, which no upload starts on.
    deleting: HashSet<PathBuf>,
    /// Why each demo's upload failed or why it waits, by file; cleared once an upload works or the
    /// target changes.
    errors: HashMap<PathBuf, String>,
    /// The pass that runs now, which [`DemoUploader::scan_all`] joins.
    draining: Option<Pass>,
    /// The key of the target that the state describes. A change resets the failures.
    target: Option<String>,
    /// The current retry delay, doubled per failure.
    backoff: Duration,
    /// Milliseconds since the epoch before which no pass uploads.
    retry_at: i64,
    /// When uploads started failing, in milliseconds since the epoch.
    failing_since: Option<i64>,
}

impl State {
    /// Uploads work again, or the target changed: no demo waits on a failure any more, but one
    /// refused as invalid stays refused.
    fn recover(&mut self) {
        self.backoff = Duration::ZERO;
        self.retry_at = 0;
        self.failing_since = None;
        self.outage = None;

        let left = &self.left;

        self.errors.retain(|file, _| left.contains(file));
    }

    fn hold_all(&mut self, pending: &[FoundDemo], reason: &str) {
        for waiting in pending {
            self.errors.insert(waiting.file_path.clone(), truncate(reason, DEMO_UPLOAD_ERROR_MAX_CHARS));
        }
    }
}

struct Inner {
    options: DemoUploaderOptions,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct DemoUploader {
    inner: Arc<Inner>,
}

fn target_key(target: Option<&DemoUpload>) -> String {
    serde_json::to_string(&target).unwrap_or_default()
}

fn valid(schema: fn(&mut ankor_protocol::validate::Payload), text: &str) -> bool {
    safe_parse(schema, Value::String(text.to_owned())).is_ok()
}

/// A directory's entries, as names and their types; `None` once it is gone, which is all discovery
/// expects to go wrong. A type is the entry's own, so a symlink reads as one, not as its target.
async fn read_entries(dir: &Path) -> io::Result<Option<Vec<(String, std::fs::FileType)>>> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut found = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let kind = entry.file_type().await?;

        found.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }

    Ok(Some(found))
}

/// Opens a demo for reading without following a symlink, and only when it is a regular file. The
/// game can write the demo's directory, so it could swap the file for a link after it was listed.
///
/// # Errors
///
/// Fails on a link, on anything other than a regular file, and as the file system does.
pub(crate) async fn open_regular(path: &Path) -> io::Result<tokio::fs::File> {
    let file = tokio::fs::OpenOptions::new().read(true).custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits()).open(path).await?;

    if !file.metadata().await?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a regular file", path.display())));
    }

    Ok(file)
}

/// Removes a file; one already gone counts as removed.
async fn remove_file(file: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(file).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The marker the plugin creates beside `demo`, a file name or a path, once the recording is finished.
fn ready_marker(demo: impl AsRef<OsStr>) -> PathBuf {
    let mut marker = demo.as_ref().to_owned();

    marker.push(DEMO_READY_SUFFIX);
    PathBuf::from(marker)
}

impl DemoUploader {
    #[must_use]
    pub fn new(options: DemoUploaderOptions) -> Self {
        Self { inner: Arc::new(Inner { options, state: Mutex::default() }) }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.inner.state)
    }

    fn options(&self) -> &DemoUploaderOptions {
        &self.inner.options
    }

    fn now(&self) -> i64 {
        (self.options().now)()
    }

    fn changed(&self) {
        if let Some(on_change) = &self.options().on_change {
            on_change(self);
        }
    }

    /// Runs a pass, or joins the one that runs.
    ///
    /// # Errors
    ///
    /// Fails when a slot's demo directories cannot be read.
    pub fn scan_all(&self) -> impl Future<Output = Result<(), ScanError>> + Send + 'static {
        join_or_spawn(
            &self.inner.state,
            |state| &mut state.draining,
            || {
                let this = self.clone();

                async move {
                    let result = this.upload().await.map_err(Arc::new);

                    this.state().draining = None;
                    this.changed();
                    result
                }
                .boxed()
            },
        )
    }

    /// Every demo on the node whose slot and file name the wire carries.
    ///
    /// # Errors
    ///
    /// Fails when a directory or a demo cannot be read, other than being gone.
    pub async fn list(&self) -> io::Result<Vec<DemoFile>> {
        let mut demos = Vec::new();

        for demo_dir in self.demo_dirs().await? {
            if !valid(s_slot_id, &demo_dir.slot_id) {
                continue;
            }

            for file_name in &demo_dir.names {
                if !valid(s_demo_file_name, file_name) {
                    continue;
                }

                let file_path = demo_dir.dir.join(file_name);
                let info = match tokio::fs::symlink_metadata(&file_path).await {
                    Ok(info) if info.is_file() => info,
                    // Replaced by something other than a file since the listing.
                    Ok(_) => continue,
                    // Uploaded and unlinked between the listing and the stat.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                let recording = self.recording(&demo_dir, file_name);
                let state = self.state();

                demos.push(DemoFile {
                    slot_id: demo_dir.slot_id.clone(),
                    file_name: file_name.clone(),
                    size_bytes: i64::try_from(info.len()).unwrap_or(i64::MAX),
                    modified_at: modified_ms(&info),
                    recording,
                    uploading: state.current.as_ref() == Some(&file_path),
                    upload_error: state.errors.get(&file_path).cloned(),
                });
            }
        }

        Ok(demos)
    }

    /// # Errors
    ///
    /// Refuses an unknown slot, a demo the slot does not have, a demo uploading now and a demo still
    /// recording; fails when the directories cannot be read or the demo cannot be removed.
    pub async fn remove(&self, slot_id: &str, file_name: &str) -> Result<(), OpError> {
        if !self.slot_ids().await?.iter().any(|slot| slot == slot_id) {
            return Err(AgentFailure::new(AgentErrorCode::UnknownSlot, format!("unknown slot {slot_id}")).into());
        }

        let unknown = || AgentFailure::new(AgentErrorCode::UnknownDemo, format!("slot {slot_id} has no demo {file_name}"));
        let holder = self.demo_dirs().await?.into_iter().find(|dir| dir.slot_id == slot_id && dir.names.iter().any(|name| name == file_name));

        if let Some(holder) = holder {
            let file_path = holder.dir.join(file_name);

            if self.recording(&holder, file_name) {
                return Err(AgentFailure::new(AgentErrorCode::DemoRecording, format!("{file_name} is still recording")).into());
            }

            // Checked and marked in one hold of the lock, so an upload either holds the demo now or
            // never starts on it, and no upload fails on a file removed under it.
            {
                let mut state = self.state();

                if state.current.as_ref() == Some(&file_path) {
                    return Err(AgentFailure::new(AgentErrorCode::DemoUploading, format!("{file_name} is uploading")).into());
                }

                state.deleting.insert(file_path.clone());
            }

            // Gone since the directory read, as an upload that finished meanwhile removes it.
            let removed = async {
                match tokio::fs::remove_file(&file_path).await {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error),
                    Ok(()) => {}
                }

                // The demo is gone, so the delete holds; a marker left behind costs nothing.
                if let Err(error) = remove_file(&ready_marker(&file_path)).await {
                    self.options().log.warn(format!("{file_name} was deleted but its ready marker was not removed: {error}"));
                }

                Ok(true)
            }
            .await;
            let removed = {
                let mut state = self.state();

                state.deleting.remove(&file_path);

                let removed = removed?;

                state.errors.remove(&file_path);
                removed
            };

            if !removed {
                return Err(unknown().into());
            }
        } else {
            return Err(unknown().into());
        }

        self.changed();

        Ok(())
    }

    async fn slot_ids(&self) -> io::Result<Vec<String>> {
        Ok(read_entries(&slots_dir(&self.options().data_dir))
            .await?
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, kind)| kind.is_dir())
            .map(|(name, _)| name)
            .collect())
    }

    /// Whether the game holding the slot may still write the demo: it is that launch's, and not
    /// marked finished.
    fn recording(&self, demo_dir: &DemoDir, file_name: &str) -> bool {
        let ready = ready_marker(file_name);

        (self.options().running_allocation)(&demo_dir.slot_id).as_deref() == Some(demo_dir.allocation_id.as_str())
            && !demo_dir.names.iter().any(|name| Path::new(name) == ready)
    }

    /// Each launch's demo directory in each slot, with its entries. Anything else in `demos/` is left alone.
    async fn demo_dirs(&self) -> io::Result<Vec<DemoDir>> {
        let mut dirs = Vec::new();

        for slot_id in self.slot_ids().await? {
            let root = slot_demo_dir(&self.options().data_dir, &slot_id);
            let Some(entries) = read_entries(&root).await? else {
                continue;
            };

            for (name, kind) in entries {
                if !kind.is_dir() || !valid(s_allocation_id, &name) {
                    continue;
                }

                let dir = root.join(&name);

                // Only regular files: the game can write here, and a link it leaves could name any
                // file the agent can read.
                if let Some(names) = read_entries(&dir).await? {
                    let names = names.into_iter().filter(|(_, kind)| kind.is_file()).map(|(name, _)| name).collect();

                    dirs.push(DemoDir { slot_id: slot_id.clone(), dir, allocation_id: name, names });
                }
            }
        }

        Ok(dirs)
    }

    /// Every demo a finished recording left, oldest first.
    async fn pending(&self) -> io::Result<Vec<FoundDemo>> {
        let mut found = Vec::new();
        let mut present = HashSet::new();

        for demo_dir in self.demo_dirs().await? {
            for name in &demo_dir.names {
                let file_path = demo_dir.dir.join(name);

                present.insert(file_path.clone());

                #[allow(clippy::case_sensitive_file_extension_comparisons, reason = "the engine names demos in lower case, and only those are uploaded")]
                // One a delete is removing would come straight back each pass until the file is gone.
                if !name.ends_with(".dem")
                    || self.state().left.contains(&file_path)
                    || self.state().deleting.contains(&file_path)
                    || self.recording(&demo_dir, name)
                {
                    continue;
                }

                let Some(parsed) = DEMO_NAME.captures(name).ok().flatten() else {
                    if self.state().unnamed.insert(file_path.clone()) {
                        self.options().log.warn(format!("{} is not named <yyyyMMdd-HHmmss>_<matchId>_<map>.dem; leaving it here", file_path.display()));
                    }

                    continue;
                };
                let match_id = parsed.get(2).map(|id| id.as_str().to_owned()).filter(|id| !id.is_empty());

                found.push(FoundDemo {
                    slot_id: demo_dir.slot_id.clone(),
                    file_path,
                    file_name: name.clone(),
                    match_id,
                    allocation_id: demo_dir.allocation_id.clone(),
                });
            }
        }

        // What the node no longer holds is forgotten, so these sets do not grow over its uptime.
        {
            let mut state = self.state();

            state.unnamed.retain(|file| present.contains(file));
            state.left.retain(|file| present.contains(file));
        }

        // Names start with the recording time.
        found.sort_by(|a, b| a.file_name.cmp(&b.file_name));

        Ok(found)
    }

    /// One upload at a time per node: a demo is hundreds of megabytes on a host also running games.
    async fn upload(&self) -> io::Result<()> {
        loop {
            // A configuration save affects the next request; an active upload keeps its target.
            let target = (self.options().get_target)();
            let key = target_key(target.as_ref());

            {
                let mut state = self.state();

                if state.target.as_ref() != Some(&key) {
                    state.target = Some(key.clone());
                    state.left.clear();
                    state.recover();
                }

                if target.is_none() || self.now() < state.retry_at {
                    return Ok(());
                }
            }

            let Some(target) = target else {
                return Ok(());
            };
            let pending = self.pending().await?;

            if key != target_key((self.options().get_target)().as_ref()) {
                continue;
            }

            {
                let mut state = self.state();

                if let Some(outage) = &state.outage
                    && pending.iter().all(|demo| outage.contains(&demo.file_path))
                {
                    return Ok(());
                }

                if pending.is_empty() {
                    return Ok(());
                }

                state.outage = None;
            }

            let Some(demo) = pending.first() else {
                return Ok(());
            };

            match self.attempt(&target, demo).await {
                // A refused token or secret is refused for every demo; asking per demo only piles up refusals.
                Outcome::Refused(refused) => {
                    let mut state = self.state();

                    state.outage = Some(pending.iter().map(|waiting| waiting.file_path.clone()).collect());
                    state.hold_all(&pending, &format!("{refused}; every demo waits until another is recorded or the target changes"));

                    return Ok(());
                }
                Outcome::Retry(outcome) => {
                    self.failed(demo, &outcome, &pending);

                    if key == target_key((self.options().get_target)().as_ref()) {
                        return Ok(());
                    }
                }
                Outcome::Done => {}
            }
        }
    }

    async fn attempt(&self, target: &DemoUpload, demo: &FoundDemo) -> Outcome {
        let size_bytes = match tokio::fs::symlink_metadata(&demo.file_path).await {
            Ok(info) if info.is_file() => info.len(),
            // Replaced by something other than a file since the listing, which is never uploaded.
            Ok(_) => return Outcome::Done,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Outcome::Done,
            Err(error) => return Outcome::Retry(error.to_string()),
        };
        let stall = Arc::new(Mutex::new(Instant::now() + DEMO_STALL_TIMEOUT));
        let signal = AbortSignal::default();
        let watchdog = {
            let stall = Arc::clone(&stall);
            let signal = signal.clone();

            tokio::spawn(async move {
                loop {
                    let deadline = *lock(&stall);

                    tokio::time::sleep_until(deadline).await;

                    if Instant::now() >= *lock(&stall) {
                        signal.abort(&format!("no progress for {} s", DEMO_STALL_TIMEOUT.as_secs()));

                        return;
                    }
                }
            })
        };

        {
            let mut state = self.state();

            // A demo a delete is removing is gone, not failed.
            if state.deleting.contains(&demo.file_path) {
                drop(state);
                watchdog.abort();

                return Outcome::Done;
            }

            state.current = Some(demo.file_path.clone());
        }

        self.changed();

        let outcome = self.send(target, demo, size_bytes, signal, stall).await;

        watchdog.abort();

        // A demo the target took counts as uploading until the upload has removed it, so a delete_demo is
        // refused until then and only the upload can remove it. The reporter hears of it first.
        if outcome.as_ref().is_ok_and(|status| is_success(*status)) {
            if let Some(on_uploaded) = &self.options().on_uploaded {
                on_uploaded(&demo.slot_id, &demo.file_name);
            }
        } else {
            self.state().current = None;
            self.changed();
        }

        let status = match outcome {
            Ok(status) => status,
            // Gone since the listing, as a delete removes it, so nothing failed.
            Err(DemoUploadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Outcome::Done,
            Err(error) => return Outcome::Retry(error.to_string()),
        };

        let refusals = [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN, StatusCode::UNPROCESSABLE_ENTITY];

        if !is_success(status) && !refusals.iter().any(|refusal| status == *refusal) {
            return Outcome::Retry(format!("HTTP {status}"));
        }

        self.state().recover();

        let log = &self.options().log;

        if status == StatusCode::UNPROCESSABLE_ENTITY {
            let mut state = self.state();

            state.left.insert(demo.file_path.clone());
            state.errors.insert(demo.file_path.clone(), format!("refused as invalid with HTTP {status}; kept until the target changes"));
            drop(state);
            log.warn(format!(
                "{} refused {} as invalid with HTTP {status}; skipping it until the target changes or the agent restarts",
                target.url, demo.file_name
            ));

            return Outcome::Done;
        }

        if is_success(status) {
            // A file already gone needs nothing more: no delete_demo can remove it while it counts as uploading.
            let kept = tokio::fs::remove_file(&demo.file_path).await.err().filter(|error| error.kind() != io::ErrorKind::NotFound);

            // The reporter forgets it before a delete_demo can remove it, which goes out as `deleted`.
            if kept.is_some()
                && let Some(on_not_removed) = &self.options().on_not_removed
            {
                on_not_removed(&demo.slot_id, &demo.file_name);
            }

            {
                let mut state = self.state();

                state.current = None;

                if kept.is_some() {
                    state.left.insert(demo.file_path.clone());
                }
            }

            self.changed();

            if let Some(error) = kept {
                log.error(format!("{} was uploaded but could not be removed: {error}", demo.file_name));
            }

            // The upload holds; a marker left behind costs nothing, and a retry would hold back
            // every other demo for one file.
            if let Err(error) = remove_file(&ready_marker(&demo.file_path)).await {
                log.warn(format!("{} was uploaded but its ready marker was not removed: {error}", demo.file_name));
            }

            log.info(format!("uploaded {} ({size_bytes} bytes)", demo.file_name));

            return Outcome::Done;
        }

        log.warn(format!(
            "{} refused {} with HTTP {status}; keeping every demo on the node until another is recorded or the target changes",
            target.url, demo.file_name
        ));

        Outcome::Refused(format!("refused with HTTP {status}"))
    }

    async fn send(
        &self,
        target: &DemoUpload,
        demo: &FoundDemo,
        size_bytes: u64,
        signal: AbortSignal,
        stall: Arc<Mutex<Instant>>,
    ) -> Result<u16, DemoUploadError> {
        let limit_mbit = target.limit_mbit;
        let games_running = Arc::clone(&self.options().games_running);
        #[allow(clippy::cast_precision_loss, reason = "a limit in megabits a second")]
        let limit: Limit = Arc::new(move || limit_mbit.filter(|_| games_running()).map(|mbit| mbit as f64 * 125_000.0));
        let reported = Arc::new(Mutex::new((self.now(), 0u64)));
        let now = Arc::clone(&self.options().now);
        let on_progress = self.options().on_progress.clone();
        let (slot_id, file_name) = (demo.slot_id.clone(), demo.file_name.clone());
        let on_sent: OnSent = Arc::new(move |sent_bytes| {
            *lock(&stall) = Instant::now() + DEMO_STALL_TIMEOUT;

            let now = now();
            let mut reported = lock(&reported);
            let (at, before) = *reported;
            let elapsed = now - at;

            if elapsed >= i64::try_from(DEMO_PROGRESS_INTERVAL.as_millis()).unwrap_or(i64::MAX) {
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, reason = "a byte rate")]
                let bytes_per_second = ((sent_bytes as f64 - before as f64) * 1000.0 / elapsed as f64).round() as i64;

                *reported = (now, sent_bytes);
                drop(reported);

                if let Some(on_progress) = &on_progress {
                    on_progress(AgentDemoProgress {
                        slot_id: slot_id.clone(),
                        file_name: file_name.clone(),
                        sent_bytes: i64::try_from(sent_bytes).unwrap_or(i64::MAX),
                        bytes_per_second,
                    });
                }
            }
        });
        let request =
            DemoRequest { url: target.url.clone(), headers: headers(target, demo), file_path: demo.file_path.clone(), size_bytes, signal, on_sent, limit };

        self.options().send.send(request).await
    }

    fn failed(&self, demo: &FoundDemo, outcome: &str, pending: &[FoundDemo]) {
        let now = self.now();
        let log = &self.options().log;
        let mut state = self.state();
        let failing_since = *state.failing_since.get_or_insert(now);

        if now - failing_since >= i64::try_from(DEMO_OUTAGE_LIMIT.as_millis()).unwrap_or(i64::MAX) {
            log.error(format!(
                "demo uploads have failed for {} h ({outcome}); keeping {} demo(s) on the node until another is recorded or the target changes",
                DEMO_OUTAGE_LIMIT.as_secs() / 3600,
                pending.len()
            ));
            state.recover();
            state.outage = Some(pending.iter().map(|waiting| waiting.file_path.clone()).collect());
            state.hold_all(
                pending,
                &format!(
                    "{outcome}; uploads have failed for {} h, so every demo waits until another is recorded or the target changes",
                    DEMO_OUTAGE_LIMIT.as_secs() / 3600
                ),
            );

            return;
        }

        state.backoff = if state.backoff.is_zero() { DEMO_RETRY_MIN } else { (state.backoff * 2).min(DEMO_RETRY_MAX) };
        state.retry_at = now.saturating_add(i64::try_from(state.backoff.as_millis()).unwrap_or(i64::MAX));

        let seconds = state.backoff.as_secs();

        state.errors.insert(demo.file_path.clone(), truncate(&format!("{outcome}; retrying in {seconds} s"), DEMO_UPLOAD_ERROR_MAX_CHARS));
        drop(state);
        log.warn(format!("{} upload failed ({outcome}); retrying in {seconds}s", demo.file_name));
    }
}

/// The modification time in whole milliseconds, rounded.
fn modified_ms(info: &std::fs::Metadata) -> i64 {
    info.modified()
        .ok()
        .and_then(|at| jiff::Timestamp::try_from(at).ok())
        .and_then(|at| i64::try_from((at.as_nanosecond() + 500_000).div_euclid(1_000_000)).ok())
        .unwrap_or(0)
}

/// The target's header first and the agent's own after it: a later name wins, so the agent's are
/// the ones sent, also for a header the contract would have refused.
fn headers(target: &DemoUpload, demo: &FoundDemo) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = target.header.iter().map(|header| (header.name.clone(), header.value.clone())).collect();

    headers.extend([
        ("content-type".to_owned(), "application/octet-stream".to_owned()),
        (PROTOCOL_HEADER.to_owned(), PROTOCOL_VERSION.to_string()),
        (demo_headers::FILE_NAME.to_owned(), demo.file_name.clone()),
    ]);

    if let Some(match_id) = &demo.match_id {
        headers.push((demo_headers::MATCH_ID.to_owned(), match_id.clone()));
    }

    headers.push((demo_headers::ALLOCATION_ID.to_owned(), demo.allocation_id.clone()));
    headers
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test fails by crashing")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_regular_refuses_a_symlink_and_opens_a_file() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("demo.dem");
        let link = root.path().join("link.dem");

        std::fs::write(&file, b"gotv").unwrap();
        std::os::unix::fs::symlink(&file, &link).unwrap();

        assert_eq!(open_regular(&link).await.unwrap_err().raw_os_error(), Some(nix::errno::Errno::ELOOP as i32));
        assert!(open_regular(root.path()).await.is_err(), "a directory is not a demo");
        assert!(open_regular(&file).await.is_ok());
    }

    /// The reason reaches the controller as the upload's error, not a generic abort.
    #[tokio::test]
    async fn an_upload_aborted_while_it_waits_for_an_answer_fails_with_the_abort_reason() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("demo.dem");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Takes the connection and never answers, as a stalled target does.
        let silent = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();

            std::future::pending::<()>().await;
        });
        let signal = AbortSignal::default();

        std::fs::write(&file, b"gotv").unwrap();

        let request = DemoRequest {
            url: format!("http://127.0.0.1:{port}/demos"),
            headers: Vec::new(),
            file_path: file,
            size_bytes: 4,
            signal: signal.clone(),
            on_sent: Arc::new(|_| {}),
            limit: Arc::new(|| None),
        };
        let sending = tokio::spawn(async move { HttpDemoSend::new().send(request).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.abort("no progress for 2 minutes");
        assert_eq!(sending.await.unwrap().unwrap_err().to_string(), "no progress for 2 minutes");
        silent.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_system_pace_clock_counts_and_sleeps_in_milliseconds() {
        let clock = SystemPaceClock::default();
        let started = Instant::now();

        tokio::time::advance(Duration::from_millis(1_500)).await;
        assert!((clock.now() - 1_500.0).abs() < f64::EPSILON, "{}", clock.now());

        clock.sleep(1_500.0, &AbortSignal::default()).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(3));
    }

    #[derive(Debug, thiserror::Error)]
    #[error("upload failed")]
    struct Outer(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("connection reset")]
    struct Inner;

    #[test]
    fn an_error_chain_lists_every_cause() {
        assert_eq!(error_chain(&Outer(Inner)), "upload failed: connection reset");
    }

    #[tokio::test]
    async fn removing_counts_a_missing_file_as_removed_and_fails_on_anything_else() {
        let dir = tempfile::tempdir().unwrap();

        remove_file(&dir.path().join("gone.dem")).await.unwrap();
        assert!(remove_file(dir.path()).await.is_err());
    }

    #[test]
    fn modification_times_round_to_the_nearest_millisecond() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dem");
        let file = std::fs::File::create(&path).unwrap();

        file.set_modified(std::time::UNIX_EPOCH + Duration::new(1_700_000_000, 123_600_000)).unwrap();
        drop(file);

        assert_eq!(modified_ms(&std::fs::metadata(&path).unwrap()), 1_700_000_000_124);
    }
}
