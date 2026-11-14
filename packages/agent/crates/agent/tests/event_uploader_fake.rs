//! The uploader against a scripted spool and the controller, on paused time: what it sends where, when it
//! retries, and what it reports of the events that wait.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::event_spool::{SpoolAck, SpoolAppend, SpoolBacklog, SpoolError, SpoolFrame, SpoolStream};
use ankor_agent::event_uploader::{EventUploader, EventsPost, EventsTarget, PostAnswer, UploadError, UploaderParts, nothing_stuck};
use ankor_agent::logger::{Entry, Logger};
use ankor_agent::spool_client::SpoolPort;
use ankor_protocol::constants::{EVENT_BACKLOG_REASON_MAX_CHARS, MAX_REPORTED_BACKLOGS, PROTOCOL_HEADER, PROTOCOL_VERSION};
use ankor_protocol::helpers::node_authorization;
use ankor_protocol::schemas::s_event_backlog;
use ankor_protocol::types::{EventBacklog, ServerExit};
use ankor_protocol::validate::safe_parse;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::watch;

const ALLOCATION: &str = "00000000-0000-4000-8000-000000000001";
const OTHER_ALLOCATION: &str = "00000000-0000-4000-8000-000000000002";
const URL: &str = "https://controller.example/api/node/events";

fn nth_allocation(index: usize) -> String {
    format!("00000000-0000-4000-8000-{index:012}")
}

fn address(host: &str) -> EventsTarget {
    EventsTarget { host: host.to_owned(), tls: true, events_path: "/api/node/events".to_owned() }
}

type Fut<T> = Pin<Box<dyn Future<Output = Result<T, SpoolError>> + Send>>;

fn ready<T: Send + 'static>(value: T) -> Fut<T> {
    Box::pin(async move { Ok(value) })
}

/// A latch a test opens; everything waiting on it continues.
#[derive(Clone)]
struct Gate(Arc<watch::Sender<bool>>);

impl Gate {
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    fn release(&self) {
        self.0.send_replace(true);
    }

    async fn wait(&self) {
        let _ = self.0.subscribe().wait_for(|open| *open).await;
    }
}

/// A spool whose reads and acknowledgement each test scripts.
struct FakeSpool {
    backlog: Box<dyn Fn() -> Fut<Vec<SpoolBacklog>> + Send + Sync>,
    pending: Box<dyn Fn(String) -> Fut<Vec<SpoolFrame>> + Send + Sync>,
    acknowledge: Box<dyn Fn(String, i64) -> Fut<()> + Send + Sync>,
    prunes: Arc<Mutex<Vec<Vec<String>>>>,
    /// The streams the next prune drops.
    pruned: Arc<Mutex<Vec<SpoolBacklog>>>,
}

fn frames(allocation: &str, count: i64) -> Vec<SpoolFrame> {
    (0..count).map(|seq| SpoolFrame { allocation_id: allocation.to_owned(), seq, frame: json!({ "seq": seq, "event": {} }).to_string() }).collect()
}

/// A spool holding one undelivered event of one server, however often it is delivered.
fn one_event() -> FakeSpool {
    FakeSpool {
        backlog: Box::new(|| ready(vec![SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 1 }])),
        pending: Box::new(|id| ready(frames(&id, 1))),
        acknowledge: Box::new(|_, _| ready(())),
        prunes: Arc::default(),
        pruned: Arc::default(),
    }
}

/// One server's undelivered events, emptied when the controller has them; `hold` holds the next `pending`
/// read.
struct Server {
    left: Arc<Mutex<i64>>,
    gate: Arc<Mutex<Option<Gate>>>,
}

impl Server {
    fn left(&self) -> i64 {
        *self.left.lock().unwrap()
    }

    /// The next `pending` read waits until the returned gate opens.
    fn hold(&self) -> Gate {
        let gate = Gate::new();

        *self.gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}

fn server() -> (FakeSpool, Server) {
    let left = Arc::new(Mutex::new(1));
    let gate: Arc<Mutex<Option<Gate>>> = Arc::default();
    let (backlog_left, pending_left, acked, held) = (Arc::clone(&left), Arc::clone(&left), Arc::clone(&left), Arc::clone(&gate));
    let spool = FakeSpool {
        backlog: Box::new(move || {
            let left = *backlog_left.lock().unwrap();

            ready(if left > 0 { vec![SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: left }] } else { vec![] })
        }),
        pending: Box::new(move |id| {
            let (left, gate) = (Arc::clone(&pending_left), held.lock().unwrap().take());

            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.wait().await;
                }

                let count = *left.lock().unwrap();

                Ok(frames(&id, count))
            })
        }),
        acknowledge: Box::new(move |_, _| {
            *acked.lock().unwrap() = 0;
            ready(())
        }),
        prunes: Arc::default(),
        pruned: Arc::default(),
    };

    (spool, Server { left, gate })
}

/// [`one_event`] until it is acknowledged, then nothing: a delivery empties it.
fn deliverable() -> (FakeSpool, Arc<AtomicBool>) {
    let acknowledged = Arc::new(AtomicBool::new(false));
    let (in_backlog, in_pending, acking) = (Arc::clone(&acknowledged), Arc::clone(&acknowledged), Arc::clone(&acknowledged));
    let spool = FakeSpool {
        backlog: Box::new(move || {
            ready(if in_backlog.load(Ordering::SeqCst) { vec![] } else { vec![SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 1 }] })
        }),
        pending: Box::new(move |id| ready(if in_pending.load(Ordering::SeqCst) { vec![] } else { frames(&id, 1) })),
        acknowledge: Box::new(move |_, _| {
            acking.store(true, Ordering::SeqCst);
            ready(())
        }),
        prunes: Arc::default(),
        pruned: Arc::default(),
    };

    (spool, acknowledged)
}

