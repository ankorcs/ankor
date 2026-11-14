//! The plugin relay against a fake plugin and a real spool.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::contract::types::{AgentToPluginMessage, ExecCommand, PluginToAgentMessage};
use ankor_agent::logger::{Entry, Logger};
use ankor_agent::plugin_relay::{PluginRelay, RelayHooks};
use ankor_agent::spool_client::{SpoolClient, SpoolPort};
use ankor_agent::testing::{plugin_message, until};
use ankor_protocol::constants::{PROTOCOL_HEADER, PROTOCOL_VERSION};
use ankor_protocol::types::AgentErrorCode;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const ALLOCATION: &str = "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e";
/// The key the relay is told a launch's plugin dials with.
const RELAY_KEY: &str = "0123456789abcdef";

type Plugin = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Setup {
    relay: PluginRelay,
    spool: Arc<SpoolClient>,
    online: Arc<Mutex<Vec<bool>>>,
    messages: Arc<Mutex<Vec<PluginToAgentMessage>>>,
    logged: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let spool = Arc::new(SpoolClient::start(dir.path().join("events/streams.sqlite"), Logger::null()));
    let online = Arc::new(Mutex::new(Vec::new()));
    let messages = Arc::new(Mutex::new(Vec::new()));
    let (seen_online, seen_messages) = (Arc::clone(&online), Arc::clone(&messages));
    let hooks = RelayHooks {
        on_plugin_online: Arc::new(move |_, connected| seen_online.lock().unwrap().push(connected)),
        on_plugin_message: Arc::new(move |_, message| seen_messages.lock().unwrap().push(message)),
    };
    let logged: Arc<Mutex<Vec<String>>> = Arc::default();
    let written = Arc::clone(&logged);
    let log = Logger::new("relay", Some(Arc::new(move |entry: Entry| written.lock().unwrap().push(entry.message))));
    let relay = PluginRelay::new(Some(Arc::clone(&spool) as Arc<dyn SpoolPort>), log);

    relay.bind(0).await.unwrap().serve(hooks);
    Setup { relay, spool, online, messages, logged, _dir: dir }
}

async fn dial(url: &str, protocol: &str) -> Result<Plugin, u16> {
    let mut request = url.into_client_request().unwrap();

    request.headers_mut().insert(PROTOCOL_HEADER, protocol.parse().unwrap());

    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(error) => panic!("{error}"),
    }
}

async fn next_json(plugin: &mut Plugin) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), plugin.next()).await.expect("a frame in time").expect("open").expect("frame") {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Close(_) => panic!("closed"),
            _ => {}
        }
    }
}

fn event(seq: i64) -> Message {
    Message::Text(json!({ "type": "event", "seq": seq, "event": { "type": "map_start", "kills": 1.5 } }).to_string().into())
}

#[tokio::test]
async fn refuses_strangers() {
    let setup = setup().await;
    let url = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);

    assert_eq!(dial(&url.replace("key=", "key=x"), PROTOCOL_VERSION).await.err(), Some(401));
    assert_eq!(dial(&url, "9.0").await.err(), Some(426));
    assert_eq!(dial(&url.replace("slot_id=s1", "slot_id=s2"), PROTOCOL_VERSION).await.err(), Some(404));
}

#[tokio::test]
async fn events_are_stored_and_acknowledged_without_their_type() {
    let setup = setup().await;
    let mut plugin = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();

    for seq in 0..3 {
        plugin.send(event(seq)).await.unwrap();
    }

    assert_eq!(next_json(&mut plugin).await, json!({ "type": "ack", "to_seq": 2 }));

    let pending = setup.spool.pending(ALLOCATION.to_owned()).await.unwrap();

    assert_eq!(pending.len(), 3);
    assert_eq!(serde_json::from_str::<Value>(&pending[0].frame).unwrap(), json!({ "seq": 0, "event": { "type": "map_start", "kills": 1.5 } }));
    assert_eq!(setup.relay.attached_allocations(), [ALLOCATION]);

    drop(plugin);

    let mut again = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();

    assert_eq!(next_json(&mut again).await, json!({ "type": "ack", "to_seq": 2 }), "a reconnect learns how far it was stored");
    assert_eq!(&setup.online.lock().unwrap()[..2], [true, false]);
}

