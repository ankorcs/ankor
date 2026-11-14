//! Image references and registry reads, the release a node moves to, and the polls that move it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::failure::{AgentFailure, OpError};
use ankor_agent::image_auto_update::{Candidate, ImageAutoUpdate, ImageKind, ImageUpdates, newest_release, target_reference};
use ankor_agent::logger::Logger;
use ankor_agent::registry_tags::{
    Method, RegistryAnswer, RegistryError, RegistryFetch, list_tags, next_link, parse_bearer_challenge, parse_image_reference, tag_digest,
};
use ankor_agent::testing::until;
use ankor_protocol::types::AgentErrorCode;
use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

fn answer(status: u16, body: &Value, headers: &[(&str, &str)]) -> RegistryAnswer {
    RegistryAnswer {
        status,
        headers: headers.iter().map(|(name, value)| (name.to_ascii_lowercase(), (*value).to_owned())).collect(),
        body: serde_json::to_vec(body).unwrap(),
    }
}

/// ghcr.io as it answers anonymously: a challenge, a token, then the tags under that token.
struct Ghcr {
    tags: BTreeMap<String, Vec<String>>,
    calls: Mutex<Vec<String>>,
}

fn ghcr(tags: &[(&str, &[&str])]) -> Arc<Ghcr> {
    Arc::new(Ghcr {
        tags: tags.iter().map(|(repository, tags)| ((*repository).to_owned(), tags.iter().map(|tag| (*tag).to_owned()).collect())).collect(),
        calls: Mutex::default(),
    })
}

