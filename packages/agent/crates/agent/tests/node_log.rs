//! What the node logs on its way to the controller: the rings behind it, the lease that lets it flow, and the
//! frames that carry it.
//!
//! The rules worth pinning are the ones that would hurt on a real host. Every ring is bounded (an
//! agent runs for weeks). Nothing flows while nobody watches. A replay fits the frame limit however
//! much is kept. And nothing here can loop: sending logs on failure, and that line comes back.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::logger::{Entry, Level, Logger};
use ankor_agent::node_log::{Clock, NodeLog, Transport};
use ankor_agent::testing::tokio_clock;
use ankor_protocol::constants::{
    AGENT_LOG_LINES, ENDED_SERVER_LOGS, ENDED_UPDATE_LOGS, LOG_BATCH_MAX_BYTES, LOG_INDEX_REFRESH, LOG_LINE_MAX_CHARS, SERVER_LOG_LINES, WATCH_LEASE,
    agent_budgets,
};
use ankor_protocol::schemas::s_agent_to_controller_message;
use ankor_protocol::types::{
    AgentLogBatch, AgentLogIndex, AgentLogLevel, AgentToControllerMessage, LogLine, LogSource, LogSourceAgent, LogSourceCs2Update, LogSourceServer,
    LogSourceSummary,
};
use ankor_protocol::validate::{format_issues, safe_parse};

const GAME_A: &str = "11111111-1111-4111-8111-111111111111";
const UPDATE_ID: &str = "00000000-0000-4000-8000-00000000a0a0";

fn agent() -> LogSource {
    LogSource::Agent(LogSourceAgent {})
}

fn update(id: &str) -> LogSource {
    LogSource::Cs2Update(LogSourceCs2Update { update_id: id.to_owned() })
}

fn server(id: &str) -> LogSource {
    LogSource::Server(LogSourceServer { allocation_id: id.to_owned() })
}

fn of_a() -> LogSource {
    server(GAME_A)
}

fn game(index: usize) -> String {
    format!("00000000-0000-4000-8000-{index:012}")
}

/// A source's kind, so a test picks batches by the contract's variant rather than by its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Agent,
    Cs2Update,
    Server,
}

fn kind(source: &LogSource) -> Kind {
    match source {
        LogSource::Agent(_) => Kind::Agent,
        LogSource::Cs2Update(_) => Kind::Cs2Update,
        LogSource::Server(_) => Kind::Server,
    }
}

fn entry(at_ms: i64, level: Level, message: &str) -> Entry {
    Entry { at_ms, level, message: message.to_owned() }
}

/// Lets the work that timers started run, then returns.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

/// Moves paused time on by `ms`, running each timer that falls due in order.
async fn advance(by: Duration) {
    settle().await;
    tokio::time::sleep(by).await;
    settle().await;
}

struct Recorder {
    connected: bool,
    sent: Mutex<Vec<AgentToControllerMessage>>,
}

impl Transport for Recorder {
    fn connected(&self) -> bool {
        self.connected
    }

    fn send(&self, frame: AgentToControllerMessage) {
        let value = serde_json::to_value(&frame).unwrap();

        if let Err(issues) = safe_parse(s_agent_to_controller_message, value) {
            panic!("an invalid frame: {}", format_issues(&issues));
        }

        self.sent.lock().unwrap().push(frame);
    }
}

struct Attached {
    log: NodeLog,
    stop: tokio_util::sync::CancellationToken,
    clock: Clock,
    recorder: Arc<Recorder>,
}

impl Attached {
    fn sent(&self) -> Vec<AgentToControllerMessage> {
        self.recorder.sent.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.recorder.sent.lock().unwrap().clear();
    }

    fn batches(&self) -> Vec<AgentLogBatch> {
        self.sent()
            .into_iter()
            .filter_map(|frame| match frame {
                AgentToControllerMessage::LogBatch(batch) => Some(batch),
                _ => None,
            })
            .collect()
    }

    fn indexes(&self) -> Vec<AgentLogIndex> {
        self.sent()
            .into_iter()
            .filter_map(|frame| match frame {
                AgentToControllerMessage::LogIndex(index) => Some(index),
                _ => None,
            })
            .collect()
    }
}

fn attached_with(connected: bool) -> Attached {
    let clock = tokio_clock();
    let stop = tokio_util::sync::CancellationToken::new();
    let log = NodeLog::new(Arc::clone(&clock), stop.clone());
    let recorder = Arc::new(Recorder { connected, sent: Mutex::default() });

    log.attach(Arc::clone(&recorder) as Arc<dyn Transport>);
    log.update_started(UPDATE_ID);

    Attached { log, stop, clock, recorder }
}

fn attached() -> Attached {
    attached_with(true)
}

fn texts(frames: &[AgentLogBatch]) -> Vec<String> {
    frames.iter().flat_map(|frame| frame.lines.iter().map(|line| line.line.clone())).collect()
}

