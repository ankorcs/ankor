//! Demo upload: the file the game left in a slot's `demos/` shadow is the only record of it, so the
//! rules that matter are when it is deleted and when it is kept — a 401/403 keeps it for an operator,
//! an outage keeps it for the next attempt, and no upload target keeps it indefinitely.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::demo_uploader::{AbortSignal, DemoRequest, DemoSend, DemoUploadError, DemoUploader, DemoUploaderOptions, OnSent, PaceClock, paced};
use ankor_agent::failure::AgentFailure;
use ankor_agent::testing::until;
use ankor_protocol::config::DemoUpload;
use ankor_protocol::constants::{DEMO_OUTAGE_LIMIT, DEMO_RETRY_MIN, DEMO_STALL_TIMEOUT, DEMO_UPLOAD_ERROR_MAX_CHARS, MAX_REPORTED_DEMOS, demo_headers};
use ankor_protocol::types::{AgentDemoProgress, AgentErrorCode, DemoFile, DemoUploadHeader};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt as _, StreamExt as _};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const DEMO: &str = "20260922-183000_match-1_de_mirage.dem";

const ALLOCATION: &str = "11111111-1111-4111-8111-111111111111";

fn demos_dir(data_dir: &Path, slot_id: &str) -> PathBuf {
    data_dir.join("slots").join(slot_id).join("game/csgo/ankor/demos")
}

/// A recording in a launch's directory, or directly in the slot's `demos/` when `allocation_id` is `None`.
fn write_demo_in(data_dir: &Path, slot_id: &str, name: &str, contents: &[u8], allocation_id: Option<&str>) -> PathBuf {
    let mut dir = demos_dir(data_dir, slot_id);

    if let Some(allocation_id) = allocation_id {
        dir = dir.join(allocation_id);
    }

    std::fs::create_dir_all(&dir).unwrap();

    let file = dir.join(name);

    std::fs::write(&file, contents).unwrap();
    file
}

fn write_demo(data_dir: &Path, slot_id: &str, name: &str) -> PathBuf {
    write_demo_in(data_dir, slot_id, name, b"gotv-bytes", Some(ALLOCATION))
}

fn target(url: &str) -> DemoUpload {
    DemoUpload { url: url.to_owned(), header: None, limit_mbit: None }
}

/// A one-shot signal between a test and a fake upload.
#[derive(Clone, Default)]
struct Deferred(Arc<Notify>);

impl Deferred {
    fn resolve(&self) {
        self.0.notify_one();
    }

    async fn wait(&self) {
        self.0.notified().await;
    }
}

type Handler = Arc<dyn Fn(DemoRequest) -> BoxFuture<'static, Result<u16, String>> + Send + Sync>;

/// Answers each upload with what `handler` returns, counting them.
struct FakeSend {
    handler: Handler,
    calls: AtomicUsize,
}

#[async_trait]
impl DemoSend for FakeSend {
    async fn send(&self, request: DemoRequest) -> Result<u16, DemoUploadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.handler)(request).await.map_err(|message| DemoUploadError::Io(std::io::Error::other(message)))
    }
}

fn answering(handler: impl Fn(DemoRequest) -> BoxFuture<'static, Result<u16, String>> + Send + Sync + 'static) -> Arc<FakeSend> {
    Arc::new(FakeSend { handler: Arc::new(handler), calls: AtomicUsize::new(0) })
}

fn ok() -> Arc<FakeSend> {
    answering(|_| async { Ok(200) }.boxed())
}

/// A clock the test sets.
#[derive(Clone, Default)]
struct Now(Arc<AtomicI64>);

impl Now {
    fn at(start: i64) -> Self {
        Self(Arc::new(AtomicI64::new(start)))
    }

    fn set(&self, at: i64) {
        self.0.store(at, Ordering::SeqCst);
    }

    fn add(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    fn get(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }

    fn clock(&self) -> ankor_agent::node_log::Clock {
        let now = Arc::clone(&self.0);

        Arc::new(move || now.load(Ordering::SeqCst))
    }
}

/// Idle slots, no games, a fixed target, and uploads over HTTP.
fn options(data_dir: &Path, target: Option<DemoUpload>) -> DemoUploaderOptions {
    DemoUploaderOptions::new(data_dir.to_owned(), Arc::new(move || target.clone()), Arc::new(|_| None), Arc::new(|| false))
}

/// A target the test changes.
#[derive(Clone)]
struct Target(Arc<Mutex<Option<DemoUpload>>>);

impl Target {
    fn new(target: Option<DemoUpload>) -> Self {
        Self(Arc::new(Mutex::new(target)))
    }

    fn set(&self, target: Option<DemoUpload>) {
        *self.0.lock().unwrap() = target;
    }

    fn get(&self) -> Option<DemoUpload> {
        self.0.lock().unwrap().clone()
    }
}

fn options_with(data_dir: &Path, target: &Target) -> DemoUploaderOptions {
    let target = target.clone();

    DemoUploaderOptions::new(data_dir.to_owned(), Arc::new(move || target.get()), Arc::new(|_| None), Arc::new(|| false))
}

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// How a test server answers an upload.
#[derive(Clone, Copy)]
enum Answer {
    /// Reads the body, then answers the next status.
    Status,
    /// Answers `status` on the first byte, before the body finishes, as a size or auth check does,
    /// and keeps reading so unread request bytes cannot reset the connection.
    Early(u16),
    /// Reads the body and drops the connection without an answer, as a receiver that crashes would.
    Drop,
}

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    refusals: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Server {
    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn read_head(stream: &mut TcpStream) -> Option<(String, String, BTreeMap<String, String>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut request = httparse::Request::new(&mut headers);

            request.parse(&buffer).ok()?;

            // A repeated header is kept as one, its values joined with a comma.
            let mut fields: BTreeMap<String, String> = BTreeMap::new();

            for header in request.headers.iter() {
                let value = String::from_utf8_lossy(header.value).into_owned();

                fields.entry(header.name.to_ascii_lowercase()).and_modify(|joined| *joined = format!("{joined}, {value}")).or_insert(value);
            }
            let rest = buffer[end + 4..].to_vec();

            return Some((request.method?.to_owned(), request.path?.to_owned(), fields, rest));
        }

        let read = stream.read(&mut chunk).await.ok()?;

        if read == 0 {
            return None;
        }

        buffer.extend_from_slice(&chunk[..read]);
    }
}

async fn read_body(stream: &mut TcpStream, mut body: Vec<u8>, length: usize) -> Option<Vec<u8>> {
    let mut chunk = vec![0u8; 64 * 1024];

    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;

        if read == 0 {
            return None;
        }

        body.extend_from_slice(&chunk[..read]);
    }

    Some(body)
}

async fn respond(stream: &mut TcpStream, status: u16) -> bool {
    stream.write_all(format!("HTTP/1.1 {status} Status\r\ncontent-length: 0\r\n\r\n").as_bytes()).await.is_ok()
}

/// Records what each request carried and answers as `answer` says, with the next of `statuses`.
async fn demo_server_with(statuses: Vec<u16>, answer: Answer) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let refusals = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let server = Server {
        url: format!("http://127.0.0.1:{port}/api/plugin/demos"),
        requests: Arc::clone(&requests),
        refusals: Arc::clone(&refusals),
        drops: Arc::clone(&drops),
    };
    let statuses = Arc::new(statuses);

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (requests, refusals, drops, statuses) = (Arc::clone(&requests), Arc::clone(&refusals), Arc::clone(&drops), Arc::clone(&statuses));

            tokio::spawn(async move {
                while let Some((method, path, headers, rest)) = read_head(&mut stream).await {
                    let length: usize = headers.get("content-length").and_then(|length| length.parse().ok()).unwrap_or(0);

                    match answer {
                        Answer::Status => {
                            let Some(body) = read_body(&mut stream, rest, length).await else { return };
                            let count = {
                                let mut requests = requests.lock().unwrap();

                                requests.push(Recorded { method, path, headers, body });
                                requests.len()
                            };
                            let status = statuses.get(count - 1).or(statuses.last()).copied().unwrap_or(200);

                            if !respond(&mut stream, status).await {
                                return;
                            }
                        }
                        Answer::Early(status) => {
                            let rest = if rest.is_empty() && length > 0 { read_body(&mut stream, rest, 1).await } else { Some(rest) };
                            let Some(rest) = rest else { return };

                            refusals.fetch_add(1, Ordering::SeqCst);

                            if !respond(&mut stream, status).await || read_body(&mut stream, rest, length).await.is_none() {
                                return;
                            }
                        }
                        Answer::Drop => {
                            let _ = read_body(&mut stream, rest, length).await;

                            drops.fetch_add(1, Ordering::SeqCst);

                            return;
                        }
                    }
                }
            });
        }
    });

    server
}

async fn demo_server(statuses: &[u16]) -> Server {
    demo_server_with(statuses.to_vec(), Answer::Status).await
}

fn names(requests: &[Recorded]) -> Vec<String> {
    requests.iter().map(|request| request.header(demo_headers::FILE_NAME).unwrap_or_default().to_owned()).collect()
}

fn base_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

mod demo_uploader {
    use super::*;

