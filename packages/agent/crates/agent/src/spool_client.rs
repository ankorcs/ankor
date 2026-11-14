//! The event spool on a thread of its own, so its synchronous commits stay off the agent's
//! network and Docker tasks.

use crate::sync::lock;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use ankor_protocol::constants::NODE_UNAVAILABLE_DETAIL_MAX_CHARS;
use ankor_protocol::types::{NodeUnavailable, NodeUnavailableCode, ServerExit};
use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::event_spool::{EventSpool, SpoolAck, SpoolAppend, SpoolBacklog, SpoolError, SpoolFrame, SpoolStream};
use crate::failure::truncate;
use crate::logger::Logger;

const MAX_PENDING_REQUESTS: usize = 256;

/// How often [`SpoolWatch`] looks at the spool's thread.
pub const SPOOL_CHECK: Duration = Duration::from_secs(10);
/// How long one job may run before the spool counts as stuck. SQLite answers in milliseconds and a
/// prune of a large spool in seconds, so a job past it means the disk under the spool hangs.
pub const SPOOL_STUCK_LIMIT: Duration = Duration::from_secs(30);

type Result<T> = std::result::Result<T, SpoolError>;

/// The spool's operations, answered asynchronously, so components can run against a fake.
#[async_trait]
pub trait SpoolPort: Send + Sync {
    async fn version(&self) -> Result<u32>;
    async fn position(&self, allocation_id: String) -> Result<Option<SpoolAck>>;
    async fn append(&self, streams: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>>;
    async fn pending(&self, allocation_id: String) -> Result<Vec<SpoolFrame>>;
    async fn backlog(&self) -> Result<Vec<SpoolBacklog>>;
    async fn acknowledge(&self, allocation_id: String, to_seq: i64) -> Result<()>;
    /// Drops idle streams, and returns each with the events it held that never reached the controller.
    async fn prune(&self, active: Vec<String>) -> Result<Vec<SpoolBacklog>>;
    async fn record_exit(&self, exit: ServerExit) -> Result<()>;
    async fn exits(&self) -> Result<Vec<ServerExit>>;
    async fn forget_exits(&self, allocation_ids: Vec<String>) -> Result<()>;
    async fn allocation_used(&self, allocation_id: String) -> Result<bool>;
    async fn record_allocation(&self, allocation_id: String) -> Result<()>;
    /// Refuses new work, lets accepted work finish and closes the file.
    async fn close(&self) -> Result<()>;
}

type Job = Box<dyn FnOnce(Option<&mut EventSpool>) + Send>;

/// A handle to the spool's thread. The thread opens the file, so a file that cannot open fails
/// every call with the reason.
pub struct SpoolClient {
    /// Sends jobs to the thread; `None` once closing or when the thread never started.
    jobs: std::sync::Mutex<Option<std::sync::mpsc::Sender<Job>>>,
    /// Answered once the thread has closed the file, with how the close went.
    closed: std::sync::Mutex<Option<oneshot::Receiver<Result<()>>>>,
    /// Calls queued or running now.
    pending: Arc<AtomicUsize>,
    /// Calls queued or running at once, past which a call is refused as `Busy`.
    limit: usize,
    /// Why the thread could not start or the file could not open.
    failure: Arc<std::sync::Mutex<Option<String>>>,
    /// False when the system refused the thread, so every call fails with the reason.
    started: bool,
    /// When each unanswered call was queued, oldest first. It uses a monotonic clock, because the
    /// thread runs outside the runtime and tokio's clock cannot measure it.
    queued: Arc<std::sync::Mutex<VecDeque<std::time::Instant>>>,
}

/// A call queued to the thread, until it is answered or dropped unrun: it frees its place in the
/// count and the queue's clock on every path, a job the thread never took among them.
struct Outstanding {
    pending: Arc<AtomicUsize>,
    queued: Arc<std::sync::Mutex<VecDeque<std::time::Instant>>>,
}

impl Drop for Outstanding {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::SeqCst);
        lock(&self.queued).pop_front();
    }
}

impl SpoolClient {
    /// A thread the system refuses leaves a client whose every call fails with the reason.
    #[must_use]
    pub fn start(path: PathBuf, log: Logger) -> Self {
        Self::start_with_limit(path, log, MAX_PENDING_REQUESTS)
    }

