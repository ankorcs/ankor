//! Reads an image repository's tags, and the digest a tag points to, from its registry over the
//! Docker Registry HTTP API. It reads anonymously, because the images are public. A registry that
//! wants a token answers `401` and names its token service, and the agent then sends a pull token
//! from that service.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use reqwest::StatusCode;
use serde::Deserialize;

use crate::docker_api::DEFAULT_TAG;
use crate::http_body::is_success;
use crate::logger::Logger;
use crate::sync::lock;

const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// The tags asked for per page; a registry may send fewer and link the next page.
const PAGE_SIZE: u32 = 1000;
/// Pages read before giving up, against a registry that keeps linking a next page.
const MAX_PAGES: usize = 20;

/// Docker Hub as image references name it.
const DOCKER_HUB: &str = "docker.io";
/// The older name of Docker Hub that references may still use.
const DOCKER_HUB_INDEX: &str = "index.docker.io";
/// The host that serves Docker Hub's registry API, which `docker.io` itself does not.
const DOCKER_HUB_API: &str = "registry-1.docker.io";

/// The manifest types `docker pull` accepts. The registry answers a tag with the digest of the type
/// it serves, so asking for the same types gives the digest that the pull recorded.
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static BEARER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^bearer\s").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static PARAMETER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([A-Za-z0-9_]+)="([^"]*)""#).expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static NEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)<([^>]+)>\s*;\s*rel="?next"?"#).expect("valid pattern"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
}

/// A registry's answer, its header names in lower case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryAnswer {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl RegistryAnswer {
    #[must_use]
    pub fn ok(&self) -> bool {
        is_success(self.status)
    }

    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// Why a registry read failed.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("no HTTP client: {0}")]
    NoClient(#[source] Arc<reqwest::Error>),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("the registry asked for a token without naming where to get one")]
    NoRealm,
    #[error("the registry named a token service at an invalid URL: {0}")]
    TokenUrl(#[source] url::ParseError),
    #[error("the token service answered HTTP {0}")]
    TokenStatus(u16),
    #[error("the token service answered {0}")]
    TokenJson(#[source] serde_json::Error),
    #[error("the token service answered without a token")]
    NoToken,
    #[error("the registry refused an anonymous read")]
    Anonymous,
    #[error("{}", crate::http_body::too_large(crate::http_body::ANSWER_MAX_BYTES))]
    TooLarge,
    #[error("{base} answered HTTP {status} for the tags of {repository}")]
    TagsStatus { base: String, status: u16, repository: String },
    #[error("{base} answered an unreadable tag list: {source}")]
    TagList { base: String, source: serde_json::Error },
    #[error("{reference} names no tag")]
    NoTag { reference: String },
    #[error("{base} gave no digest for {reference} (HTTP {status})")]
    NoDigest { base: String, reference: String, status: u16 },
}

/// The registry calls the agent makes, so tests can answer them.
#[async_trait]
pub trait RegistryFetch: Send + Sync {
    /// `Err` is a request that got no answer.
    async fn fetch(&self, url: &str, method: Method, headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError>;
}

pub struct HttpRegistry(Result<reqwest::Client, Arc<reqwest::Error>>);

impl HttpRegistry {
    #[must_use]
    pub fn new() -> Self {
        crate::init_tls();

        Self(reqwest::Client::builder().timeout(FETCH_TIMEOUT).build().map_err(Arc::new))
    }
}

impl Default for HttpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RegistryFetch for HttpRegistry {
    async fn fetch(&self, url: &str, method: Method, headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError> {
        let client = self.0.as_ref().map_err(|error| RegistryError::NoClient(Arc::clone(error)))?;
        let mut request = match method {
            Method::Get => client.get(url),
            Method::Head => client.head(url),
        };

        for (name, value) in headers {
            request = request.header(*name, value);
        }

        let response = request.send().await?;
        let status = response.status().as_u16();
        // A header sent more than once reads as its values joined, as fetch's Headers.get gives them.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();

        for (name, value) in response.headers() {
            if let Ok(value) = value.to_str() {
                headers
                    .entry(name.as_str().to_ascii_lowercase())
                    .and_modify(|joined| *joined = format!("{joined}, {value}"))
                    .or_insert_with(|| value.to_owned());
            }
        }
        let body = crate::http_body::read_limited(response, crate::http_body::ANSWER_MAX_BYTES).await?.ok_or(RegistryError::TooLarge)?;

        Ok(RegistryAnswer { status, headers, body })
    }
}

/// An image reference split into its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageName {
    /// The registry's host, such as `ghcr.io`; `docker.io` for a reference that names none.
    pub registry: String,
    /// The repository path on the registry, such as `ankorcs/ankor-cs2`, or `library/nginx` for `nginx`.
    pub repository: String,
    /// The reference without its tag or digest.
    pub name: String,
    /// The tag, or `latest` when the reference names neither a tag nor a digest.
    pub tag: Option<String>,
    /// The digest after `@`, when the reference pins one.
    pub digest: Option<String>,
}

/// Splits `reference` the way `docker pull` does. The first part is the registry when it is a host.
/// Otherwise the registry is Docker Hub, where a one-part name is an official image under `library/`.
#[must_use]
pub fn parse_image_reference(reference: &str) -> ImageName {
    let (named, digest) = reference.split_once('@').map_or((reference, None), |(named, digest)| (named, Some(digest.to_owned())));
    let colon = named.rfind(':');
    let has_tag = colon.is_some_and(|colon| named.rfind('/').is_none_or(|slash| colon > slash));
    let (name, tag) = match colon.filter(|_| has_tag).and_then(|colon| named.get(..colon).zip(named.get(colon + 1..))) {
        Some((name, tag)) => (name, Some(tag.to_owned())),
        None => (named, None),
    };
    let first = name.split_once('/').map_or("", |(first, _)| first);
    let has_host = first.contains('.') || first.contains(':') || first == "localhost";
    // Docker reads index.docker.io as another name for Docker Hub.
    let registry = if has_host && first != DOCKER_HUB_INDEX { first } else { DOCKER_HUB };
    let path = if has_host { name.split_once('/').map_or(name, |(_, path)| path) } else { name };
    let repository = if registry == DOCKER_HUB && !path.contains('/') { format!("library/{path}") } else { path.to_owned() };
    let tag = tag.or_else(|| digest.is_none().then(|| DEFAULT_TAG.to_owned()));

    ImageName { registry: registry.to_owned(), repository, name: name.to_owned(), tag, digest }
}

/// The parameters of a `Bearer` challenge, such as `realm`, `service` and `scope`.
#[must_use]
pub fn parse_bearer_challenge(header: Option<&str>) -> Option<BTreeMap<String, String>> {
    let header = header.filter(|header| BEARER.is_match(header))?;
    let parameters = header.split_once(' ').map_or(header, |(_, rest)| rest);

    Some(
        PARAMETER.captures_iter(parameters).filter_map(|captures| Some((captures.get(1)?.as_str().to_owned(), captures.get(2)?.as_str().to_owned()))).collect(),
    )
}

/// The URL a `Link` header names as `rel="next"`, resolved against `base`.
#[must_use]
pub fn next_link(header: Option<&str>, base: &str) -> Option<String> {
    let link = NEXT.captures(header?)?.get(1)?.as_str().to_owned();

    url::Url::parse(base).ok()?.join(&link).ok().map(String::from)
}

#[derive(Deserialize)]
struct TagList {
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    token: Option<String>,
    access_token: Option<String>,
}

fn set_query(url: &mut url::Url, name: &str, value: &str) {
    let kept: Vec<(String, String)> = url.query_pairs().filter(|(key, _)| key != name).map(|(key, value)| (key.into_owned(), value.into_owned())).collect();

    url.query_pairs_mut().clear().extend_pairs(kept).append_pair(name, value);
}

async fn pull_token(fetch: &dyn RegistryFetch, challenge: &BTreeMap<String, String>, repository: &str) -> Result<String, RegistryError> {
    let realm = challenge.get("realm").ok_or(RegistryError::NoRealm)?;
    let mut url = url::Url::parse(realm).map_err(RegistryError::TokenUrl)?;

    if let Some(service) = challenge.get("service") {
        set_query(&mut url, "service", service);
    }

    set_query(&mut url, "scope", &challenge.get("scope").cloned().unwrap_or_else(|| format!("repository:{repository}:pull")));

    let answer = fetch.fetch(url.as_str(), Method::Get, &[]).await?;

    if !answer.ok() {
        return Err(RegistryError::TokenStatus(answer.status));
    }

    let parsed: TokenAnswer = serde_json::from_slice(&answer.body).map_err(RegistryError::TokenJson)?;

    parsed.token.filter(|token| !token.is_empty()).or_else(|| parsed.access_token.filter(|token| !token.is_empty())).ok_or(RegistryError::NoToken)
}

/// The registry of a reference, with the pull token it asks for, fetched on its first `401`.
struct Registry<'a> {
    base: String,
    repository: String,
    fetch: &'a dyn RegistryFetch,
    token: Mutex<Option<String>>,
}

impl<'a> Registry<'a> {
    fn new(reference: &str, fetch: &'a dyn RegistryFetch) -> Self {
        let image = parse_image_reference(reference);
        let host = if image.registry == DOCKER_HUB { DOCKER_HUB_API.to_owned() } else { image.registry };

        Self { base: format!("https://{host}"), repository: image.repository, fetch, token: Mutex::new(None) }
    }

    async fn send(&self, url: &str, method: Method, headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError> {
        let mut all = headers.to_vec();

        if let Some(token) = lock(&self.token).clone() {
            all.push(("Authorization", format!("Bearer {token}")));
        }

        self.fetch.fetch(url, method, &all).await
    }

    async fn request(&self, url: &str, method: Method, headers: &[(&str, String)]) -> Result<RegistryAnswer, RegistryError> {
        let answer = self.send(url, method, headers).await?;

        if answer.status != StatusCode::UNAUTHORIZED || lock(&self.token).is_some() {
            return Ok(answer);
        }

        let challenge = parse_bearer_challenge(answer.header("www-authenticate")).ok_or(RegistryError::Anonymous)?;

        let token = pull_token(self.fetch, &challenge, &self.repository).await?;

        *lock(&self.token) = Some(token);
        self.send(url, method, headers).await
    }
}

/// Reads at most [`MAX_PAGES`] pages, all on the registry's origin. A shorter list says why in the
/// log.
///
/// # Errors
///
/// Fails when the registry or its token service refuses, or answers something unreadable.
pub async fn list_tags(reference: &str, fetch: &dyn RegistryFetch, log: &Logger) -> Result<Vec<String>, RegistryError> {
    let registry = Registry::new(reference, fetch);
    let mut tags = Vec::new();
    let mut url = Some(format!("{}/v2/{}/tags/list?n={PAGE_SIZE}", registry.base, registry.repository));
    let mut page = 0;
    let origin = url::Url::parse(&registry.base).ok().map(|base| base.origin());

    while let Some(current) = url.take() {
        if page == MAX_PAGES {
            log.warn(format!("{} lists more than {MAX_PAGES} pages of tags for {}; reading only those", registry.base, registry.repository));
            break;
        }

        // The token is the registry's, so a page on another origin (scheme, host or port) is not
        // asked for.
        let elsewhere = url::Url::parse(&current).ok().map(|next| next.origin()) != origin;

        if elsewhere {
            log.warn(format!("{} linked the tags of {} to another origin; reading no further", registry.base, registry.repository));
            break;
        }

        let answer = registry.request(&current, Method::Get, &[]).await?;

        if !answer.ok() {
            return Err(RegistryError::TagsStatus { base: registry.base, status: answer.status, repository: registry.repository });
        }

        let list: TagList = serde_json::from_slice(&answer.body).map_err(|source| RegistryError::TagList { base: registry.base.clone(), source })?;

        tags.extend(list.tags.unwrap_or_default());
        url = next_link(answer.header("link"), &registry.base);
        page += 1;
    }

    Ok(tags)
}

/// The digest the tag in `reference` points to on its registry now.
///
/// # Errors
///
/// Fails when the reference names no tag, or the registry gives no digest.
pub async fn tag_digest(reference: &str, fetch: &dyn RegistryFetch) -> Result<String, RegistryError> {
    let image = parse_image_reference(reference);
    let tag = image.tag.ok_or_else(|| RegistryError::NoTag { reference: reference.to_owned() })?;
    let registry = Registry::new(reference, fetch);
    let url = format!("{}/v2/{}/manifests/{tag}", registry.base, registry.repository);
    let answer = registry.request(&url, Method::Head, &[("Accept", MANIFEST_TYPES.to_owned())]).await?;

    match answer.header("docker-content-digest") {
        Some(digest) if answer.ok() => Ok(digest.to_owned()),
        _ => Err(RegistryError::NoDigest { base: registry.base, reference: reference.to_owned(), status: answer.status }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(reference: &str) -> (String, String, String, Option<String>, Option<String>) {
        let image = parse_image_reference(reference);

        (image.registry, image.repository, image.name, image.tag, image.digest)
    }

    fn own(text: &str) -> String {
        text.to_owned()
    }

    #[test]
    fn splits_a_reference_the_way_docker_pull_does() {
        assert_eq!(name("nginx"), (own("docker.io"), own("library/nginx"), own("nginx"), Some(own("latest")), None));
        assert_eq!(name("ghcr.io/ankorcs/ankor-cs2:1.2"), (own("ghcr.io"), own("ankorcs/ankor-cs2"), own("ghcr.io/ankorcs/ankor-cs2"), Some(own("1.2")), None));
        assert_eq!(name("localhost:5000/agent"), (own("localhost:5000"), own("agent"), own("localhost:5000/agent"), Some(own("latest")), None));
        assert_eq!(name("index.docker.io/user/app:v1"), (own("docker.io"), own("user/app"), own("index.docker.io/user/app"), Some(own("v1")), None));
        assert_eq!(name("user/app@sha256:abc"), (own("docker.io"), own("user/app"), own("user/app"), None, Some(own("sha256:abc"))));
    }

    #[test]
    fn reads_a_bearer_challenge_and_a_next_link() {
        let challenge = parse_bearer_challenge(Some(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull""#)).unwrap();

        assert_eq!(challenge["realm"], "https://ghcr.io/token");
        assert_eq!(challenge["scope"], "repository:a/b:pull");
        assert_eq!(parse_bearer_challenge(Some("Basic realm=\"x\"")), None);
        assert_eq!(
            next_link(Some(r#"</v2/a/tags/list?n=2&last=b>; rel="next""#), "https://ghcr.io").as_deref(),
            Some("https://ghcr.io/v2/a/tags/list?n=2&last=b")
        );
        assert_eq!(next_link(None, "https://ghcr.io"), None);
    }
}