    #[tokio::test]
    async fn backs_off_on_a_file_inspection_error_and_uploads_after_the_file_is_repaired() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);

        // A directory the agent can list but not search gives a real, persistent stat error. Root
        // would ignore the mode; the checks run as a non-root user.
        let dir = file.parent().unwrap().to_owned();

        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();

        let now = Now::default();
        let send = ok();
        let mut options = options(root.path(), Some(target("http://receiver/demos")));

        options.now = now.clock();
        options.send = Arc::clone(&send) as Arc<dyn DemoSend>;

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 0);
        assert_eq!(uploader.list().await.unwrap_err().raw_os_error(), Some(nix::errno::Errno::EACCES as i32));

        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        now.set(29_999);
        uploader.scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 0);
        assert!(file.exists());

        now.set(30_000);
        uploader.scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 1);
        assert!(!file.exists());
    }

    /// The game can write the demo directory, so a demo it replaced with a link to a file of the
    /// node, such as `agent.yml`, must not be uploaded or listed.
    #[tokio::test]
    async fn neither_uploads_nor_lists_a_demo_that_is_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("agent.yml");
        let file = write_demo(root.path(), "a", DEMO);

        std::fs::write(&secret, b"token: node-secret-token\n").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&secret, &file).unwrap();

        let send = ok();
        let mut options = options(root.path(), Some(target("http://receiver/demos")));

        options.send = Arc::clone(&send) as Arc<dyn DemoSend>;

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 0);
        assert_eq!(uploader.list().await.unwrap(), []);
        assert_eq!(std::fs::read(&secret).unwrap(), b"token: node-secret-token\n");
    }

    #[tokio::test]
    async fn skips_a_demo_deleted_between_discovery_and_file_inspection() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let send = ok();
        let mut options = options(root.path(), Some(target("http://receiver/demos")));
        let doomed = file.clone();

        options.send = Arc::clone(&send) as Arc<dyn DemoSend>;
        options.running_allocation = Arc::new(move |_| {
            if doomed.exists() {
                std::fs::remove_file(&doomed).unwrap();
            }

            None
        });

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 0);
        assert_eq!(uploader.list().await.unwrap(), []);
    }

    #[tokio::test]
    async fn skips_a_demo_deleted_between_the_listing_and_its_upload_without_backing_off() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let send = ok();
        let reads = Arc::new(AtomicUsize::new(0));
        let (doomed, counted) = (file.clone(), Arc::clone(&reads));
        let mut options = options(root.path(), Some(target("http://receiver/demos")));

        options.send = Arc::clone(&send) as Arc<dyn DemoSend>;
        // The pass reads the target once before it lists the demos and again after: the demo goes in between.
        options.get_target = Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 1 && doomed.exists() {
                std::fs::remove_file(&doomed).unwrap();
            }

            Some(target("http://receiver/demos"))
        });

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 0);
        assert!(!file.exists());
        assert_eq!(uploader.list().await.unwrap(), []);

        // No backoff: the next demo goes out on the next pass.
        write_demo(root.path(), "a", "20260922-190000_match-2_de_mirage.dem");
        uploader.scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 1);
    }

    /// Asked between the pass's two target reads, `get_target` puts a directory where the listed
    /// demo was. The pass skips it as no demo, without sending anything.
    #[tokio::test]
    async fn skips_a_demo_replaced_by_a_directory_between_the_listing_and_its_upload() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let send = ok();
        let reads = Arc::new(AtomicUsize::new(0));
        let (replaced, counted) = (file.clone(), Arc::clone(&reads));
        let mut options = options(root.path(), Some(target("http://receiver/demos")));

        options.send = Arc::clone(&send) as Arc<dyn DemoSend>;
        options.get_target = Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 1 && replaced.is_file() {
                std::fs::remove_file(&replaced).unwrap();
                std::fs::create_dir(&replaced).unwrap();
            }

            Some(target("http://receiver/demos"))
        });

        DemoUploader::new(options).scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 0);
        assert!(file.is_dir());
    }

    /// Fails the way a demo deleted after it was opened for upload fails.
    struct Vanishing;

    #[async_trait]
    impl DemoSend for Vanishing {
        async fn send(&self, request: DemoRequest) -> Result<u16, DemoUploadError> {
            std::fs::remove_file(&request.file_path).unwrap();
            Err(DemoUploadError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)))
        }
    }

    #[tokio::test]
    async fn takes_a_demo_whose_file_goes_during_its_upload_as_done_without_backing_off() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let mut options = options(root.path(), Some(target("http://receiver/demos")));

        options.send = Arc::new(Vanishing);

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        assert!(!file.exists());

        // No backoff: the next demo goes out on the next pass.
        let next = write_demo(root.path(), "a", "20260922-190000_match-2_de_mirage.dem");

        uploader.scan_all().await.unwrap();
        assert!(!next.exists(), "the next demo waited for a backoff");
    }

    #[tokio::test]
    async fn uploads_demos_with_optional_match_id() {
        for match_id in ["external-match", ""] {
            let root = tempfile::tempdir().unwrap();
            let name = format!("20260922-175959_{match_id}_de_dust2.dem");
            let file = write_demo(root.path(), "a", &name);
            let server = demo_server(&[200]).await;
            let uploader = DemoUploader::new(options(root.path(), Some(target(&server.url))));

            uploader.scan_all().await.unwrap();

            let requests = server.requests();

            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].header(demo_headers::MATCH_ID), Some(match_id).filter(|id| !id.is_empty()));
            assert_eq!(requests[0].header(demo_headers::FILE_NAME), Some(name.as_str()));
            assert!(!file.exists());
        }
    }

    #[tokio::test]
    async fn streams_the_demo_with_its_match_headers_to_the_configured_target_then_removes_it() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo_in(root.path(), "a", DEMO, "x".repeat(2048).as_bytes(), Some(ALLOCATION));
        let server = demo_server(&[200]).await;
        let progress: Arc<Mutex<Vec<AgentDemoProgress>>> = Arc::default();
        let reports = Arc::clone(&progress);
        let clock = Arc::new(AtomicI64::new(0));
        let mut options = options(root.path(), Some(target(&server.url)));

        options.now = Arc::new(move || clock.fetch_add(1000, Ordering::SeqCst) + 1000);
        options.on_progress = Some(Arc::new(move |report| reports.lock().unwrap().push(report)));
        DemoUploader::new(options).scan_all().await.unwrap();

        let requests = server.requests();

        assert_eq!(requests.len(), 1);

        let sent = &requests[0];

        assert_eq!(progress.lock().unwrap().iter().map(|report| report.sent_bytes).collect::<Vec<_>>(), [2048]);
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.path, "/api/plugin/demos");
        assert_eq!(sent.header("content-type"), Some("application/octet-stream"));
        assert_eq!(sent.header("content-length"), Some("2048"));
        assert_eq!(sent.header(demo_headers::FILE_NAME), Some(DEMO));
        assert_eq!(sent.header(demo_headers::MATCH_ID), Some("match-1"));
        assert_eq!(sent.header(demo_headers::ALLOCATION_ID), Some(ALLOCATION));
        // The node's token authenticates the node to the controller; a demo target is its own arrangement.
        assert_eq!(sent.header("authorization"), None);
        assert_eq!(sent.body, "x".repeat(2048).as_bytes());
        // The node keeps no copy: the controller has it, and a game host fills up.
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn takes_demos_only_from_a_launchs_directory_and_names_its_server() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        // Any other directory is no launch's, and what it and `demos/` itself hold is left alone.
        let stray_dir = write_demo_in(root.path(), "a", "20260922-190000_match-2_de_nuke.dem", b"gotv-bytes", Some("not-an-allocation"));
        let stray_root = write_demo_in(root.path(), "a", "20260922-200000_match-3_de_ancient.dem", b"gotv-bytes", None);
        let server = demo_server(&[200]).await;
        let uploader = DemoUploader::new(options(root.path(), Some(target(&server.url))));

        assert_eq!(
            uploader.list().await.unwrap().iter().map(|demo| (demo.slot_id.clone(), demo.file_name.clone())).collect::<Vec<_>>(),
            [("a".to_owned(), DEMO.to_owned())]
        );
        uploader.scan_all().await.unwrap();

        let sent: Vec<_> = server
            .requests()
            .iter()
            .map(|request| (request.header(demo_headers::FILE_NAME).map(str::to_owned), request.header(demo_headers::ALLOCATION_ID).map(str::to_owned)))
            .collect();

        assert_eq!(sent, [(Some(DEMO.to_owned()), Some(ALLOCATION.to_owned()))]);
        assert!(!file.exists());
        assert!(stray_dir.exists());
        assert!(stray_root.exists());
    }

    #[tokio::test]
    async fn lists_the_demo_it_uploads_reports_its_progress_once_a_second_and_says_when_the_listing_changed() {
        let root = tempfile::tempdir().unwrap();

        write_demo_in(root.path(), "a", DEMO, "x".repeat(4096).as_bytes(), Some(ALLOCATION));

        let clock = Now::default();
        let changes = Arc::new(AtomicUsize::new(0));
        let progress: Arc<Mutex<Vec<AgentDemoProgress>>> = Arc::default();
        let (midway, release) = (Deferred::default(), Deferred::default());
        let mut options = options(root.path(), Some(target("https://demos.example/upload")));
        let (seen, reports, counted) = (Arc::clone(&progress), Arc::clone(&progress), Arc::clone(&changes));
        let (set, reached, released) = (clock.clone(), midway.clone(), release.clone());

        options.now = clock.clock();
        options.on_change = Some(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        options.on_progress = Some(Arc::new(move |report| reports.lock().unwrap().push(report)));
        options.send = answering(move |request| {
            let (seen, set, reached, released) = (Arc::clone(&seen), set.clone(), reached.clone(), released.clone());

            async move {
                (request.on_sent)(1024);
                // Inside the one-second window: no report yet.
                assert!(seen.lock().unwrap().is_empty());
                set.set(1000);
                (request.on_sent)(3072);
                set.set(1500);
                (request.on_sent)(4096);
                reached.resolve();
                released.wait().await;
                set.set(2000);
                (request.on_sent)(5120);

                Ok(200)
            }
            .boxed()
        });

        let uploader = DemoUploader::new(options);
        let scan = uploader.scan_all();

        midway.wait().await;
        assert_eq!(
            *progress.lock().unwrap(),
            [AgentDemoProgress { slot_id: "a".to_owned(), file_name: DEMO.to_owned(), sent_bytes: 3072, bytes_per_second: 3072 }]
        );
        // The upload's start only: progress leaves the listing alone.
        assert_eq!(changes.load(Ordering::SeqCst), 1);

        let listed = uploader.list().await.unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0],
            DemoFile {
                slot_id: "a".to_owned(),
                file_name: DEMO.to_owned(),
                size_bytes: 4096,
                modified_at: listed[0].modified_at,
                recording: false,
                uploading: true,
                upload_error: None
            }
        );
        release.resolve();
        scan.await.unwrap();
        assert_eq!(
            progress.lock().unwrap()[1],
            AgentDemoProgress { slot_id: "a".to_owned(), file_name: DEMO.to_owned(), sent_bytes: 5120, bytes_per_second: 2048 }
        );
        assert_eq!(uploader.list().await.unwrap(), []);
        // The upload's end, then the pass's end.
        assert_eq!(changes.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn sends_the_configured_header_verbatim() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", DEMO);

        let server = demo_server(&[200]).await;
        let target = DemoUpload {
            url: server.url.clone(),
            header: Some(DemoUploadHeader { name: "X-Demo-Key".to_owned(), value: "opensesame".to_owned() }),
            limit_mbit: None,
        };

        DemoUploader::new(options(root.path(), Some(target))).scan_all().await.unwrap();

        assert_eq!(server.requests()[0].header("x-demo-key"), Some("opensesame"));
    }

    /// The contract refuses these names, and the agent sends its own value even if one gets through.
    #[tokio::test]
    async fn sends_its_own_headers_over_a_configured_one_of_the_same_name_whatever_its_case() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", DEMO);

        for (name, check) in [("Content-Type", "content-type"), ("host", "host"), ("ANKOR-ALLOCATION-ID", "ankor-allocation-id")] {
            let server = demo_server(&[200]).await;
            let target = DemoUpload {
                url: server.url.clone(),
                header: Some(DemoUploadHeader { name: name.to_owned(), value: "from the target".to_owned() }),
                limit_mbit: None,
            };

            write_demo(root.path(), "a", DEMO);
            DemoUploader::new(options(root.path(), Some(target))).scan_all().await.unwrap();

            assert_ne!(server.requests()[0].header(check), Some("from the target"), "{name}");
        }
    }

    /// Basic authorization is a header like any other; the URL carries no credentials.
    #[tokio::test]
    async fn sends_basic_authorization_as_the_header_it_is_given() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", DEMO);

        let server = demo_server(&[200]).await;
        // "uploader:p@ss"
        let header = DemoUploadHeader { name: "Authorization".to_owned(), value: "Basic dXBsb2FkZXI6cEBzcw==".to_owned() };
        let target = DemoUpload { url: server.url.clone(), header: Some(header), limit_mbit: None };

        DemoUploader::new(options(root.path(), Some(target))).scan_all().await.unwrap();

        assert_eq!(server.requests()[0].header("authorization"), Some("Basic dXBsb2FkZXI6cEBzcw=="));
    }

    #[tokio::test]
    async fn uploads_what_it_can_name_and_leaves_anything_else_where_it_is() {
        let root = tempfile::tempdir().unwrap();
        let named = write_demo(root.path(), "a", "20260922-175959_match-1_de_dust2.dem");
        let strange = write_demo(root.path(), "a", "hand-made.dem");
        let partial = write_demo(root.path(), "a", "20260922-175959_match-1_de_dust2.dem.part");
        let server = demo_server(&[200]).await;
        let uploader = DemoUploader::new(options(root.path(), Some(target(&server.url))));

        uploader.scan_all().await.unwrap();

        assert_eq!(names(&server.requests()), ["20260922-175959_match-1_de_dust2.dem"]);
        assert!(!named.exists());
        assert!(strange.exists());
        assert!(partial.exists());
    }

    #[tokio::test]
    async fn takes_every_slot_on_the_node_one_upload_at_a_time() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", "20260922-175959_match-1_de_dust2.dem");
        write_demo(root.path(), "b", "20260921-120000_match-2_de_nuke.dem");

        let in_flight = Arc::new(AtomicUsize::new(0));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let uploaded: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));
        let (flying, most, done) = (Arc::clone(&in_flight), Arc::clone(&concurrent), Arc::clone(&uploaded));

        options.send = answering(move |request| {
            let (flying, most, done) = (Arc::clone(&flying), Arc::clone(&most), Arc::clone(&done));

            async move {
                let now = flying.fetch_add(1, Ordering::SeqCst) + 1;

                most.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                done.lock().unwrap().push(base_name(&request.file_path));
                flying.fetch_sub(1, Ordering::SeqCst);

                Ok(200)
            }
            .boxed()
        });
        DemoUploader::new(options).scan_all().await.unwrap();

        let mut uploaded = uploaded.lock().unwrap().clone();

        uploaded.sort();
        assert_eq!(uploaded, ["20260921-120000_match-2_de_nuke.dem", "20260922-175959_match-1_de_dust2.dem"]);
        // A demo is hundreds of megabytes and the node is also running games.
        assert_eq!(concurrent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lets_an_active_upload_finish_when_the_target_changes() {
        for disable in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let first = write_demo(root.path(), "a", "20260921-120000_match-1_de_nuke.dem");
            let second = write_demo(root.path(), "a", DEMO);
            let (started, finish) = (Deferred::default(), Deferred::default());
            let original = target("https://old.example/upload");
            let current = Target::new(Some(original.clone()));
            let requests: Arc<Mutex<Vec<(String, AbortSignal)>>> = Arc::default();
            let mut options = options_with(root.path(), &current);
            let (seen, begun, finished) = (Arc::clone(&requests), started.clone(), finish.clone());

            options.send = answering(move |request| {
                let (seen, begun, finished) = (Arc::clone(&seen), begun.clone(), finished.clone());

                async move {
                    let count = {
                        let mut seen = seen.lock().unwrap();

                        seen.push((request.url.clone(), request.signal.clone()));
                        seen.len()
                    };

                    if count == 1 {
                        begun.resolve();
                        finished.wait().await;
                    }

                    Ok(200)
                }
                .boxed()
            });

            let uploader = DemoUploader::new(options);
            let drain = uploader.scan_all();

            started.wait().await;

            let changed = if disable { None } else { Some(target("https://new.example/upload")) };

            current.set(changed.clone());

            let saved = uploader.scan_all();

            assert_eq!(requests.lock().unwrap().len(), 1);
            assert!(!requests.lock().unwrap()[0].1.aborted());
            finish.resolve();
            drain.await.unwrap();
            saved.await.unwrap();

            let urls: Vec<_> = requests.lock().unwrap().iter().map(|(url, _)| url.clone()).collect();

            assert_eq!(urls, if disable { vec![original.url.clone()] } else { vec![original.url.clone(), changed.unwrap().url] });
            assert!(!first.exists());
            assert_eq!(second.exists(), disable);
        }
    }

    #[tokio::test]
    async fn retries_against_a_changed_target_without_inheriting_the_old_request_failure() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let current = Target::new(Some(target("https://old.example/upload")));
        let requested: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options_with(root.path(), &current);
        let (seen, changing) = (Arc::clone(&requested), current.clone());

        options.send = answering(move |request| {
            let count = {
                let mut seen = seen.lock().unwrap();

                seen.push(request.url.clone());
                seen.len()
            };

            if count == 1 {
                changing.set(Some(target("https://new.example/upload")));
            }

            async move { Ok(if count == 1 { 404 } else { 200 }) }.boxed()
        });
        DemoUploader::new(options).scan_all().await.unwrap();

        assert_eq!(*requested.lock().unwrap(), ["https://old.example/upload", "https://new.example/upload"]);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn sends_a_demo_to_the_target_that_changed_while_it_looked_for_demos() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", DEMO);

        let current = Target::new(Some(target("https://old.example/upload")));
        let requested: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options_with(root.path(), &current);
        let (seen, changing) = (Arc::clone(&requested), current.clone());

        options.running_allocation = Arc::new(move |_| {
            changing.set(Some(target("https://new.example/upload")));

            None
        });
        options.send = answering(move |request| {
            seen.lock().unwrap().push(request.url.clone());

            async { Ok(200) }.boxed()
        });
        DemoUploader::new(options).scan_all().await.unwrap();

        assert_eq!(*requested.lock().unwrap(), ["https://new.example/upload"]);
    }

    #[tokio::test]
    async fn cuts_a_long_failure_to_what_the_listing_carries() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", DEMO);

        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));
        let longest = usize::try_from(DEMO_UPLOAD_ERROR_MAX_CHARS).unwrap();

        options.send = answering(move |_| async move { Err("x".repeat(longest + 100)) }.boxed());

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        assert_eq!(uploader.list().await.unwrap()[0].upload_error.as_ref().unwrap().len(), longest);
    }

    #[tokio::test]
    async fn paces_a_real_upload_while_a_game_runs() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let server = demo_server(&[200]).await;
        let mut options = options(root.path(), Some(DemoUpload { url: server.url.clone(), header: None, limit_mbit: Some(8) }));

        options.games_running = Arc::new(|| true);
        DemoUploader::new(options).scan_all().await.unwrap();

        assert_eq!(server.requests().iter().map(|request| request.body.clone()).collect::<Vec<_>>(), [b"gotv-bytes".to_vec()]);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn keeps_a_demo_refused_with_http_status_until_the_target_changes() {
        for status in [401, 403] {
            let root = tempfile::tempdir().unwrap();
            let file = write_demo(root.path(), "a", DEMO);
            let server = demo_server(&[status, 200]).await;
            let current = Target::new(Some(target(&server.url)));
            let uploader = DemoUploader::new(options_with(root.path(), &current));

            uploader.scan_all().await.unwrap();
            // Unauthorized uploads wait for an operator to change the target or credentials.
            uploader.scan_all().await.unwrap();
            uploader.scan_all().await.unwrap();

            assert_eq!(server.requests().len(), 1);
            assert!(file.exists());

            current.set(Some(DemoUpload {
                url: server.url.clone(),
                header: Some(DemoUploadHeader { name: "Authorization".to_owned(), value: "Bearer fixed".to_owned() }),
                limit_mbit: None,
            }));
            uploader.scan_all().await.unwrap();

            assert_eq!(server.requests().len(), 2);
            assert!(!file.exists());
        }
    }

    #[tokio::test]
    async fn skips_a_demo_refused_as_invalid_with_http_422_uploads_the_next_and_offers_it_again_to_a_new_target() {
        let root = tempfile::tempdir().unwrap();
        let invalid = write_demo(root.path(), "a", "20260922-183000_match-1_de_mirage.dem");
        let next = write_demo(root.path(), "a", "20260922-190000_match-2_de_inferno.dem");
        let server = demo_server(&[422, 200, 200]).await;
        let current = Target::new(Some(target(&server.url)));
        let removed: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options_with(root.path(), &current);
        let gone = Arc::clone(&removed);

        options.on_uploaded = Some(Arc::new(move |_, file_name| gone.lock().unwrap().push(file_name.to_owned())));

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        uploader.scan_all().await.unwrap();

        // The refused one says why it stays; the uploaded one says how it left.
        let listed = uploader.list().await.unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].file_name, "20260922-183000_match-1_de_mirage.dem");
        assert!(listed[0].upload_error.as_ref().unwrap().contains("HTTP 422"));
        assert_eq!(*removed.lock().unwrap(), ["20260922-190000_match-2_de_inferno.dem"]);
        assert_eq!(names(&server.requests()), ["20260922-183000_match-1_de_mirage.dem", "20260922-190000_match-2_de_inferno.dem"]);
        assert!(invalid.exists());
        assert!(!next.exists());

        current.set(Some(DemoUpload {
            url: server.url.clone(),
            header: Some(DemoUploadHeader { name: "Authorization".to_owned(), value: "Bearer other".to_owned() }),
            limit_mbit: None,
        }));
        uploader.scan_all().await.unwrap();

        assert_eq!(server.requests().len(), 3);
        assert!(!invalid.exists());
    }

    #[tokio::test]
    async fn stops_the_whole_pass_on_http_status_asking_again_only_for_a_demo_recorded_after_it() {
        for status in [401, 403] {
            let root = tempfile::tempdir().unwrap();
            let waiting: Vec<PathBuf> = (0..25).map(|index| write_demo(root.path(), "a", &format!("20260922-1830{index:02}_match-1_de_mirage.dem"))).collect();
            let server = demo_server(&[status, status]).await;
            let uploader = DemoUploader::new(options(root.path(), Some(target(&server.url))));

            uploader.scan_all().await.unwrap();
            uploader.scan_all().await.unwrap();
            assert_eq!(server.requests().len(), 1);

            // Every waiting demo says why it waits.
            let errors: Vec<_> = uploader.list().await.unwrap().into_iter().map(|demo| demo.upload_error.unwrap_or_default()).collect();

            assert_eq!(errors.len(), waiting.len());
            assert!(errors.iter().all(|error| error.contains(&format!("HTTP {status}"))), "{errors:?}");

            write_demo(root.path(), "a", "20260922-190000_match-2_de_inferno.dem");
            uploader.scan_all().await.unwrap();
            assert_eq!(server.requests().len(), 2);
            assert!(waiting.iter().all(|file| file.exists()));
        }
    }

    #[tokio::test]
    async fn says_why_a_demo_waits_for_a_retry_and_forgets_it_once_an_upload_works() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", "20260922-183000_match-1_de_mirage.dem");

        let server = demo_server(&[503, 200]).await;
        let now = Now::at(1_000_000);
        let current = Target::new(Some(target(&server.url)));
        let mut options = options_with(root.path(), &current);

        options.now = now.clock();

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();

        let listed = uploader.list().await.unwrap();

        assert_eq!(listed.len(), 1);
        assert!(!listed[0].uploading);
        assert_eq!(listed[0].upload_error.as_deref(), Some(format!("HTTP 503; retrying in {} s", DEMO_RETRY_MIN.as_secs()).as_str()));

        // A new target is a fresh start, so the old target's failure no longer applies.
        current.set(None);
        uploader.scan_all().await.unwrap();
        assert_eq!(uploader.list().await.unwrap()[0].upload_error, None);
        current.set(Some(target(&server.url)));

        now.add(i64::try_from(DEMO_RETRY_MIN.as_millis()).unwrap());
        uploader.scan_all().await.unwrap();
        assert_eq!(uploader.list().await.unwrap(), []);
    }

    #[tokio::test]
    async fn keeps_the_demo_and_tries_again_when_the_target_drops_the_connection_after_reading_the_body() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        // A receiver that crashes or restarts once it has the upload, before it answers.
        let server = demo_server_with(vec![], Answer::Drop).await;
        let elapsed = Now::default();
        let mut options = options(root.path(), Some(target(&server.url)));

        options.now = elapsed.clock();

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        assert_eq!(server.drops.load(Ordering::SeqCst), 1);
        assert!(file.exists());
        elapsed.set(30_000);
        uploader.scan_all().await.unwrap();
        assert_eq!(server.drops.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn uses_http_status_received_mid_body_to_decide_whether_to_retry() {
        for status in [401, 403, 404, 422] {
            let root = tempfile::tempdir().unwrap();
            // Large enough that the answer lands while the body is still going out.
            let file = write_demo_in(root.path(), "a", DEMO, &vec![b'x'; 4 << 20], Some(ALLOCATION));
            let server = demo_server_with(vec![], Answer::Early(status)).await;
            let elapsed = Now::default();
            let mut options = options(root.path(), Some(target(&server.url)));

            options.now = elapsed.clock();

            let uploader = DemoUploader::new(options);

            uploader.scan_all().await.unwrap();
            elapsed.set(29_000);
            uploader.scan_all().await.unwrap();

            assert_eq!(server.refusals.load(Ordering::SeqCst), 1, "HTTP {status}");
            elapsed.set(30_000);
            uploader.scan_all().await.unwrap();
            assert_eq!(server.refusals.load(Ordering::SeqCst), if status == 404 { 2 } else { 1 }, "HTTP {status}");
            assert!(file.exists());
        }
    }

    #[tokio::test]
    async fn leaves_a_slot_that_is_still_running_alone_and_takes_it_when_the_container_exits() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let server = demo_server(&[200]).await;
        let busy = Arc::new(AtomicBool::new(true));
        let mut options = options(root.path(), Some(target(&server.url)));
        let running = Arc::clone(&busy);

        options.running_allocation = Arc::new(move |slot_id| (slot_id == "a" && running.load(Ordering::SeqCst)).then(|| ALLOCATION.to_owned()));

        let uploader = DemoUploader::new(options);

        // Boot, with a game the agent adopted still recording into this file.
        uploader.scan_all().await.unwrap();
        assert_eq!(server.requests().len(), 0);
        assert!(file.exists());

        busy.store(false, Ordering::SeqCst);
        uploader.scan_all().await.unwrap();

        assert_eq!(server.requests().len(), 1);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn retries_http_status_on_a_lengthening_node_wide_backoff_and_uploads_when_it_recovers() {
        for status in [199, 300, 302, 400, 404, 409, 413, 426, 429, 500, 503] {
            let root = tempfile::tempdir().unwrap();
            let file = write_demo(root.path(), "a", DEMO);
            let statuses = [status, status, 200];
            let attempts: Arc<Mutex<Vec<i64>>> = Arc::default();
            let elapsed = Now::default();
            let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));
            let (seen, at) = (Arc::clone(&attempts), elapsed.clone());

            options.now = elapsed.clock();
            options.send = answering(move |_| {
                let count = {
                    let mut seen = seen.lock().unwrap();

                    seen.push(at.get());
                    seen.len()
                };
                let status = statuses.get(count - 1).copied().unwrap_or(200);

                async move { Ok(status) }.boxed()
            });

            let uploader = DemoUploader::new(options);

            // Every scan trigger runs a pass; one inside the backoff does nothing.
            for step in [0, 29_000, 1_000, 59_000, 1_000] {
                elapsed.add(step);
                uploader.scan_all().await.unwrap();
            }

            assert_eq!(*attempts.lock().unwrap(), [0, 30_000, 90_000], "HTTP {status}");
            assert!(!file.exists());
        }
    }

    /// Answers every upload with `status`, recording which demo it carried.
    fn recording(status: &Arc<AtomicUsize>, attempts: &Arc<Mutex<Vec<String>>>) -> Arc<FakeSend> {
        let (status, attempts) = (Arc::clone(status), Arc::clone(attempts));

        answering(move |request| {
            attempts.lock().unwrap().push(base_name(&request.file_path));

            let status = u16::try_from(status.load(Ordering::SeqCst)).unwrap();

            async move { Ok(status) }.boxed()
        })
    }

    #[tokio::test]
    async fn holds_the_whole_queue_oldest_recording_first_while_the_target_fails() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", "20260922-175959_match-1_de_dust2.dem");
        write_demo(root.path(), "b", "20260921-120000_match-2_de_nuke.dem");

        let attempts: Arc<Mutex<Vec<String>>> = Arc::default();
        let elapsed = Now::default();
        let status = Arc::new(AtomicUsize::new(500));
        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));

        options.now = elapsed.clock();
        options.send = recording(&status, &attempts);

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        // A new recording waits its turn behind the backoff with the rest.
        write_demo(root.path(), "a", "20260923-100000_match-3_de_inferno.dem");
        uploader.scan_all().await.unwrap();
        elapsed.add(30_000);
        status.store(200, Ordering::SeqCst);
        uploader.scan_all().await.unwrap();

        assert_eq!(
            *attempts.lock().unwrap(),
            [
                "20260921-120000_match-2_de_nuke.dem",
                "20260921-120000_match-2_de_nuke.dem",
                "20260922-175959_match-1_de_dust2.dem",
                "20260923-100000_match-3_de_inferno.dem",
            ]
        );
    }

    #[tokio::test]
    async fn keeps_every_waiting_demo_after_a_day_of_http_status_until_another_is_recorded() {
        for initial in [404, 502] {
            let root = tempfile::tempdir().unwrap();
            let file = write_demo(root.path(), "a", DEMO);
            let attempts: Arc<Mutex<Vec<String>>> = Arc::default();
            // A real clock: an outage measured from 0 would hide one never started.
            let start = 1_700_000_000_000;
            let elapsed = Now::at(start);
            let status = Arc::new(AtomicUsize::new(initial));
            let day = 24 * 60 * 60 * 1000;
            let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));

            options.now = elapsed.clock();
            options.send = recording(&status, &attempts);

            let uploader = DemoUploader::new(options);

            while elapsed.get() <= start + day {
                uploader.scan_all().await.unwrap();
                elapsed.add(15 * 60 * 1000);
            }

            let given = attempts.lock().unwrap().len();

            assert!(given > 90, "{given}");
            assert!(file.exists());
            assert!(
                uploader.list().await.unwrap()[0]
                    .upload_error
                    .as_ref()
                    .unwrap()
                    .contains(&format!("uploads have failed for {} h", DEMO_OUTAGE_LIMIT.as_secs() / 3600))
            );
            uploader.scan_all().await.unwrap();
            assert_eq!(attempts.lock().unwrap().len(), given);

            // A new recording is a new try for the whole queue, oldest first.
            status.store(200, Ordering::SeqCst);
            write_demo(root.path(), "b", "20260923-100000_match-3_de_inferno.dem");
            uploader.scan_all().await.unwrap();
            assert_eq!(attempts.lock().unwrap()[given..], [DEMO.to_owned(), "20260923-100000_match-3_de_inferno.dem".to_owned()]);
            assert!(!file.exists());
        }
    }

    #[tokio::test]
    async fn backs_off_from_the_shortest_wait_again_after_an_upload_succeeds() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", DEMO);

        let attempts: Arc<Mutex<Vec<String>>> = Arc::default();
        let elapsed = Now::at(1_700_000_000_000);
        let status = Arc::new(AtomicUsize::new(500));
        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));
        let retry = i64::try_from(DEMO_RETRY_MIN.as_millis()).unwrap();

        options.now = elapsed.clock();
        options.send = recording(&status, &attempts);

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        elapsed.add(retry);
        status.store(200, Ordering::SeqCst);
        uploader.scan_all().await.unwrap();
        assert_eq!(*attempts.lock().unwrap(), [DEMO, DEMO]);

        status.store(500, Ordering::SeqCst);
        write_demo(root.path(), "b", "20260923-100000_match-3_de_inferno.dem");
        uploader.scan_all().await.unwrap();
        elapsed.add(retry - 1);
        uploader.scan_all().await.unwrap();
        assert_eq!(attempts.lock().unwrap().len(), 3);
        elapsed.add(1);
        uploader.scan_all().await.unwrap();
        assert_eq!(attempts.lock().unwrap().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn aborts_an_upload_that_has_sent_nothing_for_two_minutes_and_keeps_the_demo() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let upload: Arc<Mutex<Option<(OnSent, AbortSignal)>>> = Arc::default();
        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));
        let held = Arc::clone(&upload);

        options.send = answering(move |request| {
            *held.lock().unwrap() = Some((Arc::clone(&request.on_sent), request.signal.clone()));

            async move {
                request.signal.cancelled().await;

                Err(request.signal.reason())
            }
            .boxed()
        });

        let uploader = DemoUploader::new(options);
        let pass = uploader.scan_all();

        until("the upload", || upload.lock().unwrap().is_some()).await;

        let (on_sent, signal) = upload.lock().unwrap().clone().unwrap();

        // Each write restarts the wait, so a slow upload that keeps moving runs on.
        for sent in 1..=3 {
            tokio::time::sleep(DEMO_STALL_TIMEOUT.checked_sub(Duration::from_secs(1)).unwrap()).await;
            on_sent(sent);
        }

        assert!(!signal.aborted());
        tokio::time::sleep(DEMO_STALL_TIMEOUT).await;

        // The timers due by now run before the check.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(signal.aborted());
        pass.await.unwrap();
        assert!(file.exists());
        assert!(uploader.list().await.unwrap()[0].upload_error.as_ref().unwrap().contains(&format!("no progress for {} s", DEMO_STALL_TIMEOUT.as_secs())));
    }

    #[tokio::test]
    async fn limits_the_upload_to_the_targets_rate_only_while_a_game_runs() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", DEMO);
        write_demo(root.path(), "a", "20260922-190000_match-2_de_inferno.dem");

        let games = Arc::new(AtomicBool::new(true));
        let limit_mbit = Arc::new(Mutex::new(Some(8)));
        let limits: Arc<Mutex<Vec<Option<f64>>>> = Arc::default();
        let (running, limited) = (Arc::clone(&games), Arc::clone(&limit_mbit));
        let mut options = DemoUploaderOptions::new(
            root.path().to_owned(),
            Arc::new(move || {
                Some(DemoUpload { url: "https://controller.example/api/plugin/demos".to_owned(), header: None, limit_mbit: *limited.lock().unwrap() })
            }),
            Arc::new(|_| None),
            Arc::new(move || running.load(Ordering::SeqCst)),
        );
        let (seen, flip, unlimited) = (Arc::clone(&limits), Arc::clone(&games), Arc::clone(&limit_mbit));

        options.send = answering(move |request| {
            seen.lock().unwrap().push((request.limit)());
            flip.fetch_xor(true, Ordering::SeqCst);
            // Read per write: the last game ending mid-upload lifts it.
            seen.lock().unwrap().push((request.limit)());
            flip.store(true, Ordering::SeqCst);
            *unlimited.lock().unwrap() = None;

            async { Ok(200) }.boxed()
        });
        DemoUploader::new(options).scan_all().await.unwrap();

        // 8 Mbit/s is a million bytes a second; a target without a limit has none, games or not.
        assert_eq!(*limits.lock().unwrap(), [Some(1_000_000.0), None, None, None]);
    }

    #[tokio::test]
    async fn does_not_upload_a_demo_again_that_it_uploaded_but_could_not_remove() {
        let root = tempfile::tempdir().unwrap();

        write_demo(root.path(), "a", DEMO);

        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));
        let send = answering(|request| {
            // A directory in the demo's place makes the unlink after the upload fail.
            std::fs::remove_file(&request.file_path).unwrap();
            std::fs::create_dir(&request.file_path).unwrap();

            async { Ok(200) }.boxed()
        });

        options.send = Arc::clone(&send) as Arc<dyn DemoSend>;

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        uploader.scan_all().await.unwrap();
        assert_eq!(send.calls.load(Ordering::SeqCst), 1);
    }

    /// A demo file the agent cannot remove after the target took it is an error in the node log.
    #[tokio::test]
    async fn logs_an_error_when_it_cannot_remove_a_demo_the_target_took() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let folder = file.parent().unwrap().to_owned();
        let told: Arc<Mutex<Vec<(String, bool)>>> = Arc::default();
        let errors: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options(root.path(), Some(target("http://receiver/demos")));
        let (record, path, locked, seen) = (Arc::clone(&told), file.clone(), folder.clone(), Arc::clone(&errors));

        options.log = ankor_agent::logger::Logger::new(
            "demos",
            Some(Arc::new(move |entry: ankor_agent::logger::Entry| {
                if entry.level == ankor_agent::logger::Level::Error {
                    seen.lock().unwrap().push(entry.message);
                }
            })),
        );

        // A folder that refuses changes makes the unlink after the upload fail.
        options.send = answering(move |_| {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

            async { Ok(200) }.boxed()
        });
        options.on_uploaded = Some(Arc::new(move |_, file_name| record.lock().unwrap().push((file_name.to_owned(), path.exists()))));

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(file.exists());
        assert_eq!(*told.lock().unwrap(), [(DEMO.to_owned(), true)]);
        assert_eq!(errors.lock().unwrap().len(), 1);
        assert!(errors.lock().unwrap()[0].contains(&format!("{DEMO} was uploaded but could not be removed")), "{:?}", errors.lock().unwrap());
    }

    /// A demo the agent could not remove after the target took it is not uploaded again to that
    /// target, and is uploaded to the next one.
    #[tokio::test]
    async fn uploads_a_demo_it_could_not_remove_again_only_once_the_target_changes() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let folder = file.parent().unwrap().to_owned();
        let current = Target::new(Some(target("http://receiver/demos")));
        let mut options = options_with(root.path(), &current);
        let (locked, first) = (folder.clone(), Arc::new(AtomicBool::new(true)));
        // The first upload finds the folder refusing changes, so its unlink fails.
        let send = answering(move |_| {
            if first.swap(false, Ordering::SeqCst) {
                std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
            }

            async { Ok(200) }.boxed()
        });

        options.send = Arc::clone(&send) as Arc<dyn DemoSend>;

        let uploader = DemoUploader::new(options);
        // A pass that kept uploading the demo it could not remove would never end.
        let pass = || async { tokio::time::timeout(Duration::from_secs(5), uploader.scan_all()).await.expect("the pass ended").unwrap() };

        pass().await;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        pass().await;
        assert_eq!(send.calls.load(Ordering::SeqCst), 1);
        assert!(file.exists());

        current.set(Some(target("http://other/demos")));
        pass().await;
        assert_eq!(send.calls.load(Ordering::SeqCst), 2);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn leaves_demos_alone_while_the_node_has_no_upload_target() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let current = Target::new(None);
        let requested: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options_with(root.path(), &current);
        let seen = Arc::clone(&requested);

        options.send = answering(move |request| {
            seen.lock().unwrap().push(request.url.clone());

            async { Ok(200) }.boxed()
        });

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();

        assert!(requested.lock().unwrap().is_empty());
        assert!(file.exists());

        // Configuring a target is what sends them: the agent keeps no other record that they exist.
        current.set(Some(target("https://controller.example/api/plugin/demos")));
        uploader.scan_all().await.unwrap();

        assert_eq!(*requested.lock().unwrap(), ["https://controller.example/api/plugin/demos"]);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn finds_nothing_to_do_on_a_node_whose_slots_have_never_recorded_anything() {
        let root = tempfile::tempdir().unwrap();
        let send = ok();
        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));

        options.send = Arc::clone(&send) as Arc<dyn DemoSend>;

        let uploader = DemoUploader::new(options);

        uploader.scan_all().await.unwrap();
        uploader.scan_all().await.unwrap();

        assert_eq!(send.calls.load(Ordering::SeqCst), 0);
    }
}