#[tokio::test]
async fn a_batch_under_the_frame_limit_waits_for_the_flush_interval() {
    let setup = setup().await;
    let mut plugin = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();
    let sent = std::time::Instant::now();

    plugin.send(event(0)).await.unwrap();

    assert_eq!(next_json(&mut plugin).await, json!({ "type": "ack", "to_seq": 0 }));
    // The relay commits a partial batch 50 ms after its first frame, so later frames share the
    // commit; a busy machine only makes the ack later.
    assert!(sent.elapsed() >= Duration::from_millis(50), "acked after {:?}", sent.elapsed());
}

#[tokio::test]
async fn close_stores_what_is_queued_before_it_returns() {
    let setup = setup().await;
    let mut plugin = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();
    let status = json!({ "type": "server_status", "request_id": null, "map_name": "de_inferno", "match": null, "players": [], "config_error": null, "ready": true, "server_steam_id": null });

    for seq in 0..3 {
        plugin.send(event(seq)).await.unwrap();
    }

    // The relay reads a socket's frames in order: once it has passed on the status that follows
    // them, the events are queued, and well before the 50 ms flush.
    plugin.send(Message::Text(status.to_string().into())).await.unwrap();
    until("the status passed on", || setup.messages.lock().unwrap().iter().any(|message| matches!(message, PluginToAgentMessage::ServerStatus(_)))).await;
    setup.relay.close().await;

    assert_eq!(setup.spool.pending(ALLOCATION.to_owned()).await.unwrap().len(), 3);
}

/// Writes `parts` to the relay's port with `pause` between them, then reads the answer to its end,
/// and says how long the answer took after the last part.
async fn raw_exchange(url: &str, parts: &[&[u8]], pause: Duration, half_close: bool) -> (String, Duration) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let port = url.split(':').nth(2).unwrap().split('/').next().unwrap();
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();

    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(pause).await;
        }

        stream.write_all(part).await.unwrap();
    }

    if half_close {
        stream.shutdown().await.unwrap();
    }

    let started = std::time::Instant::now();
    let mut answer = Vec::new();

    // A 404 closes the connection; an accepted upgrade keeps it open, so a short read is enough.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        let mut chunk = [0u8; 4096];

        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    answer.extend_from_slice(&chunk[..read]);

                    if answer.windows(4).any(|end| end == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
    })
    .await;

    (String::from_utf8_lossy(&answer).into_owned(), started.elapsed())
}

fn upgrade_request(url: &str) -> String {
    let path = &url[url.find("/ws/").unwrap()..];

    format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAnkor-Protocol: {PROTOCOL_VERSION}\r\n\r\n"
    )
}

#[tokio::test]
async fn refuses_a_key_that_differs_in_two_places_whose_differences_cancel_out() {
    let setup = setup().await;
    let url = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);
    let start = url.find("key=").unwrap() + 4;
    let mut key: Vec<u8> = url[start..start + RELAY_KEY.len()].bytes().collect();
    let flippable: Vec<usize> = (0..key.len()).filter(|&index| key[index] != b'a').take(2).collect();

    // The same bit flipped in two bytes: a check that XORs the differences together sees none.
    for index in flippable {
        key[index] ^= 1;
    }

    let forged = format!("{}{}{}", &url[..start], String::from_utf8(key).unwrap(), &url[start + RELAY_KEY.len()..]);

    assert_eq!(dial(&forged, PROTOCOL_VERSION).await.err(), Some(401));
}

#[tokio::test]
async fn logs_a_frame_it_drops_for_having_no_place_in_the_stream() {
    let setup = setup().await;
    let mut plugin = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();

    plugin.send(Message::Text("not json at all".into())).await.unwrap();
    until("the drop logged", || setup.logged.lock().unwrap().iter().any(|line| line.contains("dropped a producer frame"))).await;

    assert_eq!(
        setup.logged.lock().unwrap().iter().filter(|line| line.contains("dropped")).cloned().collect::<Vec<_>>(),
        ["[relay] slot s1: dropped a producer frame with not JSON: not json at all"]
    );
}

