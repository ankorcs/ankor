//! The connection a demo upload goes out on. A game server shares the host's uplink with it, so the
//! socket takes two measures:
//!
//! - It keeps little unsent data queued, so the upload's pace is the pace on the wire.
//! - It marks its packets as lower-effort traffic, which a router that honours DSCP sends last.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use http_body_util::{BodyExt as _, StreamBody};
use hyper::body::Frame;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpSocket, TcpStream};

/// A few milliseconds of a paced upload, so what the pace lets out reaches the wire soon after.
#[cfg(target_os = "linux")]
pub(crate) const NOTSENT_LOWAT_BYTES: u32 = 16 * 1024;

/// DSCP CS1, lower-effort traffic (RFC 8622), in the IP header's traffic-class byte.
pub(crate) const LOWER_EFFORT_TOS: u32 = 8 << 2;

trait Connection: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Connection for T {}

/// An option the system refuses leaves the socket as it is, because the upload goes out either way.
async fn connect(address: SocketAddr) -> std::io::Result<TcpStream> {
    let socket = if address.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    let options = socket2::SockRef::from(&socket);

    if address.is_ipv4() {
        let _ = options.set_tos_v4(LOWER_EFFORT_TOS);
    } else {
        let _ = options.set_tclass_v6(LOWER_EFFORT_TOS);
    }

    #[cfg(target_os = "linux")]
    let _ = options.set_tcp_notsent_lowat(NOTSENT_LOWAT_BYTES);

    socket.connect(address).await
}

/// How long one address of several gets before the next is tried.
const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(250);

/// Every address but the last gets a short try, so one family that drops packets does not hold up
/// the other.
fn attempt_timeout(index: usize, count: usize) -> Option<Duration> {
    (index + 1 < count).then_some(ATTEMPT_TIMEOUT)
}

fn interleave(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let first_v4 = addresses.first().is_some_and(SocketAddr::is_ipv4);
    let (mut first, mut second): (std::collections::VecDeque<_>, std::collections::VecDeque<_>) =
        addresses.into_iter().partition(|address| address.is_ipv4() == first_v4);
    let mut ordered = Vec::with_capacity(first.len() + second.len());

    while !first.is_empty() || !second.is_empty() {
        ordered.extend(first.pop_front());
        ordered.extend(second.pop_front());
    }

    ordered
}

/// Why an upload got no answer.
#[derive(Debug, thiserror::Error)]
pub enum UploadSocketError {
    /// The upload URL does not parse.
    #[error(transparent)]
    Url(#[from] url::ParseError),
    /// The upload URL has no host.
    #[error("the upload URL names no host")]
    NoHost,
    /// The upload URL has no port, and its scheme has no default one.
    #[error("the upload URL names no port")]
    NoPort,
    /// The upload URL is neither `http` nor `https`.
    #[error("unsupported upload scheme {0}")]
    UnsupportedScheme(String),
    /// The host resolved to no address to connect to.
    #[error("{host} resolves to no address")]
    NoAddress { host: String },
    /// The lookup, the last connection attempt or the TLS handshake failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The TLS client could not be configured.
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    /// The host is not a valid TLS server name.
    #[error(transparent)]
    ServerName(#[from] rustls::pki_types::InvalidDnsNameError),
    /// A caller's header name is invalid.
    #[error(transparent)]
    HeaderName(#[from] hyper::header::InvalidHeaderName),
    /// A caller's header value, or the `Host` value, is invalid.
    #[error(transparent)]
    HeaderValue(#[from] hyper::header::InvalidHeaderValue),
    /// The request could not be built.
    #[error(transparent)]
    Request(#[from] hyper::http::Error),
    /// The HTTP/1.1 handshake on the open connection failed.
    #[error(transparent)]
    Handshake(hyper::Error),
    /// The request or its answer broke off. The message already lists the causes, so the error is
    /// not a `source`.
    #[error("{}", super::demo_uploader::error_chain(.0))]
    Exchange(hyper::Error),
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, rustls::Error>> = OnceLock::new();

    CONFIG
        .get_or_init(|| {
            use rustls_platform_verifier::BuilderVerifierExt as _;

            crate::init_tls();

            let mut config = rustls::ClientConfig::builder().with_platform_verifier()?.with_no_client_auth();

            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Ok(Arc::new(config))
        })
        .clone()
}

async fn open(url: &url::Url) -> Result<Box<dyn Connection>, UploadSocketError> {
    let host = url.host_str().ok_or(UploadSocketError::NoHost)?;
    let port = url.port_or_known_default().ok_or(UploadSocketError::NoPort)?;
    let lookup_host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses = interleave(tokio::net::lookup_host((lookup_host, port)).await?.collect());
    let mut failure = None;
    let mut stream = None;

    for (index, address) in addresses.iter().enumerate() {
        let attempt = match attempt_timeout(index, addresses.len()) {
            None => connect(*address).await,
            Some(limit) => tokio::time::timeout(limit, connect(*address)).await.unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into())),
        };

        match attempt {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => failure = Some(error),
        }
    }

    let stream = stream.ok_or_else(|| failure.map_or_else(|| UploadSocketError::NoAddress { host: host.to_owned() }, UploadSocketError::Io))?;

    match url.scheme() {
        "http" => Ok(Box::new(stream)),
        "https" => {
            let name = rustls::pki_types::ServerName::try_from(lookup_host.to_owned())?;
            let tls = tokio_rustls::TlsConnector::from(tls_config()?).connect(name, stream).await?;

            Ok(Box::new(tls))
        }
        other => Err(UploadSocketError::UnsupportedScheme(other.to_owned())),
    }
}

/// Uses a connection of its own and follows no redirects.
///
/// # Errors
///
/// Fails when the target cannot be reached or the exchange breaks off.
pub async fn post<S>(url: &str, headers: &[(String, String)], body: S) -> Result<u16, UploadSocketError>
where
    S: Stream<Item = std::io::Result<Bytes>> + Send + 'static,
{
    use futures_util::StreamExt as _;

    let parsed = url::Url::parse(url)?;
    let connection = open(&parsed).await?;
    let (mut sender, driver) = hyper::client::conn::http1::handshake(TokioIo::new(connection)).await.map_err(UploadSocketError::Handshake)?;

    tokio::spawn(async move {
        let _ = driver.await;
    });

    let host = match parsed.port() {
        Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or_default()),
        None => parsed.host_str().unwrap_or_default().to_owned(),
    };
    let target = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_owned(),
    };
    let mut fields = hyper::HeaderMap::new();

    // A name given twice in any case is sent once, with the later value.
    for (name, value) in headers {
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes())?;

        fields.insert(name, hyper::header::HeaderValue::from_str(value)?);
    }

