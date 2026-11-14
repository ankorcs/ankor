//! Logs a game server login token on to Steam and straight off again, which keeps the token from
//! expiring, without starting a game server. It uses Steam's client protocol over a connection
//! manager's WebSocket endpoint, where TLS stands in for the channel encryption of the raw TCP
//! protocol. The messages follow `SteamKit2`'s `SteamGameServer.LogOn`.

use crate::cs2_install::CS2_APP_ID;
use std::io::Read as _;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

const CM_LIST_URL: &str = "https://api.steampowered.com/ISteamDirectory/GetCMListForConnect/v1/?cellid=0&cmtype=websockets";

const PROTOCOL_VERSION: u64 = 65581;
/// Set on a message id whose header is a `CMsgProtoBufHeader`.
const PROTO_FLAG: u32 = 0x8000_0000;
/// `k_EOSTypeLinuxUnknown`, sent as its unsigned 32-bit value.
const OS_TYPE_LINUX: i32 = -203;
/// An unassigned game server account in the public universe; Steam assigns the real one on logon.
pub(crate) const GAME_SERVER_STEAM_ID: u64 = (1 << 56) | (3 << 52);

const EMSG_MULTI: u32 = 1;
/// The most a packed multi-message may unpack to. A logon's answers are a few hundred bytes.
const PACKED_MAX_BYTES: usize = 4 * 1024 * 1024;
const EMSG_CLIENT_LOG_OFF: u32 = 706;
const EMSG_CLIENT_LOG_ON_RESPONSE: u32 = 751;
const EMSG_CLIENT_LOGGED_OFF: u32 = 757;
const EMSG_CLIENT_LOGON_GAME_SERVER: u32 = 5559;

/// The `EResult` codes a token logon can answer with that callers act on.
pub mod eresult {
    pub const OK: u32 = 1;
    pub(crate) const FAIL: u32 = 2;
    pub const INVALID_PARAM: u32 = 8;
    pub const ACCESS_DENIED: u32 = 15;
    pub(crate) const BANNED: u32 = 17;
    pub const SERVICE_UNAVAILABLE: u32 = 20;
    pub const EXPIRED: u32 = 27;
    pub(crate) const ACCOUNT_DISABLED: u32 = 43;
    pub(crate) const TRY_ANOTHER_CM: u32 = 48;
    pub(crate) const RATE_LIMIT_EXCEEDED: u32 = 84;
}

#[must_use]
pub(crate) fn eresult_name(code: u32) -> String {
    let name = match code {
        eresult::OK => "ok",
        eresult::FAIL => "fail",
        eresult::INVALID_PARAM => "invalidParam",
        eresult::ACCESS_DENIED => "accessDenied",
        eresult::BANNED => "banned",
        eresult::SERVICE_UNAVAILABLE => "serviceUnavailable",
        eresult::EXPIRED => "expired",
        eresult::ACCOUNT_DISABLED => "accountDisabled",
        eresult::TRY_ANOTHER_CM => "tryAnotherCM",
        eresult::RATE_LIMIT_EXCEEDED => "rateLimitExceeded",
        _ => return format!("EResult {code}"),
    };

    name.to_owned()
}

#[derive(Default)]
struct ProtoWriter(Vec<u8>);

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

impl ProtoWriter {
    fn tag(mut self, field: u32, wire: u32) -> Self {
        varint(u64::from((field << 3) | wire), &mut self.0);
        self
    }

    fn varint(self, field: u32, value: u64) -> Self {
        let mut writer = self.tag(field, 0);

        varint(value, &mut writer.0);
        writer
    }

    fn fixed32(self, field: u32, value: u32) -> Self {
        let mut writer = self.tag(field, 5);

        writer.0.extend_from_slice(&value.to_le_bytes());
        writer
    }

    fn fixed64(self, field: u32, value: u64) -> Self {
        let mut writer = self.tag(field, 1);

        writer.0.extend_from_slice(&value.to_le_bytes());
        writer
    }