mod the_demos_the_controller_can_see_and_delete {
    use super::*;

    fn failure(error: &ankor_agent::failure::OpError) -> &AgentFailure {
        error.refusal().unwrap_or_else(|| panic!("not a refusal: {error}"))
    }

    #[tokio::test]
    async fn treats_absent_slot_and_demo_directories_and_disappeared_recordings_as_empty() {
        let root = tempfile::tempdir().unwrap();
        let uploader = DemoUploader::new(options(root.path(), None));

        assert_eq!(uploader.list().await.unwrap(), []);

        let dir = demos_dir(root.path(), "a");

        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        assert_eq!(uploader.list().await.unwrap(), []);
        std::fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink("missing-recording", dir.join(DEMO)).unwrap();
        assert_eq!(uploader.list().await.unwrap(), []);
        std::fs::create_dir(dir.join(ALLOCATION)).unwrap();
        std::os::unix::fs::symlink("missing-recording", dir.join(ALLOCATION).join(DEMO)).unwrap();
        assert_eq!(uploader.list().await.unwrap(), []);
    }

    #[tokio::test]
    async fn reports_an_unreadable_directory_instead_of_an_empty_inventory() {
        for relative in ["slots", "slots/a/game/csgo/ankor/demos"] {
            let root = tempfile::tempdir().unwrap();
            let blocked = root.path().join(relative);

            std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
            std::fs::write(&blocked, "a file where a directory should be").unwrap();

            let uploader = DemoUploader::new(options(root.path(), Some(target("http://receiver/demos"))));

            assert_eq!(uploader.list().await.unwrap_err().kind(), std::io::ErrorKind::NotADirectory, "{relative}");
            assert_eq!(uploader.scan_all().await.unwrap_err().kind(), std::io::ErrorKind::NotADirectory, "{relative}");
        }
    }

