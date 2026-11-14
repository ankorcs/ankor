//! The Docker Engine API calls the agent makes, over the local socket at one pinned API version.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use futures_util::TryStreamExt as _;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

use crate::host_layout::DOCKER_SOCKET;
use crate::registry_tags::parse_image_reference;

/// The Engine API version every call names. Docker Engine 25.0 and newer answer it, Docker 29
/// answers nothing older, and an older daemon refuses it, which the agent reports at start.
pub(crate) const DOCKER_API_VERSION: &str = "1.44";

/// The Docker Engine release that first supports [`DOCKER_API_VERSION`].
pub(crate) const MIN_DOCKER_ENGINE: &str = "25.0";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ListedContainer {
    pub id: String,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(rename = "ImageID")]
    pub image_id: String,
    pub state: String,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerState {
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub exit_code: Option<i64>,
}

/// An inspected container. `Config` and `HostConfig` stay raw, because a swap copies them whole.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerInspect {
    pub id: String,
    pub name: String,
    pub image: String,
    pub config: Value,
    #[serde(default)]
    pub host_config: Option<Value>,
    #[serde(default)]
    pub state: Option<ContainerState>,
    #[serde(default)]
    pub restart_count: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ListedImage {
    pub id: String,
    #[serde(default)]
    pub repo_tags: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageConfig {
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageInspect {
    /// The image id, `sha256:…`.
    #[serde(default)]
    pub id: Option<String>,
    /// `repository@sha256:…` for each registry it came from; empty for a built or loaded image.
    #[serde(default)]
    pub repo_digests: Option<Vec<String>>,
    /// The image's environment and labels.
    #[serde(default)]
    pub config: Option<ImageConfig>,
}

/// Docker's `filters` parameter: each key to the values it must match.
pub type Filters = BTreeMap<String, Vec<String>>;

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    /// The daemon answered outside 2xx, with this message.
    #[error("{message}")]
    Api { status: u16, message: String },
    /// The socket could not be reached, read or written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The HTTP exchange failed, or an answer lacked a field the call needs.
    #[error("{0}")]
    Http(String),
    /// An answer or progress line was not the expected JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A pull whose progress stream reported an error.
    #[error("{0}")]
    Pull(String),
    /// A pull whose progress stream went quiet for [`PULL_QUIET`].
    #[error("no progress for {} s", PULL_QUIET.as_secs())]
    PullStalled,
    /// A copy out of an image that took longer than [`crate::docker::COPY_TIMEOUT`].
    #[error("the copy took longer than {} s", crate::docker::COPY_TIMEOUT.as_secs())]
    CopyTimedOut,
    /// The daemon did not answer within the call's limit. The call may still take effect.
    #[error("Docker gave no answer within {} s", .0.as_secs())]
    NoAnswer(Duration),
}

/// How long a call may wait for its answer. A daemon on the local socket that has not answered by
/// then has gone wrong, so the call fails instead of waiting on.
pub const ANSWER_WITHIN: Duration = ankor_protocol::constants::DOCKER_ANSWER_TIMEOUT;

/// How long a pull's progress stream may stay quiet before the pull is given up. Docker sends a
/// progress line for every layer's download and extraction, so a quiet stream is a stalled pull.
/// Dropping the stream cancels the pull in the daemon.
pub const PULL_QUIET: std::time::Duration = std::time::Duration::from_mins(5);

impl DockerError {
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    #[must_use]
    pub fn has_status(&self, statuses: &[StatusCode]) -> bool {
        self.status().is_some_and(|status| statuses.iter().any(|expected| status == *expected))
    }
}

impl From<hyper::Error> for DockerError {
    fn from(error: hyper::Error) -> Self {
        Self::Http(error.to_string())
    }
}

/// A container's output as one byte stream.
pub type OutputStream = Pin<Box<dyn AsyncRead + Send>>;

/// A wait the daemon has registered. It resolves with the container's exit code when the daemon
/// gives one, however long the container runs.
pub type PendingExit = Pin<Box<dyn Future<Output = Result<Option<i64>, DockerError>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitCondition {
    /// When the container is not running, which may be at once.
    NotRunning,
    /// When the container next exits. A wait registered before the start keeps the exit code of a
    /// container that Docker removes as it exits.
    NextExit,
    /// When the container is removed, with its exit code. Docker lists a container as running for a
    /// moment after a `NotRunning` wait returns; by now it lists it no longer.
    Removed,
}

impl WaitCondition {
    const fn as_query(self) -> &'static str {
        match self {
            Self::NotRunning => "not-running",
            Self::NextExit => "next-exit",
            Self::Removed => "removed",
        }
    }
}

