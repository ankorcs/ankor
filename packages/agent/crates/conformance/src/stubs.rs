//! The outside services the agent calls, served over TLS from one port and told apart by `Host`:
//! the public address lookup, steamcmd.net's app info, Steam's CM directory and a CM socket, and
//! two image registries with their token services.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use crate::sandbox::CS2_APP_ID;
use crate::tls::TestPki;
use crate::transcript::{Recorder, channels};

/// The bearer token the registry stub hands out and then requires.
const REGISTRY_TOKEN: &str = "conformance-registry-token";

const IPINFO_HOST: &str = "ipinfo.io";
const STEAMCMD_HOST: &str = "api.steamcmd.net";
const STEAM_API_HOST: &str = "api.steampowered.com";
const GHCR_HOST: &str = "ghcr.io";
const DOCKER_REGISTRY_HOST: &str = "registry-1.docker.io";
const DOCKER_AUTH_HOST: &str = "auth.docker.io";
const DOCKER_INDEX_HOST: &str = "index.docker.io";
/// Where both registries hand out tokens, as their challenges name it.
const TOKEN_PATH: &str = "/token";

/// The hosts the agent reaches on the stub port, all resolved to one address in the sandbox.
pub const STUB_HOSTS: &[&str] = &[IPINFO_HOST, STEAMCMD_HOST, STEAM_API_HOST, CM_HOST, GHCR_HOST, DOCKER_REGISTRY_HOST, DOCKER_AUTH_HOST, DOCKER_INDEX_HOST];

/// The connection manager the stubbed CM directory lists.
pub const CM_HOST: &str = "cm.conformance.test";

/// The address the stubbed lookup answers with.
pub const PUBLIC_IP: &str = "203.0.113.10";
/// The country the stubbed lookup answers with.
pub const COUNTRY: &str = "DK";
/// The build the stubbed steamcmd.net names as the newest public one, and the fake install holds.
pub const PUBLIC_BUILD: &str = "20260101";
/// A token whose logon the stubbed CM refuses starts with this.
pub const REFUSED_TOKEN_PREFIX: &str = "REFUSED";
/// The tags the stubbed registries list for every repository.
pub const REGISTRY_TAGS: &[&str] = &["1", "1.0", "1.0.0", "0", "0.3", "0.3.0", "latest"];

const PROTO_FLAG: u32 = 0x8000_0000;
const EMSG_LOGON_RESPONSE: u32 = 751;
const EMSG_LOGOFF: u32 = 706;
const EMSG_LOGON_GAME_SERVER: u32 = 5559;
const ERESULT_OK: u64 = 1;
const ERESULT_ACCESS_DENIED: u64 = 15;

/// A gate an answer waits behind until the scenario opens it.
#[derive(Clone)]
pub struct Gate(watch::Sender<bool>);

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    /// An open gate.
    pub fn new() -> Self {
        Self(watch::channel(true).0)
    }

    pub fn open(&self) {
        self.0.send_replace(true);
    }

    pub fn close(&self) {
        self.0.send_replace(false);
    }

    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|open| *open).await;
    }
}

struct StubState {
    rec: Recorder,
    steamcmd: Gate,
    steam: Gate,
}

/// The port the stubs listen on: HTTPS, as the agent calls every outside service.
pub const STUB_PORT: u16 = 443;

/// The running stubs.
#[derive(Clone)]
pub struct Stubs {
    /// Holds steamcmd.net's answers.
    pub steamcmd: Gate,
    /// Holds the CM's logon answers.
    pub steam: Gate,
}

impl Stubs {
    pub async fn start(rec: Recorder, pki: &TestPki) -> Result<Self> {
        let state = Arc::new(StubState { rec, steamcmd: Gate::new(), steam: Gate::new() });
        let router = Router::new().fallback(any(route)).with_state(Arc::clone(&state));
        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], STUB_PORT))).await.context("bind stubs")?;
        let acceptor = TlsAcceptor::from(Arc::clone(&pki.server));
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { continue };
                let acceptor = acceptor.clone();
                let router = router.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let service = TowerToHyperService::new(router);
                    let _ = Builder::new(TokioExecutor::new()).serve_connection_with_upgrades(TokioIo::new(tls), service).await;
                });
            }
        });
        Ok(Self { steamcmd: state.steamcmd.clone(), steam: state.steam.clone() })
    }
}