/// A summary whose last line came at some time.
fn summary(source: LogSource, line_count: i64, ended: bool) -> (LogSource, i64, bool, bool) {
    (source, line_count, true, ended)
}

/// A summary as [`summary`] compares it: whether a last line time is known, not which.
fn shape(entry: &LogSourceSummary) -> (LogSource, i64, bool, bool) {
    (entry.source.clone(), entry.line_count, entry.last_line_at.is_some(), entry.ended)
}

fn json_len(frame: &AgentLogBatch) -> usize {
    serde_json::to_string(&AgentToControllerMessage::LogBatch(frame.clone())).unwrap().len()
}

/// The bytes a game line takes in a frame, with the comma before it.
fn line_bytes(at: i64, text: &str) -> usize {
    serde_json::to_string(&LogLine { at, level: None, line: text.to_owned() }).unwrap().len() + 1
}

mod node_log {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn keeps_every_source_while_unwatched_and_replays_only_the_one_whose_lease_starts_after_the_index() {
        let attached = attached();
        let log = &attached.log;

        log.agent_line(&entry(1, Level::Info, "[agent] public ip: 203.0.113.10"));
        log.game_line(GAME_A, "Server is hibernating");
        log.update_line("Update state (0x61) downloading, progress: 12.50");
        advance(Duration::from_secs(1)).await;
        assert!(attached.sent().is_empty());

        log.watch(&of_a(), None);

        let sent = attached.sent();

        assert_eq!(sent.len(), 2);
        let AgentToControllerMessage::LogIndex(index) = &sent[0] else { panic!("{sent:?}") };
        assert_eq!(
            index.sources.iter().map(shape).collect::<Vec<_>>(),
            [summary(agent(), 1, false), summary(update(UPDATE_ID), 1, false), summary(of_a(), 1, false)]
        );
        let AgentToControllerMessage::LogBatch(batch) = &sent[1] else { panic!("{sent:?}") };
        assert_eq!((&batch.source, &batch.replay_id, batch.reset), (&of_a(), &None, true));
        assert_eq!(batch.lines.iter().map(|line| (line.level, line.line.as_str())).collect::<Vec<_>>(), [(None, "Server is hibernating")]);
    }