    fn start_with_limit(path: PathBuf, log: Logger, limit: usize) -> Self {
        let (jobs, receiver) = std::sync::mpsc::channel::<Job>();
        let failure: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let failed = Arc::clone(&failure);
        let (close_result, closed) = oneshot::channel();

        let started = std::thread::Builder::new().name("event-spool".to_owned()).spawn(move || {
            let mut spool = match EventSpool::open(&path, &log) {
                Ok(spool) => Some(spool),
                Err(error) => {
                    *lock(&failed) = Some(error.to_string());
                    None
                }
            };

            while let Ok(job) = receiver.recv() {
                job(spool.as_mut());
            }

            let _ = close_result.send(spool.map_or(Ok(()), EventSpool::close));
        });

        if let Err(error) = &started {
            *lock(&failure) = Some(format!("the event spool thread did not start: {error}"));
        }

        Self {
            jobs: std::sync::Mutex::new(started.is_ok().then_some(jobs)),
            closed: std::sync::Mutex::new(Some(closed)),
            pending: Arc::default(),
            limit,
            failure,
            started: started.is_ok(),
            queued: Arc::default(),
        }
    }

    /// How long the oldest call still waiting for its answer has waited, in the queue and while it
    /// runs; `None` while no call waits.
    #[must_use]
    pub fn waiting_for(&self) -> Option<Duration> {
        lock(&self.queued).front().map(std::time::Instant::elapsed)
    }

    async fn call<T: Send + 'static>(&self, work: impl FnOnce(&mut EventSpool) -> Result<T> + Send + 'static) -> Result<T> {
        // The check and the count are one step, so callers on several threads cannot all pass the
        // check before any of them counts.
        if self.pending.try_update(Ordering::SeqCst, Ordering::SeqCst, |pending| (pending < self.limit).then_some(pending + 1)).is_err() {
            return Err(SpoolError::Busy);
        }

        lock(&self.queued).push_back(std::time::Instant::now());

        let (answer, answered) = oneshot::channel();
        let failure = Arc::clone(&self.failure);
        let outstanding = Outstanding { pending: Arc::clone(&self.pending), queued: Arc::clone(&self.queued) };
        let job: Job = Box::new(move |spool| {
            let result = match spool {
                Some(spool) => work(spool),
                None => Err(lock(&failure).clone().map_or(SpoolError::NotOpened, SpoolError::Unavailable)),
            };

            // Freed before the answer, so a caller that has it sees its place given back.
            drop(outstanding);
            let _ = answer.send(result);
        });
        // A job the thread did not take is dropped here, and frees its place as it goes.
        let sent = lock(&self.jobs).as_ref().is_some_and(|jobs| jobs.send(job).is_ok());

        if !sent {
            let reason = if self.started { None } else { lock(&self.failure).clone() };

            return Err(reason.map_or(SpoolError::Closing, SpoolError::Unavailable));
        }

        answered.await.unwrap_or_else(|_| Err(SpoolError::Stopped))
    }
}

#[async_trait]
impl SpoolPort for SpoolClient {
    async fn version(&self) -> Result<u32> {
        self.call(|spool| spool.version()).await
    }

    async fn position(&self, allocation_id: String) -> Result<Option<SpoolAck>> {
        self.call(move |spool| spool.position(&allocation_id)).await
    }