/// A binary frame or one over the frame limit ends the socket, and the log says why; a plugin
/// that closes its socket gets no line.
#[tokio::test]
async fn logs_why_it_ended_a_socket_the_plugin_broke_and_nothing_for_a_close() {
    let setup = setup().await;
    let ended = || setup.logged.lock().unwrap().iter().filter(|line| line.contains("ended the plugin's socket")).cloned().collect::<Vec<_>>();
    let url = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);

    let mut plugin = dial(&url, PROTOCOL_VERSION).await.unwrap();
    plugin.send(Message::Binary(vec![0xff].into())).await.unwrap();
    until("the binary frame logged", || ended().len() == 1).await;
    assert_eq!(ended(), ["[relay] slot s1: ended the plugin's socket: it sent a binary frame, which the protocol does not use"]);

    let mut plugin = dial(&url, PROTOCOL_VERSION).await.unwrap();
    let limit = usize::try_from(ankor_agent::contract::constants::MAX_FRAME_BYTES).unwrap();
    let _ = plugin.send(Message::Text("x".repeat(limit + 1).into())).await;
    until("the oversized frame logged", || ended().len() == 2).await;
    assert!(ended()[1].starts_with("[relay] slot s1: ended the plugin's socket: Space limit exceeded"), "{:?}", ended());

    let mut plugin = dial(&url, PROTOCOL_VERSION).await.unwrap();
    plugin.close(None).await.unwrap();
    // As a server that is killed: the connection drops without a close.
    drop(dial(&url, PROTOCOL_VERSION).await.unwrap());
    let mut plugin = dial(&url, PROTOCOL_VERSION).await.unwrap();
    plugin.send(Message::Text(json!({ "type": "quitting" }).to_string().into())).await.unwrap();
    until("the next plugin's message", || !setup.messages.lock().unwrap().is_empty()).await;
    assert_eq!(ended().len(), 2);
}

#[tokio::test]
async fn drops_frames_that_only_the_contract_finds_wrong() {
    let setup = setup().await;
    let mut plugin = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();

    // Each reads into its Rust type; the contract refuses a negative `seq` and a negative score.
    let event = json!({ "type": "event", "seq": -1, "event": { "type": "map_start" } }).to_string();
    let score = json!({ "type": "score", "team1_score": -1, "team2_score": 0 }).to_string();

    plugin.send(Message::Text(event.clone().into())).await.unwrap();
    plugin.send(Message::Text(score.clone().into())).await.unwrap();
    plugin.send(Message::Text(json!({ "type": "quitting" }).to_string().into())).await.unwrap();
    until("the valid message after them", || setup.messages.lock().unwrap().len() == 1).await;

    assert!(matches!(setup.messages.lock().unwrap().as_slice(), [PluginToAgentMessage::Quitting(_)]));
    assert_eq!(
        setup.logged.lock().unwrap().iter().filter(|line| line.contains("dropped")).cloned().collect::<Vec<_>>(),
        [
            format!("[relay] slot s1: dropped a producer frame with no seq: {event}"),
            format!("[relay] slot s1: dropped a producer frame with an unreadable message: {score}")
        ]
    );
}

#[tokio::test]
async fn skips_a_message_a_newer_plugin_sends_and_logs_its_type_once() {
    let setup = setup().await;
    let mut plugin = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();

    for _ in 0..2 {
        plugin.send(Message::Text(json!({ "type": "crowd_noise", "level": 3 }).to_string().into())).await.unwrap();
    }

    plugin.send(Message::Text(json!({ "type": "quitting" }).to_string().into())).await.unwrap();
    until("the known message after them", || setup.messages.lock().unwrap().len() == 1).await;

    let logged = setup.logged.lock().unwrap().clone();

    assert!(matches!(setup.messages.lock().unwrap().as_slice(), [PluginToAgentMessage::Quitting(_)]));
    assert_eq!(logged.iter().filter(|line| line.contains("skipping crowd_noise frames, which this agent does not know")).count(), 1, "{logged:?}");
    assert!(!logged.iter().any(|line| line.contains("dropped")), "{logged:?}");
}

#[tokio::test]
async fn upgrades_a_request_that_arrives_in_parts() {
    let setup = setup().await;
    let url = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);
    let request = upgrade_request(&url);
    let (head, tail) = request.split_at(20);
    let (answer, _) = raw_exchange(&url, &[head.as_bytes(), tail.as_bytes()], Duration::from_millis(50), false).await;

    assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");
}

#[tokio::test]
async fn answers_a_request_for_another_upgrade_with_a_404() {
    let setup = setup().await;
    let url = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);
    let request = upgrade_request(&url).replace("Upgrade: websocket", "Upgrade: h2c");
    let (answer, _) = raw_exchange(&url, &[request.as_bytes()], Duration::ZERO, false).await;

    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
}

