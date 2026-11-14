//! How a node finds its address and country: one lookup at ipinfo.io, or neither.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use ankor_protocol::schemas::{s_country_code, s_ipv4};
use ankor_protocol::validate::{Payload, safe_parse};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::Value;

use crate::failure::truncate;
use crate::logger::Logger;

pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const PUBLIC_IP_URL: &str = "https://ipinfo.io/json";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the lookup got no answer.
#[derive(Debug, thiserror::Error)]
pub enum PublicIpError {
    /// The HTTP client could not be built.
    #[error("no HTTP client: {0}")]
    NoClient(#[source] Arc<reqwest::Error>),
    /// The connect, or a wait for more of the answer, ran past the timeout.
    #[error("no answer within {ms} ms")]
    Timeout { ms: u128 },
    /// The message already lists the causes, so the error is not a `source`.
    #[error("{}", crate::demo_uploader::error_chain(.0))]
    Http(reqwest::Error),
    /// The answer's body exceeded `MAX_BODY_BYTES`.
    #[error("lookup answer too large")]
    TooLarge,
    /// The service answered with a status other than 200.
    #[error("lookup answered {0}")]
    Status(u16),
}

/// Answers with the lookup service's body.
#[async_trait]
pub trait PublicIpResolver: Send + Sync {
    async fn resolve(&self) -> Result<String, PublicIpError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicAddress {
    /// `None` when the lookup failed, because a guessed address could be one players cannot reach.
    pub public_ip: Option<String>,
    /// The node's country code, or `None` when the lookup did not answer one.
    pub country: Option<String>,
}

/// `GET url` over IPv4 only, so the service sees the address players reach. The timeout bounds the
/// connect and every wait for more of the answer.
pub struct HttpsIpv4Resolver {
    url: String,
    timeout: Duration,
    client: Result<reqwest::Client, Arc<reqwest::Error>>,
}

impl HttpsIpv4Resolver {
    #[must_use]
    pub fn new(url: &str, timeout: Duration) -> Self {
        Self::with_builder(url, timeout, reqwest::Client::builder())
    }

    /// The resolver on a client built from `builder`, such as one that trusts another authority.
    #[must_use]
    pub fn with_builder(url: &str, timeout: Duration, builder: reqwest::ClientBuilder) -> Self {
        crate::init_tls();

        let client = builder.local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).connect_timeout(timeout).read_timeout(timeout).build().map_err(Arc::new);

        Self { url: url.to_owned(), timeout, client }
    }

    fn failure(&self, error: reqwest::Error) -> PublicIpError {
        if error.is_timeout() {
            return PublicIpError::Timeout { ms: self.timeout.as_millis() };
        }

        PublicIpError::Http(error)
    }
}

#[async_trait]
impl PublicIpResolver for HttpsIpv4Resolver {
    async fn resolve(&self) -> Result<String, PublicIpError> {
        let client = self.client.as_ref().map_err(|error| PublicIpError::NoClient(Arc::clone(error)))?;
        let mut response = client.get(&self.url).header("accept", "application/json").send().await.map_err(|error| self.failure(error))?;
        let mut body = Vec::new();

        while let Some(chunk) = response.chunk().await.map_err(|error| self.failure(error))? {
            body.extend_from_slice(&chunk);

            if body.len() > MAX_BODY_BYTES {
                return Err(PublicIpError::TooLarge);
            }
        }

        if response.status() != StatusCode::OK {
            return Err(PublicIpError::Status(response.status().as_u16()));
        }

        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// A field that is missing or of another shape is `None`.
fn lookup_answer(value: &Value) -> (Option<String>, Option<String>) {
    let Value::Object(fields) = value else {
        return (None, None);
    };
    let read = |name: &str, schema: fn(&mut Payload)| {
        fields.get(name).and_then(|field| safe_parse(schema, field.clone()).ok()).and_then(|field| field.as_str().map(str::to_owned))
    };

    (read("ip", s_ipv4), read("country", s_country_code))
}

pub async fn resolve_public_address(resolver: &dyn PublicIpResolver, log: &Logger) -> PublicAddress {
    let mut ip = None;
    let mut country = None;

    match resolver.resolve().await {
        Err(error) => log.error(format!("public ip lookup failed: {error}")),
        Ok(body) => match serde_json::from_str::<Value>(&body) {
            Err(error) => log.error(format!("public ip lookup failed: {error}")),
            Ok(value) => {
                (ip, country) = lookup_answer(&value);

                if ip.is_none() {
                    let quoted = serde_json::to_string(&truncate(&body, 80)).unwrap_or_default();

                    log.error(format!("public ip lookup did not answer with an IPv4 address: {quoted}"));
                }

                if country.is_none() {
                    log.warn("public ip lookup did not answer with a country; it is looked up again when the agent restarts");
                }
            }
        },
    }

    if ip.is_none() {
        log.error("no public IPv4 address; players reach this node only at an address pinned in admin");
    }

    PublicAddress { public_ip: ip, country }
}
