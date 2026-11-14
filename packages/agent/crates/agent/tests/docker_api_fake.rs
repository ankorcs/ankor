//! The Docker client against a stand-in daemon on a Unix socket: the requests it sends and how it
//! reads the answers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::docker_api::{DockerApi, DockerApiPort, DockerError, Filters, WaitCondition, split_reference};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixListener;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Received {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    body: String,
    content_type: Option<String>,
}

#[derive(Clone)]
enum Answer {
    Plain {
        status: u16,
        chunks: Vec<Vec<u8>>,
        body: Vec<u8>,
    },
    /// Upgrades, then writes these pieces of raw stream.
    Upgrade(Vec<Vec<u8>>),
    /// Answers 200, writes these bytes, then keeps the connection open and sends nothing more.
    Stall(Vec<u8>),
    /// Reads the request and never answers, as a hung daemon would.
    Silent,
}

fn answer(status: u16, body: &str) -> Answer {
    Answer::Plain { status, chunks: Vec::new(), body: body.as_bytes().to_vec() }
}

struct Daemon {
    api: DockerApi,
    received: Arc<Mutex<Vec<Received>>>,
    answer: Arc<Mutex<Answer>>,
    dir: tempfile::TempDir,
}

/// Reads one request head and its body, as far as the client sends one.
async fn read_request(stream: &mut tokio::net::UnixStream) -> Option<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    let head_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;

        if read == 0 {
            return None;
        }

        buffer.extend_from_slice(&chunk[..read]);

        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut start = lines.next()?.split(' ');
    let (method, target) = (start.next()?.to_owned(), start.next()?.to_owned());
    let headers: Vec<(String, String)> =
        lines.filter_map(|line| line.split_once(": ").map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))).collect();
    let length: usize = headers.iter().find(|(name, _)| name == "content-length").and_then(|(_, value)| value.parse().ok()).unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();

    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;

        if read == 0 {
            break;
        }

        body.extend_from_slice(&chunk[..read]);
    }

    Some((method, target, headers, body))
}

fn daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let received: Arc<Mutex<Vec<Received>>> = Arc::default();
    let answer_slot = Arc::new(Mutex::new(answer(204, "")));
    let (seen, answering) = (Arc::clone(&received), Arc::clone(&answer_slot));

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (seen, answering) = (Arc::clone(&seen), Arc::clone(&answering));

            tokio::spawn(async move {
                let Some((method, target, headers, body)) = read_request(&mut stream).await else {
                    return;
                };
                let url = url::Url::parse(&format!("http://docker{target}")).unwrap();

                seen.lock().unwrap().push(Received {
                    method,
                    path: url.path().to_owned(),
                    query: url.query_pairs().into_owned().collect(),
                    body: String::from_utf8_lossy(&body).into_owned(),
                    content_type: headers.iter().find(|(name, _)| name == "content-type").map(|(_, value)| value.clone()),
                });

                let answer = answering.lock().unwrap().clone();

                match answer {
                    Answer::Silent => std::future::pending::<()>().await,
                    Answer::Upgrade(pieces) => {
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 101 UPGRADED\r\nContent-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n",
                            )
                            .await;

                        for piece in pieces {
                            let _ = stream.write_all(&piece).await;
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                    Answer::Stall(bytes) => {
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n").await;
                        let _ = stream.write_all(format!("{:x}\r\n", bytes.len()).as_bytes()).await;
                        let _ = stream.write_all(&bytes).await;
                        let _ = stream.write_all(b"\r\n").await;
                        let _ = stream.flush().await;
                        std::future::pending::<()>().await;
                    }
                    Answer::Plain { status, chunks, body } => {
                        let _ =
                            stream.write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n").as_bytes()).await;

                        for chunk in chunks {
                            let _ = stream.write_all(&chunk).await;
                            let _ = stream.flush().await;
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }

                        let _ = stream.write_all(&body).await;
                    }
                }

                let _ = stream.shutdown().await;
            });
        }
    });

    Daemon { api: DockerApi::new(&path), received, answer: answer_slot, dir }
}