#[tokio::test]
async fn answers_a_client_that_closes_without_a_request_without_waiting() {
    let setup = setup().await;
    let url = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);
    let (answer, elapsed) = raw_exchange(&url, &[], Duration::ZERO, true).await;

    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
    assert!(elapsed < Duration::from_millis(500), "answered after {elapsed:?}");
}

#[tokio::test]
async fn answers_a_request_head_larger_than_it_reads_without_waiting() {
    let setup = setup().await;
    let url = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);
    let request = format!("GET /ws/plugin HTTP/1.1\r\nX-Filler: {}\r\n", "f".repeat(20 * 1024));
    let (answer, elapsed) = raw_exchange(&url, &[request.as_bytes()], Duration::ZERO, false).await;

    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
    assert!(elapsed < Duration::from_millis(500), "answered after {elapsed:?}");
}

#[tokio::test]
async fn a_client_that_sends_nothing_is_dropped() {
    let setup = setup().await;
    let url = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);
    let port = url.split(':').nth(2).unwrap().split('/').next().unwrap();
    let mut silent = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
    let mut answer = Vec::new();

    let read = tokio::time::timeout(Duration::from_secs(3), tokio::io::AsyncReadExt::read_to_end(&mut silent, &mut answer)).await;

    assert!(read.is_ok(), "the relay kept a silent client");
    assert!(String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 404"));
}

#[tokio::test]
async fn releasing_an_ended_launch_leaves_a_newer_launchs_entry() {
    const NEWER: &str = "7c2d3e4f-1a2b-4c5d-8e9f-0a1b2c3d4e5f";
    let setup = setup().await;

    let _ = setup.relay.prepare("s1", ALLOCATION, RELAY_KEY);

    let newer = setup.relay.prepare("s1", NEWER, RELAY_KEY);

    setup.relay.release_allocation("s1", ALLOCATION);
    assert!(dial(&newer, PROTOCOL_VERSION).await.is_ok(), "the newer launch's plugin can still connect");

    setup.relay.release_allocation("s1", NEWER);
    assert_eq!(dial(&newer, PROTOCOL_VERSION).await.err(), Some(404));
}

#[tokio::test]
async fn a_gap_ends_the_socket_for_a_replay() {
    let setup = setup().await;
    let mut plugin = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();

    plugin.send(event(0)).await.unwrap();
    assert_eq!(next_json(&mut plugin).await["to_seq"], 0);
    plugin.send(event(5)).await.unwrap();

    let ended = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match plugin.next().await {
                None | Some(Err(_) | Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            }
        }
    });

    assert!(ended.await.is_ok(), "the relay ends the socket");
}