    #[tokio::test(start_paused = true)]
    async fn batches_new_lines_of_watched_sources_alone_and_goes_quiet_once_a_lease_lapses() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&of_a(), None);
        attached.clear();
        log.game_line(GAME_A, "one");
        log.game_line(GAME_A, "two");
        log.update_line("unwatched");
        log.agent_line(&entry(5, Level::Warn, "unwatched too"));
        assert!(attached.sent().is_empty());

        advance(Duration::from_millis(250)).await;
        let batches = attached.batches();
        assert_eq!(batches.len(), 1);
        assert_eq!((&batches[0].source, &batches[0].replay_id, batches[0].reset), (&of_a(), &None, false));
        assert_eq!(batches[0].lines.iter().map(|line| (line.level, line.line.as_str())).collect::<Vec<_>>(), [(None, "one"), (None, "two")]);

        advance(WATCH_LEASE).await;
        attached.clear();
        log.game_line(GAME_A, "after the lease");
        advance(Duration::from_secs(1)).await;
        assert!(attached.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn sends_nothing_for_a_source_whose_lease_lapses_while_its_lines_wait_for_the_flush() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&of_a(), None);
        advance(WATCH_LEASE.checked_sub(Duration::from_millis(150)).unwrap()).await;
        log.watch(&agent(), None);
        attached.clear();
        log.game_line(GAME_A, "late");
        advance(Duration::from_millis(250)).await;
        assert!(attached.batches().is_empty());

        advance(WATCH_LEASE.checked_sub(Duration::from_millis(400)).unwrap()).await;
        attached.clear();
        log.game_line(&game(1), "new source");
        advance(Duration::from_millis(250)).await;
        assert!(attached.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn sends_no_waiting_lines_once_stopped() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&of_a(), None);
        attached.clear();
        log.game_line(GAME_A, "waiting");
        attached.stop.cancel();
        advance(Duration::from_secs(1)).await;
        assert!(attached.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn keeps_a_lease_per_source_two_watched_sources_stream_side_by_side() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&agent(), None);
        log.watch(&update(UPDATE_ID), None);
        attached.clear();
        log.agent_line(&entry(1, Level::Error, "agent line"));
        log.update_line("update line");
        advance(Duration::from_millis(250)).await;

        let batches: Vec<_> = attached.batches().iter().map(|frame| (kind(&frame.source), texts(std::slice::from_ref(frame)))).collect();

        assert_eq!(batches, [(Kind::Agent, vec!["agent line".to_owned()]), (Kind::Cs2Update, vec!["update line".to_owned()])]);
    }

    // An agent line can quote what it read from outside, such as a frame it dropped, so it gets the
    // same escape handling as server output.
    #[tokio::test(start_paused = true)]
    async fn sanitises_an_agent_line_as_it_does_server_output() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&agent(), None);
        attached.clear();
        log.agent_line(&entry(1, Level::Warn, "dropped \x1b]8;;https://example.com\x1b\\frame\x07 \x1b[31mred\x1b[0m\u{202e}"));
        advance(Duration::from_millis(250)).await;

        assert_eq!(texts(&attached.batches()), ["dropped frame \x1b[31mred\x1b[0m"]);
    }

    #[tokio::test(start_paused = true)]
    async fn renews_without_a_replay_and_replays_on_request_or_for_a_new_connection_while_watched() {
        let attached = attached();
        let log = &attached.log;
        let resets = || attached.batches().into_iter().filter(|frame| frame.reset).map(|frame| frame.replay_id).collect::<Vec<_>>();

        log.game_line(GAME_A, "kept");
        log.watch(&of_a(), None);
        log.watch(&of_a(), None);
        assert_eq!(resets(), [None]);

        log.watch(&of_a(), Some("r1"));
        log.game_line(&game(1), "new source");
        log.resend();
        assert_eq!(resets(), [None, Some("r1".to_owned()), None]);
        assert_eq!(attached.indexes().len(), 3);

        advance(WATCH_LEASE).await;
        log.resend();
        assert_eq!(resets().len(), 3);
        assert_eq!(attached.indexes().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn replays_a_lease_that_starts_to_the_watcher_that_asked_and_to_every_watcher_when_none_did() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&agent(), Some("r1"));
        log.watch(&of_a(), None);

        let batches: Vec<_> = attached.batches().into_iter().map(|frame| (kind(&frame.source), frame.replay_id)).collect();

        assert_eq!(batches, [(Kind::Agent, Some("r1".to_owned())), (Kind::Server, None)]);
    }

    #[tokio::test(start_paused = true)]
    async fn sends_waiting_lines_to_every_watcher_before_a_replay_only_one_of_them_takes() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&of_a(), None);
        attached.clear();
        log.game_line(GAME_A, "new");
        log.watch(&of_a(), Some("r1"));

        let batches: Vec<_> = attached.batches().iter().map(|frame| (frame.replay_id.clone(), frame.reset, texts(std::slice::from_ref(frame)))).collect();

        assert_eq!(batches, [(None, false, vec!["new".to_owned()]), (Some("r1".to_owned()), true, vec!["new".to_owned()])]);
        advance(Duration::from_millis(250)).await;
        assert_eq!(attached.batches().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn sends_nothing_while_the_connection_would_not_carry_it() {
        let attached = attached_with(false);
        let log = &attached.log;

        log.watch(&of_a(), None);
        log.game_line(GAME_A, "lost to the socket, kept in the ring");
        advance(Duration::from_secs(1)).await;
        assert!(attached.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn sends_the_index_again_when_a_game_starts_printing_ends_or_is_let_go() {
        let attached = attached();
        let log = &attached.log;
        let ended = usize::try_from(ENDED_SERVER_LOGS).unwrap();

        for index in 0..ended {
            log.game_line(&game(index), &format!("game {index}"));
            log.game_ended(&game(index));
        }

        log.watch(&agent(), None);
        attached.clear();
        log.game_line(GAME_A, "first line");
        log.game_line(GAME_A, "second line");
        advance(Duration::from_millis(250)).await;
        assert_eq!(attached.indexes().len(), 1);
        assert_eq!(shape(attached.indexes()[0].sources.last().unwrap()), summary(of_a(), 2, false));

        log.game_ended(GAME_A);
        advance(Duration::from_millis(250)).await;

        let indexes = attached.indexes();
        let games = &indexes[1].sources[2..];
        let mut expected: Vec<_> = (1..ended).map(|index| server(&game(index))).collect();

        expected.push(of_a());
        assert_eq!(games.iter().map(|entry| entry.source.clone()).collect::<Vec<_>>(), expected);
        assert!(games.iter().all(|entry| entry.ended));

        advance(Duration::from_secs(1)).await;
        assert_eq!(attached.indexes().len(), 2);
    }

    /// The exit watch can report a game's last output after its end; once its output went, such a
    /// line opens no source that would never end.
    #[tokio::test(start_paused = true)]
    async fn drops_a_late_line_from_a_game_whose_output_already_went() {
        let attached = attached();
        let log = &attached.log;
        let ended = usize::try_from(ENDED_SERVER_LOGS).unwrap();

        for index in 0..=ended {
            log.game_line(&game(index), &format!("game {index}"));
            log.game_ended(&game(index));
        }

        log.game_line(&game(0), "late line");
        log.watch(&agent(), None);
        advance(Duration::from_millis(250)).await;

        let index = attached.indexes().last().unwrap().clone();

        assert!(index.sources.iter().all(|entry| entry.source != server(&game(0))));
        assert_eq!(index.sources.iter().filter(|entry| entry.ended).count(), ended);
    }

    /// The agent remembers as many games whose output went as it keeps ended ones.
    #[tokio::test(start_paused = true)]
    async fn drops_a_late_line_from_each_of_as_many_games_whose_output_went_as_it_keeps_ended() {
        let attached = attached();
        let log = &attached.log;
        let ended = usize::try_from(ENDED_SERVER_LOGS).unwrap();

        for index in 0..2 * ended {
            log.game_line(&game(index), &format!("game {index}"));
            log.game_ended(&game(index));
        }

        log.game_line(&game(0), "late line");
        log.watch(&agent(), None);

        let index = attached.indexes().last().unwrap().clone();

        assert!(index.sources.iter().all(|entry| entry.source != server(&game(0))));
    }

    #[tokio::test(start_paused = true)]
    async fn replays_a_source_whose_lease_lapsed_once_it_is_watched_again() {
        let attached = attached();
        let log = &attached.log;

        log.game_line(GAME_A, "kept");
        log.watch(&of_a(), None);
        advance(WATCH_LEASE).await;
        attached.clear();
        log.watch(&of_a(), None);

        let batches = attached.batches();

        assert_eq!(batches.len(), 1);
        assert!(batches[0].reset);
        assert_eq!(texts(&batches), ["kept"]);
    }

    #[tokio::test(start_paused = true)]
    async fn keeps_the_level_of_each_agent_line() {
        let attached = attached();
        let log = &attached.log;

        log.agent_line(&entry(1, Level::Info, "info"));
        log.agent_line(&entry(2, Level::Warn, "warn"));
        log.agent_line(&entry(3, Level::Error, "error"));
        log.watch(&agent(), None);

        let levels: Vec<_> = attached.batches()[0].lines.iter().map(|line| line.level).collect();

        assert_eq!(levels, [Some(AgentLogLevel::Info), Some(AgentLogLevel::Warn), Some(AgentLogLevel::Error)]);
    }

    #[tokio::test(start_paused = true)]
    async fn brings_the_index_up_to_date_once_a_refresh_period_after_lines_arrive_the_newest_line_too_and_only_while_watched() {
        let attached = attached();
        let log = &attached.log;

        log.game_line(GAME_A, "first line");
        log.watch(&agent(), None);
        assert_eq!(attached.indexes().len(), 1);

        advance(Duration::from_secs(5)).await;
        log.game_line(GAME_A, "second line");
        let last_at = (attached.clock)();

        advance(Duration::from_millis(250)).await;
        // Lines alone send no index until the refresh is due.
        assert_eq!(attached.indexes().len(), 1);

        advance(LOG_INDEX_REFRESH).await;
        assert_eq!(attached.indexes().len(), 2);
        assert_eq!(
            attached.indexes()[1].sources.last().unwrap(),
            &LogSourceSummary { source: of_a(), line_count: 2, last_line_at: Some(last_at), ended: false }
        );

        // Nothing new: no refresh.
        advance(LOG_INDEX_REFRESH * 2).await;
        assert_eq!(attached.indexes().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn refreshes_the_index_a_refresh_period_after_the_first_new_line_however_often_lines_keep_coming() {
        let attached = attached();
        let log = &attached.log;

        log.game_line(GAME_A, "first line");
        log.watch(&agent(), None);
        assert_eq!(attached.indexes().len(), 1);

        // Lines at 5, 12, 19, 26, 33 and 40 seconds; the refresh the first of them started is due at 35.
        advance(Duration::from_secs(5)).await;

        for index in 0..6 {
            log.game_line(GAME_A, &format!("line {index}"));
            advance(Duration::from_secs(7)).await;
        }

        assert_eq!(attached.indexes().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn waits_a_whole_flush_period_for_a_line_that_comes_after_a_replay_sent_the_waiting_ones() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&agent(), None);
        log.agent_line(&entry(1, Level::Info, "one"));
        advance(Duration::from_millis(100)).await;
        log.watch(&agent(), Some("r1"));
        attached.clear();

        advance(Duration::from_millis(50)).await;
        log.agent_line(&entry(2, Level::Info, "two"));

        // The flush "one" armed was due 250 ms in; the replay took its lines, so "two" waits its own 250 ms.
        advance(Duration::from_millis(110)).await;
        assert_eq!(texts(&attached.batches()), Vec::<String>::new());

        advance(Duration::from_millis(150)).await;
        assert_eq!(texts(&attached.batches()), ["two"]);
    }

    #[tokio::test(start_paused = true)]
    async fn keeps_the_budget_for_the_connection_while_none_is_attached() {
        let clock = tokio_clock();
        let log = NodeLog::new(Arc::clone(&clock), tokio_util::sync::CancellationToken::new());

        log.agent_line(&entry(1, Level::Info, "started"));

        for replay in 0..200 {
            log.watch(&agent(), Some(&format!("r{replay}")));
        }

        let recorder = Arc::new(Recorder { connected: true, sent: Mutex::default() });

        log.attach(Arc::clone(&recorder) as Arc<dyn Transport>);
        log.watch(&agent(), Some("attached"));

        let batches: Vec<AgentLogBatch> = recorder
            .sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|frame| match frame {
                AgentToControllerMessage::LogBatch(batch) => Some(batch.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(texts(&batches), ["started"]);
        assert_eq!(batches[0].replay_id.as_deref(), Some("attached"));
    }

    #[tokio::test(start_paused = true)]
    async fn drops_a_due_refresh_once_an_index_goes_out_for_another_reason() {
        let attached = attached();
        let log = &attached.log;

        log.game_line(GAME_A, "first line");
        log.watch(&agent(), None);
        log.game_line(GAME_A, "second line");
        log.game_ended(GAME_A);
        advance(Duration::from_millis(250)).await;
        let sent = attached.indexes().len();

        advance(LOG_INDEX_REFRESH).await;

        assert_eq!(attached.indexes().len(), sent);
    }

    #[tokio::test(start_paused = true)]
    async fn counts_a_game_that_ended_before_its_first_line_among_the_ended_ones() {
        let attached = attached();
        let log = &attached.log;

        log.game_ended(GAME_A);
        log.game_line(GAME_A, "late");

        for index in 0..usize::try_from(ENDED_SERVER_LOGS).unwrap() {
            log.game_ended(&game(index));
        }

        log.watch(&agent(), None);
        assert_eq!(attached.indexes()[0].sources.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn counts_a_game_that_ended_twice_once() {
        let attached = attached();
        let log = &attached.log;

        log.game_line(GAME_A, "kept");
        log.game_ended(GAME_A);
        log.game_ended(GAME_A);

        for index in 0..usize::try_from(ENDED_SERVER_LOGS).unwrap() - 1 {
            log.game_ended(&game(index));
        }

        log.watch(&agent(), None);
        assert_eq!(shape(attached.indexes()[0].sources.last().unwrap()), summary(of_a(), 1, true));
    }

    #[tokio::test(start_paused = true)]
    async fn drops_the_waiting_lines_of_a_source_that_is_let_go() {
        let attached = attached();
        let log = &attached.log;

        log.watch(&of_a(), None);
        log.watch(&update(UPDATE_ID), None);
        attached.clear();
        log.game_line(GAME_A, "late");
        log.update_line("late");
        log.game_ended(GAME_A);

        for index in 0..usize::try_from(ENDED_SERVER_LOGS).unwrap() {
            log.game_ended(&game(index));
        }

        for index in 0..=usize::try_from(ENDED_UPDATE_LOGS).unwrap() {
            log.update_started(&game(index));
        }

        advance(Duration::from_millis(250)).await;
        assert!(texts(&attached.batches()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn gives_each_cs2_update_a_source_of_its_own_and_keeps_only_the_last_few_that_ended() {
        let attached = attached();
        let log = &attached.log;
        let ended = usize::try_from(ENDED_UPDATE_LOGS).unwrap();

        log.update_line("first update");

        for index in 1..=ended + 1 {
            log.update_started(&game(index));
            log.update_line(&format!("update {index}"));
        }

        log.watch(&agent(), None);
        // The first one ended as the next started, and has been pushed out by the later ones; the newest still runs.
        let indexes = attached.indexes();
        let updates: Vec<_> = indexes.last().unwrap().sources.iter().filter(|entry| kind(&entry.source) == Kind::Cs2Update).map(shape).collect();
        let mut expected: Vec<_> = (0..ended).map(|index| summary(update(&game(index + 1)), 1, true)).collect();

        expected.push(summary(update(&game(ended + 1)), 1, false));
        assert_eq!(updates, expected);

        log.watch(&update(&game(ended + 1)), None);
        log.watch(&update(UPDATE_ID), None);
        let replays: Vec<_> =
            attached.batches().iter().filter(|frame| kind(&frame.source) == Kind::Cs2Update).map(|frame| texts(std::slice::from_ref(frame))).collect();
        assert_eq!(replays, [vec![format!("update {}", ended + 1)], vec![]]);

        log.update_ended();
        assert_eq!(attached.indexes().last().unwrap().sources.iter().filter(|entry| entry.ended).count(), ended);
    }

    #[tokio::test(start_paused = true)]
    async fn sends_the_index_again_when_an_update_ends_and_when_one_starts_before_its_first_line() {
        let attached = attached();
        let log = &attached.log;
        let next = "00000000-0000-4000-8000-00000000b0b0";

        log.watch(&agent(), None);
        attached.clear();
        log.update_ended();
        advance(Duration::from_millis(250)).await;
        assert!(attached.indexes().last().unwrap().sources.contains(&LogSourceSummary {
            source: update(UPDATE_ID),
            line_count: 0,
            last_line_at: None,
            ended: true
        }));

        attached.clear();
        log.update_started(next);
        advance(Duration::from_millis(250)).await;
        assert!(attached.indexes().last().unwrap().sources.contains(&LogSourceSummary {
            source: update(next),
            line_count: 0,
            last_line_at: None,
            ended: false
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn adds_a_line_that_comes_after_its_update_ended_to_that_update_not_a_new_source() {
        let attached = attached();
        let log = &attached.log;

        log.update_line("during");
        log.update_ended();
        log.update_line("after the exit was reported");
        log.watch(&agent(), None);

        let indexes = attached.indexes();
        let updates: Vec<_> = indexes.last().unwrap().sources.iter().filter(|entry| kind(&entry.source) == Kind::Cs2Update).map(shape).collect();

        assert_eq!(updates, [summary(update(UPDATE_ID), 2, true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn is_bounded_per_source_and_cuts_long_lines() {
        let attached = attached();
        let log = &attached.log;
        let longest = usize::try_from(LOG_LINE_MAX_CHARS).unwrap();

        for index in 0..usize::try_from(SERVER_LOG_LINES).unwrap() + 5 {
            log.game_line(GAME_A, &format!("line {index}"));
            log.agent_line(&entry(i64::try_from(index).unwrap(), Level::Info, &format!("agent {index}")));
        }

        log.update_line(&"x".repeat(longest + 10));
        log.agent_line(&entry(0, Level::Error, &"x".repeat(longest + 10)));
        log.watch(&agent(), None);
        log.watch(&of_a(), None);
        log.watch(&update(UPDATE_ID), None);

        let of = |wanted: Kind| -> Vec<LogLine> {
            attached.batches().into_iter().filter(|frame| kind(&frame.source) == wanted).flat_map(|frame| frame.lines).collect()
        };

        assert_eq!(of(Kind::Agent).len(), usize::try_from(AGENT_LOG_LINES).unwrap());
        assert_eq!(of(Kind::Server).len(), usize::try_from(SERVER_LOG_LINES).unwrap());
        assert_eq!(of(Kind::Server)[0].line, "line 5");
        assert_eq!(of(Kind::Cs2Update)[0].line.len(), longest);
        assert_eq!(of(Kind::Agent).last().unwrap(), &LogLine { at: 0, level: Some(AgentLogLevel::Error), line: "x".repeat(longest) });
    }

    #[tokio::test(start_paused = true)]
    async fn splits_a_replay_into_frames_under_the_limit_with_the_reset_on_the_first_alone() {
        let attached = attached();
        let log = &attached.log;

        for index in 0..SERVER_LOG_LINES {
            log.game_line(GAME_A, &format!("{index} {}", "ü".repeat(400)));
        }

        log.watch(&of_a(), None);

        let frames = attached.batches();

        assert!(frames.len() > 1);
        assert_eq!(frames.iter().map(|frame| frame.reset).collect::<Vec<_>>(), [vec![true], vec![false; frames.len() - 1]].concat());

        for frame in &frames {
            assert!(json_len(frame) <= usize::try_from(LOG_BATCH_MAX_BYTES).unwrap());
        }

        assert_eq!(texts(&frames).len(), usize::try_from(SERVER_LOG_LINES).unwrap());
    }

    /// The `replay_id` pads the envelope, so the lines fill the first frame to the limit exactly.
    #[tokio::test(start_paused = true)]
    async fn fills_a_frame_up_to_the_limit_with_lines_that_reach_it_exactly() {
        let attached = attached();
        let log = &attached.log;
        let max = usize::try_from(LOG_BATCH_MAX_BYTES).unwrap();
        let at = (attached.clock)();
        let size = 1000;
        let pad = "x".repeat(size - line_bytes(at, "0000"));
        let envelope = json_len(&AgentLogBatch { source: of_a(), replay_id: Some(String::new()), reset: true, lines: Vec::new() });
        let fits = (max - envelope - 1) / size;
        let replay_id = "r".repeat(max - envelope - fits * size);

        assert_eq!(line_bytes(at, &format!("0000{pad}")), size);

        for index in 0..2 * fits {
            log.game_line(GAME_A, &format!("{index:04}{pad}"));
        }

        log.watch(&of_a(), Some(&replay_id));

        let frames = attached.batches();

        assert_eq!(frames[0].lines.len(), fits);
        assert_eq!(texts(&frames).len(), 2 * fits);
    }

    /// Lines just over a 22nd of a frame leave each frame almost a whole line short of the limit,
    /// the least a frame can carry. The longest `replay_id` widens every frame's envelope too.
    #[tokio::test(start_paused = true)]
    async fn keeps_the_newest_line_of_a_replay_trimmed_to_its_frames_when_every_frame_carries_the_least_it_can() {
        let attached = attached();
        let log = &attached.log;
        let max = usize::try_from(LOG_BATCH_MAX_BYTES).unwrap();
        let at = (attached.clock)();
        let replay_id = "r".repeat(64);
        let envelope = json_len(&AgentLogBatch { source: of_a(), replay_id: Some(replay_id.clone()), reset: true, lines: Vec::new() });
        let size = (max - envelope) / 22 + 1;
        // A control character takes six bytes as JSON, the most a character can take.
        let escaped = "\u{1}".repeat((size - line_bytes(at, "0000")) / 6);
        let pad = "x".repeat(size - line_bytes(at, &format!("0000{escaped}")));
        let line = |index: usize| format!("{index:04}{escaped}{pad}");

        assert_eq!(line_bytes(at, &line(0)), size);

        for index in 0..1500 {
            log.game_line(GAME_A, &line(index));
        }

        // 48 of the budget's 96 frames go; the lease's index takes one more, and 47 are left.
        for replay in 0..24 {
            log.watch(&agent(), Some(&format!("r{replay}")));
        }

        assert_eq!(attached.sent().len(), 48);
        attached.clear();
        log.watch(&of_a(), Some(&replay_id));

        let lines = texts(&attached.batches());

        assert!(lines.len() < 1500);
        assert_eq!(lines.last().unwrap(), &line(1499));
    }

    #[tokio::test(start_paused = true)]
    async fn trims_a_replay_to_what_its_budget_has_left_newest_lines_kept() {
        let attached = attached();
        let log = &attached.log;
        let games: Vec<String> = (0..8).map(game).collect();
        let room = usize::try_from(agent_budgets::log_bytes::base::BURST).unwrap();

        for allocation_id in &games {
            for index in 0..SERVER_LOG_LINES {
                log.game_line(allocation_id, &format!("{index} {}", "x".repeat(usize::try_from(LOG_LINE_MAX_CHARS).unwrap() - 6)));
            }
        }

        for allocation_id in &games {
            log.watch(&server(allocation_id), None);
        }

        let batches = attached.batches();
        let bytes: usize = batches.iter().map(json_len).sum();
        let replayed: Vec<Vec<String>> = games
            .iter()
            .map(|allocation_id| texts(&batches.iter().filter(|frame| frame.source == server(allocation_id)).cloned().collect::<Vec<_>>()))
            .collect();
        let trimmed = replayed.iter().find(|lines| !lines.is_empty() && lines.len() < usize::try_from(SERVER_LOG_LINES).unwrap());

        assert!(bytes <= room);
        assert_eq!(replayed[0].len(), usize::try_from(SERVER_LOG_LINES).unwrap());
        assert!(trimmed.unwrap().last().unwrap().starts_with(&format!("{} ", SERVER_LOG_LINES - 1)));
        assert!(replayed.last().unwrap().is_empty());

        // A quarter second refills room for a short line, which goes out with the next flush.
        log.game_line(&games[0], "after the refill");
        advance(Duration::from_millis(250)).await;
        assert_eq!(texts(&attached.batches()).last().unwrap(), "after the refill");
    }

    #[tokio::test(start_paused = true)]
    async fn sends_every_newest_line_a_replay_trimmed_to_the_byte_budget_sets_out_to_send() {
        let attached = attached();
        let log = &attached.log;
        let games: Vec<String> = (0..8).map(game).collect();
        let short = game(9);

        // Short lines, so the room each line takes in the estimate decides where the replay starts.
        for index in 0..SERVER_LOG_LINES {
            log.game_line(&short, &format!("{index:04} {}", "s".repeat(140)));
        }

        for allocation_id in &games {
            for index in 0..SERVER_LOG_LINES {
                log.game_line(allocation_id, &format!("{index} {}", "x".repeat(usize::try_from(LOG_LINE_MAX_CHARS).unwrap() - 6)));
            }
        }

        for allocation_id in &games {
            log.watch(&server(allocation_id), None);
        }

        // About 270 KB refills: more than one frame holds, less than the short source keeps.
        advance(Duration::from_millis(1400)).await;
        attached.clear();
        log.watch(&server(&short), None);

        let replayed: Vec<AgentLogBatch> = attached.batches().into_iter().filter(|frame| frame.source == server(&short)).collect();
        let lines = texts(&replayed);

        assert!(replayed.len() > 1, "{} frames", replayed.len());
        assert!(lines.len() < usize::try_from(SERVER_LOG_LINES).unwrap());
        assert!(lines.last().unwrap().starts_with(&format!("{} ", SERVER_LOG_LINES - 1)), "{:?}", lines.last());
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_loop_when_sending_a_frame_logs_a_line() {
        struct Failing {
            log: Mutex<Option<Logger>>,
            sent: AtomicUsize,
        }

        impl Transport for Failing {
            fn connected(&self) -> bool {
                true
            }

            fn send(&self, _: AgentToControllerMessage) {
                self.sent.fetch_add(1, Ordering::SeqCst);

                let log = self.log.lock().unwrap().clone();

                log.unwrap().warn("send failed: socket is not open");
            }
        }

        let node_log = NodeLog::new(tokio_clock(), tokio_util::sync::CancellationToken::new());
        let sink = node_log.clone();
        let log = Logger::new("agent", Some(Arc::new(move |line: Entry| sink.agent_line(&line))));
        let transport = Arc::new(Failing { log: Mutex::new(Some(log.clone())), sent: AtomicUsize::new(0) });

        node_log.attach(Arc::clone(&transport) as Arc<dyn Transport>);
        node_log.watch(&agent(), None);
        log.info("connected");
        advance(Duration::from_millis(250)).await;

        // The failure's own line rides out with the next batch rather than recursing.
        let settled = transport.sent.load(Ordering::SeqCst);

        advance(Duration::from_millis(250)).await;
        assert_eq!(transport.sent.load(Ordering::SeqCst), settled + 1);
        advance(Duration::from_millis(250)).await;
        assert_eq!(transport.sent.load(Ordering::SeqCst), settled + 2);
    }
}

mod logger_with_a_sink {
    use super::*;

    #[test]
    fn mirrors_every_level_with_the_child_prefix_on_the_message() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let log = Logger::new("agent", Some(Arc::new(move |line: Entry| seen.lock().unwrap().push((line.level, line.message)))));

        log.info("connected");
        log.child("ws").warn("reconnecting in 4000ms");
        log.child("overlay").error("extraction failed");

        assert_eq!(
            *lines.lock().unwrap(),
            [
                (Level::Info, "[agent] connected".to_owned()),
                (Level::Warn, "[agent:ws] reconnecting in 4000ms".to_owned()),
                (Level::Error, "[agent:overlay] extraction failed".to_owned()),
            ]
        );
    }
}

mod node_log_budget {
    use super::*;

    /// The agent's share of the controller's log frame budget, which it holds itself to.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss, reason = "a small whole number")]
    fn share() -> usize {
        (agent_budgets::log_frames::base::BURST as f64 * 0.75).floor() as usize
    }

    #[tokio::test(start_paused = true)]
    async fn sends_no_more_frames_than_its_share_of_the_budget_however_often_watchers_ask() {
        let attached = attached();
        let log = &attached.log;

        log.agent_line(&entry(1, Level::Info, "started"));

        for replay in 0..200 {
            log.watch(&agent(), Some(&format!("r{replay}")));
        }

        assert_eq!(attached.sent().len(), share());
    }

    #[tokio::test(start_paused = true)]
    async fn trims_a_replay_to_the_frames_its_budget_has_left_newest_lines_kept() {
        let attached = attached();
        let log = &attached.log;

        for index in 0..SERVER_LOG_LINES {
            log.game_line(GAME_A, &format!("{index} {}", "ü".repeat(400)));
        }

        let mut replay = 0;

        while attached.sent().len() < share() - 3 {
            log.watch(&agent(), Some(&format!("r{replay}")));
            replay += 1;
        }

        log.watch(&of_a(), None);

        let replayed: Vec<_> = attached.batches().into_iter().filter(|frame| kind(&frame.source) == Kind::Server).collect();

        assert!(!replayed.is_empty());
        assert!(replayed[0].reset);
        assert!(texts(&replayed).last().unwrap().starts_with(&format!("{} ", SERVER_LOG_LINES - 1)));
    }

    #[tokio::test(start_paused = true)]
    async fn refills_its_share_of_the_budget_each_second() {
        let attached = attached();
        let log = &attached.log;

        log.agent_line(&entry(1, Level::Info, "started"));

        for replay in 0..200 {
            log.watch(&agent(), Some(&format!("r{replay}")));
        }

        let drained = attached.sent().len();

        advance(Duration::from_secs(1)).await;

        for replay in 0..200 {
            log.watch(&agent(), Some(&format!("s{replay}")));
        }

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss, reason = "a small whole number")]
        let refill = (agent_budgets::log_frames::base::PER_SECOND as f64 * 0.75).floor() as usize;

        assert_eq!(attached.sent().len() - drained, refill);
    }

    #[tokio::test(start_paused = true)]
    async fn sends_an_index_the_budget_held_back_once_the_budget_has_room_again() {
        let attached = attached();
        let log = &attached.log;

        log.agent_line(&entry(1, Level::Info, "started"));

        for replay in 0..200 {
            log.watch(&agent(), Some(&format!("r{replay}")));
        }

        let drained = attached.sent().len();
        let indexed = attached.indexes().len();

        // A lease that starts sends the index at once; there is no room, and nothing else would retry it.
        log.watch(&update(UPDATE_ID), None);
        assert_eq!(attached.sent().len(), drained);

        advance(Duration::from_secs(1)).await;
        assert_eq!(attached.indexes().len(), indexed + 1);
    }
}