fn host_of(headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|value| value.to_str().ok()).unwrap_or_default();
    host.split(':').next().unwrap_or_default().to_owned()
}

async fn route(State(state): State<Arc<StubState>>, request: Request) -> Response {
    let host = host_of(request.headers());
    let method = request.method().clone();
    let path = request.uri().path_and_query().map(ToString::to_string).unwrap_or_default();
    let auth = request.headers().get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).map(str::to_owned);
    state.rec.record(channels::STUBS, json!({ "host": host, "method": method.as_str(), "path": path, "authorization": auth }));
    let uri_path = request.uri().path().to_owned();
    match host.as_str() {
        IPINFO_HOST if uri_path == "/json" => {
            axum::Json(json!({ "ip": PUBLIC_IP, "hostname": "node.conformance.test", "city": "Copenhagen", "country": COUNTRY, "timezone": "Europe/Copenhagen" }))
                .into_response()
        }
        STEAMCMD_HOST if uri_path == format!("/v1/info/{CS2_APP_ID}") => {
            state.steamcmd.wait().await;
            axum::Json(json!({ "status": "success", "data": { (CS2_APP_ID): { "depots": { "branches": { "public": { "buildid": PUBLIC_BUILD } } } } } })).into_response()
        }
        STEAM_API_HOST if uri_path == "/ISteamDirectory/GetCMListForConnect/v1/" => axum::Json(json!({
            "response": { "serverlist": [{ "endpoint": format!("{CM_HOST}:443"), "legacy_endpoint": "198.18.0.1:27017", "type": "websockets", "dc": "test", "realm": "steamglobal", "load": 0, "wtd_load": 0.0 }], "success": true, "message": "" }
        }))
        .into_response(),
        CM_HOST if uri_path == "/cmsocket/" => {
            let (mut parts, _) = request.into_parts();
            match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
                Ok(upgrade) => upgrade.on_upgrade(move |socket| run_cm(state, socket)).into_response(),
                Err(rejection) => rejection.into_response(),
            }
        }
        GHCR_HOST | DOCKER_REGISTRY_HOST | DOCKER_INDEX_HOST => registry(&host, &method, &uri_path, auth.as_deref()),
        DOCKER_AUTH_HOST if uri_path == TOKEN_PATH => axum::Json(json!({ "token": REGISTRY_TOKEN })).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The registry API: anonymous reads are challenged, tags are listed, and manifests are not found,
/// so an image update never gets as far as a pull.
fn registry(host: &str, method: &Method, path: &str, auth: Option<&str>) -> Response {
    if host == GHCR_HOST && path == TOKEN_PATH {
        return axum::Json(json!({ "token": REGISTRY_TOKEN })).into_response();
    }
    let Some(rest) = path.strip_prefix("/v2/") else { return StatusCode::NOT_FOUND.into_response() };
    let (repository, kind) = if let Some(repository) = rest.strip_suffix("/tags/list") {
        (repository, "tags")
    } else if let Some(at) = rest.find("/manifests/") {
        (rest.get(..at).unwrap_or_default(), "manifest")
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if auth != Some(format!("Bearer {REGISTRY_TOKEN}").as_str()) {
        let realm = if host == GHCR_HOST { format!("https://{GHCR_HOST}{TOKEN_PATH}") } else { format!("https://{DOCKER_AUTH_HOST}{TOKEN_PATH}") };
        let service = if host == GHCR_HOST { GHCR_HOST } else { "registry.docker.io" };
        let challenge = format!("Bearer realm=\"{realm}\",service=\"{service}\",scope=\"repository:{repository}:pull\"");
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, challenge)], axum::Json(json!({ "errors": [{ "code": "UNAUTHORIZED" }] })))
            .into_response();
    }
    if kind == "tags" {
        return axum::Json(json!({ "name": repository, "tags": REGISTRY_TAGS })).into_response();
    }
    if method == Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    (StatusCode::NOT_FOUND, axum::Json(json!({ "errors": [{ "code": "MANIFEST_UNKNOWN" }] }))).into_response()
}