#[async_trait]
impl SpoolPort for FakeSpool {
    async fn version(&self) -> Result<u32, SpoolError> {
        Ok(1)
    }

    async fn position(&self, allocation_id: String) -> Result<Option<SpoolAck>, SpoolError> {
        Ok(Some(SpoolAck { allocation_id, to_seq: 0 }))
    }

    async fn append(&self, _: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>, SpoolError> {
        Ok(vec![])
    }

    async fn pending(&self, allocation_id: String) -> Result<Vec<SpoolFrame>, SpoolError> {
        (self.pending)(allocation_id).await
    }

    async fn backlog(&self) -> Result<Vec<SpoolBacklog>, SpoolError> {
        (self.backlog)().await
    }

    async fn acknowledge(&self, allocation_id: String, to_seq: i64) -> Result<(), SpoolError> {
        (self.acknowledge)(allocation_id, to_seq).await
    }

    async fn prune(&self, active: Vec<String>) -> Result<Vec<SpoolBacklog>, SpoolError> {
        self.prunes.lock().unwrap().push(active);
        Ok(std::mem::take(&mut *self.pruned.lock().unwrap()))
    }

    async fn record_exit(&self, _: ServerExit) -> Result<(), SpoolError> {
        Ok(())
    }

    async fn exits(&self) -> Result<Vec<ServerExit>, SpoolError> {
        Ok(vec![])
    }

    async fn forget_exits(&self, _: Vec<String>) -> Result<(), SpoolError> {
        Ok(())
    }

    async fn allocation_used(&self, _: String) -> Result<bool, SpoolError> {
        Ok(false)
    }

    async fn record_allocation(&self, _: String) -> Result<(), SpoolError> {
        Ok(())
    }

    async fn close(&self) -> Result<(), SpoolError> {
        Ok(())
    }
}

type Answer = Pin<Box<dyn Future<Output = Result<PostAnswer, UploadError>> + Send>>;
type AnswerFn = fn() -> Answer;

/// A POST as the uploader made it.
#[derive(Clone, Debug)]
struct Request {
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// The controller, answering each POST with what `answer` makes of its URL.
struct FakeController {
    requests: Mutex<Vec<Request>>,
    answer: Box<dyn Fn(&str) -> Answer + Send + Sync>,
}

impl FakeController {
    fn new(answer: impl Fn(&str) -> Answer + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { requests: Mutex::default(), answer: Box::new(answer) })
    }