    async fn append(&self, streams: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>> {
        self.call(move |spool| spool.append(&streams)).await
    }

    async fn pending(&self, allocation_id: String) -> Result<Vec<SpoolFrame>> {
        self.call(move |spool| spool.pending(&allocation_id)).await
    }

    async fn backlog(&self) -> Result<Vec<SpoolBacklog>> {
        self.call(|spool| spool.backlog()).await
    }

    async fn acknowledge(&self, allocation_id: String, to_seq: i64) -> Result<()> {
        self.call(move |spool| spool.acknowledge(&allocation_id, to_seq)).await
    }

    async fn prune(&self, active: Vec<String>) -> Result<Vec<SpoolBacklog>> {
        self.call(move |spool| spool.prune(&active)).await
    }

    async fn record_exit(&self, exit: ServerExit) -> Result<()> {
        self.call(move |spool| spool.record_exit(&exit)).await
    }

    async fn exits(&self) -> Result<Vec<ServerExit>> {
        self.call(|spool| spool.exits()).await
    }

    async fn forget_exits(&self, allocation_ids: Vec<String>) -> Result<()> {
        self.call(move |spool| spool.forget_exits(&allocation_ids)).await
    }

    async fn allocation_used(&self, allocation_id: String) -> Result<bool> {
        self.call(move |spool| spool.allocation_used(&allocation_id)).await
    }

    async fn record_allocation(&self, allocation_id: String) -> Result<()> {
        self.call(move |spool| spool.record_allocation(&allocation_id)).await
    }

    /// The thread closes the file before this returns. Only the first call reports how the close went.
    async fn close(&self) -> Result<()> {
        let Some(jobs) = lock(&self.jobs).take() else {
            return Ok(());
        };
        let closed = lock(&self.closed).take();

        // The thread ends its loop once the queued jobs ran and no sender is left.
        drop(jobs);

        match closed {
            Some(closed) => closed.await.unwrap_or(Err(SpoolError::Stopped)),
            None => Ok(()),
        }
    }
}

/// Called on each change of [`SpoolWatch::unavailable`].
pub type SpoolChanged = Arc<dyn Fn() + Send + Sync>;

/// Whether the spool's thread keeps a call waiting past its limit, in its queue or in the job it
/// runs, as on a hung disk. Calls already waiting on it keep waiting, but the node reports itself
/// `unavailable`, so the controller sends it no launches and the operator sees why.
#[derive(Clone)]
pub struct SpoolWatch {
    client: Arc<SpoolClient>,
    stuck: Arc<AtomicBool>,
    changed: SpoolChanged,
    log: Logger,
    limit: Duration,
}

impl SpoolWatch {
    #[must_use]
    pub fn new(client: Arc<SpoolClient>, changed: SpoolChanged, log: Logger) -> Self {
        Self { client, stuck: Arc::default(), changed, log, limit: SPOOL_STUCK_LIMIT }
    }

    #[must_use]
    pub fn with_limit(&self, limit: Duration) -> Self {
        Self { limit, ..self.clone() }
    }

    /// Reports a change of state. It never waits on the thread.
    pub fn check(&self) {
        let stuck = self.stuck_now();

        if self.stuck.swap(stuck, Ordering::SeqCst) == stuck {
            return;
        }

        if stuck {
            self.log.error(format!("the event spool has not answered for {} s; launches are refused until it does", self.limit.as_secs()));
        } else {
            self.log.info("the event spool answers again; launches are taken");
        }

        (self.changed)();
    }

    fn stuck_now(&self) -> bool {
        self.client.waiting_for().is_some_and(|waiting| waiting >= self.limit)
    }