/// A CM socket: answers a game server logon with `OK`, or with `AccessDenied` for a token that
/// starts with [`REFUSED_TOKEN_PREFIX`], once the steam gate is open.
async fn run_cm(state: Arc<StubState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    while let Some(Ok(message)) = stream.next().await {
        let Message::Binary(data) = message else { continue };
        let Some((emsg, body)) = unpack(&data) else {
            state.rec.record(channels::STEAM, json!({ "unreadable": data.len() }));
            continue;
        };
        match emsg {
            EMSG_LOGON_GAME_SERVER => {
                let fields = proto_fields(body);
                let token = fields
                    .iter()
                    .find(|(number, _)| *number == 52)
                    .and_then(|(_, value)| value.as_bytes())
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
                let app = fields.iter().find(|(number, _)| *number == 94).and_then(|(_, value)| value.as_int());
                let token = token.unwrap_or_default();
                let eresult = if token.starts_with(REFUSED_TOKEN_PREFIX) { ERESULT_ACCESS_DENIED } else { ERESULT_OK };
                let hint: String = token.chars().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect();
                state.rec.record(channels::STEAM, json!({ "logon": { "token_hint": hint, "app": app, "answered": eresult } }));
                state.steam.wait().await;
                if sink.send(Message::Binary(logon_response(eresult).into())).await.is_err() {
                    return;
                }
            }
            EMSG_LOGOFF => state.rec.record(channels::STEAM, json!({ "logoff": true })),
            other => state.rec.record(channels::STEAM, json!({ "emsg": other })),
        }
    }
}

fn unpack(data: &[u8]) -> Option<(u32, &[u8])> {
    let raw = u32::from_le_bytes(data.get(0..4)?.try_into().ok()?);
    let header_len = usize::try_from(i32::from_le_bytes(data.get(4..8)?.try_into().ok()?)).ok()?;
    if raw & PROTO_FLAG == 0 {
        return None;
    }
    Some((raw & !PROTO_FLAG, data.get(8 + header_len..)?))
}

enum Field<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

impl Field<'_> {
    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Field::Bytes(bytes) => Some(bytes),
            Field::Int(_) => None,
        }
    }

    fn as_int(&self) -> Option<u64> {
        match self {
            Field::Int(value) => Some(*value),
            Field::Bytes(_) => None,
        }
    }
}

fn read_varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*offset)?;
        *offset += 1;
        result |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

fn proto_fields(data: &[u8]) -> Vec<(u64, Field<'_>)> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let Some(key) = read_varint(data, &mut offset) else { break };
        let value = match key & 7 {
            0 => match read_varint(data, &mut offset) {
                Some(value) => Field::Int(value),
                None => break,
            },
            1 => {
                let Some(bytes) = data.get(offset..offset + 8) else { break };
                offset += 8;
                Field::Int(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])))
            }
            2 => {
                let Some(len) = read_varint(data, &mut offset).and_then(|len| usize::try_from(len).ok()) else { break };
                let Some(bytes) = data.get(offset..offset + len) else { break };
                offset += len;
                Field::Bytes(bytes)
            }
            5 => {
                let Some(bytes) = data.get(offset..offset + 4) else { break };
                offset += 4;
                Field::Int(u64::from(u32::from_le_bytes(bytes.try_into().unwrap_or([0; 4]))))
            }
            _ => break,
        };
        fields.push((key >> 3, value));
    }
    fields
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = u8::try_from(value & 0x7f).unwrap_or(0);
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn logon_response(eresult: u64) -> Vec<u8> {
    let mut header = vec![0x09];
    header.extend_from_slice(&0x0184_0000_0000_0001u64.to_le_bytes());
    header.push(0x10);
    varint(1, &mut header);
    let mut body = vec![0x08];
    varint(eresult, &mut body);
    let mut packet = Vec::new();
    packet.extend_from_slice(&(EMSG_LOGON_RESPONSE | PROTO_FLAG).to_le_bytes());
    packet.extend_from_slice(&i32::try_from(header.len()).unwrap_or(0).to_le_bytes());
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&body);
    packet
}