    #[tokio::test]
    async fn lists_every_recording_still_on_the_node_with_the_size_and_time_a_browser_shows() {
        let root = tempfile::tempdir().unwrap();

        write_demo_in(root.path(), "a", DEMO, b"twelve bytes", Some(ALLOCATION));
        write_demo_in(root.path(), "b", "20260921-120000_match-2_de_nuke.dem", b"x", Some(ALLOCATION));
        write_demo_in(root.path(), "b", "20260921-120000_match-2_de_nuke.dem.part", b"still recording", Some(ALLOCATION));
        // Listed like any other: only uploading needs the name to parse.
        write_demo_in(root.path(), "b", "hand-made.dem", b"y", Some(ALLOCATION));

        let listed = DemoUploader::new(options(root.path(), None)).list().await.unwrap();
        let mut names: Vec<_> = listed.iter().map(|demo| format!("{}/{}", demo.slot_id, demo.file_name)).collect();

        names.sort();
        assert_eq!(names, ["a/20260922-183000_match-1_de_mirage.dem", "b/20260921-120000_match-2_de_nuke.dem", "b/hand-made.dem"]);

        let mirage = listed.iter().find(|demo| demo.slot_id == "a").unwrap();

        assert_eq!(mirage.size_bytes, 12);
        assert!(mirage.modified_at > jiff::Timestamp::now().as_millisecond() - 60_000);
    }

