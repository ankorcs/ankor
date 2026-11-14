//! The uploader against a fake events endpoint and a real spool.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::event_spool::{EventSpool, SpoolAck, SpoolAppend, SpoolBacklog, SpoolError, SpoolFrame, SpoolStream, SpooledFrame};
use ankor_agent::event_uploader::{EventUploader, EventsPost, EventsTarget, PostAnswer, UploadError, UploaderParts};
use ankor_agent::logger::Logger;
use ankor_agent::spool_client::{SpoolClient, SpoolPort};
use ankor_protocol::constants::{PROTOCOL_HEADER, PROTOCOL_VERSION};
use ankor_protocol::helpers::node_authorization;
use ankor_protocol::types::{EventBacklog, ServerExit};
use async_trait::async_trait;
use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use serde_json::{Value, json};

const ALLOCATION: &str = "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e";

#[derive(Clone, Default)]
struct Controller {
    batches: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    answer: Arc<Mutex<(u16, Value)>>,
}

async fn serve(controller: Controller) -> String {
    let app = Router::new().route(
        "/api/node/events",
        post(move |headers: HeaderMap, body: String| {
            let controller = controller.clone();

            async move {
                controller.batches.lock().unwrap().push((headers, serde_json::from_str(&body).unwrap()));

                let (status, body) = controller.answer.lock().unwrap().clone();

                (StatusCode::from_u16(status).unwrap(), body.to_string())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();

    tokio::spawn(async move { axum::serve(listener, app).await });
    address
}

fn frame(seq: i64) -> SpooledFrame {
    SpooledFrame {
        seq,
        raw: json!({ "seq": seq, "event": { "type": "log", "timestamp": 1_700_000_000_000_i64, "tick": 0, "level": "debug", "message": "hello" } }).to_string(),
    }
}

async fn wait_for(mut check: impl FnMut() -> bool) {
    for _ in 0..60 {
        if check() {
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    panic!("condition not met in time");
}

#[tokio::test]
async fn uploads_acknowledge_and_failures_report_a_backlog() {
    let dir = tempfile::tempdir().unwrap();
    let spool = Arc::new(SpoolClient::start(dir.path().join("events/streams.sqlite"), Logger::null()));
    let controller = Controller::default();

    *controller.answer.lock().unwrap() = (200, json!({ "rejected": [] }));

    let host = serve(controller.clone()).await;
    let backlogs: Arc<Mutex<Vec<EventBacklog>>> = Arc::default();
    let seen = Arc::clone(&backlogs);
    let uploader = EventUploader::new(
        Arc::clone(&spool) as Arc<dyn SpoolPort>,
        "t".repeat(64),
        Arc::new(move || EventsTarget { host: host.clone(), tls: false, events_path: "/api/node/events".to_owned() }),
        Arc::new(Vec::new),
        Arc::new(move |backlog| seen.lock().unwrap().push(backlog)),
        Logger::null(),
    );

    spool.append(vec![SpoolStream { allocation_id: ALLOCATION.to_owned(), frames: (0..3).map(frame).collect() }]).await.unwrap();
    uploader.start().await;
    wait_for(|| !controller.batches.lock().unwrap().is_empty()).await;

    {
        let batches = controller.batches.lock().unwrap();
        let (headers, batch) = &batches[0];

        assert_eq!(headers["authorization"], node_authorization(&"t".repeat(64)));
        assert_eq!(headers[PROTOCOL_HEADER], PROTOCOL_VERSION.to_string());
        assert_eq!(batch["allocation_id"], ALLOCATION);
        assert_eq!(batch["frames"].as_array().unwrap().len(), 3);
    }

    for _ in 0..60 {
        if spool.pending(ALLOCATION.to_owned()).await.unwrap().is_empty() {
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(spool.pending(ALLOCATION.to_owned()).await.unwrap().len(), 0, "acknowledged after the upload");

    *controller.answer.lock().unwrap() = (409, json!({ "code": "event_integrity_error", "detail": "seq 3 is not next" }));
    spool.append(vec![SpoolStream { allocation_id: ALLOCATION.to_owned(), frames: vec![frame(3)] }]).await.unwrap();
    wait_for(|| !backlogs.lock().unwrap().is_empty()).await;

    let backlog = backlogs.lock().unwrap()[0].clone();

    assert_eq!(backlog.server_count, 1);
    assert_eq!(backlog.event_count, 1);
    assert_eq!(backlog.reason.as_deref(), Some("event backend refused the batch: seq 3 is not next"));

    uploader.close().await;
}

/// The real SQLite spool, called in place, so paused time stays deterministic.
struct Direct(Mutex<EventSpool>);

#[async_trait]
impl SpoolPort for Direct {
    async fn version(&self) -> Result<u32, SpoolError> {
        self.0.lock().unwrap().version()
    }
    async fn position(&self, allocation_id: String) -> Result<Option<SpoolAck>, SpoolError> {
        self.0.lock().unwrap().position(&allocation_id)
    }
    async fn append(&self, streams: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>, SpoolError> {
        self.0.lock().unwrap().append(&streams)
    }
    async fn pending(&self, allocation_id: String) -> Result<Vec<SpoolFrame>, SpoolError> {
        self.0.lock().unwrap().pending(&allocation_id)
    }
    async fn backlog(&self) -> Result<Vec<SpoolBacklog>, SpoolError> {
        self.0.lock().unwrap().backlog()
    }
    async fn acknowledge(&self, allocation_id: String, to_seq: i64) -> Result<(), SpoolError> {
        self.0.lock().unwrap().acknowledge(&allocation_id, to_seq)
    }
    async fn prune(&self, active: Vec<String>) -> Result<Vec<SpoolBacklog>, SpoolError> {
        self.0.lock().unwrap().prune(&active)
    }
    async fn record_exit(&self, exit: ServerExit) -> Result<(), SpoolError> {
        self.0.lock().unwrap().record_exit(&exit)
    }
    async fn exits(&self) -> Result<Vec<ServerExit>, SpoolError> {
        self.0.lock().unwrap().exits()
    }
    async fn forget_exits(&self, allocation_ids: Vec<String>) -> Result<(), SpoolError> {
        self.0.lock().unwrap().forget_exits(&allocation_ids)
    }
    async fn allocation_used(&self, allocation_id: String) -> Result<bool, SpoolError> {
        self.0.lock().unwrap().allocation_used(&allocation_id)
    }
    async fn record_allocation(&self, allocation_id: String) -> Result<(), SpoolError> {
        self.0.lock().unwrap().record_allocation(&allocation_id)
    }
    async fn close(&self) -> Result<(), SpoolError> {
        Ok(())
    }
}

const POISONED: &str = "00000000-0000-4000-8000-000000000000";

fn nth(index: usize) -> String {
    format!("00000000-0000-4000-8000-{index:012}")
}

/// The controller refuses every batch of one server (here a 409, as for a batch it cannot read) and takes
/// every other server's.
struct OneServerRefused;

#[async_trait]
impl EventsPost for OneServerRefused {
    async fn post(&self, _: &str, _: &[(&str, String)], body: String) -> Result<PostAnswer, UploadError> {
        if body.contains(POISONED) {
            return Ok(PostAnswer { status: 409, body: r#"{"code":"event_integrity_error","detail":"unreadable"}"#.to_owned() });
        }

        Ok(PostAnswer { status: 200, body: r#"{"rejected":[]}"#.to_owned() })
    }
}

fn stream(allocation: &str, count: i64) -> SpoolStream {
    SpoolStream {
        allocation_id: allocation.to_owned(),
        frames: (0..count)
            .map(|seq| SpooledFrame {
                seq,
                raw: json!({ "seq": seq, "event": { "type": "log", "timestamp": 1, "tick": 0, "level": "info", "message": "x" } }).to_string(),
            })
            .collect(),
    }
}

/// A server whose batches the controller always refuses holds back no other server: each backs off on its
/// own. Its events wait until the server has been idle for the retention period, and then go.
#[tokio::test(start_paused = true)]
async fn a_refused_server_holds_back_no_other_and_its_events_go_after_the_retention_period() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicI64::new(1_760_000_000_000));
    let reading = Arc::clone(&clock);
    let spool =
        EventSpool::open_with_clock(&dir.path().join("events/streams.sqlite"), &Logger::null(), Box::new(move || reading.load(Ordering::SeqCst))).unwrap();
    let spool = Arc::new(Direct(Mutex::new(spool)));

    spool.append(vec![stream(POISONED, 1)]).await.unwrap();

    let uploader = EventUploader::from_parts(UploaderParts {
        spool: Arc::clone(&spool) as Arc<dyn SpoolPort>,
        token: "t".to_owned(),
        target: Arc::new(|| EventsTarget { host: "controller.example".to_owned(), tls: true, events_path: "/api/node/events".to_owned() }),
        attached: Arc::new(Vec::new),
        on_backlog: Arc::new(|_| {}),
        log: Logger::null(),
        post: Arc::new(OneServerRefused),
        random: Arc::new(|| 0.5),
    });

    uploader.start().await;

    for index in 1..=5 {
        spool.append(vec![stream(&nth(index), 100)]).await.unwrap();
        // Long enough for every upload and for the refused server's longest backoff.
        tokio::time::sleep(Duration::from_mins(1)).await;
    }

    let waiting: Vec<(String, i64)> = spool.backlog().await.unwrap().into_iter().map(|server| (server.allocation_id, server.pending)).collect();

    assert_eq!(waiting, [(POISONED.to_owned(), 1)], "only the refused server's event waits");

    // The spool's clock jumps past the retention period; the uploader's hourly prune then runs once.
    clock.fetch_add(8 * 24 * 3_600_000, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_hours(1)).await;

    let backlog = spool.backlog().await.unwrap();

    uploader.close().await;
    assert!(backlog.is_empty(), "{backlog:?}");
}

/// The controller takes every batch and records whose it was, in order.
struct Recording(Mutex<Vec<String>>, Option<PruneDuring>);

/// Ages the spool past retention and prunes it while the controller answers one server's batch.
struct PruneDuring {
    allocation: String,
    /// A server whose plugin is still attached, which the prune keeps.
    keep: String,
    spool: Arc<Direct>,
    clock: Arc<AtomicI64>,
}

#[async_trait]
impl EventsPost for Recording {
    async fn post(&self, _: &str, _: &[(&str, String)], body: String) -> Result<PostAnswer, UploadError> {
        let batch: Value = serde_json::from_str(&body).unwrap();
        let allocation = batch["allocation_id"].as_str().unwrap().to_owned();

        if let Some(prune) = self.1.as_ref().filter(|prune| prune.allocation == allocation) {
            prune.clock.fetch_add(8 * 24 * 3_600_000, Ordering::SeqCst);
            prune.spool.prune(vec![prune.keep.clone()]).await.unwrap();
        }

        self.0.lock().unwrap().push(allocation);
        Ok(PostAnswer { status: 200, body: r#"{"rejected":[]}"#.to_owned() })
    }
}

fn direct_spool(dir: &std::path::Path) -> (Arc<Direct>, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(1_760_000_000_000));
    let reading = Arc::clone(&clock);
    let spool = EventSpool::open_with_clock(&dir.join("events/streams.sqlite"), &Logger::null(), Box::new(move || reading.load(Ordering::SeqCst))).unwrap();

    (Arc::new(Direct(Mutex::new(spool))), clock)
}

fn recording_uploader(spool: &Arc<Direct>, post: Arc<Recording>, log: Logger) -> EventUploader {
    EventUploader::from_parts(UploaderParts {
        spool: Arc::clone(spool) as Arc<dyn SpoolPort>,
        token: "t".to_owned(),
        target: Arc::new(|| EventsTarget { host: "controller.example".to_owned(), tls: true, events_path: "/api/node/events".to_owned() }),
        attached: Arc::new(Vec::new),
        on_backlog: Arc::new(|_| {}),
        log,
        post: post as Arc<dyn EventsPost>,
        random: Arc::new(|| 0.5),
    })
}

/// A server with a long backlog gets one batch per round, so a later server's events go out
/// without waiting for all of it.
#[tokio::test(start_paused = true)]
async fn uploads_one_batch_per_server_in_turn_so_a_long_backlog_holds_no_other_back() {
    let dir = tempfile::tempdir().unwrap();
    let (spool, _) = direct_spool(dir.path());
    let (busy, quiet) = (nth(1), nth(2));

    spool.append(vec![stream(&busy, 3 * 128)]).await.unwrap();
    spool.append(vec![stream(&quiet, 1)]).await.unwrap();

    let post = Arc::new(Recording(Mutex::default(), None));
    let uploader = recording_uploader(&spool, Arc::clone(&post), Logger::null());

    uploader.start().await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    uploader.close().await;

    assert_eq!(*post.0.lock().unwrap(), [busy.clone(), quiet, busy.clone(), busy]);
    assert!(spool.backlog().await.unwrap().is_empty());
}

/// Retention can drop a stream while the controller takes its batch. That server is done, the drop was
/// logged by the prune, and no other server backs off for it.
#[tokio::test(start_paused = true)]
async fn takes_a_stream_retention_dropped_mid_upload_as_done_and_holds_no_other_back() {
    let dir = tempfile::tempdir().unwrap();
    let (spool, clock) = direct_spool(dir.path());
    let (pruned, other) = (nth(1), nth(2));

    spool.append(vec![stream(&pruned, 1)]).await.unwrap();
    spool.append(vec![stream(&other, 1)]).await.unwrap();

    let prune = PruneDuring { allocation: pruned.clone(), keep: other.clone(), spool: Arc::clone(&spool), clock };
    let post = Arc::new(Recording(Mutex::default(), Some(prune)));
    let warnings: Arc<Mutex<Vec<String>>> = Arc::default();
    let warned = Arc::clone(&warnings);
    let log = Logger::new(
        "events",
        Some(Arc::new(move |entry: ankor_agent::logger::Entry| {
            if entry.level == ankor_agent::logger::Level::Warn {
                warned.lock().unwrap().push(entry.message);
            }
        })),
    );
    let uploader = recording_uploader(&spool, Arc::clone(&post), log);

    uploader.start().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    uploader.close().await;

    // The other server goes out in the same pass, as nothing failed.
    assert_eq!(*post.0.lock().unwrap(), [pruned, other]);
    assert!(warnings.lock().unwrap().is_empty(), "{:?}", warnings.lock().unwrap());
}