#[tokio::test]
async fn status_and_commands_round_trip() {
    let setup = setup().await;
    let mut plugin = dial(&setup.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();
    let relay = setup.relay.clone();
    let status = tokio::spawn(async move { relay.request_status(ALLOCATION, Duration::from_secs(2)).await });
    let request = next_json(&mut plugin).await;

    assert!(matches!(plugin_message(&request), Some(AgentToPluginMessage::GetServerStatus(_))));

    let answer = json!({ "type": "server_status", "request_id": request["request_id"], "map_name": "de_inferno", "match": null, "players": [], "config_error": null, "ready": true, "server_steam_id": null });

    plugin.send(Message::Text(answer.to_string().into())).await.unwrap();

    let state = status.await.unwrap().unwrap();

    assert_eq!(state.map_name, "de_inferno");
    assert!(state.ready);

    let relay = setup.relay.clone();
    let command = tokio::spawn(async move {
        relay
            .run_command(
                ALLOCATION,
                |request_id| AgentToPluginMessage::Exec(ExecCommand { command: "mp_restartgame 1".to_owned(), request_id }),
                Duration::from_secs(2),
            )
            .await
    });
    let request = next_json(&mut plugin).await;

    assert_eq!(request["command"], "mp_restartgame 1");
    plugin.send(Message::Text(json!({ "type": "command_result", "request_id": request["request_id"], "error": "no_match" }).to_string().into())).await.unwrap();

    let refused = command.await.unwrap().unwrap_err();

    assert_eq!(refused.code, AgentErrorCode::NoMatch);
    assert!(matches!(setup.messages.lock().unwrap().as_slice(), [PluginToAgentMessage::ServerStatus(_), PluginToAgentMessage::CommandResult(_)]));

    let silent = setup.relay.request_status(ALLOCATION, Duration::from_millis(100)).await.unwrap_err();

    assert_eq!(silent.code, AgentErrorCode::PluginStatusTimeout);

    drop(plugin);
    until("the relay to see the plugin go", || setup.online.lock().unwrap().last() == Some(&false)).await;
    assert_eq!(setup.relay.request_status(ALLOCATION, Duration::from_secs(1)).await.unwrap_err().code, AgentErrorCode::PluginOffline);
}

mod gated_spool {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ankor_agent::event_spool::{SpoolAck, SpoolAppend, SpoolBacklog, SpoolError, SpoolFrame, SpoolStream};
    use ankor_agent::spool_client::SpoolPort;
    use ankor_protocol::types::ServerExit;
    use async_trait::async_trait;
    use tokio::sync::watch;

    use super::*;

    /// A spool whose commits wait until the test opens it, then store every stream, or fail each
    /// when `fail` is set.
    struct GatedSpool {
        open: watch::Sender<bool>,
        appends: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl SpoolPort for GatedSpool {
        async fn version(&self) -> Result<u32, SpoolError> {
            Ok(1)
        }

        async fn position(&self, _: String) -> Result<Option<SpoolAck>, SpoolError> {
            Ok(None)
        }

        async fn append(&self, streams: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>, SpoolError> {
            self.appends.fetch_add(1, Ordering::SeqCst);

            let _ = self.open.subscribe().wait_for(|open| *open).await;

            Ok(streams
                .into_iter()
                .map(|stream| {
                    if self.fail {
                        Err("disk full".to_owned())
                    } else {
                        Ok(stream.frames.last().map(|frame| SpoolAck { allocation_id: stream.allocation_id.clone(), to_seq: frame.seq }))
                    }
                })
                .collect())
        }

        async fn pending(&self, _: String) -> Result<Vec<SpoolFrame>, SpoolError> {
            Ok(vec![])
        }

        async fn backlog(&self) -> Result<Vec<SpoolBacklog>, SpoolError> {
            Ok(vec![])
        }

        async fn acknowledge(&self, _: String, _: i64) -> Result<(), SpoolError> {
            Ok(())
        }

        async fn prune(&self, _: Vec<String>) -> Result<Vec<SpoolBacklog>, SpoolError> {
            Ok(vec![])
        }

        async fn record_exit(&self, _: ServerExit) -> Result<(), SpoolError> {
            Ok(())
        }

        async fn exits(&self) -> Result<Vec<ServerExit>, SpoolError> {
            Ok(vec![])
        }

        async fn forget_exits(&self, _: Vec<String>) -> Result<(), SpoolError> {
            Ok(())
        }

        async fn allocation_used(&self, _: String) -> Result<bool, SpoolError> {
            Ok(false)
        }

        async fn record_allocation(&self, _: String) -> Result<(), SpoolError> {
            Ok(())
        }

        async fn close(&self) -> Result<(), SpoolError> {
            Ok(())
        }
    }

    struct Gated {
        relay: PluginRelay,
        spool: Arc<GatedSpool>,
        messages: Arc<Mutex<Vec<PluginToAgentMessage>>>,
        logged: Arc<Mutex<Vec<String>>>,
    }

    async fn gated(fail: bool) -> Gated {
        let spool = Arc::new(GatedSpool { open: watch::channel(false).0, appends: AtomicUsize::new(0), fail });
        let messages: Arc<Mutex<Vec<PluginToAgentMessage>>> = Arc::default();
        let seen = Arc::clone(&messages);
        let hooks = RelayHooks { on_plugin_online: Arc::new(|_, _| {}), on_plugin_message: Arc::new(move |_, message| seen.lock().unwrap().push(message)) };
        let logged: Arc<Mutex<Vec<String>>> = Arc::default();
        let written = Arc::clone(&logged);
        let log = Logger::new("relay", Some(Arc::new(move |entry: Entry| written.lock().unwrap().push(entry.message))));
        let relay = PluginRelay::new(Some(Arc::clone(&spool) as Arc<dyn SpoolPort>), log);

        relay.bind(0).await.unwrap().serve(hooks);
        Gated { relay, spool, messages, logged }
    }

    impl Gated {
        /// Sends a status after what was sent and waits until it is passed on, so every event before it
        /// is queued.
        async fn queued(&self, plugin: &mut Plugin) {
            let status = json!({ "type": "server_status", "request_id": null, "map_name": "de_inferno", "match": null, "players": [], "config_error": null, "ready": true, "server_steam_id": null });
            let before = self.messages.lock().unwrap().len();

            plugin.send(Message::Text(status.to_string().into())).await.unwrap();
            until("the status passed on", || self.messages.lock().unwrap().len() > before).await;
        }

        async fn committing(&self) {
            until("a commit under way", || self.spool.appends.load(Ordering::SeqCst) > 0).await;
        }

        fn open(&self) {
            self.spool.open.send_replace(true);
        }
    }

    /// An event frame whose stored form, the frame without its `type`, is `size` bytes long.
    fn sized(seq: i64, size: usize) -> Message {
        let stored = |pad: &str| json!({ "seq": seq, "event": { "type": "map_start", "pad": pad } });
        let pad = "x".repeat(size - stored("").to_string().len());
        let mut frame = stored(&pad);

        assert_eq!(frame.to_string().len(), size);
        frame["type"] = json!("event");
        Message::Text(frame.to_string().into())
    }

    fn logged(gated: &Gated, part: &str) -> Vec<String> {
        gated.logged.lock().unwrap().iter().filter(|line| line.contains(part)).cloned().collect()
    }

    /// The events queued while the failed commit ran follow its lost ones, so they are dropped as
    /// well, and the plugin replays all of them on a new socket. Their bytes leave the queue with them.
    #[tokio::test]
    async fn ends_the_socket_for_a_replay_once_a_failed_commit_and_the_events_queued_behind_it_are_settled() {
        const MIB: usize = 1 << 20;

        let gated = gated(true).await;
        let url = gated.relay.prepare("s1", ALLOCATION, RELAY_KEY);
        let mut plugin = dial(&url, PROTOCOL_VERSION).await.unwrap();

        plugin.send(sized(0, MIB)).await.unwrap();
        gated.committing().await;
        plugin.send(sized(1, MIB)).await.unwrap();
        plugin.send(sized(2, MIB)).await.unwrap();
        gated.queued(&mut plugin).await;
        gated.open();

        let ended = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match plugin.next().await {
                    None | Some(Err(_) | Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        });

        assert!(ended.await.is_ok(), "the relay ends the socket");
        assert_eq!(logged(&gated, "event commit failed, replay required: disk full").len(), 1);

        // The replay's first event fits the emptied queue.
        let mut replay = dial(&url, PROTOCOL_VERSION).await.unwrap();

        replay.send(sized(0, MIB)).await.unwrap();
        gated.queued(&mut replay).await;
        assert_eq!(logged(&gated, "history queue full"), Vec::<String>::new());
    }

    /// History that fills the queue to exactly `RELAY_QUEUE_MAX_BYTES` is kept; only a byte more pauses
    /// it. A commit before it leaves the queue empty.
    #[tokio::test]
    async fn queues_history_up_to_exactly_the_queue_byte_limit() {
        const FRAMES: i64 = 64;

        let gated = gated(false).await;
        let mut plugin = dial(&gated.relay.prepare("s1", ALLOCATION, RELAY_KEY), PROTOCOL_VERSION).await.unwrap();
        let limit = usize::try_from(ankor_agent::contract::constants::RELAY_QUEUE_MAX_BYTES).unwrap();
        let each = limit / usize::try_from(FRAMES).unwrap();

        gated.open();
        plugin.send(sized(0, 1000)).await.unwrap();
        assert_eq!(next_json(&mut plugin).await, json!({ "type": "ack", "to_seq": 0 }));
        gated.spool.open.send_replace(false);

        plugin.send(sized(1, each)).await.unwrap();
        until("a second commit under way", || gated.spool.appends.load(Ordering::SeqCst) > 1).await;

        for seq in 2..=FRAMES {
            plugin.send(sized(seq, each)).await.unwrap();
        }

        gated.queued(&mut plugin).await;
        assert_eq!(logged(&gated, "history queue full"), Vec::<String>::new());
        gated.open();

        loop {
            if next_json(&mut plugin).await == json!({ "type": "ack", "to_seq": FRAMES }) {
                break;
            }
        }
    }
}