    #[tokio::test]
    async fn lists_only_names_the_wire_carries_however_many_demos_the_node_keeps() {
        let root = tempfile::tempdir().unwrap();
        let kept = usize::try_from(MAX_REPORTED_DEMOS).unwrap() + 2;

        write_demo(root.path(), "a", "with space.dem");
        write_demo(root.path(), "not a slot", "fine.dem");

        for index in 0..kept {
            let file = write_demo(root.path(), "a", &format!("demo-{index}.dem"));
            let at = std::time::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000 + u64::try_from(index).unwrap() * 1000);

            std::fs::File::options().write(true).open(&file).unwrap().set_modified(at).unwrap();
        }

        let listed = DemoUploader::new(options(root.path(), None)).list().await.unwrap();
        let names: std::collections::HashSet<_> = listed.iter().map(|demo| demo.file_name.clone()).collect();

        assert_eq!(listed.len(), kept);
        assert!(names.contains("demo-0.dem") && names.contains(&format!("demo-{}.dem", kept - 1)));
        assert!(!names.contains("with space.dem"));
        assert!(!listed.iter().any(|demo| demo.slot_id == "not a slot"));
    }

    #[tokio::test]
    async fn deletes_one_demo_and_refuses_a_slot_it_does_not_have() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let earlier = write_demo_in(root.path(), "a", "20260921-120000_match-2_de_nuke.dem", b"gotv-bytes", Some("22222222-2222-4222-8222-222222222222"));
        let other_slot = write_demo(root.path(), "b", DEMO);
        let changes = Arc::new(AtomicUsize::new(0));
        let mut options = options(root.path(), None);
        let counted = Arc::clone(&changes);

        options.on_change = Some(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        }));

        let uploader = DemoUploader::new(options);

        std::fs::write(root.path().join("slots/c"), "a file, not a slot").unwrap();

        for slot_id in ["..", "c", "d"] {
            let error = uploader.remove(slot_id, DEMO).await.unwrap_err();

            assert!(failure(&error).detail.contains("unknown slot"), "{error}");
        }

        assert!(file.exists());

        uploader.remove("b", DEMO).await.unwrap();
        assert!(!other_slot.exists());
        assert!(file.exists());
        uploader.remove("a", &base_name(&earlier)).await.unwrap();
        uploader.remove("a", DEMO).await.unwrap();

        assert!(!earlier.exists());
        assert!(!file.exists());
        assert_eq!(changes.load(Ordering::SeqCst), 3);
        // A demo the slot no longer has is refused.
        let error = uploader.remove("a", DEMO).await.unwrap_err();

        assert_eq!(failure(&error).code, AgentErrorCode::UnknownDemo);
        assert_eq!(changes.load(Ordering::SeqCst), 3);

        // A recording the uploader would not touch is still an operator's to delete.
        let strange = write_demo(root.path(), "a", "hand-made.dem");

        uploader.remove("a", "hand-made.dem").await.unwrap();
        assert!(!strange.exists());
    }

    /// Asked between the delete's directory read and its unlink, `running_allocation` removes the file
    /// there, as an upload that finished meanwhile does. The delete finds no such demo.
    #[tokio::test]
    async fn refuses_to_delete_a_demo_that_is_gone_once_the_delete_removes_it() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let gone = file.clone();
        let uploader = DemoUploader::new(DemoUploaderOptions::new(
            root.path().to_owned(),
            Arc::new(|| None),
            Arc::new(move |_| {
                let _ = std::fs::remove_file(&gone);
                None
            }),
            Arc::new(|| false),
        ));
        let error = uploader.remove("a", DEMO).await.unwrap_err();

        assert_eq!(failure(&error).code, AgentErrorCode::UnknownDemo);
        assert!(!file.exists());
    }

    /// Asked between the delete's directory read and its unlink, `running_allocation` puts a directory
    /// where the demo was. The unlink fails, and the delete says why instead of calling the demo unknown.
    #[tokio::test]
    async fn reports_a_failed_unlink_as_the_failure_and_not_as_an_unknown_demo() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let replaced = file.clone();
        let uploader = DemoUploader::new(DemoUploaderOptions::new(
            root.path().to_owned(),
            Arc::new(|| None),
            Arc::new(move |_| {
                if replaced.is_file() {
                    std::fs::remove_file(&replaced).unwrap();
                    std::fs::create_dir(&replaced).unwrap();
                }

                None
            }),
            Arc::new(|| false),
        ));
        let error = uploader.remove("a", DEMO).await.unwrap_err();

        assert!(error.refusal().is_none(), "{error}");
        assert!(file.is_dir());
    }

    /// Two demos, the first stat'ed before `running_allocation` is asked about it; `change` then acts on
    /// both files, so the second is changed between the listing and its stat, whichever is read first.
    async fn list_changing_the_demos_after_the_first_stat(change: fn(&Path)) -> std::io::Result<Vec<DemoFile>> {
        let root = tempfile::tempdir().unwrap();
        let files = [write_demo(root.path(), "a", DEMO), write_demo(root.path(), "a", "20260922-190000_match-2_de_mirage.dem")];
        let changed = AtomicBool::new(false);
        let uploader = DemoUploader::new(DemoUploaderOptions::new(
            root.path().to_owned(),
            Arc::new(|| None),
            Arc::new(move |_| {
                if !changed.swap(true, Ordering::SeqCst) {
                    for file in &files {
                        change(file);
                    }
                }

                None
            }),
            Arc::new(|| false),
        ));

        uploader.list().await
    }

    #[tokio::test]
    async fn lists_no_demo_deleted_between_the_listing_and_its_stat() {
        let listed = list_changing_the_demos_after_the_first_stat(|file| std::fs::remove_file(file).unwrap()).await.unwrap();

        assert_eq!(listed.len(), 1, "{listed:?}");
    }

    #[tokio::test]
    async fn lists_no_demo_replaced_by_a_directory_between_the_listing_and_its_stat() {
        let listed = list_changing_the_demos_after_the_first_stat(|file| {
            std::fs::remove_file(file).unwrap();
            std::fs::create_dir(file).unwrap();
        })
        .await
        .unwrap();

        assert_eq!(listed.len(), 1, "{listed:?}");
    }

    /// An upload that cannot remove a demo the target took says so while the demo still counts as
    /// uploading, so the reporter forgets it before a delete can remove it.
    #[tokio::test]
    async fn says_it_could_not_remove_a_demo_while_a_delete_is_still_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let folder = file.parent().unwrap().to_owned();
        let told: Arc<Mutex<Vec<String>>> = Arc::default();
        let made: Arc<std::sync::OnceLock<DemoUploader>> = Arc::default();
        let mut options = options(root.path(), Some(target("http://receiver/demos")));
        let (by_upload, by_failure, cell, locked) = (Arc::clone(&told), Arc::clone(&told), Arc::clone(&made), folder.clone());

        // A folder that refuses changes makes the unlink after the upload fail.
        options.send = answering(move |_| {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

            async { Ok(200) }.boxed()
        });
        options.on_uploaded = Some(Arc::new(move |_, _| by_upload.lock().unwrap().push("uploaded".to_owned())));
        // The delete runs on a thread and runtime of its own, which nothing in the test's runtime can
        // hold up while the callback holds the upload.
        options.on_not_removed = Some(Arc::new(move |_, _| {
            let uploader = cell.get().unwrap().clone();
            let (answer, answered) = std::sync::mpsc::channel();

            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let _ = answer.send(runtime.block_on(uploader.remove("a", DEMO)));
            });

            let error = answered.recv_timeout(Duration::from_secs(10)).expect("the delete never answered").unwrap_err();

            by_failure.lock().unwrap().push(format!("not removed, delete refused as {:?}", failure(&error).code));
        }));

        let uploader = DemoUploader::new(options);

        assert!(made.set(uploader.clone()).is_ok());
        uploader.scan_all().await.unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(file.exists());
        assert_eq!(*told.lock().unwrap(), ["uploaded", "not removed, delete refused as DemoUploading"]);
    }

    /// A delete that removed the demo succeeds even when its ready marker stays; the marker costs
    /// nothing and the node log warns of it.
    #[tokio::test]
    async fn deletes_a_demo_whose_ready_marker_stays_and_warns_of_the_marker() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let marker = PathBuf::from(format!("{}.ready", file.display()));
        let warnings: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options(root.path(), None);
        let seen = Arc::clone(&warnings);

        // A folder in the marker's place, which a file removal refuses.
        std::fs::create_dir(&marker).unwrap();
        std::fs::write(marker.join("held"), "").unwrap();
        options.log = ankor_agent::logger::Logger::new(
            "demos",
            Some(Arc::new(move |entry: ankor_agent::logger::Entry| {
                if entry.level == ankor_agent::logger::Level::Warn {
                    seen.lock().unwrap().push(entry.message);
                }
            })),
        );
        DemoUploader::new(options).remove("a", DEMO).await.unwrap();
        assert!(!file.exists());
        assert!(marker.exists());
        assert_eq!(warnings.lock().unwrap().len(), 1);
        assert!(warnings.lock().unwrap()[0].contains("was deleted but its ready marker was not removed"), "{:?}", warnings.lock().unwrap());
    }

    #[tokio::test]
    async fn refuses_to_delete_the_demo_it_is_uploading_and_deletes_it_once_the_upload_failed() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let (started, release) = (Deferred::default(), Deferred::default());
        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));
        let (begun, released) = (started.clone(), release.clone());

        options.send = answering(move |_| {
            let (begun, released) = (begun.clone(), released.clone());

            async move {
                begun.resolve();
                released.wait().await;

                Ok(503)
            }
            .boxed()
        });

        let uploader = DemoUploader::new(options);
        let scan = uploader.scan_all();

        started.wait().await;

        let error = uploader.remove("a", DEMO).await.unwrap_err();

        assert_eq!(failure(&error).code, AgentErrorCode::DemoUploading);
        assert!(failure(&error).detail.contains(DEMO));
        assert!(file.exists());
        release.resolve();
        scan.await.unwrap();
        uploader.remove("a", DEMO).await.unwrap();
        assert!(!file.exists());

        // A new recording under the same name starts with no failure.
        write_demo(root.path(), "a", DEMO);
        assert_eq!(uploader.list().await.unwrap()[0].upload_error, None);
    }

    /// Opens the demo as the real send does, so a demo deleted under an upload fails to open.
    struct Opening(Arc<Mutex<Vec<String>>>);

    #[async_trait::async_trait]
    impl DemoSend for Opening {
        async fn send(&self, request: DemoRequest) -> Result<u16, DemoUploadError> {
            tokio::fs::File::open(&request.file_path).await?;
            self.0.lock().unwrap().push(base_name(&request.file_path));
            Ok(200)
        }
    }

    /// The upload says the target took the demo while the demo still counts as uploading, so a delete
    /// is refused until the reporter knows and cannot remove it first.
    #[tokio::test]
    async fn says_the_target_took_a_demo_while_a_delete_is_still_refused() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let mut options = options(root.path(), Some(target("http://receiver/demos")));
        let refused: Arc<Mutex<Vec<AgentErrorCode>>> = Arc::default();
        let made: Arc<std::sync::OnceLock<DemoUploader>> = Arc::default();
        let (cell, record) = (Arc::clone(&made), Arc::clone(&refused));

        options.send = answering(|_| async { Ok(200) }.boxed());
        // The callback holds the upload, on a thread of the test's runtime, while the delete runs on
        // a thread and runtime of its own, which nothing in the test's runtime can hold up.
        options.on_uploaded = Some(Arc::new(move |_, _| {
            let uploader = cell.get().unwrap().clone();
            let (answer, answered) = std::sync::mpsc::channel();

            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let _ = answer.send(runtime.block_on(uploader.remove("a", DEMO)));
            });

            let error = answered.recv_timeout(Duration::from_secs(10)).expect("the delete never answered").unwrap_err();

            record.lock().unwrap().push(failure(&error).code);
        }));

        let uploader = DemoUploader::new(options);

        assert!(made.set(uploader.clone()).is_ok());
        uploader.scan_all().await.unwrap();
        assert_eq!(*refused.lock().unwrap(), [AgentErrorCode::DemoUploading]);
        assert!(!file.exists());
    }

    /// The target took the demo: the upload says so while the file is still there, so a report that
    /// finds it gone already knows.
    #[tokio::test]
    async fn says_the_target_took_a_demo_before_it_removes_the_file() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let told: Arc<Mutex<Vec<(String, bool)>>> = Arc::default();
        let mut options = options(root.path(), Some(target("http://receiver/demos")));
        let (record, path) = (Arc::clone(&told), file.clone());

        options.send = answering(|_| async { Ok(200) }.boxed());
        options.on_uploaded = Some(Arc::new(move |_, file_name| record.lock().unwrap().push((file_name.to_owned(), path.exists()))));
        DemoUploader::new(options).scan_all().await.unwrap();
        assert!(!file.exists());
        assert_eq!(*told.lock().unwrap(), [(DEMO.to_owned(), true)]);
    }

    /// A file already gone when the upload removes it needs nothing more: the target took it, and the
    /// upload does not warn.
    #[tokio::test]
    async fn an_upload_whose_file_is_gone_once_it_is_answered_says_the_target_took_it_and_does_not_warn() {
        let root = tempfile::tempdir().unwrap();
        let file = write_demo(root.path(), "a", DEMO);
        let removed: Arc<Mutex<Vec<String>>> = Arc::default();
        let warnings: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options(root.path(), Some(target("http://receiver/demos")));
        let (gone, seen) = (Arc::clone(&removed), Arc::clone(&warnings));

        options.send = answering(move |request| {
            std::fs::remove_file(&request.file_path).unwrap();

            async { Ok(200) }.boxed()
        });
        options.on_uploaded = Some(Arc::new(move |_, file_name| gone.lock().unwrap().push(file_name.to_owned())));
        options.log = ankor_agent::logger::Logger::new(
            "demos",
            Some(Arc::new(move |entry: ankor_agent::logger::Entry| {
                if entry.level == ankor_agent::logger::Level::Warn {
                    seen.lock().unwrap().push(entry.message);
                }
            })),
        );

        DemoUploader::new(options).scan_all().await.unwrap();

        assert!(!file.exists());
        assert_eq!(*removed.lock().unwrap(), [DEMO]);
        assert_eq!(*warnings.lock().unwrap(), Vec::<String>::new());
    }

    /// A delete and an upload that race for one demo: the upload holds it and the delete is refused,
    /// or the upload removed it first and the delete finds no such demo, or the upload never starts
    /// on it, or finds it gone. None of it counts as a failed upload, which would hold every other
    /// demo back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delete_racing_an_upload_holds_no_other_demo_back() {
        let root = tempfile::tempdir().unwrap();
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut options = options(root.path(), Some(target("https://controller.example/api/plugin/demos")));

        options.send = Arc::new(Opening(Arc::clone(&sent)));

        let uploader = DemoUploader::new(options);

        for _ in 0..200 {
            write_demo(root.path(), "a", DEMO);

            let (scan, removed) = tokio::join!(uploader.scan_all(), uploader.remove("a", DEMO));

            scan.unwrap();

            if let Err(error) = removed {
                assert!(matches!(failure(&error).code, AgentErrorCode::DemoUploading | AgentErrorCode::UnknownDemo), "{error}");
            }
        }

        let witness = "20260922-180000_match-2_de_ancient.dem";

        write_demo(root.path(), "a", witness);
        uploader.scan_all().await.unwrap();
        assert_eq!(sent.lock().unwrap().last().map(String::as_str), Some(witness));
    }

    #[tokio::test]
    async fn reports_the_running_launchs_unfinished_demo_as_recording_and_keeps_it_from_deletion_and_upload_until_it_is_finished() {
        let root = tempfile::tempdir().unwrap();
        let earlier = "22222222-2222-4222-8222-222222222222";
        let live = write_demo(root.path(), "a", DEMO);
        let marked = write_demo(root.path(), "a", "20260922-170000_match-1_de_nuke.dem");
        let crashed = write_demo_in(root.path(), "a", "20260922-160000_match-0_de_inferno.dem", b"gotv-bytes", Some(earlier));

        std::fs::write(format!("{}.ready", marked.display()), "").unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let sent: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let (holding, any) = (Arc::clone(&running), Arc::clone(&running));
        let mut options = DemoUploaderOptions::new(
            root.path().to_owned(),
            Arc::new(|| Some(target("http://receiver/demos"))),
            Arc::new(move |_| holding.load(Ordering::SeqCst).then(|| ALLOCATION.to_owned())),
            Arc::new(move || any.load(Ordering::SeqCst)),
        );
        let seen = Arc::clone(&sent);

        options.send = answering(move |request| {
            seen.lock().unwrap().push(request.file_path.clone());

            async { Ok(200) }.boxed()
        });

        let uploader = DemoUploader::new(options);
        let recording = || async { uploader.list().await.unwrap().into_iter().map(|demo| (demo.file_name, demo.recording)).collect::<BTreeMap<_, _>>() };

        assert_eq!(recording().await, BTreeMap::from([(DEMO.to_owned(), true), (base_name(&marked), false), (base_name(&crashed), false)]));

        let error = uploader.remove("a", DEMO).await.unwrap_err();

        assert_eq!(failure(&error).code, AgentErrorCode::DemoRecording);
        assert!(failure(&error).detail.contains(DEMO));
        assert!(live.exists());

        uploader.scan_all().await.unwrap();

        let mut uploaded = sent.lock().unwrap().clone();
        let mut expected = vec![crashed, marked];

        uploaded.sort();
        expected.sort();
        assert_eq!(uploaded, expected);

        running.store(false, Ordering::SeqCst);
        assert_eq!(recording().await, BTreeMap::from([(DEMO.to_owned(), false)]));
        uploader.remove("a", DEMO).await.unwrap();
        assert!(!live.exists());
    }
}