trait Duplex: AsyncRead + AsyncWrite + Send {}

impl<T: AsyncRead + AsyncWrite + Send> Duplex for T {}

#[derive(Clone, Debug)]
pub struct DockerApi {
    socket_path: PathBuf,
}

impl Default for DockerApi {
    fn default() -> Self {
        Self::new(DOCKER_SOCKET)
    }
}

#[derive(Clone, Copy)]
enum Query<'a> {
    Text(&'a str),
    Number(i64),
    Flag(bool),
    Filters(&'a Filters),
}

impl DockerApi {
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self { socket_path: socket_path.into() }
    }

    /// The daemon's release and newest API version.
    ///
    /// # Errors
    ///
    /// Fails when the daemon cannot be reached or refuses the pinned API version.
    pub async fn engine(&self) -> Result<(String, String), DockerError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Version {
            version: String,
            api_version: String,
        }

        let answer: Version = self.json(Method::GET, "/version", &[], None).await?;

        Ok((answer.version, answer.api_version))
    }

    /// Returns the new container's id.
    ///
    /// # Errors
    ///
    /// Fails as the daemon refuses the payload.
    pub async fn create_container(&self, name: &str, spec: &Value) -> Result<String, DockerError> {
        #[derive(Deserialize)]
        struct Created {
            #[serde(rename = "Id")]
            id: String,
        }

        let created: Created = self.json(Method::POST, "/containers/create", &[("name", Query::Text(name))], Some(spec)).await?;

        Ok(created.id)
    }

    /// # Errors
    ///
    /// Fails as the daemon refuses.
    pub async fn start_container(&self, id: &str) -> Result<(), DockerError> {
        self.call(Method::POST, &format!("/containers/{id}/start"), &[], None).await
    }

    /// # Errors
    ///
    /// Fails with status 304 when the container is already stopped.
    pub async fn stop_container(&self, id: &str, timeout_seconds: i64) -> Result<(), DockerError> {
        // The daemon answers once the grace is over and the container killed.
        let within = ANSWER_WITHIN + Duration::from_secs(u64::try_from(timeout_seconds).unwrap_or(0));

        self.exchange(Some(within), Method::POST, &format!("/containers/{id}/stop"), &[("t", Query::Number(timeout_seconds))], None).await.map(|_| ())
    }

    /// # Errors
    ///
    /// Fails as the daemon refuses.
    pub async fn remove_container(&self, id: &str, force: bool, volumes: bool) -> Result<(), DockerError> {
        self.call(Method::DELETE, &format!("/containers/{id}"), &[("force", Query::Flag(force)), ("v", Query::Flag(volumes))], None).await
    }

    /// # Errors
    ///
    /// Fails as the daemon refuses.
    pub async fn rename_container(&self, id: &str, name: &str) -> Result<(), DockerError> {
        self.call(Method::POST, &format!("/containers/{id}/rename"), &[("name", Query::Text(name))], None).await
    }

    /// # Errors
    ///
    /// Fails with status 404 for an unknown container.
    pub async fn inspect_container(&self, id: &str) -> Result<ContainerInspect, DockerError> {
        self.json(Method::GET, &format!("/containers/{id}/json"), &[], None).await
    }

    /// # Errors
    ///
    /// Fails as the daemon refuses.
    pub async fn list_containers(&self, all: bool, filters: Option<&Filters>) -> Result<Vec<ListedContainer>, DockerError> {
        let mut query = vec![("all", Query::Flag(all))];

        if let Some(filters) = filters {
            query.push(("filters", Query::Filters(filters)));
        }

        self.json(Method::GET, "/containers/json", &query, None).await
    }

    /// Registers a wait for `condition`. The daemon answers with its headers once the wait is
    /// registered, and with the exit code in the body when the condition is met.
    ///
    /// # Errors
    ///
    /// Fails as the daemon refuses, or when it does not register the wait in time.
    pub async fn wait_container(&self, id: &str, condition: WaitCondition) -> Result<PendingExit, DockerError> {
        let query = [("condition", Query::Text(condition.as_query()))];
        let response = within(ANSWER_WITHIN, self.send(Method::POST, &format!("/containers/{id}/wait"), &query, None)).await?;

        Ok(Box::pin(async move {
            let answer: Option<Value> = parse(&read_body(response).await?)?;

            Ok(answer.and_then(|answer| answer.get("StatusCode").and_then(Value::as_i64)))
        }))
    }

    /// The container's output as one stream; a container made with `Tty` sends it unmultiplexed.
    ///
    /// # Errors
    ///
    /// Fails as the daemon refuses the attach.
    pub async fn attach_container(&self, id: &str) -> Result<OutputStream, DockerError> {
        let path = path_with_query(
            &format!("/containers/{id}/attach"),
            &[("stream", Query::Flag(true)), ("stdout", Query::Flag(true)), ("stderr", Query::Flag(true))],
        );
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("Host", "docker")
            .header("Connection", "Upgrade")
            .header("Upgrade", "tcp")
            .body(Empty::<Bytes>::new())
            .map_err(|error| DockerError::Http(error.to_string()))?;
        // Only the upgrade is limited; the stream after it lasts as long as the container.
        let upgrade = async {
            let stream = UnixStream::connect(&self.socket_path).await?;
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;

            tokio::spawn(connection.with_upgrades());

            let response = sender.send_request(request).await?;
            let status = response.status();

            if status != StatusCode::SWITCHING_PROTOCOLS && !status.is_success() {
                return Err(api_error(status.as_u16(), &read_body(response).await?));
            }

            Ok(hyper::upgrade::on(response).await?)
        };
        let upgraded = within(ANSWER_WITHIN, upgrade).await?;
        let io: Pin<Box<dyn Duplex>> = Box::pin(TokioIo::new(upgraded));

        Ok(io)
    }

    /// # Errors
    ///
    /// Fails with status 404 for an unknown image.
    pub async fn inspect_image(&self, reference: &str) -> Result<ImageInspect, DockerError> {
        self.json(Method::GET, &format!("/images/{reference}/json"), &[], None).await
    }

    /// # Errors
    ///
    /// Fails as the daemon refuses.
    pub async fn list_images(&self, filters: Option<&Filters>) -> Result<Vec<ListedImage>, DockerError> {
        let query: Vec<_> = filters.map(|filters| ("filters", Query::Filters(filters))).into_iter().collect();

        self.json(Method::GET, "/images/json", &query, None).await
    }

    /// # Errors
    ///
    /// Fails as the daemon refuses, as for an image a container uses.
    pub async fn remove_image(&self, reference: &str) -> Result<(), DockerError> {
        self.call(Method::DELETE, &format!("/images/{reference}"), &[], None).await
    }

    /// The digest the registry holds for the reference now, asked of the daemon, which uses the
    /// credentials it pulls with.
    ///
    /// # Errors
    ///
    /// Fails as the daemon or the registry refuses, or when the answer carries no digest.
    pub async fn registry_digest(&self, reference: &str) -> Result<String, DockerError> {
        let answer: Value = self.json(Method::GET, &format!("/distribution/{reference}/json"), &[], None).await?;

        answer
            .pointer("/Descriptor/digest")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| DockerError::Http(format!("the registry gave no digest for {reference}")))
    }

    /// Unpacks `path` from the container's filesystem into `target`, as `<target>/<last component
    /// of path>`, keeping modes and symlinks. The container need not be running.
    ///
    /// # Errors
    ///
    /// Fails as the daemon refuses, as with a 404 for a path the container lacks, or when the
    /// archive cannot be written out.
    pub async fn copy_from_container(&self, id: &str, path: &str, target: &Path) -> Result<(), DockerError> {
        let response = within(ANSWER_WITHIN, self.send(Method::GET, &format!("/containers/{id}/archive"), &[("path", Query::Text(path))], None)).await?;
        let body = response.into_body().into_data_stream().map_err(std::io::Error::other);
        let reader = tokio_util::io::SyncIoBridge::new(tokio_util::io::StreamReader::new(body));
        let target = target.to_owned();

        tokio::task::spawn_blocking(move || {
            let mut archive = tar::Archive::new(reader);

            // Executables need their mode; the owner is the agent's.
            archive.set_preserve_permissions(true);
            archive.unpack(&target)
        })
        .await
        .map_err(std::io::Error::other)??;

        Ok(())
    }

    /// Pulls the image and returns when the daemon's progress stream ends.
    ///
    /// # Errors
    ///
    /// Fails as the daemon refuses, or when a progress line carries an error.
    pub async fn pull_image(&self, reference: &str) -> Result<(), DockerError> {
        let (repository, tag) = split_reference(reference);
        let response =
            within(ANSWER_WITHIN, self.send(Method::POST, "/images/create", &[("fromImage", Query::Text(&repository)), ("tag", Query::Text(&tag))], None))
                .await?;
        let mut body = response.into_body();
        let mut pending: Vec<u8> = Vec::new();

        while let Some(frame) = tokio::time::timeout(PULL_QUIET, body.frame()).await.map_err(|_| DockerError::PullStalled)? {
            if let Some(data) = frame?.data_ref() {
                pending.extend_from_slice(data);

                while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();

                    check_progress(&line, reference)?;
                }
            }
        }

        check_progress(&pending, reference)
    }

    async fn json<T: DeserializeOwned>(&self, method: Method, path: &str, query: &[(&str, Query<'_>)], body: Option<&Value>) -> Result<T, DockerError> {
        parse(&self.exchange(Some(ANSWER_WITHIN), method, path, query, body).await?)
    }

    async fn call(&self, method: Method, path: &str, query: &[(&str, Query<'_>)], body: Option<&Value>) -> Result<(), DockerError> {
        self.exchange(Some(ANSWER_WITHIN), method, path, query, body).await.map(|_| ())
    }

    async fn exchange(
        &self,
        limit: Option<Duration>,
        method: Method,
        path: &str,
        query: &[(&str, Query<'_>)],
        body: Option<&Value>,
    ) -> Result<String, DockerError> {
        let answered = async { read_body(self.send(method, path, query, body).await?).await };

        match limit {
            Some(limit) => within(limit, answered).await,
            None => answered.await,
        }
    }

    /// The response of a 2xx answer, unread; anything else fails with its status and message.
    async fn send(&self, method: Method, path: &str, query: &[(&str, Query<'_>)], body: Option<&Value>) -> Result<Response<Incoming>, DockerError> {
        let builder = Request::builder().method(method).uri(path_with_query(path, query)).header("Host", "docker");
        let request = match body {
            Some(body) => {
                let payload = serde_json::to_vec(body)?;

                builder.header("Content-Type", "application/json").body(BoxBody::new(Full::new(Bytes::from(payload)).map_err(|never| match never {})))
            }
            None => builder.body(BoxBody::new(Empty::<Bytes>::new().map_err(|never| match never {}))),
        }
        .map_err(|error| DockerError::Http(error.to_string()))?;
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;

        tokio::spawn(connection);

        let response = sender.send_request(request).await?;
        let status = response.status();

        if status.is_success() {
            return Ok(response);
        }

        Err(api_error(status.as_u16(), &read_body(response).await?))
    }
}

/// The versioned path with its query. `filters` goes as JSON, flags as `true`/`false`.
fn path_with_query(unversioned: &str, query: &[(&str, Query<'_>)]) -> String {
    let path = format!("/v{DOCKER_API_VERSION}{unversioned}");
    let mut params = form_urlencoded::Serializer::new(String::new());

    for (key, value) in query {
        let value = match value {
            Query::Text(text) => (*text).to_owned(),
            Query::Number(number) => number.to_string(),
            Query::Flag(flag) => flag.to_string(),
            Query::Filters(filters) => serde_json::to_string(filters).unwrap_or_default(),
        };

        params.append_pair(key, &value);
    }

    let encoded = params.finish();

    if encoded.is_empty() { path } else { format!("{path}?{encoded}") }
}

fn check_progress(line: &[u8], reference: &str) -> Result<(), DockerError> {
    let text = String::from_utf8_lossy(line);

    if text.trim().is_empty() {
        return Ok(());
    }

    let progress: Value = serde_json::from_str(&text)?;

    match progress.get("error") {
        Some(error) => Err(DockerError::Pull(format!("pull of {reference} failed: {}", error.as_str().map_or_else(|| error.to_string(), str::to_owned)))),
        None => Ok(()),
    }
}

async fn within<T>(limit: Duration, call: impl Future<Output = Result<T, DockerError>>) -> Result<T, DockerError> {
    tokio::time::timeout(limit, call).await.unwrap_or(Err(DockerError::NoAnswer(limit)))
}

fn parse<T: DeserializeOwned>(text: &str) -> Result<T, DockerError> {
    Ok(serde_json::from_str(if text.trim().is_empty() { "null" } else { text })?)
}

async fn read_body(response: Response<Incoming>) -> Result<String, DockerError> {
    let bytes = response.into_body().collect().await?.to_bytes();

    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn api_error(status: u16, body: &str) -> DockerError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("message").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned());

    DockerError::Api { status, message: if message.is_empty() { format!("Docker answered HTTP {status}") } else { message } }
}

/// The `State` values Docker lists a container with, as far as the agent tells them apart.
pub mod container_state {
    pub const CREATED: &str = "created";
    pub const RUNNING: &str = "running";
    pub const RESTARTING: &str = "restarting";
    pub const EXITED: &str = "exited";
    /// One that Docker failed to remove; a forced removal clears it.
    pub const DEAD: &str = "dead";
}

/// The tag Docker reads into a reference that names none.
pub const DEFAULT_TAG: &str = "latest";

/// Whether an image was pulled at `digest`: one of its repo digests, `repository@sha256:…`, ends in it.
#[must_use]
pub fn pulled_at(repo_digests: &[String], digest: &str) -> bool {
    repo_digests.iter().any(|repo_digest| repo_digest.rsplit_once('@').is_some_and(|(_, pulled)| pulled == digest))
}

/// `fromImage` and `tag` as `docker pull` sends them: the digest after `@`, else the tag after
/// the last colon that is not a registry port, else `latest`, which keeps the daemon from pulling
/// every tag.
#[must_use]
pub fn split_reference(reference: &str) -> (String, String) {
    let image = parse_image_reference(reference);
    let tag = image.digest.or(image.tag).unwrap_or_else(|| DEFAULT_TAG.to_owned());

    (image.name, tag)
}

/// The calls the Docker service makes, so its tests can hand it a fake daemon.
#[async_trait::async_trait]
#[allow(clippy::missing_errors_doc, reason = "each method fails as the Docker call it names")]
pub trait DockerApiPort: Send + Sync {
    async fn engine(&self) -> Result<(String, String), DockerError>;
    async fn create_container(&self, name: &str, spec: &Value) -> Result<String, DockerError>;
    async fn start_container(&self, id: &str) -> Result<(), DockerError>;
    async fn stop_container(&self, id: &str, timeout_seconds: i64) -> Result<(), DockerError>;
    async fn remove_container(&self, id: &str, force: bool, volumes: bool) -> Result<(), DockerError>;
    async fn rename_container(&self, id: &str, name: &str) -> Result<(), DockerError>;
    async fn inspect_container(&self, id: &str) -> Result<ContainerInspect, DockerError>;
    async fn list_containers(&self, all: bool, filters: Option<&Filters>) -> Result<Vec<ListedContainer>, DockerError>;
    async fn wait_container(&self, id: &str, condition: WaitCondition) -> Result<PendingExit, DockerError>;
    async fn attach_container(&self, id: &str) -> Result<OutputStream, DockerError>;
    async fn inspect_image(&self, reference: &str) -> Result<ImageInspect, DockerError>;
    async fn list_images(&self, filters: Option<&Filters>) -> Result<Vec<ListedImage>, DockerError>;
    async fn remove_image(&self, reference: &str) -> Result<(), DockerError>;
    async fn registry_digest(&self, reference: &str) -> Result<String, DockerError>;
    async fn pull_image(&self, reference: &str) -> Result<(), DockerError>;
    async fn copy_from_container(&self, id: &str, path: &str, target: &Path) -> Result<(), DockerError>;
}

#[async_trait::async_trait]
impl DockerApiPort for DockerApi {
    async fn engine(&self) -> Result<(String, String), DockerError> {
        DockerApi::engine(self).await
    }

    async fn create_container(&self, name: &str, spec: &Value) -> Result<String, DockerError> {
        DockerApi::create_container(self, name, spec).await
    }

    async fn start_container(&self, id: &str) -> Result<(), DockerError> {
        DockerApi::start_container(self, id).await
    }

    async fn stop_container(&self, id: &str, timeout_seconds: i64) -> Result<(), DockerError> {
        DockerApi::stop_container(self, id, timeout_seconds).await
    }

    async fn remove_container(&self, id: &str, force: bool, volumes: bool) -> Result<(), DockerError> {
        DockerApi::remove_container(self, id, force, volumes).await
    }

    async fn rename_container(&self, id: &str, name: &str) -> Result<(), DockerError> {
        DockerApi::rename_container(self, id, name).await
    }

    async fn inspect_container(&self, id: &str) -> Result<ContainerInspect, DockerError> {
        DockerApi::inspect_container(self, id).await
    }

    async fn list_containers(&self, all: bool, filters: Option<&Filters>) -> Result<Vec<ListedContainer>, DockerError> {
        DockerApi::list_containers(self, all, filters).await
    }

    async fn wait_container(&self, id: &str, condition: WaitCondition) -> Result<PendingExit, DockerError> {
        DockerApi::wait_container(self, id, condition).await
    }

    async fn attach_container(&self, id: &str) -> Result<OutputStream, DockerError> {
        DockerApi::attach_container(self, id).await
    }

    async fn inspect_image(&self, reference: &str) -> Result<ImageInspect, DockerError> {
        DockerApi::inspect_image(self, reference).await
    }

    async fn list_images(&self, filters: Option<&Filters>) -> Result<Vec<ListedImage>, DockerError> {
        DockerApi::list_images(self, filters).await
    }

    async fn remove_image(&self, reference: &str) -> Result<(), DockerError> {
        DockerApi::remove_image(self, reference).await
    }

    async fn copy_from_container(&self, id: &str, path: &str, target: &Path) -> Result<(), DockerError> {
        DockerApi::copy_from_container(self, id, path, target).await
    }

    async fn registry_digest(&self, reference: &str) -> Result<String, DockerError> {
        DockerApi::registry_digest(self, reference).await
    }

    async fn pull_image(&self, reference: &str) -> Result<(), DockerError> {
        DockerApi::pull_image(self, reference).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_split_like_docker_pull() {
        assert_eq!(split_reference("ghcr.io/ankorcs/cs2:1.2"), ("ghcr.io/ankorcs/cs2".to_owned(), "1.2".to_owned()));
        assert_eq!(split_reference("localhost:5000/cs2"), ("localhost:5000/cs2".to_owned(), "latest".to_owned()));
        assert_eq!(split_reference("cs2@sha256:abc"), ("cs2".to_owned(), "sha256:abc".to_owned()));
        assert_eq!(split_reference("alpine"), ("alpine".to_owned(), "latest".to_owned()));
    }

    #[test]
    fn queries_are_versioned_and_encoded() {
        let filters = Filters::from([("label".to_owned(), vec!["ankor.role=agent".to_owned()])]);

        assert_eq!(path_with_query("/version", &[]), "/v1.44/version");
        assert_eq!(
            path_with_query("/containers/json", &[("all", Query::Flag(true)), ("filters", Query::Filters(&filters))]),
            "/v1.44/containers/json?all=true&filters=%7B%22label%22%3A%5B%22ankor.role%3Dagent%22%5D%7D"
        );
    }

    #[test]
    fn errors_carry_the_daemon_message() {
        assert_eq!(api_error(404, r#"{"message":"No such container: x"}"#).to_string(), "No such container: x");
        assert_eq!(api_error(500, "").to_string(), "Docker answered HTTP 500");
        assert!(check_progress(br#"{"error":"denied"}"#, "x").is_err());
        assert!(check_progress(br#"{"status":"Downloading"}"#, "x").is_ok());
    }
}
