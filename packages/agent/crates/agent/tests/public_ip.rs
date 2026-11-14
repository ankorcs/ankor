//! The public address and country, from one lookup.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::logger::Logger;
use ankor_agent::public_ip::{HttpsIpv4Resolver, MAX_BODY_BYTES, PublicAddress, PublicIpError, PublicIpResolver, resolve_public_address};
use ankor_agent::testing::loopback_tls;
use async_trait::async_trait;
use serde_json::{Value, json};

struct Answer(Result<String, u16>);

#[async_trait]
impl PublicIpResolver for Answer {
    async fn resolve(&self) -> Result<String, PublicIpError> {
        self.0.clone().map_err(PublicIpError::Status)
    }
}

fn answer(body: &Value) -> Answer {
    Answer(Ok(body.to_string()))
}

fn found(ip: Option<&str>, country: Option<&str>) -> PublicAddress {
    PublicAddress { public_ip: ip.map(str::to_owned), country: country.map(str::to_owned) }
}

mod resolve_public_address {
    use super::*;

    #[tokio::test]
    async fn takes_the_address_and_country_from_one_lookup() {
        let lookup = answer(&json!({ "ip": "198.51.100.7", "country": "US", "city": "New York" }));

        assert_eq!(resolve_public_address(&lookup, &Logger::null()).await, found(Some("198.51.100.7"), Some("US")));
    }

    #[tokio::test]
    async fn reports_no_country_for_an_answer_without_a_two_letter_code_and_keeps_the_address() {
        for country in [None, Some(json!("dk")), Some(json!("DNK")), Some(json!(7))] {
            let mut body = json!({ "ip": "198.51.100.7" });

            if let Some(country) = country {
                body["country"] = country;
            }

            assert_eq!(resolve_public_address(&answer(&body), &Logger::null()).await, found(Some("198.51.100.7"), None));
        }
    }

    #[tokio::test]
    async fn reports_no_address_when_the_lookup_fails_or_talks_nonsense_rather_than_a_guess_players_may_not_reach() {
        let lookups =
            [Answer(Err(503)), Answer(Ok("<html>".to_owned())), Answer(Ok("null".to_owned())), answer(&json!({ "ip": "2001:db8::1", "country": "FR" }))];
        let mut results = Vec::new();

        for lookup in &lookups {
            results.push(resolve_public_address(lookup, &Logger::null()).await);
        }

        assert_eq!(results, [found(None, None), found(None, None), found(None, None), found(None, Some("FR"))]);
    }
}

mod https_ipv4_resolver {
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::server::TlsStream;

    use super::*;

    type Handler = Arc<dyn Fn(String, TlsStream<tokio::net::TcpStream>) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

    /// A lookup service over HTTPS, under an authority the client trusts.
    /// The handler gets the request's head and the stream to answer on.
    async fn lookup_service(handle: Handler) -> String {
        ankor_agent::init_tls();

        let certs = vec![CertificateDer::from_pem_slice(loopback_tls::CERT.as_bytes()).unwrap()];
        let key = PrivateKeyDer::from_pem_slice(loopback_tls::KEY.as_bytes()).unwrap();
        let config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(certs, key).unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let handle = Arc::clone(&handle);

                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(socket).await else {
                        return;
                    };
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];

                    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).await.unwrap_or(0) == 1 {
                        head.push(byte[0]);
                    }

                    handle(String::from_utf8_lossy(&head).into_owned(), stream).await;
                });
            }
        });

        format!("https://127.0.0.1:{port}/json")
    }

    fn resolver(url: &str, timeout: Duration) -> HttpsIpv4Resolver {
        let ca = reqwest::Certificate::from_pem(loopback_tls::CA.as_bytes()).unwrap();

        HttpsIpv4Resolver::with_builder(url, timeout, reqwest::Client::builder().tls_certs_only([ca]))
    }

    fn lookup(url: &str) -> HttpsIpv4Resolver {
        resolver(url, Duration::from_secs(5))
    }

    async fn respond(stream: &mut TlsStream<tokio::net::TcpStream>, status: &str, body: &[u8]) {
        let head = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());

        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        stream.shutdown().await.ok();
    }

    #[tokio::test]
    async fn answers_with_the_body_of_a_200_having_asked_for_json() {
        let accept = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&accept);
        let url = lookup_service(Arc::new(move |head, mut stream| {
            let header = head.lines().find_map(|line| line.strip_prefix("accept: ").map(str::to_owned));

            *seen.lock().unwrap() = header;
            Box::pin(async move { respond(&mut stream, "200 OK", br#"{"ip":"198.51.100.7"}"#).await })
        }))
        .await;

        assert_eq!(lookup(&url).resolve().await.unwrap(), r#"{"ip":"198.51.100.7"}"#);
        assert_eq!(accept.lock().unwrap().as_deref(), Some("application/json"));
    }

    #[tokio::test]
    async fn rejects_any_other_status_whatever_the_body_says() {
        let url =
            lookup_service(Arc::new(|_, mut stream| Box::pin(async move { respond(&mut stream, "429 Too Many Requests", br#"{"ip":"198.51.100.7"}"#).await })))
                .await;

        assert_eq!(lookup(&url).resolve().await.unwrap_err().to_string(), "lookup answered 429");
    }

    #[tokio::test]
    async fn takes_an_answer_of_exactly_16_kib_and_rejects_a_longer_one() {
        assert_eq!(MAX_BODY_BYTES, 16 * 1024);

        let exact = lookup_service(Arc::new(|_, mut stream| Box::pin(async move { respond(&mut stream, "200 OK", &vec![b'x'; 16 * 1024]).await }))).await;
        let long = lookup_service(Arc::new(|_, mut stream| Box::pin(async move { respond(&mut stream, "200 OK", &vec![b'x'; 16 * 1024 + 1]).await }))).await;

        assert_eq!(lookup(&exact).resolve().await.unwrap().len(), 16 * 1024);
        assert_eq!(lookup(&long).resolve().await.unwrap_err().to_string(), "lookup answer too large");
    }

    #[tokio::test]
    async fn reads_a_character_split_across_two_chunks_as_one() {
        let city = r#"{"city":"Århus"}"#.as_bytes();
        let url = lookup_service(Arc::new(move |_, mut stream| {
            Box::pin(async move {
                // Å is two bytes in UTF-8; the cut falls between them.
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", city.len());

                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(&city[..10]).await.unwrap();
                stream.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                stream.write_all(&city[10..]).await.unwrap();
                stream.shutdown().await.ok();
            })
        }))
        .await;

        assert_eq!(lookup(&url).resolve().await.unwrap(), r#"{"city":"Århus"}"#);
    }

    #[tokio::test]
    async fn rejects_an_answer_cut_off_mid_body() {
        let url = lookup_service(Arc::new(|_, mut stream| {
            Box::pin(async move {
                stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"ip\":").await.unwrap();
                stream.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(stream);
            })
        }))
        .await;

        assert!(lookup(&url).resolve().await.is_err());
    }

    #[tokio::test]
    async fn rejects_a_service_that_does_not_answer_in_time() {
        let url = lookup_service(Arc::new(|_, stream| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                drop(stream);
            })
        }))
        .await;

        assert_eq!(resolver(&url, Duration::from_millis(100)).resolve().await.unwrap_err().to_string(), "no answer within 100 ms");
    }
}