/// The file the engine writes is the payload; nothing reads it back in, so this is the shape check.
#[tokio::test]
async fn sends_the_bytes_on_disk_unmodified() {
    let root = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..5000).map(|index| u8::try_from(index % 251).unwrap()).collect();
    let file = write_demo_in(root.path(), "a", DEMO, &bytes, Some(ALLOCATION));
    let on_disk = std::fs::read(&file).unwrap();
    let server = demo_server(&[200]).await;

    DemoUploader::new(options(root.path(), Some(target(&server.url)))).scan_all().await.unwrap();

    assert_eq!(server.requests()[0].body, on_disk);
}

#[tokio::test]
async fn uploads_only_completed_recordings_from_a_running_slot_and_removes_their_markers() {
    let root: TempDir = tempfile::tempdir().unwrap();
    let complete = write_demo(root.path(), "a", DEMO);
    let active = write_demo(root.path(), "a", "20260922-190000_match-2_de_nuke.dem");
    let marker = PathBuf::from(format!("{}.ready", complete.display()));

    std::fs::write(&marker, "").unwrap();

    let sent: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let mut options = options(root.path(), Some(target("http://receiver/demos")));
    let seen = Arc::clone(&sent);

    options.running_allocation = Arc::new(|_| Some(ALLOCATION.to_owned()));
    options.send = answering(move |request| {
        seen.lock().unwrap().push(request.file_path.clone());

        async { Ok(200) }.boxed()
    });
    DemoUploader::new(options).scan_all().await.unwrap();

    assert_eq!(*sent.lock().unwrap(), std::slice::from_ref(&complete));
    assert!(!complete.exists());
    assert!(!marker.exists());
    assert!(active.exists());
}