    fn bytes(self, field: u32, value: &[u8]) -> Self {
        let mut writer = self.tag(field, 2);

        varint(value.len() as u64, &mut writer.0);
        writer.0.extend_from_slice(value);
        writer
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Field {
    Varint(u64),
    Fixed64(u64),
    Bytes(Vec<u8>),
    Fixed32(u32),
}

impl Field {
    fn number(&self) -> u64 {
        match self {
            Self::Varint(value) | Self::Fixed64(value) => *value,
            Self::Fixed32(value) => u64::from(*value),
            Self::Bytes(_) => 0,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SteamError {
    #[error("Truncated protobuf")]
    TruncatedProtobuf,
    #[error("Truncated protobuf varint")]
    TruncatedVarint,
    #[error("Truncated message")]
    TruncatedMessage,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("Steam's server directory answered HTTP {0}")]
    DirectoryStatus(u16),
    #[error("Steam listed no WebSocket connection manager")]
    NoWebSocketServer,
    #[error("{}", crate::http_body::too_large(crate::http_body::ANSWER_MAX_BYTES))]
    TooLarge,
    #[error("Steam's packed answer is larger than {} bytes", PACKED_MAX_BYTES)]
    PackedTooLarge,
    #[error("Steam's packed answer nests more than {} deep", MULTI_MAX_DEPTH)]
    NestedTooDeep,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("Steam's server directory did not answer in time")]
    DirectoryTimeout,
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("Steam sent a message the agent cannot read: {0}")]
    Unreadable(#[source] Box<SteamError>),
    #[error("Steam closed the connection before answering the logon")]
    Closed,
    #[error("Steam did not answer the logon in time")]
    Timeout,
}

/// Every field by number. An unknown wire type ends the read, as nothing here needs one.
fn read_fields(buffer: &[u8]) -> Result<Vec<(u32, Field)>, SteamError> {
    let mut fields = Vec::new();
    let mut offset = 0;
    let truncated = || SteamError::TruncatedProtobuf;
    let read_varint = |offset: &mut usize| -> Result<u64, SteamError> {
        let mut result = 0u64;
        let mut shift = 0;

        loop {
            let byte = *buffer.get(*offset).ok_or(SteamError::TruncatedVarint)?;

            *offset += 1;
            result |= u64::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);

            if byte & 0x80 == 0 {
                return Ok(result);
            }

            shift += 7;
        }
    };

    while offset < buffer.len() {
        let key = read_varint(&mut offset)?;
        let field = u32::try_from(key >> 3).unwrap_or(u32::MAX);
        let value = match key & 0x7 {
            0 => Field::Varint(read_varint(&mut offset)?),
            1 => {
                let bytes = buffer.get(offset..offset + 8).ok_or_else(truncated)?;

                offset += 8;
                Field::Fixed64(u64::from_le_bytes(bytes.try_into().map_err(|_| truncated())?))
            }
            2 => {
                let length = usize::try_from(read_varint(&mut offset)?).unwrap_or(usize::MAX);
                let bytes = buffer.get(offset..offset.saturating_add(length)).ok_or_else(truncated)?;

                offset += length;
                Field::Bytes(bytes.to_vec())
            }
            5 => {
                let bytes = buffer.get(offset..offset + 4).ok_or_else(truncated)?;

                offset += 4;
                Field::Fixed32(u32::from_le_bytes(bytes.try_into().map_err(|_| truncated())?))
            }
            _ => return Ok(fields),
        };

        fields.push((field, value));
    }

    Ok(fields)
}

fn first(fields: &[(u32, Field)], number: u32) -> Option<&Field> {
    fields.iter().find(|(field, _)| *field == number).map(|(_, value)| value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    pub steam_id: u64,
    pub session_id: u32,
}

/// One client message: the message id with the protobuf flag, the header length, the header and
/// the body, all little-endian.
#[must_use]
pub(crate) fn pack_message(emsg: u32, session: Session, body: &[u8]) -> Vec<u8> {
    let header = ProtoWriter::default().fixed64(1, session.steam_id).varint(2, u64::from(session.session_id)).0;
    let mut packet = Vec::with_capacity(8 + header.len() + body.len());

    packet.extend_from_slice(&(emsg | PROTO_FLAG).to_le_bytes());
    packet.extend_from_slice(&i32::try_from(header.len()).unwrap_or(i32::MAX).to_le_bytes());
    packet.extend_from_slice(&header);
    packet.extend_from_slice(body);
    packet
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SteamMessage {
    pub emsg: u32,
    pub session: Session,
    pub body: Vec<u8>,
}

/// How deep `Multi` packets may nest. Steam bundles plain messages, so a deeper packet is broken or
/// hostile, and would otherwise recurse without bound.
const MULTI_MAX_DEPTH: usize = 4;

/// A received packet, with a `Multi` opened into the messages it bundles; non-protobuf messages
/// are skipped.
///
/// # Errors
///
/// Fails on a packet that cannot be read.
pub(crate) fn unpack_messages(packet: &[u8]) -> Result<Vec<SteamMessage>, SteamError> {
    unpack_at(packet, 0)
}

fn unpack_at(packet: &[u8], depth: usize) -> Result<Vec<SteamMessage>, SteamError> {
    let bad = || SteamError::TruncatedMessage;
    let raw = u32::from_le_bytes(packet.get(0..4).ok_or_else(bad)?.try_into().map_err(|_| bad())?);

    if raw & PROTO_FLAG == 0 {
        return Ok(Vec::new());
    }

    let emsg = raw & !PROTO_FLAG;
    let header_length = usize::try_from(i32::from_le_bytes(packet.get(4..8).ok_or_else(bad)?.try_into().map_err(|_| bad())?)).map_err(|_| bad())?;
    let header = read_fields(packet.get(8..8 + header_length).ok_or_else(bad)?)?;
    let body = packet.get(8 + header_length..).ok_or_else(bad)?;

    if emsg != EMSG_MULTI {
        let session = Session {
            steam_id: first(&header, 1).map_or(0, Field::number),
            session_id: u32::try_from(first(&header, 2).map_or(0, Field::number)).unwrap_or(0),
        };

        return Ok(vec![SteamMessage { emsg, session, body: body.to_vec() }]);
    }

    if depth >= MULTI_MAX_DEPTH {
        return Err(SteamError::NestedTooDeep);
    }

    let multi = read_fields(body)?;
    let zipped = first(&multi, 1).map_or(0, Field::number) > 0;
    let content = match first(&multi, 2) {
        Some(Field::Bytes(bytes)) => bytes.clone(),
        _ => Vec::new(),
    };
    let payload = if zipped {
        let mut unzipped = Vec::new();
        // One byte past the limit tells an answer at the limit from one over it.
        let past_limit = u64::try_from(PACKED_MAX_BYTES).unwrap_or(u64::MAX).saturating_add(1);

        flate2::read::GzDecoder::new(content.as_slice()).take(past_limit).read_to_end(&mut unzipped)?;

        if unzipped.len() > PACKED_MAX_BYTES {
            return Err(SteamError::PackedTooLarge);
        }

        unzipped
    } else {
        content
    };
    let mut messages = Vec::new();
    let mut offset = 0;

    while offset < payload.len() {
        let length = usize::try_from(u32::from_le_bytes(payload.get(offset..offset + 4).ok_or_else(bad)?.try_into().map_err(|_| bad())?)).map_err(|_| bad())?;

        messages.extend(unpack_at(payload.get(offset + 4..offset + 4 + length).ok_or_else(bad)?, depth + 1)?);
        offset += 4 + length;
    }

    Ok(messages)
}

/// The first non-loopback IPv4 address as Steam wants it, combined with its fixed mask by
/// exclusive or.
fn obfuscate_first_external(addresses: impl IntoIterator<Item = std::net::Ipv4Addr>) -> Option<u32> {
    addresses.into_iter().find(|address| !address.is_loopback()).map(|address| u32::from(address) ^ 0xbaad_f00d)
}

fn obfuscated_private_ip() -> Option<u32> {
    let addresses: Vec<std::net::Ipv4Addr> =
        nix::ifaddrs::getifaddrs().ok()?.filter_map(|interface| interface.address?.as_sockaddr_in().map(nix::sys::socket::SockaddrIn::ip)).collect();

    obfuscate_first_external(addresses)
}

#[must_use]
pub(crate) fn logon_body(token: &str, private_ip: Option<u32>) -> Vec<u8> {
    #[allow(clippy::cast_sign_loss, reason = "Steam encodes the negative OS type as its unsigned bits")]
    let os_type = u64::from(OS_TYPE_LINUX as u32);
    let mut writer = ProtoWriter::default().varint(1, PROTOCOL_VERSION).varint(7, os_type);

    if let Some(ip) = private_ip {
        writer = writer.bytes(11, &ProtoWriter::default().fixed32(1, ip).0);
    }

    writer.bytes(52, token.as_bytes()).varint(94, u64::from(CS2_APP_ID)).varint(102, 1).0
}

/// The `eresult` of a logon response or logged-off notice; protobuf's default for it is `Fail`.
fn eresult_of(body: &[u8]) -> u32 {
    read_fields(body).ok().and_then(|fields| first(&fields, 1).map(Field::number)).and_then(|value| u32::try_from(value).ok()).unwrap_or(eresult::FAIL)
}

/// A connection manager that takes WebSocket clients, from Steam's directory.
async fn pick_endpoint(directory: &str) -> Result<String, SteamError> {
    crate::init_tls();

    let response = reqwest::get(directory).await?;

    if !response.status().is_success() {
        return Err(SteamError::DirectoryStatus(response.status().as_u16()));
    }

    let body = crate::http_body::read_limited(response, crate::http_body::ANSWER_MAX_BYTES).await?.ok_or(SteamError::TooLarge)?;
    let list: Value = serde_json::from_slice(&body)?;

    list.pointer("/response/serverlist")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|server| {
            server.get("type").and_then(Value::as_str) == Some("websockets")
                && server.get("endpoint").and_then(Value::as_str).is_some_and(|endpoint| !endpoint.is_empty())
        })
        .and_then(|server| server.get("endpoint").and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or(SteamError::NoWebSocketServer)
}

/// Logs `token` on as a CS2 game server and back off, and returns the `EResult` Steam answered.
///
/// # Errors
///
/// Fails when Steam cannot be reached or does not answer within `timeout`.
pub async fn log_on_game_server(token: &str, endpoint: Option<String>, timeout: Duration) -> Result<u32, SteamError> {
    let endpoint = match endpoint {
        Some(endpoint) => endpoint,
        // Bounded too: an answer that never comes would keep the slot reserved for its logon.
        None => tokio::time::timeout(timeout, pick_endpoint(CM_LIST_URL)).await.map_err(|_| SteamError::DirectoryTimeout)??,
    };

    log_on(token, &format!("wss://{endpoint}/cmsocket/"), obfuscated_private_ip(), timeout).await
}

async fn log_on(token: &str, url: &str, private_ip: Option<u32>, timeout: Duration) -> Result<u32, SteamError> {
    crate::init_tls();

    let attempt = async {
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await?;
        let logon = pack_message(EMSG_CLIENT_LOGON_GAME_SERVER, Session { steam_id: GAME_SERVER_STEAM_ID, session_id: 0 }, &logon_body(token, private_ip));

        socket.send(Message::Binary(logon.into())).await?;

        while let Some(frame) = socket.next().await {
            let Ok(Message::Binary(data)) = frame else {
                continue;
            };
            let messages = unpack_messages(&data).map_err(|error| SteamError::Unreadable(Box::new(error)))?;

            for message in messages {
                if message.emsg == EMSG_CLIENT_LOG_ON_RESPONSE {
                    let code = eresult_of(&message.body);

                    if code == eresult::OK {
                        let _ = socket.send(Message::Binary(pack_message(EMSG_CLIENT_LOG_OFF, message.session, &[]).into())).await;
                    }

                    let _ = socket.close(None).await;
                    return Ok(code);
                }

                if message.emsg == EMSG_CLIENT_LOGGED_OFF {
                    let _ = socket.close(None).await;
                    return Ok(eresult_of(&message.body));
                }
            }
        }

        Err(SteamError::Closed)
    };

    tokio::time::timeout(timeout, attempt).await.unwrap_or_else(|_| Err(SteamError::Timeout))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    const SESSION: Session = Session { steam_id: 0x0130_0001_0000_002a, session_id: 7 };

    fn eresult_body(code: Option<u32>) -> Vec<u8> {
        code.map_or_else(Vec::new, |code| vec![0x08, u8::try_from(code).unwrap()])
    }

    fn message(emsg: u32, session: Session, body: &[u8]) -> SteamMessage {
        SteamMessage { emsg, session, body: body.to_vec() }
    }

    #[test]
    fn names_known_codes_and_numbers_others() {
        let names = [1, 2, 8, 15, 17, 20, 27, 43, 48, 84, 9999].map(eresult_name);

        assert_eq!(
            names,
            [
                "ok",
                "fail",
                "invalidParam",
                "accessDenied",
                "banned",
                "serviceUnavailable",
                "expired",
                "accountDisabled",
                "tryAnotherCM",
                "rateLimitExceeded",
                "EResult 9999"
            ]
        );
    }

    #[test]
    fn opens_a_gzipped_multi_that_unpacks_to_exactly_four_mebibytes() {
        let session = Session { steam_id: GAME_SERVER_STEAM_ID, session_id: 0 };
        let header_bytes = pack_message(751, session, &[]).len();
        let body = vec![0x5a; 4 * 1024 * 1024 - 4 - header_bytes];
        let inner = pack_message(751, session, &body);
        let mut payload = u32::try_from(inner.len()).unwrap().to_le_bytes().to_vec();

        payload.extend_from_slice(&inner);
        assert_eq!(payload.len(), 4 * 1024 * 1024);

        let mut zipper = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());

        std::io::Write::write_all(&mut zipper, &payload).unwrap();

        let zipped = zipper.finish().unwrap();
        let mut multi = vec![0x01, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x08, 0x01, 0x12];
        let mut length = zipped.len();

        while length >= 0x80 {
            multi.push(u8::try_from(length & 0x7f).unwrap() | 0x80);
            length >>= 7;
        }
        multi.push(u8::try_from(length).unwrap());
        multi.extend_from_slice(&zipped);

        let messages = unpack_messages(&multi).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body.len(), body.len());
    }

    #[tokio::test]
    async fn a_logon_fails_when_the_connection_manager_cannot_be_reached() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();

        drop(listener);
        assert!(log_on_game_server("TOKEN", Some(endpoint), Duration::from_secs(5)).await.is_err());
    }

    #[test]
    fn frames_a_message_as_id_header_length_header_and_body() {
        assert_eq!(
            pack_message(5559, Session { steam_id: GAME_SERVER_STEAM_ID, session_id: 0 }, &[0xaa]),
            [0xb7, 0x15, 0x00, 0x80, 0x0b, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x01, 0x10, 0x00, 0xaa]
        );
    }

    #[test]
    fn writes_a_logon_with_the_token_cs2_and_the_private_address() {
        assert_eq!(
            logon_body("TOKEN", Some(0x0a00_0001)),
            [
                0x08, 0xad, 0x80, 0x04, 0x38, 0xb5, 0xfe, 0xff, 0xff, 0x0f, 0x5a, 0x05, 0x0d, 0x01, 0x00, 0x00, 0x0a, 0xa2, 0x03, 0x05, 0x54, 0x4f, 0x4b, 0x45,
                0x4e, 0xf0, 0x05, 0xda, 0x05, 0xb0, 0x06, 0x01
            ]
        );
        assert!(!logon_body("TOKEN", None).contains(&0x5a));
        // The bytes the reference encoder produces for the same input.
        assert_eq!(hex::encode(logon_body("TOK", None)), "08ad800438b5feffff0fa20303544f4bf005da05b00601");
        assert_eq!(
            hex::encode(pack_message(EMSG_CLIENT_LOG_OFF, Session { steam_id: 90_071_992_547_409_920, session_id: 12345 }, &[])),
            "c20200800c00000009000000000000400110b960"
        );
    }

    #[test]
    fn reads_the_session_and_body_of_a_logon_response() {
        let packet = [0xef, 0x02, 0x00, 0x80, 0x0b, 0x00, 0x00, 0x00, 0x09, 0x2a, 0x00, 0x00, 0x00, 0x01, 0x00, 0x30, 0x01, 0x10, 0x07, 0x08, 0x01];

        assert_eq!(unpack_messages(&packet).unwrap(), [message(751, SESSION, &[0x08, 0x01])]);
        assert_eq!(unpack_messages(&[0xef, 0x02, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x08, 0x01]).unwrap()[0].session, Session { steam_id: 0, session_id: 0 });
    }

    #[test]
    fn opens_nested_multis_up_to_the_depth_limit_and_refuses_deeper() {
        let inner = vec![0xef, 0x02, 0x00, 0x80, 0x02, 0x00, 0x00, 0x00, 0x10, 0x03, 0x08, 0x1b];
        // An unzipped Multi bundling one packet.
        let wrap = |packet: Vec<u8>| {
            let mut payload = vec![u8::try_from(packet.len()).unwrap(), 0, 0, 0];

            payload.extend_from_slice(&packet);

            let mut multi = vec![0x01, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x12, u8::try_from(payload.len()).unwrap()];

            multi.extend_from_slice(&payload);
            multi
        };
        let nested = |depth: usize| (0..depth).fold(inner.clone(), |packet, _| wrap(packet));

        assert_eq!(unpack_messages(&nested(MULTI_MAX_DEPTH)).unwrap(), [message(751, Session { steam_id: 0, session_id: 3 }, &[0x08, 0x1b])]);
        assert!(matches!(unpack_messages(&nested(MULTI_MAX_DEPTH + 1)), Err(SteamError::NestedTooDeep)));
    }

    #[test]
    fn opens_a_gzipped_multi() {
        let inner = [0xef, 0x02, 0x00, 0x80, 0x02, 0x00, 0x00, 0x00, 0x10, 0x03, 0x08, 0x1b];
        let mut payload = vec![u8::try_from(inner.len()).unwrap(), 0, 0, 0];

        payload.extend_from_slice(&inner);

        let mut zipper = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());

        std::io::Write::write_all(&mut zipper, &payload).unwrap();

        let zipped = zipper.finish().unwrap();
        let mut multi =
            vec![0x01, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x08, u8::try_from(payload.len()).unwrap(), 0x12, u8::try_from(zipped.len()).unwrap()];

        multi.extend_from_slice(&zipped);
        assert_eq!(unpack_messages(&multi).unwrap(), [message(751, Session { steam_id: 0, session_id: 3 }, &[0x08, 0x1b])]);
    }