    // Set last, so no header the caller passes replaces it.
    fields.insert(hyper::header::HOST, hyper::header::HeaderValue::from_str(&host)?);

    let body = StreamBody::new(body.map(|chunk| chunk.map(Frame::data)));
    let mut request = hyper::Request::builder().method(hyper::Method::POST).uri(target).body(body)?;

    *request.headers_mut() = fields;
    let response = sender.send_request(request).await.map_err(UploadSocketError::Exchange)?;
    let status = response.status().as_u16();

    // The answer's body is read to its end and dropped as it comes, so the connection closes
    // cleanly and a large answer takes no memory.
    let mut answer = response.into_body();

    while let Some(Ok(_)) = answer.frame().await {}

    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn moves_on_to_the_next_address_when_one_refuses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();

            stream.write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await.unwrap();
        });
        // `localhost` names ::1 and 127.0.0.1; nothing listens on ::1.
        let body = futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"x"))]);
        let status = post(&format!("http://localhost:{port}/up"), &[("content-length".to_owned(), "1".to_owned())], body).await;

        assert_eq!(status.unwrap(), 204);
        server.await.unwrap();
    }

    #[test]
    fn limits_every_attempt_but_the_last() {
        assert_eq!((0..3).map(|index| attempt_timeout(index, 3)).collect::<Vec<_>>(), [Some(ATTEMPT_TIMEOUT), Some(ATTEMPT_TIMEOUT), None]);
        assert_eq!(attempt_timeout(0, 1), None);
    }

    #[test]
    fn tries_the_families_in_turn_starting_with_the_first_address() {
        let address = |text: &str| text.parse::<SocketAddr>().unwrap();
        let ordered = interleave(vec![address("[::1]:80"), address("[::2]:80"), address("10.0.0.1:80"), address("10.0.0.2:80")]);

        assert_eq!(ordered, [address("[::1]:80"), address("10.0.0.1:80"), address("[::2]:80"), address("10.0.0.2:80")]);
    }

    #[tokio::test]
    async fn marks_the_upload_as_lower_effort_and_keeps_little_unsent_data_queued() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = connect(listener.local_addr().unwrap()).await.unwrap();
        let options = socket2::SockRef::from(&stream);

        // CS1 is DSCP 8, the top six bits of the byte.
        assert_eq!(options.tos_v4().unwrap(), 0x20);

        #[cfg(target_os = "linux")]
        assert_eq!(options.tcp_notsent_lowat().unwrap(), 16 * 1024);
    }
}