/// The warnings are for files the node holds: one removed is forgotten, so the set of them does not
/// grow over the agent's uptime, and the same name coming back is warned about again.
#[tokio::test]
async fn forgets_a_misnamed_demo_once_it_is_gone() {
    let root: TempDir = tempfile::tempdir().unwrap();
    let misnamed = write_demo(root.path(), "a", "match.dem");
    let warnings: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&warnings);
    let mut options = options(root.path(), Some(target("http://receiver/demos")));

    options.log = ankor_agent::logger::Logger::new(
        "demos",
        Some(Arc::new(move |entry: ankor_agent::logger::Entry| {
            if entry.level == ankor_agent::logger::Level::Warn {
                seen.lock().unwrap().push(entry.message);
            }
        })),
    );

    let uploader = DemoUploader::new(options);

    uploader.scan_all().await.unwrap();
    uploader.scan_all().await.unwrap();
    assert_eq!(warnings.lock().unwrap().len(), 1);
    std::fs::remove_file(&misnamed).unwrap();
    uploader.scan_all().await.unwrap();
    write_demo(root.path(), "a", "match.dem");
    uploader.scan_all().await.unwrap();
    assert_eq!(warnings.lock().unwrap().len(), 2);
}

mod paced_upload_writes {
    use super::*;