    fn urls(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(|request| request.url.clone()).collect()
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl EventsPost for FakeController {
    async fn post(&self, url: &str, headers: &[(&str, String)], body: String) -> Result<PostAnswer, UploadError> {
        self.requests.lock().unwrap().push(Request {
            url: url.to_owned(),
            headers: headers.iter().map(|(name, value)| ((*name).to_owned(), value.clone())).collect(),
            body,
        });
        (self.answer)(url).await
    }
}

fn status(status: u16, body: &str) -> Answer {
    let body = body.to_owned();

    Box::pin(async move { Ok(PostAnswer { status, body }) })
}

fn receipt() -> Answer {
    status(200, r#"{"rejected":[]}"#)
}

fn refused(message: &str) -> Answer {
    let message = message.to_owned();

    Box::pin(async move { Err(UploadError::Io(std::io::Error::other(message))) })
}

/// A request that never answers; the uploader can only abandon it.
fn hanging() -> Answer {
    Box::pin(std::future::pending())
}

/// Answers each POST with the next of `answers`, the last one for every POST after it.
fn controller(answers: Vec<AnswerFn>) -> Arc<FakeController> {
    let answers = Mutex::new(answers);

    FakeController::new(move |_| {
        let mut answers = answers.lock().unwrap();
        let next = if answers.len() > 1 { answers.remove(0) } else { answers[0] };

        next()
    })
}

type Reports = Arc<Mutex<Vec<EventBacklog>>>;
type Lines = Arc<Mutex<Vec<String>>>;

struct Setup {
    uploader: EventUploader,
    reports: Reports,
    lines: Lines,
    target: Arc<Mutex<EventsTarget>>,
    targets_read: Arc<AtomicUsize>,
}

impl Setup {
    fn move_to(&self, target: EventsTarget) {
        *self.target.lock().unwrap() = target;
    }

    fn reports(&self) -> Vec<EventBacklog> {
        self.reports.lock().unwrap().clone()
    }

    fn reasons(&self) -> Vec<Option<String>> {
        self.reports().into_iter().map(|report| report.reason).collect()
    }

    fn logged(&self, part: &str) -> Vec<String> {
        self.lines.lock().unwrap().iter().filter(|line| line.contains(part)).cloned().collect()
    }
}

fn uploader(spool: Arc<FakeSpool>, post: Arc<FakeController>) -> Setup {
    uploader_with(spool, post, vec![])
}

fn uploader_with(spool: Arc<FakeSpool>, post: Arc<FakeController>, attached: Vec<String>) -> Setup {
    let reports: Reports = Arc::default();
    let lines: Lines = Arc::default();
    let target = Arc::new(Mutex::new(address("controller.example")));
    let targets_read = Arc::new(AtomicUsize::new(0));
    let (seen, written, reading, read) = (Arc::clone(&reports), Arc::clone(&lines), Arc::clone(&target), Arc::clone(&targets_read));
    let uploader = EventUploader::from_parts(UploaderParts {
        spool,
        token: "test-token".to_owned(),
        target: Arc::new(move || {
            read.fetch_add(1, Ordering::SeqCst);
            reading.lock().unwrap().clone()
        }),
        attached: Arc::new(move || attached.clone()),
        on_backlog: Arc::new(move |report| seen.lock().unwrap().push(report)),
        log: Logger::new("events", Some(Arc::new(move |entry: Entry| written.lock().unwrap().push(entry.message.trim_start_matches("[events] ").to_owned())))),
        post,
        random: Arc::new(|| 0.5),
    });

    Setup { uploader, reports, lines, target, targets_read }
}

/// Lets every task run until it waits on time.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

async fn advance(by: Duration) {
    tokio::time::sleep(by).await;
}

async fn closing_during_a_read(close_all: bool, during_backlog: bool) {
    let gate = Gate::new();
    let reading = Arc::new(AtomicBool::new(false));
    let retained = Arc::new(AtomicBool::new(true));
    let acknowledged = Arc::new(AtomicBool::new(false));
    let (backlog_gate, pending_gate) = (gate.clone(), gate.clone());
    let (backlog_reading, pending_reading) = (Arc::clone(&reading), Arc::clone(&reading));
    let (backlog_retained, pending_retained, acked_retained, acked) =
        (Arc::clone(&retained), Arc::clone(&retained), Arc::clone(&retained), Arc::clone(&acknowledged));
    let spool = FakeSpool {
        backlog: Box::new(move || {
            let (gate, reading, retained) = (backlog_gate.clone(), Arc::clone(&backlog_reading), Arc::clone(&backlog_retained));

            Box::pin(async move {
                if during_backlog {
                    reading.store(true, Ordering::SeqCst);
                    gate.wait().await;
                }

                Ok(if retained.load(Ordering::SeqCst) { vec![SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 1 }] } else { vec![] })
            })
        }),
        pending: Box::new(move |id| {
            let (gate, reading, retained) = (pending_gate.clone(), Arc::clone(&pending_reading), Arc::clone(&pending_retained));

            Box::pin(async move {
                if !during_backlog {
                    reading.store(true, Ordering::SeqCst);
                    gate.wait().await;
                }

                Ok(if retained.load(Ordering::SeqCst) { frames(&id, 1) } else { vec![] })
            })
        }),
        acknowledge: Box::new(move |_, _| {
            acked_retained.store(false, Ordering::SeqCst);
            acked.store(true, Ordering::SeqCst);
            ready(())
        }),
        prunes: Arc::default(),
        pruned: Arc::default(),
    };
    let post = controller(vec![receipt]);
    let setup = uploader(Arc::new(spool), Arc::clone(&post));

    setup.uploader.start().await;
    settle().await;
    assert!(reading.load(Ordering::SeqCst));

    let closed = close_all.then(|| {
        let uploader = setup.uploader.clone();

        tokio::spawn(async move { uploader.close().await })
    });

    if !close_all {
        setup.uploader.close_upstream();
    }

    tokio::task::yield_now().await;
    gate.release();
    advance(Duration::from_secs(2)).await;

    if let Some(closed) = closed {
        closed.await.unwrap();
    }

    assert_eq!(post.count(), 0);
    assert!(!acknowledged.load(Ordering::SeqCst));
    assert!(retained.load(Ordering::SeqCst));
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn starts_no_http_request_after_closing_upstream_uploads_during_a_backlog_read() {
    closing_during_a_read(false, true).await;
}

#[tokio::test(start_paused = true)]
async fn starts_no_http_request_after_closing_upstream_uploads_during_a_pending_read() {
    closing_during_a_read(false, false).await;
}

#[tokio::test(start_paused = true)]
async fn starts_no_http_request_after_closing_all_uploads_during_a_backlog_read() {
    closing_during_a_read(true, true).await;
}

#[tokio::test(start_paused = true)]
async fn starts_no_http_request_after_closing_all_uploads_during_a_pending_read() {
    closing_during_a_read(true, false).await;
}

#[tokio::test(start_paused = true)]
async fn sends_the_next_batch_to_the_controllers_new_address_when_set_config_changes_it() {
    let moves = [
        (address("other.example"), "https://other.example/api/node/events"),
        (EventsTarget { tls: false, ..address("controller.example") }, "http://controller.example/api/node/events"),
        (EventsTarget { events_path: "/api/v2/events".to_owned(), ..address("controller.example") }, "https://controller.example/api/v2/events"),
    ];

    for (moved, url) in moves {
        let post = FakeController::new(|_| refused("connection refused"));
        let setup = uploader(Arc::new(one_event()), Arc::clone(&post));

        setup.uploader.start().await;
        settle().await;
        assert_eq!(post.urls(), [URL]);
        setup.move_to(moved);
        advance(Duration::from_secs(1)).await;
        assert_eq!(post.urls().last().map(String::as_str), Some(url));
        setup.uploader.close().await;
    }
}

/// A server that never runs out of events keeps the pass going; one that starts meanwhile still
/// gets its batch within a tick instead of waiting for the busy one to fall quiet.
#[tokio::test(start_paused = true)]
async fn a_server_that_starts_during_a_pass_joins_it_within_a_tick() {
    let started = Arc::new(AtomicBool::new(false));
    let listed = Arc::clone(&started);
    let spool = FakeSpool {
        backlog: Box::new(move || {
            let mut servers = vec![SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 1 }];

            if listed.load(Ordering::SeqCst) {
                servers.push(SpoolBacklog { allocation_id: OTHER_ALLOCATION.to_owned(), pending: 1 });
            }

            ready(servers)
        }),
        pending: Box::new(|id| ready(frames(&id, 1))),
        acknowledge: Box::new(|_, _| ready(())),
        prunes: Arc::default(),
        pruned: Arc::default(),
    };
    // Each answer takes a moment, so the busy server's pass lets time run.
    let post = FakeController::new(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            receipt().await
        })
    });
    let setup = uploader(Arc::new(spool), Arc::clone(&post));