    #[test]
    fn refuses_a_gzipped_multi_that_unpacks_past_the_limit() {
        let mut zipper = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());

        std::io::Write::write_all(&mut zipper, &vec![0; PACKED_MAX_BYTES + 1]).unwrap();

        let zipped = zipper.finish().unwrap();
        let mut multi = vec![0x01, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x08, 0x01, 0x12];
        let mut length = zipped.len();

        // The bytes field's length as a varint.
        while length >= 0x80 {
            multi.push(u8::try_from(length & 0x7f).unwrap() | 0x80);
            length >>= 7;
        }
        multi.push(u8::try_from(length).unwrap());
        multi.extend_from_slice(&zipped);
        assert!(matches!(unpack_messages(&multi), Err(SteamError::PackedTooLarge)));
    }

    #[test]
    fn reads_back_session_ids_of_several_varint_bytes() {
        for session_id in [300, 70_000] {
            let session = Session { steam_id: GAME_SERVER_STEAM_ID, session_id };

            assert_eq!(unpack_messages(&pack_message(751, session, &[0x08, 0x01])).unwrap(), [message(751, session, &[0x08, 0x01])]);
        }
    }

    #[test]
    fn opens_an_uncompressed_multi() {
        let first = pack_message(751, Session { steam_id: 0, session_id: 3 }, &[0x08, 0x1b]);
        let second = pack_message(757, Session { steam_id: 0, session_id: 3 }, &[0x08, 0x02]);
        let mut payload = Vec::new();

        for part in [&first, &second] {
            payload.extend_from_slice(&[u8::try_from(part.len()).unwrap(), 0, 0, 0]);
            payload.extend_from_slice(part);
        }

        let mut multi = vec![0x01, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x12, u8::try_from(payload.len()).unwrap()];

        multi.extend_from_slice(&payload);
        assert_eq!(
            unpack_messages(&multi).unwrap(),
            [message(751, Session { steam_id: 0, session_id: 3 }, &[0x08, 0x1b]), message(757, Session { steam_id: 0, session_id: 3 }, &[0x08, 0x02])]
        );
    }

    #[test]
    fn reads_past_fixed32_and_bytes_fields_and_stops_at_an_unknown_wire_type() {
        let response = |header: &[u8]| [&[0xef, 0x02, 0x00, 0x80, u8::try_from(header.len()).unwrap(), 0x00, 0x00, 0x00][..], header, &[0x08, 0x01]].concat();
        let steam_id = [0x09, 0x2a, 0x00, 0x00, 0x00, 0x01, 0x00, 0x30, 0x01];

        assert_eq!(unpack_messages(&response(&[&[0x1d, 0xff, 0xff, 0xff, 0xff][..], &steam_id, &[0x10, 0x07]].concat())).unwrap()[0].session, SESSION);
        assert_eq!(unpack_messages(&response(&[&[0x22, 0x03, 0x61, 0x62, 0x63][..], &steam_id, &[0x10, 0x07]].concat())).unwrap()[0].session, SESSION);
        assert_eq!(unpack_messages(&response(&[&steam_id[..], &[0x10, 0x07, 0x1b, 0x87]].concat())).unwrap()[0].session, SESSION);
    }

    #[test]
    fn refuses_a_truncated_varint_and_skips_a_non_protobuf_message() {
        assert_eq!(
            unpack_messages(&[0xef, 0x02, 0x00, 0x80, 0x02, 0x00, 0x00, 0x00, 0x10, 0x87, 0x08, 0x01]).unwrap_err().to_string(),
            "Truncated protobuf varint"
        );
        assert!(unpack_messages(&[0x7c, 0x15, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).unwrap().is_empty());
    }

    #[test]
    fn tells_steam_the_first_external_ipv4_address() {
        let addresses = ["127.0.0.1", "192.168.1.24", "10.0.0.5"].map(|address| address.parse().unwrap());

        assert_eq!(obfuscate_first_external(addresses), Some(0xc0a8_0118 ^ 0xbaad_f00d));
        assert_eq!(obfuscate_first_external(["127.0.0.1".parse().unwrap()]), None);
        assert_eq!(obfuscate_first_external([]), None);
    }

    /// The answer depends on the host, so the test checks it against the host's addresses: it
    /// unmasks to one of them that is not loopback, and is there whenever the host has one.
    #[test]
    fn tells_steam_an_external_ipv4_address_of_this_host() {
        let external: Vec<std::net::Ipv4Addr> = nix::ifaddrs::getifaddrs()
            .unwrap()
            .filter_map(|interface| interface.address?.as_sockaddr_in().map(nix::sys::socket::SockaddrIn::ip))
            .filter(|address| !address.is_loopback())
            .collect();
        let told = obfuscated_private_ip().map(|masked| std::net::Ipv4Addr::from(masked ^ 0xbaad_f00d));

        assert_eq!(told.is_some(), !external.is_empty());
        assert!(told.is_none_or(|address| external.contains(&address)), "{told:?} is not among {external:?}");
    }

    struct Cm {
        url: String,
        received: Arc<Mutex<Vec<SteamMessage>>>,
        closed: Arc<Mutex<usize>>,
    }

    /// A connection manager on loopback, answering each logon with `answer`.
    async fn connection_manager(answer: fn() -> Vec<Vec<u8>>, close: bool) -> Cm {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/cmsocket/", listener.local_addr().unwrap());
        let received: Arc<Mutex<Vec<SteamMessage>>> = Arc::default();
        let closed: Arc<Mutex<usize>> = Arc::default();
        let (seen, ended) = (Arc::clone(&received), Arc::clone(&closed));

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (seen, ended) = (Arc::clone(&seen), Arc::clone(&ended));

                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();

                    while let Some(Ok(frame)) = socket.next().await {
                        let Message::Binary(data) = frame else {
                            continue;
                        };
                        let messages = unpack_messages(&data).unwrap();
                        let logon = messages.iter().any(|message| message.emsg == EMSG_CLIENT_LOGON_GAME_SERVER);

                        seen.lock().unwrap().extend(messages);

                        if logon {
                            if close {
                                let _ = socket.close(None).await;
                            }

                            for reply in answer() {
                                let _ = socket.send(Message::Binary(reply.into())).await;
                            }
                        }
                    }

                    *ended.lock().unwrap() += 1;
                });
            }
        });

        Cm { url, received, closed }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn logs_the_token_on_then_off_with_the_session_steam_assigned() {
        let cm = connection_manager(|| vec![pack_message(EMSG_CLIENT_LOG_ON_RESPONSE, SESSION, &eresult_body(Some(eresult::OK)))], false).await;

        assert_eq!(log_on("TOKEN", &cm.url, None, Duration::from_secs(5)).await.unwrap(), eresult::OK);
        settle().await;

        let received = cm.received.lock().unwrap();

        assert_eq!(received.iter().map(|message| message.emsg).collect::<Vec<_>>(), [EMSG_CLIENT_LOGON_GAME_SERVER, EMSG_CLIENT_LOG_OFF]);
        assert!(received[0].body.windows(5).any(|window| window == b"TOKEN"));
        assert_eq!(received[1].session, SESSION);
    }

    #[tokio::test]
    async fn answers_a_refusal_with_its_code_and_sends_no_log_off() {
        let cm = connection_manager(|| vec![pack_message(EMSG_CLIENT_LOG_ON_RESPONSE, SESSION, &eresult_body(Some(eresult::ACCESS_DENIED)))], false).await;

        assert_eq!(log_on("TOKEN", &cm.url, None, Duration::from_secs(5)).await.unwrap(), eresult::ACCESS_DENIED);
        settle().await;
        assert_eq!(*cm.closed.lock().unwrap(), 1);
        assert_eq!(cm.received.lock().unwrap().iter().map(|message| message.emsg).collect::<Vec<_>>(), [EMSG_CLIENT_LOGON_GAME_SERVER]);
    }

    #[tokio::test]
    async fn reads_codes_from_responses_and_logged_off_notices_alone() {
        let fail = connection_manager(|| vec![pack_message(EMSG_CLIENT_LOG_ON_RESPONSE, SESSION, &eresult_body(None))], false).await;
        let logged_off = connection_manager(|| vec![pack_message(EMSG_CLIENT_LOGGED_OFF, SESSION, &eresult_body(Some(eresult::EXPIRED)))], false).await;
        let other_first = connection_manager(
            || {
                vec![
                    pack_message(5500, SESSION, &eresult_body(Some(eresult::OK))),
                    pack_message(EMSG_CLIENT_LOG_ON_RESPONSE, SESSION, &eresult_body(Some(eresult::ACCESS_DENIED))),
                ]
            },
            false,
        )
        .await;

        assert_eq!(log_on("TOKEN", &fail.url, None, Duration::from_secs(5)).await.unwrap(), eresult::FAIL);
        assert_eq!(log_on("TOKEN", &logged_off.url, None, Duration::from_secs(5)).await.unwrap(), eresult::EXPIRED);
        assert_eq!(log_on("TOKEN", &other_first.url, None, Duration::from_secs(5)).await.unwrap(), eresult::ACCESS_DENIED);
    }

    #[tokio::test]
    async fn fails_without_judging_the_token_when_steam_closes_stays_silent_or_garbles() {
        let closing = connection_manager(Vec::new, true).await;
        let silent = connection_manager(Vec::new, false).await;
        let garbled = connection_manager(|| vec![vec![1, 2, 3]], false).await;

        assert_eq!(
            log_on("TOKEN", &closing.url, None, Duration::from_secs(5)).await.unwrap_err().to_string(),
            "Steam closed the connection before answering the logon"
        );
        assert_eq!(log_on("TOKEN", &silent.url, None, Duration::from_millis(100)).await.unwrap_err().to_string(), "Steam did not answer the logon in time");
        assert!(
            log_on("TOKEN", &garbled.url, None, Duration::from_secs(5))
                .await
                .unwrap_err()
                .to_string()
                .starts_with("Steam sent a message the agent cannot read")
        );
        assert!(log_on("TOKEN", "ws://127.0.0.1:1/cmsocket/", None, Duration::from_secs(5)).await.is_err());
    }

    async fn directory(status: u16, body: serde_json::Value) -> String {
        use axum::routing::get;

        let app = axum::Router::new().route("/list", get(move || async move { (axum::http::StatusCode::from_u16(status).unwrap(), body.to_string()) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/list?cmtype=websockets", listener.local_addr().unwrap());

        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    #[tokio::test]
    async fn takes_the_first_websocket_manager_the_directory_lists() {
        let url = directory(200, serde_json::json!({ "response": { "serverlist": [{ "endpoint": "127.0.0.1:1", "type": "netfilter" }, { "endpoint": "cm.example:443", "type": "websockets" }] } })).await;

        assert_eq!(pick_endpoint(&url).await.unwrap(), "cm.example:443");
        assert!(pick_endpoint(&directory(503, serde_json::json!("")).await).await.unwrap_err().to_string().contains("HTTP 503"));

        for body in [serde_json::json!({ "response": { "serverlist": [{ "endpoint": "127.0.0.1:1", "type": "netfilter" }] } }), serde_json::json!({})] {
            assert_eq!(pick_endpoint(&directory(200, body).await).await.unwrap_err().to_string(), "Steam listed no WebSocket connection manager");
        }
    }
}

#[cfg(test)]
mod live {
    #[tokio::test]
    #[ignore = "reaches Steam"]
    async fn steam_refuses_a_made_up_token() {
        let code = super::log_on_game_server("0123456789ABCDEF0123456789ABCDEF", None, std::time::Duration::from_secs(20)).await.unwrap();

        assert_ne!(code, super::eresult::OK, "{}", super::eresult_name(code));
        println!("Steam answered {}", super::eresult_name(code));
    }
}