    /// A clock that only moves when the pacer sleeps; `late` is how far each wake-up overshoots.
    struct FakeClock {
        now: Mutex<f64>,
        late: f64,
        sleeps: Mutex<Vec<f64>>,
    }

    impl PaceClock for FakeClock {
        fn now(&self) -> f64 {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, ms: f64, _: &AbortSignal) -> BoxFuture<'static, Result<(), DemoUploadError>> {
            self.sleeps.lock().unwrap().push(ms);
            *self.now.lock().unwrap() += ms + self.late;

            async { Ok(()) }.boxed()
        }
    }

    fn fake_clock(late: f64) -> Arc<FakeClock> {
        Arc::new(FakeClock { now: Mutex::new(0.0), late, sleeps: Mutex::default() })
    }

    fn file(lengths: &[usize]) -> futures_util::stream::Iter<std::vec::IntoIter<std::io::Result<Bytes>>> {
        futures_util::stream::iter(lengths.iter().map(|&length| Ok(Bytes::from(vec![0u8; length]))).collect::<Vec<_>>())
    }

    async fn sizes(limit: impl Fn() -> Option<f64> + Send + Sync + 'static, lengths: &[usize], clock: &Arc<FakeClock>) -> Vec<usize> {
        let stream = paced(file(lengths), Arc::new(limit), AbortSignal::default(), Arc::clone(clock) as Arc<dyn PaceClock>);

        stream.map(|chunk| chunk.unwrap().len()).collect().await
    }

    fn sleeps(clock: &FakeClock) -> Vec<f64> {
        clock.sleeps.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn passes_chunks_through_whole_without_a_limit() {
        let clock = fake_clock(0.0);

        assert_eq!(sizes(|| None, &[65_536, 1000], &clock).await, [65_536, 1000]);
        assert!(sleeps(&clock).is_empty());
    }

    #[tokio::test]
    async fn writes_5_ms_worth_at_a_time_5_ms_apart_at_the_limit() {
        let clock = fake_clock(0.0);

        // 1 MB/s: 5000 bytes every 5 ms.
        assert_eq!(sizes(|| Some(1_000_000.0), &[20_000], &clock).await, [5000, 5000, 5000, 5000]);
        assert_eq!(sleeps(&clock), [5.0, 5.0, 5.0]);
    }

    #[tokio::test]
    async fn never_writes_less_than_a_packet_however_low_the_limit() {
        let clock = fake_clock(0.0);

        // 1 Mbit/s would be 625 bytes a slice.
        assert_eq!(sizes(|| Some(125_000.0), &[3000], &clock).await, [1500, 1500]);
        assert_eq!(sleeps(&clock), [12.0]);
    }

    #[tokio::test]
    async fn drops_the_limit_mid_upload_once_it_is_lifted_and_paces_again_from_scratch_when_it_returns() {
        let clock = fake_clock(0.0);
        let limits = [Some(1_000_000.0), Some(1_000_000.0), None, Some(1_000_000.0), Some(1_000_000.0)];
        let call = AtomicUsize::new(0);
        let written = sizes(move || limits[call.fetch_add(1, Ordering::SeqCst).min(limits.len() - 1)], &[10_000, 8000, 10_000], &clock).await;

        assert_eq!(written, [5000, 5000, 8000, 5000, 5000]);
        // The first slice after the limit returns goes at once; only the next one waits.
        assert_eq!(sleeps(&clock), [5.0, 5.0]);
    }

    #[tokio::test]
    async fn makes_up_for_a_late_wake_up_with_one_slice_at_most() {
        let clock = fake_clock(20.0);

        assert_eq!(sizes(|| Some(1_000_000.0), &[20_000], &clock).await, [5000, 5000, 5000, 5000]);
        // 20 ms late after the first sleep: the third slice follows at once, the fourth waits again.
        assert_eq!(sleeps(&clock), [5.0, 5.0]);
    }
}