    setup.uploader.start().await;
    advance(Duration::from_millis(15)).await;
    started.store(true, Ordering::SeqCst);
    advance(Duration::from_millis(500)).await;

    let bodies = || post.requests.lock().unwrap().iter().map(|request| request.body.clone()).collect::<Vec<_>>();

    // Not yet: the spool is read again only once a tick, so a large backlog does not scan it after every batch.
    assert!(!bodies().iter().any(|body| body.contains(OTHER_ALLOCATION)));
    advance(Duration::from_millis(600)).await;
    assert!(bodies().iter().any(|body| body.contains(OTHER_ALLOCATION)), "the new server waited for the busy one: {:?}", bodies());
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn drops_a_request_to_the_controllers_old_address_for_the_new_one_at_once_as_no_failure() {
    let post = FakeController::new(|_| hanging());
    let setup = uploader(Arc::new(one_event()), Arc::clone(&post));

    setup.uploader.start().await;
    settle().await;
    setup.move_to(address("other.example"));
    advance(Duration::from_secs(1)).await;
    assert_eq!(post.urls(), [URL, "https://other.example/api/node/events"]);
    assert_eq!(setup.reports(), []);
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn reports_a_failure_at_the_controllers_new_address_as_at_any_other() {
    let post = FakeController::new(|url| if url.starts_with("https://controller.example") { hanging() } else { refused("connection refused") });
    let setup = uploader(Arc::new(one_event()), Arc::clone(&post));

    setup.uploader.start().await;
    settle().await;
    setup.move_to(address("other.example"));
    advance(Duration::from_secs(1)).await;
    assert_eq!(post.urls(), [URL, "https://other.example/api/node/events"]);
    assert_eq!(
        setup.reports().iter().map(|report| (report.event_count, report.reason.clone())).collect::<Vec<_>>(),
        [(1, Some("connection refused".to_owned()))]
    );
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn takes_a_failure_for_a_failure_when_the_controller_moved_between_two_requests_aborting_none() {
    let acknowledging = Gate::new();
    let held = acknowledging.clone();
    let spool = FakeSpool {
        acknowledge: Box::new(move |_, _| {
            let gate = held.clone();

            Box::pin(async move {
                gate.wait().await;
                Ok(())
            })
        }),
        ..one_event()
    };
    let post = FakeController::new(|url| if url.starts_with("https://controller.example") { receipt() } else { refused("connection refused") });
    let setup = uploader(Arc::new(spool), Arc::clone(&post));

    setup.uploader.start().await;
    settle().await;
    // The first batch is delivered and being acknowledged when the controller moves.
    setup.move_to(address("other.example"));
    advance(Duration::from_secs(1)).await;
    acknowledging.release();
    settle().await;
    assert_eq!(post.urls(), [URL, "https://other.example/api/node/events"]);
    assert_eq!(
        setup.reports().iter().map(|report| (report.event_count, report.reason.clone())).collect::<Vec<_>>(),
        [(1, Some("connection refused".to_owned()))]
    );
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn takes_a_spool_failure_for_a_failure_when_the_controller_moved_during_the_read() {
    let failing = Gate::new();
    let reads = Arc::new(AtomicUsize::new(0));
    let (held, counted) = (failing.clone(), Arc::clone(&reads));
    let spool = FakeSpool {
        // The first read finds the event; the second, after its delivery, waits and then fails.
        pending: Box::new(move |id| {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                return ready(frames(&id, 1));
            }

            let gate = held.clone();

            Box::pin(async move {
                gate.wait().await;
                Err(SpoolError::Unavailable("disk I/O error".to_owned()))
            })
        }),
        ..one_event()
    };
    let setup = uploader(Arc::new(spool), controller(vec![receipt]));

    setup.uploader.start().await;
    settle().await;
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    setup.move_to(address("other.example"));
    advance(Duration::from_secs(1)).await;
    failing.release();
    settle().await;
    assert_eq!(setup.reports().iter().map(|report| (report.event_count, report.reason.clone())).collect::<Vec<_>>(), [(1, Some("disk I/O error".to_owned()))]);
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn keeps_one_upload_in_flight_while_the_controller_is_slow_to_answer() {
    let post = FakeController::new(|_| hanging());
    let setup = uploader(Arc::new(one_event()), Arc::clone(&post));

    setup.uploader.start().await;
    advance(Duration::from_secs(5)).await;
    assert_eq!(post.count(), 1);
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn says_nothing_about_a_server_it_delivers_at_the_first_try() {
    let (spool, server) = server();
    let setup = uploader(Arc::new(spool), controller(vec![receipt]));

    setup.uploader.start().await;
    settle().await;
    assert_eq!(server.left(), 0);
    assert_eq!(setup.reports(), []);
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn reports_a_server_backlogged_when_the_controller_fails_and_clear_once_it_is_delivered() {
    let (spool, _server) = server();
    let setup = uploader(Arc::new(spool), controller(vec![|| status(503, ""), receipt]));

    setup.uploader.start().await;
    settle().await;
    assert_eq!(
        setup.reports(),
        [EventBacklog {
            servers: vec![ankor_protocol::types::StuckServer { allocation_id: ALLOCATION.to_owned(), pending: 1 }],
            server_count: 1,
            event_count: 1,
            reason: Some("event backend returned HTTP 503".to_owned()),
        }]
    );
    advance(Duration::from_secs(2)).await;
    assert_eq!(setup.reports().len(), 2);
    assert_eq!(setup.reports()[1], nothing_stuck());
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn reports_a_backlog_that_grows_while_uploads_fail_and_nothing_stuck_once_all_of_it_is_delivered() {
    let queued = Arc::new(Mutex::new(vec![(ALLOCATION.to_owned(), 1_i64), (OTHER_ALLOCATION.to_owned(), 0)]));
    let (listed, read, acked) = (Arc::clone(&queued), Arc::clone(&queued), Arc::clone(&queued));
    let spool = FakeSpool {
        backlog: Box::new(move || {
            let servers = listed
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, left)| *left > 0)
                .map(|(id, pending)| SpoolBacklog { allocation_id: id.clone(), pending: *pending })
                .collect();

            ready(servers)
        }),
        pending: Box::new(move |id| {
            let count = read.lock().unwrap().iter().find(|(each, _)| *each == id).map_or(0, |(_, left)| *left);

            ready(frames(&id, count))
        }),
        acknowledge: Box::new(move |id, _| {
            for (each, left) in acked.lock().unwrap().iter_mut() {
                if *each == id {
                    *left = 0;
                }
            }

            ready(())
        }),
        ..one_event()
    };
    let setup = uploader(Arc::new(spool), controller(vec![|| status(503, ""), || status(503, ""), receipt]));
    let listed = |setup: &Setup| {
        setup.reports().iter().map(|report| report.servers.iter().map(|stuck| stuck.allocation_id.clone()).collect::<Vec<_>>()).collect::<Vec<_>>()
    };

    setup.uploader.start().await;
    settle().await;
    assert_eq!(listed(&setup), [[ALLOCATION]]);
    // Another game's events arrive while uploads fail: the next hello lists them too, whatever the frames said.
    queued.lock().unwrap()[1].1 = 1;

    let backlog = setup.uploader.backlog().await.unwrap();

    assert_eq!((backlog.server_count, backlog.event_count), (2, 2));

    advance(Duration::from_secs(2)).await;

    let last = setup.reports().last().cloned().unwrap();

    assert_eq!((last.server_count, last.event_count), (2, 2));
    advance(Duration::from_secs(4)).await;
    assert_eq!(setup.reports().last(), Some(&nothing_stuck()));
    assert_eq!(setup.reports().len(), 3);
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn does_not_call_a_backlogged_server_clear_when_uploads_close_while_it_is_still_read() {
    let (spool, server) = server();
    let spool = Arc::new(spool);
    let setup = uploader(Arc::clone(&spool), controller(vec![|| status(503, ""), receipt]));

    setup.uploader.start().await;
    settle().await;
    assert_eq!(setup.reports().len(), 1);

    // The retry reads the server's events; the controller refuses the node before the read returns.
    let release = server.hold();

    advance(Duration::from_secs(2)).await;
    setup.uploader.close_upstream();
    release.release();
    settle().await;
    assert_eq!(server.left(), 1);
    assert_eq!(setup.reports().len(), 1);
    assert_eq!(setup.uploader.backlog().await.unwrap().server_count, 1);
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn reads_no_backlog_once_uploads_to_the_controller_are_closed() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    let spool = FakeSpool {
        backlog: Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            ready(vec![SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 1 }])
        }),
        ..one_event()
    };
    let setup = uploader(Arc::new(spool), controller(vec![|| status(503, "")]));

    setup.uploader.start().await;
    settle().await;
    setup.uploader.close_upstream();

    let before = reads.load(Ordering::SeqCst);

    advance(Duration::from_mins(1)).await;
    assert_eq!(reads.load(Ordering::SeqCst), before);
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn tells_the_controller_why_it_refused_a_batch_with_409() {
    let cases: [(AnswerFn, &str); 3] = [
        (|| status(409, r#"{"code":"event_integrity_error","detail":"seq 4 repeats"}"#), "event backend refused the batch: seq 4 repeats"),
        (|| status(409, r#"{"error":"conflict"}"#), "event backend refused the batch: HTTP 409"),
        (|| status(409, "<html>Conflict</html>"), "event backend refused the batch: HTTP 409"),
    ];

    for (answer, reason) in cases {
        let (spool, server) = server();
        let setup = uploader(Arc::new(spool), controller(vec![answer]));

        setup.uploader.start().await;
        settle().await;
        assert_eq!(server.left(), 1);
        assert_eq!(setup.reports().iter().map(|report| (report.event_count, report.reason.clone())).collect::<Vec<_>>(), [(1, Some(reason.to_owned()))]);
        setup.uploader.close().await;
    }
}

#[tokio::test(start_paused = true)]
async fn posts_the_batch_as_json_to_the_controllers_events_path_with_the_nodes_token_and_versions() {
    let (spool, _server) = server();
    let post = controller(vec![receipt]);
    let setup = uploader(Arc::new(spool), Arc::clone(&post));

    setup.uploader.start().await;
    settle().await;

    let request = post.requests.lock().unwrap()[0].clone();
    let header = |name: &str| request.headers.iter().find(|(each, _)| each.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone());

    assert_eq!(request.url, URL);
    assert_eq!(header("authorization"), Some(node_authorization("test-token")));
    assert_eq!(header("content-type").as_deref(), Some("application/json"));
    assert_eq!(header(PROTOCOL_HEADER), Some(PROTOCOL_VERSION.to_string()));
    assert_eq!(serde_json::from_str::<Value>(&request.body).unwrap(), json!({ "allocation_id": ALLOCATION, "frames": [{ "seq": 0, "event": {} }] }));
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn prunes_the_spool_at_start_and_every_hour_and_no_more_once_closed() {
    let (spool, _server) = server();
    let spool = Arc::new(spool);
    let prunes = Arc::clone(&spool.prunes);
    let setup = uploader_with(spool, controller(vec![receipt]), vec![OTHER_ALLOCATION.to_owned()]);

    setup.uploader.start().await;
    assert_eq!(*prunes.lock().unwrap(), [[OTHER_ALLOCATION]]);
    advance(Duration::from_mins(59)).await;
    assert_eq!(prunes.lock().unwrap().len(), 1);
    advance(Duration::from_mins(1)).await;
    settle().await;
    assert_eq!(prunes.lock().unwrap().len(), 2);

    setup.uploader.close().await;

    let ticks = setup.targets_read.load(Ordering::SeqCst);

    advance(Duration::from_hours(2)).await;
    assert_eq!(prunes.lock().unwrap().len(), 2);
    assert_eq!(setup.targets_read.load(Ordering::SeqCst), ticks);
}

#[tokio::test(start_paused = true)]
async fn logs_a_prune_only_when_it_dropped_streams_and_names_each_that_held_undelivered_events() {
    let (spool, _server) = server();
    let spool = Arc::new(spool);
    let dropped = |allocation_id: String, pending: i64| SpoolBacklog { allocation_id, pending };

    spool.pruned.lock().unwrap().extend([dropped(OTHER_ALLOCATION.to_owned(), 0), dropped(nth_allocation(3), 0)]);

    let pruned = Arc::clone(&spool.pruned);
    let setup = uploader(spool, controller(vec![receipt]));

    setup.uploader.start().await;
    assert_eq!(setup.logged("spool retention"), ["spool retention dropped 2 stream(s)"]);
    assert!(pruned.lock().unwrap().is_empty());

    advance(Duration::from_hours(1)).await;
    settle().await;
    assert_eq!(setup.logged("spool retention"), ["spool retention dropped 2 stream(s)"]);

    // A stream outside any backlog report still counts what it lost.
    pruned.lock().unwrap().extend([dropped(OTHER_ALLOCATION.to_owned(), 0), dropped(nth_allocation(4), 3)]);
    advance(Duration::from_hours(1)).await;
    settle().await;
    assert_eq!(
        setup.logged("spool retention").last().unwrap(),
        &format!("spool retention dropped 2 stream(s), with events that reached no controller within the retention period: {} (3)", nth_allocation(4))
    );
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn retries_a_batch_the_controller_answers_with_a_3xx_as_it_does_any_answer_but_a_2xx() {
    let setup = uploader(Arc::new(one_event()), controller(vec![|| status(300, "")]));

    setup.uploader.start().await;
    settle().await;
    assert_eq!(
        setup.reports().iter().map(|report| (report.event_count, report.reason.clone())).collect::<Vec<_>>(),
        [(1, Some("event backend returned HTTP 300".to_owned()))]
    );
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn reports_no_backlog_for_the_request_it_aborts_by_closing_uploads_to_the_controller_or_shutting_down() {
    for shut_down in [false, true] {
        let post = FakeController::new(|_| hanging());
        let setup = uploader(Arc::new(one_event()), Arc::clone(&post));

        setup.uploader.start().await;
        settle().await;
        assert_eq!(post.count(), 1);

        let closing = if shut_down {
            let uploader = setup.uploader.clone();

            Some(tokio::spawn(async move { uploader.close().await }))
        } else {
            setup.uploader.close_upstream();
            None
        };

        advance(Duration::from_secs(1)).await;
        assert_eq!(setup.reports(), []);
        assert_eq!(setup.logged("retained for retry"), Vec::<String>::new());

        if let Some(closing) = closing {
            closing.await.unwrap();
        }

        setup.uploader.close().await;
    }
}

#[tokio::test(start_paused = true)]
async fn stops_a_backlog_report_that_is_still_counting_when_uploads_to_the_controller_close() {
    let counting = Gate::new();
    let reads = Arc::new(AtomicUsize::new(0));
    let (held, counted) = (counting.clone(), Arc::clone(&reads));
    let spool = FakeSpool {
        // The drain reads the backlog first; the failure's report reads it again, and waits.
        backlog: Box::new(move || {
            let (gate, second) = (held.clone(), counted.fetch_add(1, Ordering::SeqCst) == 1);

            Box::pin(async move {
                if second {
                    gate.wait().await;
                }

                Ok(vec![SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 1 }])
            })
        }),
        ..one_event()
    };
    let setup = uploader(Arc::new(spool), FakeController::new(|_| refused("connection refused")));

    setup.uploader.start().await;
    settle().await;
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    setup.uploader.close_upstream();
    counting.release();
    settle().await;
    assert_eq!(setup.reports(), []);
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn logs_one_failure_and_no_second_for_the_recovery_after_it() {
    let fail = Arc::new(AtomicBool::new(true));
    let failing = Arc::clone(&fail);
    let (spool, acknowledged) = deliverable();
    let setup = uploader(Arc::new(spool), FakeController::new(move |_| if failing.load(Ordering::SeqCst) { refused("connection refused") } else { receipt() }));

    setup.uploader.start().await;
    settle().await;
    fail.store(false, Ordering::SeqCst);
    advance(Duration::from_secs(1)).await;
    // One failure, then nothing more: the recovery is no second failure.
    assert_eq!(setup.logged("retained for retry"), ["events retained for retry: connection refused"]);
    assert_eq!(setup.logged("backlog count failed"), Vec::<String>::new());
    assert!(acknowledged.load(Ordering::SeqCst));
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn logs_the_events_controller_quarantined_and_only_when_it_quarantined_some() {
    let cases = [
        (json!([]), vec![]),
        (
            json!([{ "seq": 0, "reason": "unknown event type" }]),
            vec![format!("the controller quarantined 1 event(s) of {ALLOCATION}, dropped here: 0: unknown event type")],
        ),
        (
            json!([{ "seq": 0, "reason": "unknown event type" }, { "seq": 0, "reason": "bad round" }]),
            vec![format!("the controller quarantined 2 event(s) of {ALLOCATION}, dropped here: 0: unknown event type; 0: bad round")],
        ),
    ];

    for (rejected, logged) in cases {
        let body = json!({ "rejected": rejected }).to_string();
        let (spool, acknowledged) = deliverable();
        let setup = uploader(Arc::new(spool), FakeController::new(move |_| status(200, &body)));

        setup.uploader.start().await;
        settle().await;
        assert_eq!(setup.logged("quarantined"), logged);
        // A quarantined event is the controller's to keep; the node drops it like a delivered one.
        assert!(acknowledged.load(Ordering::SeqCst));
        setup.uploader.close().await;
    }
}

#[tokio::test(start_paused = true)]
async fn keeps_the_events_when_the_controller_answers_with_a_receipt_the_contract_refuses() {
    // A negative `seq` and an empty reason read into the receipt's type; only the contract refuses
    // them, and an answer it refuses is no proof that the controller stored the batch.
    let body = json!({ "rejected": [{ "seq": -1, "reason": "" }] }).to_string();
    let (spool, acknowledged) = deliverable();
    let setup = uploader(Arc::new(spool), FakeController::new(move |_| status(200, &body)));

    setup.uploader.start().await;
    settle().await;
    assert!(!acknowledged.load(Ordering::SeqCst));
    assert_eq!(setup.logged("quarantined"), Vec::<String>::new());
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn lists_every_server_still_waiting_while_uploads_fail_oldest_first_and_up_to_the_limit_and_none_once_they_work() {
    let limit = usize::try_from(MAX_REPORTED_BACKLOGS).unwrap();
    let servers: Vec<SpoolBacklog> =
        (0..=limit).map(|index| SpoolBacklog { allocation_id: nth_allocation(index), pending: i64::try_from(index).unwrap() + 1 }).collect();
    let fail = Arc::new(AtomicBool::new(true));
    let delivered = Arc::new(AtomicBool::new(false));
    let (failing, delivering, listed, read) = (Arc::clone(&fail), Arc::clone(&delivered), Arc::clone(&delivered), Arc::clone(&delivered));
    let all = servers.clone();
    let spool = FakeSpool {
        backlog: Box::new(move || ready(if listed.load(Ordering::SeqCst) { vec![] } else { all.clone() })),
        pending: Box::new(move |id| ready(if read.load(Ordering::SeqCst) { vec![] } else { frames(&id, 1) })),
        ..one_event()
    };
    let post = FakeController::new(move |_| {
        if failing.load(Ordering::SeqCst) {
            return status(502, "");
        }

        delivering.store(true, Ordering::SeqCst);
        receipt()
    });
    let setup = uploader(Arc::new(spool), post);

    // Before any upload has failed, nothing is stuck.
    assert_eq!(setup.uploader.backlog().await.unwrap(), nothing_stuck());
    setup.uploader.start().await;
    settle().await;

    let stuck = setup.uploader.backlog().await.unwrap();

    assert_eq!(stuck.servers.len(), limit);
    assert_eq!(stuck.servers[0].allocation_id, servers[0].allocation_id);
    assert_eq!(stuck.servers[0].pending, servers[0].pending);
    assert_eq!(stuck.servers.last().unwrap().allocation_id, servers[limit - 1].allocation_id);
    // The counts take in the server past the list.
    assert_eq!(stuck.server_count, i64::try_from(limit).unwrap() + 1);
    assert_eq!(stuck.event_count, servers.iter().map(|each| each.pending).sum::<i64>());
    assert_eq!(stuck.reason.as_deref(), Some("event backend returned HTTP 502"));
    assert!(safe_parse(s_event_backlog, serde_json::to_value(&stuck).unwrap()).is_ok());

    fail.store(false, Ordering::SeqCst);
    advance(Duration::from_secs(1)).await;
    assert!(delivered.load(Ordering::SeqCst));
    assert_eq!(setup.uploader.backlog().await.unwrap(), nothing_stuck());
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn reports_a_failure_whose_message_is_too_long_or_empty_within_the_contract() {
    let limit = usize::try_from(EVENT_BACKLOG_REASON_MAX_CHARS).unwrap();
    let cases = [("x".repeat(limit + 10), "x".repeat(limit)), (String::new(), "the upload failed".to_owned())];

    for (message, reason) in cases {
        let setup = uploader(Arc::new(one_event()), FakeController::new(move |_| refused(&message)));

        setup.uploader.start().await;
        settle().await;
        assert_eq!(setup.reasons(), [Some(reason.clone())]);
        assert_eq!(setup.uploader.backlog().await.unwrap().reason, Some(reason));
        setup.uploader.close().await;
    }
}

#[tokio::test(start_paused = true)]
async fn reports_the_oldest_waiting_servers_after_a_failed_upload_up_to_the_limit() {
    let limit = usize::try_from(MAX_REPORTED_BACKLOGS).unwrap();
    let servers: Vec<SpoolBacklog> = (0..limit + 5).map(|index| SpoolBacklog { allocation_id: nth_allocation(index), pending: 1 }).collect();
    let all = servers.clone();
    let spool = FakeSpool { backlog: Box::new(move || ready(all.clone())), ..one_event() };
    let setup = uploader(Arc::new(spool), controller(vec![|| status(502, "")]));

    setup.uploader.start().await;
    settle().await;

    let reports = setup.reports();

    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].servers.iter().map(|stuck| stuck.allocation_id.clone()).collect::<Vec<_>>(),
        servers[..limit].iter().map(|each| each.allocation_id.clone()).collect::<Vec<_>>()
    );
    assert_eq!((reports[0].server_count, reports[0].event_count), (i64::try_from(limit + 5).unwrap(), i64::try_from(limit + 5).unwrap()));
    setup.uploader.close().await;
}

/// One undelivered event each of `ALLOCATION` and then `OTHER_ALLOCATION`; a delivery empties its server.
fn two_servers() -> FakeSpool {
    let delivered: Arc<Mutex<Vec<String>>> = Arc::default();
    let (listing, reading, acking) = (Arc::clone(&delivered), Arc::clone(&delivered), Arc::clone(&delivered));

    FakeSpool {
        backlog: Box::new(move || {
            let delivered = listing.lock().unwrap().clone();

            ready(
                [ALLOCATION, OTHER_ALLOCATION]
                    .into_iter()
                    .filter(|id| !delivered.iter().any(|done| done == id))
                    .map(|id| SpoolBacklog { allocation_id: id.to_owned(), pending: 1 })
                    .collect(),
            )
        }),
        pending: Box::new(move |id| ready(if reading.lock().unwrap().contains(&id) { vec![] } else { frames(&id, 1) })),
        acknowledge: Box::new(move |id, _| {
            acking.lock().unwrap().push(id);
            ready(())
        }),
        prunes: Arc::default(),
        pruned: Arc::default(),
    }
}

#[tokio::test(start_paused = true)]
async fn a_server_error_for_one_batch_holds_back_only_its_server() {
    // The first batch is `ALLOCATION`'s, and only it fails.
    let post = controller(vec![|| status(500, ""), receipt]);
    let setup = uploader(Arc::new(two_servers()), Arc::clone(&post));

    setup.uploader.start().await;
    settle().await;

    let bodies: Vec<String> = post.requests.lock().unwrap().iter().map(|request| request.body.clone()).collect();

    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(bodies[1].contains(OTHER_ALLOCATION), "{bodies:?}");
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn retries_a_server_whose_batch_keeps_failing_after_a_backoff_that_doubles() {
    let started = tokio::time::Instant::now();
    let sent: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let at = Arc::clone(&sent);
    let post = FakeController::new(move |_| {
        at.lock().unwrap().push(started.elapsed());
        status(500, "")
    });
    let (spool, _server) = server();
    let setup = uploader(Arc::new(spool), post);

    setup.uploader.start().await;
    advance(Duration::from_millis(7_500)).await;
    assert_eq!(*sent.lock().unwrap(), [0, 1, 3, 7].map(Duration::from_secs));
    setup.uploader.close().await;
}

#[tokio::test(start_paused = true)]
async fn rereads_the_spool_for_new_servers_once_a_full_tick_has_passed_since_the_last_read() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    let spool = FakeSpool {
        // The second server is in the spool from the second read on.
        backlog: Box::new(move || {
            let mut servers = vec![SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 1 }];

            if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                servers.push(SpoolBacklog { allocation_id: OTHER_ALLOCATION.to_owned(), pending: 1 });
            }

            ready(servers)
        }),
        ..one_event()
    };
    // Each answer takes exactly a tick, so the first batch ends a tick after the first read.
    let post = FakeController::new(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            receipt().await
        })
    });
    let setup = uploader(Arc::new(spool), Arc::clone(&post));

    setup.uploader.start().await;
    advance(Duration::from_millis(2_500)).await;

    let bodies: Vec<String> = post.requests.lock().unwrap().iter().map(|request| request.body.clone()).collect();

    assert_eq!(bodies.len(), 3, "{bodies:?}");
    assert!(bodies[2].contains(OTHER_ALLOCATION), "the second server waited a second tick: {bodies:?}");
    setup.uploader.close().await;
}