impl Ghcr {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl RegistryFetch for Ghcr {
    async fn fetch(&self, url: &str, method: Method, headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError> {
        self.calls.lock().unwrap().push(url.to_owned());

        let path = url::Url::parse(url).unwrap().path().to_owned();

        if path == "/token" {
            return Ok(answer(200, &json!({ "token": "anon" }), &[]));
        }

        let manifest = Regex::new(r"^/v2/(.+)/manifests/(.+)$").unwrap().captures(&path).map(|captures| (captures[1].to_owned(), captures[2].to_owned()));
        let repository = manifest.as_ref().map_or_else(
            || Regex::new(r"^/v2/(.+)/tags/list$").unwrap().captures(&path).map(|captures| captures[1].to_owned()).unwrap_or_default(),
            |(repository, _)| repository.clone(),
        );
        let header = |name: &str| headers.iter().find(|(key, _)| *key == name).map(|(_, value)| value.as_str());

        if header("Authorization") != Some("Bearer anon") {
            let challenge = format!(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:{repository}:pull""#);

            return Ok(answer(401, &json!({}), &[("www-authenticate", &challenge)]));
        }

        if let Some((_, reference)) = manifest {
            let index = header("Accept").is_some_and(|accept| accept.contains("application/vnd.oci.image.index.v1+json"));

            return Ok(if method == Method::Head && index {
                answer(200, &Value::Null, &[("docker-content-digest", &format!("sha256:{reference}"))])
            } else {
                answer(406, &Value::Null, &[])
            });
        }

        Ok(answer(200, &json!({ "name": repository, "tags": self.tags.get(&repository).cloned().unwrap_or_default() }), &[]))
    }
}

/// A registry that answers every request the same way, and records the URLs asked for.
struct Fixed {
    respond: Box<dyn Fn(&str) -> RegistryAnswer + Send + Sync>,
    urls: Mutex<Vec<String>>,
}

impl Fixed {
    fn new(respond: impl Fn(&str) -> RegistryAnswer + Send + Sync + 'static) -> Self {
        Self { respond: Box::new(respond), urls: Mutex::default() }
    }
}

#[async_trait]
impl RegistryFetch for Fixed {
    async fn fetch(&self, url: &str, _method: Method, _headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError> {
        self.urls.lock().unwrap().push(url.to_owned());

        Ok((self.respond)(url))
    }
}

mod parse_image_reference {
    use super::*;

    fn parts(reference: &str) -> (String, String, Option<String>, Option<String>) {
        let image = parse_image_reference(reference);

        (image.registry, image.repository, image.tag, image.digest)
    }

    #[allow(clippy::unnecessary_wraps, reason = "it stands for a tag the reference names")]
    fn some(text: &str) -> Option<String> {
        Some(text.to_owned())
    }

    #[test]
    fn splits_a_reference_the_way_docker_pull_does() {
        let digest = format!("sha256:{}", "a".repeat(64));

        assert_eq!(parts("ghcr.io/ankorcs/ankor-agent:1"), ("ghcr.io".to_owned(), "ankorcs/ankor-agent".to_owned(), some("1"), None));
        assert_eq!(parse_image_reference("ghcr.io/ankorcs/ankor-agent:1").name, "ghcr.io/ankorcs/ankor-agent");
        assert_eq!(parts("localhost:5000/ankor-cs2"), ("localhost:5000".to_owned(), "ankor-cs2".to_owned(), some("latest"), None));
        assert_eq!(parts("nginx"), ("docker.io".to_owned(), "library/nginx".to_owned(), some("latest"), None));
        assert_eq!(parts("ankorcs/ankor-cs2:1"), ("docker.io".to_owned(), "ankorcs/ankor-cs2".to_owned(), some("1"), None));
        assert_eq!(parts("docker.io/ankorcs/ankor-cs2:1"), ("docker.io".to_owned(), "ankorcs/ankor-cs2".to_owned(), some("1"), None));
        assert_eq!(parts("index.docker.io/nginx:1"), ("docker.io".to_owned(), "library/nginx".to_owned(), some("1"), None));

        let pinned = parse_image_reference(&format!("ghcr.io/ankorcs/ankor-cs2@{digest}"));

        assert_eq!((pinned.tag, pinned.digest), (None, Some(digest)));
    }
}

mod registry_reads {
    use super::*;

    #[test]
    fn parses_a_bearer_challenge_and_a_next_link() {
        assert_eq!(
            parse_bearer_challenge(Some(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#)),
            Some(BTreeMap::from([("realm".to_owned(), "https://ghcr.io/token".to_owned()), ("service".to_owned(), "ghcr.io".to_owned())]))
        );
        assert_eq!(parse_bearer_challenge(Some(r#"Basic realm="x""#)), None);
        assert_eq!(
            next_link(Some(r#"</v2/a/tags/list?last=1.0.0&n=1000>; rel="next""#), "https://ghcr.io").as_deref(),
            Some("https://ghcr.io/v2/a/tags/list?last=1.0.0&n=1000")
        );
        assert_eq!(next_link(None, "https://ghcr.io"), None);
    }

    #[tokio::test]
    async fn lists_tags_anonymously_across_pages() {
        let pages = Fixed::new(|url| {
            if url::Url::parse(url).unwrap().query_pairs().any(|(key, _)| key == "last") {
                answer(200, &json!({ "tags": ["1.1.0"] }), &[])
            } else {
                answer(200, &json!({ "tags": ["1", "1.0.0"] }), &[("link", r#"</v2/ankorcs/ankor-agent/tags/list?last=1.0.0&n=1000>; rel="next""#)])
            }
        });

        assert_eq!(list_tags("ghcr.io/ankorcs/ankor-agent:1", &pages, &Logger::null()).await.unwrap(), ["1", "1.0.0", "1.1.0"]);

        let registry = ghcr(&[("ankorcs/ankor-agent", &["1.2.0"])]);

        assert_eq!(list_tags("ghcr.io/ankorcs/ankor-agent:1", registry.as_ref(), &Logger::null()).await.unwrap(), ["1.2.0"]);
        assert_eq!(registry.calls()[1], "https://ghcr.io/token?service=ghcr.io&scope=repository%3Aankorcs%2Fankor-agent%3Apull");
    }

    #[tokio::test]
    async fn reads_the_digest_a_tag_points_to_with_a_head_of_its_manifest_as_docker_pull_would_ask_for_it() {
        assert_eq!(tag_digest("ghcr.io/ankorcs/ankor-cs2:1", ghcr(&[]).as_ref()).await.unwrap(), "sha256:1");
        assert!(
            tag_digest("ghcr.io/ankorcs/ankor-cs2:1", &Fixed::new(|_| answer(404, &Value::Null, &[])))
                .await
                .unwrap_err()
                .to_string()
                .contains("gave no digest")
        );
        assert!(
            tag_digest(&format!("ghcr.io/ankorcs/ankor-cs2@sha256:{}", "a".repeat(64)), ghcr(&[]).as_ref())
                .await
                .unwrap_err()
                .to_string()
                .contains("names no tag")
        );
    }

    #[tokio::test]
    async fn reads_a_docker_hub_image_from_the_host_that_serves_its_api() {
        let hub = Fixed::new(|_| answer(200, &json!({ "tags": ["1.0.0"] }), &[]));

        list_tags("nginx:1", &hub, &Logger::null()).await.unwrap();
        list_tags("docker.io/ankorcs/ankor-cs2:1", &hub, &Logger::null()).await.unwrap();
        assert_eq!(
            *hub.urls.lock().unwrap(),
            ["https://registry-1.docker.io/v2/library/nginx/tags/list?n=1000", "https://registry-1.docker.io/v2/ankorcs/ankor-cs2/tags/list?n=1000"]
        );
    }

    #[tokio::test]
    async fn fails_on_a_registry_that_refuses() {
        assert!(
            list_tags("ghcr.io/ankorcs/ankor-agent:1", &Fixed::new(|_| answer(500, &json!({}), &[])), &Logger::null())
                .await
                .unwrap_err()
                .to_string()
                .contains("HTTP 500")
        );
        assert!(
            list_tags("ghcr.io/ankorcs/ankor-agent:1", &Fixed::new(|_| answer(401, &json!({}), &[])), &Logger::null())
                .await
                .unwrap_err()
                .to_string()
                .contains("anonymous")
        );
    }

    /// A registry whose token service answers with `token_body`, and which serves the tags only to
    /// the token `granted`.
    struct Tokened {
        token_body: Value,
        granted: &'static str,
    }

    #[async_trait]
    impl RegistryFetch for Tokened {
        async fn fetch(&self, url: &str, _method: Method, headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError> {
            if url::Url::parse(url).unwrap().path() == "/token" {
                return Ok(answer(200, &self.token_body, &[]));
            }

            let authorized = headers.iter().any(|(name, value)| *name == "Authorization" && *value == format!("Bearer {}", self.granted));

            Ok(if authorized {
                answer(200, &json!({ "tags": ["1.0.0"] }), &[])
            } else {
                answer(401, &json!({}), &[("www-authenticate", r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#)])
            })
        }
    }

    #[tokio::test]
    async fn takes_the_access_token_when_the_token_service_names_no_token() {
        let reference = "ghcr.io/ankorcs/ankor-agent:1";

        for body in [json!({ "access_token": "granted" }), json!({ "token": "", "access_token": "granted" })] {
            assert_eq!(list_tags(reference, &Tokened { token_body: body.clone(), granted: "granted" }, &Logger::null()).await.unwrap(), ["1.0.0"], "{body}");
        }

        assert_eq!(
            list_tags(reference, &Tokened { token_body: json!({ "token": "first", "access_token": "second" }), granted: "first" }, &Logger::null())
                .await
                .unwrap(),
            ["1.0.0"]
        );

        for body in [json!({}), json!({ "token": "", "access_token": "" })] {
            let error = list_tags(reference, &Tokened { token_body: body.clone(), granted: "granted" }, &Logger::null()).await.unwrap_err();

            assert!(matches!(error, RegistryError::NoToken), "{body}: {error}");
        }
    }

    #[tokio::test]
    async fn stops_following_next_links_after_twenty_pages() {
        let endless =
            Fixed::new(|_| answer(200, &json!({ "tags": ["1.0.0"] }), &[("link", r#"</v2/ankorcs/ankor-agent/tags/list?last=1.0.0&n=1000>; rel="next""#)]));

        let (log, lines) = warnings();

        assert_eq!(list_tags("ghcr.io/ankorcs/ankor-agent:1", &endless, &log).await.unwrap().len(), 20);
        assert_eq!(endless.urls.lock().unwrap().len(), 20);
        assert_eq!(*lines.lock().unwrap(), ["https://ghcr.io lists more than 20 pages of tags for ankorcs/ankor-agent; reading only those"]);
    }

    /// The token is the registry's, so a page another origin serves (host, scheme or port) is never
    /// asked for.
    #[tokio::test]
    async fn stops_at_a_next_link_to_another_origin() {
        for next in ["https://evil.example/v2/x/tags/list", "http://ghcr.io/v2/x/tags/list", "https://ghcr.io:8443/v2/x/tags/list"] {
            let elsewhere = Fixed::new(move |_| answer(200, &json!({ "tags": ["1.0.0"] }), &[("link", &format!(r#"<{next}>; rel="next""#))]));
            let (log, lines) = warnings();

            assert_eq!(list_tags("ghcr.io/ankorcs/ankor-agent:1", &elsewhere, &log).await.unwrap(), ["1.0.0"], "{next}");
            assert_eq!(elsewhere.urls.lock().unwrap().len(), 1, "{next}");
            assert_eq!(*lines.lock().unwrap(), ["https://ghcr.io linked the tags of ankorcs/ankor-agent to another origin; reading no further"]);
        }
    }

    /// A logger that keeps its warnings, without their prefix.
    fn warnings() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let kept = Arc::clone(&lines);
        let log = Logger::new(
            "registry",
            Some(Arc::new(move |entry: ankor_agent::logger::Entry| {
                if entry.level == ankor_agent::logger::Level::Warn {
                    kept.lock().unwrap().push(entry.message.trim_start_matches("[registry] ").to_owned());
                }
            })),
        );

        (log, lines)
    }

    #[tokio::test]
    async fn takes_no_digest_from_a_refused_manifest_head_even_when_it_names_one() {
        let refused = Fixed::new(|_| answer(404, &Value::Null, &[("docker-content-digest", "sha256:stale")]));
        let error = tag_digest("ghcr.io/ankorcs/ankor-cs2:1", &refused).await.unwrap_err();

        assert!(matches!(error, RegistryError::NoDigest { status: 404, .. }), "{error}");
    }

    /// One HTTP answer on a local port, for the client the agent sends to registries.
    async fn answer_once(response: &'static [u8]) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v2/ankorcs/ankor-cs2/manifests/1", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];

            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).await.unwrap();

                assert!(read > 0, "the request ended early");
                request.extend_from_slice(&chunk[..read]);
            }

            stream.write_all(response).await.unwrap();
            stream.shutdown().await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });

        (url, served)
    }

    #[tokio::test]
    async fn the_http_client_sends_the_headers_and_reads_the_status_headers_and_body() {
        let (url, served) = answer_once(
            b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\nX-Part: one\r\nX-Part: two\r\nDocker-Content-Digest: sha256:abc\r\nConnection: close\r\n\r\nhello",
        )
        .await;
        let registry = ankor_agent::registry_tags::HttpRegistry::new();
        let answer = registry.fetch(&url, Method::Get, &[("Authorization", "Bearer anon".to_owned())]).await.unwrap();

        assert_eq!(answer.status, 201);
        assert_eq!((answer.header("x-part"), answer.header("Docker-Content-Digest")), (Some("one, two"), Some("sha256:abc")));
        assert_eq!(answer.body, b"hello");
        assert!(served.await.unwrap().to_ascii_lowercase().contains("authorization: bearer anon"));
    }
}

mod release_selection {
    use super::*;

    #[test]
    fn takes_the_newest_release_of_the_running_major_above_the_running_version() {
        let tags: Vec<String> = ["latest", "1", "1.2", "1.2.0", "1.10.0", "1.3.1", "2.0.0", "1.11.0-rc.1", "sha-abc"].map(str::to_owned).to_vec();

        assert_eq!(newest_release(&tags, "1.2.0", false).as_deref(), Some("1.10.0"));
        assert_eq!(newest_release(&tags, "1.10.0", false), None);
        assert_eq!(newest_release(&tags, "2.0.0", false), None);
        assert_eq!(newest_release(&tags, "dev", false), None);
    }

    /// ankor.sh's nodes run `latest`, so they follow ankor.sh across every line without an operator.
    #[test]
    fn latest_takes_the_newest_release_of_all_and_stays_latest() {
        let tags: Vec<String> = ["latest", "0.9.0", "0.9.2", "0.10.0", "1.0.0", "1.11.0-rc.1"].map(str::to_owned).to_vec();

        assert_eq!(newest_release(&tags, "0.9.0", true).as_deref(), Some("1.0.0"));
        assert_eq!(newest_release(&tags, "1.0.0", true), None);
        assert_eq!(
            target_reference(&parse_image_reference("ghcr.io/ankorcs/ankor-agent:latest"), "1.0.0").as_deref(),
            Some("ghcr.io/ankorcs/ankor-agent:latest")
        );
    }

    #[test]
    fn stays_on_the_running_minor_in_0_x() {
        let tags: Vec<String> = ["0", "0.7", "0.7.0", "0.7.3", "0.8.0", "1.0.0"].map(str::to_owned).to_vec();

        assert_eq!(newest_release(&tags, "0.7.0", false).as_deref(), Some("0.7.3"));
        assert_eq!(newest_release(&tags, "0.7.3", false), None);

        let at = |reference: &str| target_reference(&parse_image_reference(reference), "0.7.3");

        assert_eq!(at("ghcr.io/ankorcs/ankor-agent:0").as_deref(), Some("ghcr.io/ankorcs/ankor-agent:0.7"));
        assert_eq!(at("ghcr.io/ankorcs/ankor-agent:0.7").as_deref(), Some("ghcr.io/ankorcs/ankor-agent:0.7"));
        assert_eq!(at("ghcr.io/ankorcs/ankor-agent:0.7.0").as_deref(), Some("ghcr.io/ankorcs/ankor-agent:0.7.3"));
    }

    #[test]
    fn keeps_the_precision_of_a_release_tag() {
        let at = |reference: &str| target_reference(&parse_image_reference(reference), "1.3.0");

        assert_eq!(at("ghcr.io/ankorcs/ankor-agent:1").as_deref(), Some("ghcr.io/ankorcs/ankor-agent:1"));
        assert_eq!(at("ghcr.io/ankorcs/ankor-agent:1.2").as_deref(), Some("ghcr.io/ankorcs/ankor-agent:1.3"));
        assert_eq!(at("ghcr.io/ankorcs/ankor-agent:1.2.0").as_deref(), Some("ghcr.io/ankorcs/ankor-agent:1.3.0"));
        assert_eq!(at("ghcr.io/ankorcs/ankor-agent:review"), None);
        assert_eq!(at(&format!("ghcr.io/ankorcs/ankor-agent@sha256:{}", "a".repeat(64))), None);
    }
}

mod image_auto_update {
    use super::*;

    /// One value for each image kind.
    #[derive(Default)]
    struct PerKind<T> {
        cs2: T,
        agent: T,
    }

    impl<T> PerKind<T> {
        fn get(&self, kind: ImageKind) -> &T {
            match kind {
                ImageKind::Cs2 => &self.cs2,
                ImageKind::Agent => &self.agent,
            }
        }

        fn get_mut(&mut self, kind: ImageKind) -> &mut T {
            match kind {
                ImageKind::Cs2 => &mut self.cs2,
                ImageKind::Agent => &mut self.agent,
            }
        }
    }

    /// The node the polls read and update, as a test steers it.
    struct Node {
        enabled: Mutex<bool>,
        images: Mutex<PerKind<String>>,
        running: PerKind<Option<String>>,
        refuse: Mutex<Option<AgentFailure>>,
        rebuilt: Mutex<PerKind<Option<String>>>,
        rebuild_checks: Mutex<Vec<String>>,
        failed_swap: Mutex<Option<Candidate>>,
        updates: Mutex<Vec<(ImageKind, String)>>,
    }

    #[async_trait]
    impl ImageUpdates for Node {
        fn enabled(&self) -> bool {
            *self.enabled.lock().unwrap()
        }

        fn image(&self, kind: ImageKind) -> String {
            self.images.lock().unwrap().get(kind).clone()
        }

        async fn running_version(&self, kind: ImageKind) -> Result<Option<String>, OpError> {
            Ok(self.running.get(kind).clone())
        }

        async fn update(&self, kind: ImageKind, image: &str) -> Result<(), OpError> {
            if let Some(refusal) = self.refuse.lock().unwrap().clone() {
                return Err(refusal.into());
            }

            self.updates.lock().unwrap().push((kind, image.to_owned()));
            Ok(())
        }

        async fn rebuild(&self, kind: ImageKind, image: &str) -> Result<Option<String>, OpError> {
            self.rebuild_checks.lock().unwrap().push(image.to_owned());

            Ok(self.rebuilt.lock().unwrap().get(kind).clone())
        }

        async fn agent_swap_failed(&self, _image: &str, candidate: &Candidate) -> Result<bool, OpError> {
            Ok(self.failed_swap.lock().unwrap().as_ref() == Some(candidate))
        }
    }

    struct Setup {
        auto: ImageAutoUpdate,
        node: Arc<Node>,
        registry: Arc<Ghcr>,
    }

    fn setup(running: &[(ImageKind, Option<&str>)], images: &[(ImageKind, &str)], enabled: bool) -> Setup {
        let mut versions = PerKind { cs2: Some("1.0.0".to_owned()), agent: Some("1.0.0".to_owned()) };
        let mut references = PerKind { cs2: "ghcr.io/ankorcs/ankor-cs2:1".to_owned(), agent: "ghcr.io/ankorcs/ankor-agent:1".to_owned() };

        for (kind, version) in running {
            *versions.get_mut(*kind) = version.map(str::to_owned);
        }

        for (kind, image) in images {
            (*image).clone_into(references.get_mut(*kind));
        }

        let node = Arc::new(Node {
            enabled: Mutex::new(enabled),
            images: Mutex::new(references),
            running: versions,
            refuse: Mutex::default(),
            rebuilt: Mutex::default(),
            rebuild_checks: Mutex::default(),
            failed_swap: Mutex::default(),
            updates: Mutex::default(),
        });
        let registry = ghcr(&[("ankorcs/ankor-cs2", &["1.0.0", "1.1.0", "2.0.0"]), ("ankorcs/ankor-agent", &["1.0.0", "1.0.1"])]);
        let auto = ImageAutoUpdate::new(
            Arc::clone(&node) as Arc<dyn ImageUpdates>,
            Arc::clone(&registry) as Arc<dyn RegistryFetch>,
            Logger::null(),
            Duration::from_hours(1),
            tokio_util::sync::CancellationToken::new(),
        );

        Setup { auto, node, registry }
    }

    fn updates(setup: &Setup) -> Vec<(ImageKind, String)> {
        setup.node.updates.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn updates_the_cs2_image_then_the_agent_each_to_the_newest_release_of_its_major() {
        let setup = setup(&[], &[], true);

        setup.auto.poll().await;
        assert_eq!(
            updates(&setup),
            [(ImageKind::Cs2, "ghcr.io/ankorcs/ankor-cs2:1".to_owned()), (ImageKind::Agent, "ghcr.io/ankorcs/ankor-agent:1".to_owned())]
        );
    }

    #[tokio::test]
    async fn updates_to_a_rebuild_of_the_running_release_when_no_newer_one_is_out() {
        let setup = setup(&[(ImageKind::Cs2, Some("1.1.0")), (ImageKind::Agent, Some("1.0.1"))], &[], true);

        setup.auto.poll().await;
        assert!(updates(&setup).is_empty());
        assert_eq!(*setup.node.rebuild_checks.lock().unwrap(), ["ghcr.io/ankorcs/ankor-cs2:1", "ghcr.io/ankorcs/ankor-agent:1"]);

        *setup.node.rebuilt.lock().unwrap().get_mut(ImageKind::Cs2) = Some("sha256:b".to_owned());
        setup.auto.poll().await;
        assert_eq!(updates(&setup), [(ImageKind::Cs2, "ghcr.io/ankorcs/ankor-cs2:1".to_owned())]);
    }

    #[tokio::test]
    async fn checks_for_a_rebuild_under_the_tag_the_node_names_at_its_precision() {
        let setup = setup(
            &[(ImageKind::Cs2, Some("1.1.0")), (ImageKind::Agent, Some("1.0.1"))],
            &[(ImageKind::Cs2, "ghcr.io/ankorcs/ankor-cs2:1.1.0"), (ImageKind::Agent, "ghcr.io/ankorcs/ankor-agent:1.0")],
            true,
        );

        setup.auto.poll().await;
        assert_eq!(*setup.node.rebuild_checks.lock().unwrap(), ["ghcr.io/ankorcs/ankor-cs2:1.1.0", "ghcr.io/ankorcs/ankor-agent:1.0"]);
    }

    #[tokio::test]
    async fn does_not_swap_the_agent_to_the_build_a_rolled_back_swap_failed_with_but_to_a_newer_one() {
        let setup = setup(&[(ImageKind::Cs2, Some("1.1.0")), (ImageKind::Agent, Some("1.0.0"))], &[], true);

        *setup.node.failed_swap.lock().unwrap() = Some(Candidate { version: "1.0.1".to_owned(), digest: None });
        setup.auto.poll().await;
        assert!(updates(&setup).is_empty());

        *setup.node.failed_swap.lock().unwrap() = Some(Candidate { version: "1.0.0".to_owned(), digest: Some("sha256:old".to_owned()) });
        setup.auto.poll().await;
        assert_eq!(updates(&setup), [(ImageKind::Agent, "ghcr.io/ankorcs/ankor-agent:1".to_owned())]);
    }

    #[tokio::test]
    async fn tries_an_update_refused_while_other_maintenance_runs_again_when_that_maintenance_ends() {
        let setup = setup(&[], &[], true);

        *setup.node.refuse.lock().unwrap() = Some(AgentFailure::new(AgentErrorCode::MaintenanceRunning, "a cs2 update runs"));
        setup.auto.poll().await;
        assert!(updates(&setup).is_empty());

        *setup.node.refuse.lock().unwrap() = None;
        setup.auto.maintenance_ended().await;
        assert_eq!(updates(&setup).len(), 2);
    }

    #[tokio::test]
    async fn polls_after_maintenance_only_when_agent_yml_names_other_images_than_the_last_poll_read() {
        let setup = setup(&[(ImageKind::Cs2, Some("1.1.0")), (ImageKind::Agent, Some("1.0.1"))], &[], true);

        setup.auto.poll().await;

        let reads = setup.registry.calls().len();

        setup.auto.maintenance_ended().await;
        assert_eq!(setup.registry.calls().len(), reads);

        *setup.node.images.lock().unwrap().get_mut(ImageKind::Cs2) = "ghcr.io/ankorcs/ankor-cs2:1.1".to_owned();
        setup.auto.maintenance_ended().await;
        assert!(setup.registry.calls().len() > reads);
    }

    #[tokio::test]
    async fn checks_again_when_maintenance_that_changed_an_image_ends_during_a_poll() {
        let setup = setup(&[(ImageKind::Cs2, Some("1.1.0")), (ImageKind::Agent, Some("1.0.1"))], &[], true);
        let first = setup.auto.poll();

        *setup.node.images.lock().unwrap().get_mut(ImageKind::Cs2) = "ghcr.io/ankorcs/ankor-cs2:1.1".to_owned();
        setup.auto.maintenance_ended().await;
        first.await;
        until("the poll after the maintenance", || setup.node.rebuild_checks.lock().unwrap().contains(&"ghcr.io/ankorcs/ankor-cs2:1.1".to_owned())).await;
    }

    #[tokio::test]
    async fn polls_once_at_the_start_and_next_after_the_interval() {
        let one = setup(&[], &[], true);

        one.auto.poll().await;

        let per_poll = one.registry.calls().len();
        let started = setup(&[], &[], true);

        started.auto.start();
        until("the first poll", || started.registry.calls().len() >= per_poll).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(started.registry.calls().len(), per_poll);
    }

    /// An interval longer than the host has been up, as on a host that booted minutes ago, still
    /// schedules the next poll one interval after the start.
    #[tokio::test(start_paused = true)]
    async fn polls_again_after_an_interval_longer_than_the_host_has_been_up() {
        let one = setup(&[], &[], true);

        one.auto.poll().await;

        let per_poll = one.registry.calls().len();
        let started = setup(&[], &[], true);
        let interval = Duration::from_hours(2 * 365 * 24);
        let auto = ImageAutoUpdate::new(
            Arc::clone(&started.node) as Arc<dyn ImageUpdates>,
            Arc::clone(&started.registry) as Arc<dyn RegistryFetch>,
            Logger::null(),
            interval,
            tokio_util::sync::CancellationToken::new(),
        );

        auto.start();
        until("the first poll", || started.registry.calls().len() >= per_poll).await;
        tokio::time::sleep(interval.checked_sub(Duration::from_secs(1)).unwrap()).await;
        assert_eq!(started.registry.calls().len(), per_poll);
        tokio::time::sleep(Duration::from_secs(2)).await;
        until("the poll after the interval", || started.registry.calls().len() >= 2 * per_poll).await;
    }

    #[tokio::test]
    async fn logs_each_update_under_the_name_of_its_image() {
        let setup = setup(&[], &[], true);
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let auto = ImageAutoUpdate::new(
            Arc::clone(&setup.node) as Arc<dyn ImageUpdates>,
            Arc::clone(&setup.registry) as Arc<dyn RegistryFetch>,
            Logger::new("test", Some(Arc::new(move |entry: ankor_agent::logger::Entry| seen.lock().unwrap().push(entry.message)))),
            Duration::from_hours(1),
            tokio_util::sync::CancellationToken::new(),
        );

        auto.poll().await;

        assert_eq!(
            *lines.lock().unwrap(),
            [
                "[test] updating the cs2 image from 1.0.0 to 1.1.0 as ghcr.io/ankorcs/ankor-cs2:1",
                "[test] updating the agent image from 1.0.0 to 1.0.1 as ghcr.io/ankorcs/ankor-agent:1",
            ]
        );
    }

    #[tokio::test]
    async fn leaves_alone_a_node_with_auto_update_off_a_pinned_image_and_an_image_without_a_release_version() {
        let off = setup(&[], &[], false);

        off.auto.poll().await;
        assert!(updates(&off).is_empty());
        assert!(off.registry.calls().is_empty());

        let pinned = setup(&[(ImageKind::Agent, None)], &[(ImageKind::Cs2, &format!("ghcr.io/ankorcs/ankor-cs2@sha256:{}", "a".repeat(64)))], true);

        pinned.auto.poll().await;
        assert!(updates(&pinned).is_empty());
        assert!(pinned.registry.calls().is_empty());
    }
}