    /// Why the node takes no launches while the spool keeps a call waiting past the limit; `None`
    /// while it answers. Read from the thread's state at the moment of asking, not from the last
    /// check, so a launch never waits behind a stuck job the check has not seen yet.
    #[must_use]
    pub fn unavailable(&self) -> Option<NodeUnavailable> {
        self.stuck_now().then(|| NodeUnavailable {
            code: NodeUnavailableCode::EventStorage,
            operation: None,
            detail: format!("the event spool has not answered for {} s", self.limit.as_secs()),
        })
    }
}

/// `None` once the spool has opened; otherwise why the node cannot run servers, so it stays
/// connected and can be updated.
pub async fn open_spool(spool: &dyn SpoolPort, log: &Logger) -> Option<NodeUnavailable> {
    match spool.version().await {
        Ok(version) => {
            log.info(format!("event spool at schema {version}"));
            None
        }
        Err(error) => {
            let message = error.to_string();
            let message = if message.is_empty() { "the event spool did not open".to_owned() } else { message };

            log.error(&message);
            Some(NodeUnavailable { code: NodeUnavailableCode::EventStorage, operation: None, detail: truncate(&message, NODE_UNAVAILABLE_DETAIL_MAX_CHARS) })
        }
    }
}

/// Polls `work` once, which puts a spool call in the spool's queue at the moment of the call, and
/// returns the rest of it. Spool calls that must keep the order of the events behind them go
/// through this rather than a spawned task.
pub(crate) fn queue_now<T: Send + 'static>(work: impl Future<Output = T> + Send + 'static) -> futures_util::future::BoxFuture<'static, T> {
    use futures_util::FutureExt as _;

    let mut work = work.boxed();

    match (&mut work).now_or_never() {
        Some(done) => futures_util::future::ready(done).boxed(),
        None => work,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn calls_reach_the_spool_thread() {
        let dir = tempfile::tempdir().unwrap();
        let client = SpoolClient::start(dir.path().join("events/streams.sqlite"), Logger::null());

        assert_eq!(open_spool(&client, &Logger::null()).await, None);
        client.record_allocation("5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e".to_owned()).await.unwrap();
        assert!(client.allocation_used("5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e".to_owned()).await.unwrap());
        client.close().await.unwrap();
        assert_eq!(client.version().await.unwrap_err().to_string(), "event spool is closing");
    }

    #[tokio::test]
    async fn refuses_a_call_past_the_limit_and_takes_calls_again_once_answered() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SpoolClient::start_with_limit(dir.path().join("events/streams.sqlite"), Logger::null(), 2));
        let (open, gate) = std::sync::mpsc::channel::<()>();
        let blocked = {
            let client = Arc::clone(&client);

            tokio::spawn(async move {
                client
                    .call(move |_| {
                        let _ = gate.recv();
                        Ok(())
                    })
                    .await
            })
        };
        let queued = {
            let client = Arc::clone(&client);

            tokio::spawn(async move { client.version().await })
        };

        while client.pending.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }

        assert!(matches!(client.version().await, Err(SpoolError::Busy)));
        open.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        queued.await.unwrap().unwrap();
        assert_eq!(client.pending.load(Ordering::SeqCst), 0);
        client.version().await.unwrap();
    }

    /// Each change of state is reported once.
    #[tokio::test]
    async fn reports_the_spool_stuck_while_one_job_holds_its_thread_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SpoolClient::start(dir.path().join("events/streams.sqlite"), Logger::null()));
        let changes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&changes);
        let watch = SpoolWatch::new(Arc::clone(&client), Arc::new(move || _ = counted.fetch_add(1, Ordering::SeqCst)), Logger::null())
            .with_limit(Duration::from_millis(20));
        let (open, gate) = std::sync::mpsc::channel::<()>();
        let blocked = {
            let client = Arc::clone(&client);

            tokio::spawn(async move {
                client
                    .call(move |_| {
                        let _ = gate.recv();
                        Ok(())
                    })
                    .await
            })
        };

        watch.check();
        assert_eq!(watch.unavailable(), None);
        assert_eq!(changes.load(Ordering::SeqCst), 0);

        while client.waiting_for().is_none_or(|waiting| waiting < Duration::from_millis(30)) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        watch.check();
        watch.check();
        assert_eq!(watch.unavailable().map(|unavailable| unavailable.code), Some(NodeUnavailableCode::EventStorage));
        assert_eq!(changes.load(Ordering::SeqCst), 1);

        open.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        watch.check();
        assert_eq!(watch.unavailable(), None);
        assert_eq!(changes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn closes_the_file_before_close_returns_so_no_wal_is_left_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events/streams.sqlite");
        let client = SpoolClient::start(path.clone(), Logger::null());
        let beside = |suffix: &str| path.with_file_name(format!("streams.sqlite{suffix}"));

        client.record_allocation("5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e".to_owned()).await.unwrap();
        assert!(beside("-wal").exists());
        client.close().await.unwrap();
        assert!(!beside("-wal").exists());
        assert!(!beside("-shm").exists());
    }

    #[tokio::test]
    async fn a_spool_that_cannot_open_marks_the_node_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("events");

        std::fs::write(&blocker, "a file where the directory goes").unwrap();

        let client = SpoolClient::start(blocker.join("streams.sqlite"), Logger::null());
        let unavailable = open_spool(&client, &Logger::null()).await.unwrap();

        assert_eq!(unavailable.code, NodeUnavailableCode::EventStorage);
        assert_ne!(unavailable.detail.len(), 0);
    }
}