impl Daemon {
    fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    fn calls(&self) -> Vec<String> {
        self.received.lock().unwrap().iter().map(|received| format!("{} {}", received.method, received.path)).collect()
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

fn query(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect()
}

#[tokio::test]
async fn names_the_pinned_api_version_and_reads_the_engines_release() {
    let daemon = daemon();

    daemon.answer(answer(200, r#"{"Version":"29.0.1","ApiVersion":"1.52","MinAPIVersion":"1.44"}"#));
    assert_eq!(daemon.api.engine().await.unwrap(), ("29.0.1".to_owned(), "1.52".to_owned()));
    assert_eq!(daemon.calls(), ["GET /v1.44/version"]);
}

#[tokio::test]
async fn creates_a_container_named_in_the_query_with_the_payload_as_the_body() {
    let daemon = daemon();
    let spec = json!({ "Image": "busybox:latest", "Tty": true, "HostConfig": { "NetworkMode": "host" } });

    daemon.answer(answer(201, r#"{"Id":"abc123","Warnings":[]}"#));
    assert_eq!(daemon.api.create_container("ankor-a-match-x", &spec).await.unwrap(), "abc123");

    let received = daemon.received().remove(0);

    assert_eq!((received.method.as_str(), received.path.as_str()), ("POST", "/v1.44/containers/create"));
    assert_eq!(received.query, query(&[("name", "ankor-a-match-x")]));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&received.body).unwrap(), spec);
    assert_eq!(received.content_type.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn sends_container_calls_with_their_query_and_no_body() {
    let daemon = daemon();

    daemon.api.start_container("abc").await.unwrap();
    daemon.api.stop_container("abc", 10).await.unwrap();
    daemon.api.remove_container("abc", true, false).await.unwrap();
    daemon.api.rename_container("abc", "ankor-agent.previous").await.unwrap();

    let received: Vec<(String, String, BTreeMap<String, String>, String)> =
        daemon.received().into_iter().map(|r| (r.method, r.path, r.query, r.body)).collect();

    assert_eq!(
        received,
        [
            ("POST".to_owned(), "/v1.44/containers/abc/start".to_owned(), query(&[]), String::new()),
            ("POST".to_owned(), "/v1.44/containers/abc/stop".to_owned(), query(&[("t", "10")]), String::new()),
            ("DELETE".to_owned(), "/v1.44/containers/abc".to_owned(), query(&[("force", "true"), ("v", "false")]), String::new()),
            ("POST".to_owned(), "/v1.44/containers/abc/rename".to_owned(), query(&[("name", "ankor-agent.previous")]), String::new()),
        ]
    );
}

#[tokio::test]
async fn json_encodes_filters() {
    let daemon = daemon();
    let role = Filters::from([("label".to_owned(), vec!["ankor.role".to_owned()])]);

    daemon.answer(answer(200, r#"[{"Id":"abc","Names":["/ankor-a"],"ImageID":"sha256:1","State":"running","Labels":{"ankor.role":"match"}}]"#));

    let listed = daemon.api.list_containers(true, Some(&role)).await.unwrap();

    assert_eq!((listed[0].id.as_str(), listed[0].names[0].as_str(), listed[0].state.as_str()), ("abc", "/ankor-a", "running"));

    daemon.answer(answer(200, "[]"));
    daemon.api.list_images(Some(&Filters::from([("label".to_owned(), vec!["ankor.image".to_owned()])]))).await.unwrap();

    let received = daemon.received();

    assert_eq!(received[0].query, query(&[("all", "true"), ("filters", r#"{"label":["ankor.role"]}"#)]));
    assert_eq!((received[1].path.as_str(), received[1].query.clone()), ("/v1.44/images/json", query(&[("filters", r#"{"label":["ankor.image"]}"#)])));
}

#[tokio::test]
async fn puts_an_image_reference_in_the_path_as_it_is() {
    let daemon = daemon();

    daemon.answer(answer(200, r#"{"Id":"sha256:1","Config":{"Env":["A=1"],"Labels":null}}"#));

    let image = daemon.api.inspect_image("ghcr.io/example/ankor-cs2:v1").await.unwrap();

    assert_eq!(image.id.as_deref(), Some("sha256:1"));
    assert_eq!(image.config.unwrap().env, Some(vec!["A=1".to_owned()]));

    daemon.answer(answer(200, r#"[{"Untagged":"x"}]"#));
    daemon.api.remove_image("sha256:1").await.unwrap();
    assert_eq!(daemon.calls(), ["GET /v1.44/images/ghcr.io/example/ankor-cs2:v1/json", "DELETE /v1.44/images/sha256:1"]);
}

#[tokio::test]
async fn reads_the_registry_digest_and_fails_on_an_answer_without_one() {
    let daemon = daemon();

    daemon.answer(answer(200, r#"{"Descriptor":{"digest":"sha256:abc"}}"#));
    assert_eq!(daemon.api.registry_digest("ghcr.io/example/ankor-agent:v2").await.unwrap(), "sha256:abc");
    assert_eq!(daemon.calls(), ["GET /v1.44/distribution/ghcr.io/example/ankor-agent:v2/json"]);

    for body in ["{}", r#"{"Descriptor":{"digest":7}}"#] {
        daemon.answer(answer(200, body));
        assert_eq!(
            daemon.api.registry_digest("ghcr.io/example/ankor-agent:v2").await.unwrap_err().to_string(),
            "the registry gave no digest for ghcr.io/example/ankor-agent:v2"
        );
    }
}

#[tokio::test]
async fn reads_the_exit_code_a_wait_answers_with_or_none() {
    let daemon = daemon();

    daemon.answer(answer(200, r#"{"StatusCode":137,"Error":null}"#));
    assert_eq!(daemon.api.wait_container("abc", WaitCondition::NextExit).await.unwrap().await.unwrap(), Some(137));
    assert_eq!(daemon.calls(), ["POST /v1.44/containers/abc/wait"]);
    assert_eq!(daemon.received()[0].query, query(&[("condition", "next-exit")]));

    daemon.answer(answer(200, ""));
    assert_eq!(daemon.api.wait_container("abc", WaitCondition::NotRunning).await.unwrap().await.unwrap(), None);
    assert_eq!(daemon.received()[1].query, query(&[("condition", "not-running")]));
}

#[tokio::test(start_paused = true)]
async fn a_call_the_daemon_never_answers_fails_after_the_answer_limit() {
    let daemon = daemon();
    let limit = ankor_agent::docker_api::ANSWER_WITHIN;

    daemon.answer(Answer::Silent);

    let started = tokio::time::Instant::now();
    let error = daemon.api.inspect_container("abc").await.unwrap_err();

    assert!(matches!(error, DockerError::NoAnswer(within) if within == limit));
    assert_eq!(started.elapsed(), limit);
    assert_eq!(error.to_string(), format!("Docker gave no answer within {} s", limit.as_secs()));
    assert!(matches!(daemon.api.attach_container("abc").await.err(), Some(DockerError::NoAnswer(_))));
    assert!(matches!(daemon.api.pull_image("busybox").await.unwrap_err(), DockerError::NoAnswer(_)));
}

#[tokio::test(start_paused = true)]
async fn a_stop_waits_its_grace_on_top_of_the_answer_limit_and_a_wait_limits_only_its_registration() {
    let daemon = daemon();
    let limit = ankor_agent::docker_api::ANSWER_WITHIN;

    daemon.answer(Answer::Silent);

    let started = tokio::time::Instant::now();

    assert!(matches!(daemon.api.stop_container("abc", 30).await.unwrap_err(), DockerError::NoAnswer(within) if within == limit + Duration::from_secs(30)));
    assert_eq!(started.elapsed(), limit + Duration::from_secs(30));
    assert!(matches!(daemon.api.wait_container("abc", WaitCondition::NextExit).await.err(), Some(DockerError::NoAnswer(within)) if within == limit));

    // Registered: the daemon has answered with its headers, and the exit code comes when it comes.
    daemon.answer(Answer::Stall(b" ".to_vec()));

    let exit = daemon.api.wait_container("abc", WaitCondition::NextExit).await.unwrap();

    assert!(tokio::time::timeout(limit * 10, exit).await.is_err());
}

#[tokio::test]
async fn errors_carry_the_status_and_dockers_message() {
    let daemon = daemon();
    let failed = |status: u16, body: &str| answer(status, body);

    daemon.answer(failed(404, r#"{"message":"No such container: abc"}"#));

    let error = daemon.api.inspect_container("abc").await.unwrap_err();

    assert_eq!((error.status(), error.to_string()), (Some(404), "No such container: abc".to_owned()));

    for (status, body, message) in
        [(500, "", "Docker answered HTTP 500"), (500, r#"{"cause":"x"}"#, r#"{"cause":"x"}"#), (500, "page not found\n", "page not found")]
    {
        daemon.answer(failed(status, body));

        let error = daemon.api.start_container("abc").await.unwrap_err();

        assert_eq!((error.status(), error.to_string()), (Some(status), message.to_owned()), "{body}");
    }

    daemon.answer(failed(304, ""));
    assert_eq!(daemon.api.stop_container("abc", 10).await.unwrap_err().status(), Some(304));
}

#[tokio::test]
async fn fails_without_a_status_when_the_socket_is_not_there() {
    let daemon = daemon();
    let missing = DockerApi::new(daemon.dir.path().join("missing.sock"));

    assert!(matches!(missing.list_containers(false, None).await.unwrap_err(), DockerError::Io(error) if error.kind() == std::io::ErrorKind::NotFound));
    assert!(matches!(missing.attach_container("abc").await.err().unwrap(), DockerError::Io(error) if error.kind() == std::io::ErrorKind::NotFound));
}

#[tokio::test]
async fn pulls_the_repository_and_tag_and_finishes_with_the_stream() {
    let daemon = daemon();

    daemon.answer(Answer::Plain {
        status: 200,
        chunks: vec![b"{\"status\":\"Pulling from library/busybox\"}\r\n{\"status\":\"Downlo".to_vec(), b"ading\"}\n".to_vec()],
        body: b"{\"status\":\"Status: Image is up to date\"}\n".to_vec(),
    });
    daemon.api.pull_image("ghcr.io/example/ankor-agent:v2").await.unwrap();

    let received = daemon.received().remove(0);

    assert_eq!((received.method.as_str(), received.path.as_str()), ("POST", "/v1.44/images/create"));
    assert_eq!(received.query, query(&[("fromImage", "ghcr.io/example/ankor-agent"), ("tag", "v2")]));
}

#[tokio::test(start_paused = true)]
async fn a_pull_whose_progress_goes_quiet_fails_after_the_quiet_limit() {
    let daemon = daemon();

    daemon.answer(Answer::Stall(b"{\"status\":\"Pulling from library/busybox\"}\n".to_vec()));

    let error = daemon.api.pull_image("ghcr.io/example/ankor-agent:v2").await.unwrap_err();

    assert!(matches!(error, DockerError::PullStalled));
    assert_eq!(error.to_string(), format!("no progress for {} s", ankor_agent::docker_api::PULL_QUIET.as_secs()));
}

#[tokio::test]
async fn a_pull_fails_on_a_progress_line_that_carries_an_error() {
    let daemon = daemon();
    let failing = |body: &[u8]| Answer::Plain { status: 200, chunks: Vec::new(), body: body.to_vec() };

    for body in
        [&b"{\"status\":\"Pulling\"}\n{\"errorDetail\":{\"message\":\"unauthorized\"},\"error\":\"unauthorized\"}\n"[..], &b"{\"error\":\"unauthorized\"}"[..]]
    {
        daemon.answer(failing(body));
        assert_eq!(
            daemon.api.pull_image("ghcr.io/example/ankor-agent:v2").await.unwrap_err().to_string(),
            "pull of ghcr.io/example/ankor-agent:v2 failed: unauthorized"
        );
    }

    let line = "{\"error\":\"Zugriff verweigert: Schlüssel fehlt\"}\n".as_bytes();
    let cut = line.iter().position(|&byte| byte == 0xc3).unwrap() + 1;

    daemon.answer(Answer::Plain { status: 200, chunks: vec![line[..cut].to_vec(), line[cut..].to_vec()], body: Vec::new() });
    assert_eq!(
        daemon.api.pull_image("ghcr.io/example/ankor-agent:v2").await.unwrap_err().to_string(),
        "pull of ghcr.io/example/ankor-agent:v2 failed: Zugriff verweigert: Schlüssel fehlt"
    );

    daemon.answer(answer(404, r#"{"message":"pull access denied"}"#));

    let error = daemon.api.pull_image("ankor-missing:v1").await.unwrap_err();

    assert_eq!((error.status(), error.to_string()), (Some(404), "pull access denied".to_owned()));
}

#[test]
fn splits_references_like_docker_pull() {
    for (reference, repository, tag) in [
        ("busybox", "busybox", "latest"),
        ("busybox:1", "busybox", "1"),
        ("registry.local:5000/ankor-agent", "registry.local:5000/ankor-agent", "latest"),
        ("registry.local:5000/ankor-agent:v2", "registry.local:5000/ankor-agent", "v2"),
        ("ghcr.io/example/ankor-agent@sha256:abc", "ghcr.io/example/ankor-agent", "sha256:abc"),
        ("ghcr.io/example/ankor-agent:v2@sha256:abc", "ghcr.io/example/ankor-agent", "sha256:abc"),
    ] {
        assert_eq!(split_reference(reference), (repository.to_owned(), tag.to_owned()), "{reference}");
    }
}

#[tokio::test]
async fn attach_upgrades_and_hands_over_the_raw_stream() {
    let daemon = daemon();

    daemon.answer(Answer::Upgrade(vec![b"first line\nsecond".to_vec(), b" half\n".to_vec()]));

    let mut stream = daemon.api.attach_container("abc").await.unwrap();
    let mut output = String::new();

    stream.read_to_string(&mut output).await.unwrap();

    let received = daemon.received().remove(0);

    assert_eq!((received.method.as_str(), received.path.as_str()), ("POST", "/v1.44/containers/abc/attach"));
    assert_eq!(received.query, query(&[("stream", "true"), ("stdout", "true"), ("stderr", "true")]));
    assert_eq!(output, "first line\nsecond half\n");
}

#[tokio::test]
async fn a_refused_attach_carries_its_status() {
    let daemon = daemon();

    daemon.answer(answer(404, r#"{"message":"No such container: abc"}"#));

    let error = daemon.api.attach_container("abc").await.err().unwrap();

    assert_eq!((error.status(), error.to_string()), (Some(404), "No such container: abc".to_owned()));
}

/// A tar archive of `overlay/` holding an executable and a config, as the daemon streams a copy.
fn overlay_archive() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());

    for (path, mode, contents) in [("overlay/bin/swiftlys2.so", 0o755, &b"elf"[..]), ("overlay/cfg/base.cfg", 0o644, &b"mp_maxrounds 24\n"[..])] {
        let mut header = tar::Header::new_gnu();

        header.set_size(contents.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append_data(&mut header, path, contents).unwrap();
    }

    builder.into_inner().unwrap()
}

#[tokio::test]
async fn unpacks_a_copied_path_into_the_target_keeping_modes_and_fails_on_a_missing_one() {
    use std::os::unix::fs::PermissionsExt as _;

    let daemon = daemon();
    let target = tempfile::tempdir().unwrap();

    daemon.answer(Answer::Plain { status: 200, chunks: Vec::new(), body: overlay_archive() });
    daemon.api.copy_from_container("abc", "/opt/ankor/overlay", target.path()).await.unwrap();

    let received = daemon.received().remove(0);

    assert_eq!((received.method.as_str(), received.path.as_str()), ("GET", "/v1.44/containers/abc/archive"));
    assert_eq!(received.query, query(&[("path", "/opt/ankor/overlay")]));
    assert_eq!(std::fs::read_to_string(target.path().join("overlay/cfg/base.cfg")).unwrap(), "mp_maxrounds 24\n");
    assert_eq!(std::fs::metadata(target.path().join("overlay/bin/swiftlys2.so")).unwrap().permissions().mode() & 0o777, 0o755);

    daemon.answer(answer(404, r#"{"message":"Could not find the file /opt/ankor/overlay in container abc"}"#));

    let error = daemon.api.copy_from_container("abc", "/opt/ankor/overlay", target.path()).await.unwrap_err();

    assert_eq!(error.status(), Some(404));
}

/// The client as the Docker service holds it: each call through the port reaches the daemon and
/// carries its answer back.
#[tokio::test]
async fn answers_through_the_port_as_it_does_directly() {
    let daemon = daemon();
    let port: &dyn DockerApiPort = &daemon.api;
    let refused = || answer(500, r#"{"message":"daemon refused"}"#);
    let refusal = |result: Result<(), DockerError>| result.unwrap_err().to_string();

    daemon.answer(answer(200, r#"{"Version":"29.0.1","ApiVersion":"1.52"}"#));
    assert_eq!(port.engine().await.unwrap(), ("29.0.1".to_owned(), "1.52".to_owned()));
    daemon.answer(answer(201, r#"{"Id":"abc123","Warnings":[]}"#));
    assert_eq!(port.create_container("ankor-a-match-x", &json!({ "Image": "busybox" })).await.unwrap(), "abc123");
    daemon.answer(answer(200, r#"{"StatusCode":137,"Error":null}"#));
    assert_eq!(port.wait_container("abc", WaitCondition::NextExit).await.unwrap().await.unwrap(), Some(137));
    daemon.answer(answer(200, r#"[{"Id":"sha256:1","RepoTags":["ghcr.io/x/agent:1"]}]"#));
    assert_eq!(port.list_images(None).await.unwrap().iter().map(|image| image.id.as_str()).collect::<Vec<_>>(), ["sha256:1"]);
    daemon.answer(answer(200, r#"[{"Id":"abc","Names":["/ankor-a"],"ImageID":"sha256:1","State":"running","Labels":{}}]"#));
    assert_eq!(port.list_containers(true, None).await.unwrap().iter().map(|container| container.id.as_str()).collect::<Vec<_>>(), ["abc"]);
    daemon.answer(answer(200, r#"{"Descriptor":{"digest":"sha256:abc"}}"#));
    assert_eq!(port.registry_digest("ghcr.io/example/ankor-agent:v2").await.unwrap(), "sha256:abc");

    let target = tempfile::tempdir().unwrap();

    daemon.answer(Answer::Plain { status: 200, chunks: Vec::new(), body: overlay_archive() });
    port.copy_from_container("abc", "/opt/ankor/overlay", target.path()).await.unwrap();
    assert!(target.path().join("overlay/cfg/base.cfg").is_file());

    daemon.answer(refused());
    assert_eq!(refusal(port.start_container("abc").await), "daemon refused");
    assert_eq!(refusal(port.remove_container("abc", true, false).await), "daemon refused");
    assert_eq!(refusal(port.remove_image("sha256:1").await), "daemon refused");
    assert_eq!(refusal(port.copy_from_container("abc", "/opt/ankor/overlay", target.path()).await), "daemon refused");
    assert_eq!(refusal(port.stop_container("abc", 10).await), "daemon refused");
    assert_eq!(refusal(port.rename_container("abc", "ankor-agent.previous").await), "daemon refused");
    assert_eq!(refusal(port.pull_image("ghcr.io/example/ankor-agent:v2").await), "daemon refused");
    assert_eq!(
        daemon.calls()[7..],
        [
            "POST /v1.44/containers/abc/start",
            "DELETE /v1.44/containers/abc",
            "DELETE /v1.44/images/sha256:1",
            "GET /v1.44/containers/abc/archive",
            "POST /v1.44/containers/abc/stop",
            "POST /v1.44/containers/abc/rename",
            "POST /v1.44/images/create",
        ]
    );
}
