//! The event spool: each server's events until the controller has them, the ends the controller has not had
//! and the allocations launched, kept in a versioned SQLite file that outlives agent updates.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::io::{Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use ankor_agent::event_spool::{
    EventSpool, SPOOL_LAYOUT_FILES, SPOOL_SCHEMA_VERSION, SpoolAck, SpoolAppend, SpoolBacklog, SpoolFrame, SpoolSchema, SpoolStream, SpooledFrame,
};
use ankor_agent::logger::{Entry, Level, Logger};
use ankor_agent::spool_client::{SpoolClient, SpoolPort as _, open_spool};
use ankor_agent::testing::fixtures::MATCH_ID;
use ankor_protocol::constants::{EVENT_SPOOL_RETENTION, REMEMBERED_ALLOCATIONS};
use ankor_protocol::types::{NodeUnavailable, NodeUnavailableCode, ServerExit, ServerExitReason};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

const ALLOCATION: &str = "00000000-0000-4000-8000-000000000001";
const OTHER: &str = "00000000-0000-4000-8000-000000000002";
const OTHER_MATCH_ID: &str = "7c2d3e4f-1a2b-4c5d-8e9f-0a1b2c3d4e5f";
#[allow(clippy::cast_possible_truncation, reason = "a week in milliseconds")]
const RETENTION: i64 = EVENT_SPOOL_RETENTION.as_millis() as i64;

fn frame_for(seq: i64, match_id: &str, message: &str, allocation: &str) -> SpooledFrame {
    let raw = json!({ "allocation_id": allocation, "seq": seq, "event": { "type": "log", "match_id": match_id, "tick": 640, "timestamp": 1000, "level": "info", "message": message } });

    SpooledFrame { seq, raw: raw.to_string() }
}

fn frame(seq: i64) -> SpooledFrame {
    frame_for(seq, MATCH_ID, &format!("event-{seq}"), ALLOCATION)
}

fn stream(allocation: &str, frames: Vec<SpooledFrame>) -> SpoolStream {
    SpoolStream { allocation_id: allocation.to_owned(), frames }
}

#[allow(clippy::unnecessary_wraps, reason = "compared with the spool's Result")]
fn ack(allocation: &str, to_seq: i64) -> SpoolAppend {
    Ok(Some(SpoolAck { allocation_id: allocation.to_owned(), to_seq }))
}

fn exit(slot: &str, allocation: &str, reason: ServerExitReason, exit_code: Option<i64>) -> ServerExit {
    ServerExit { slot_id: slot.to_owned(), allocation_id: allocation.to_owned(), reason, exit_code, detail: None }
}

struct File {
    _dir: TempDir,
    path: PathBuf,
}

fn filename() -> File {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("streams.sqlite");

    File { _dir: dir, path }
}

type Clock = Arc<AtomicI64>;

fn clock(at: i64) -> Clock {
    Arc::new(AtomicI64::new(at))
}

fn open_at(path: &Path, now: &Clock) -> EventSpool {
    let reading = Arc::clone(now);
    let mut spool = EventSpool::open_with_clock(path, &Logger::null(), Box::new(move || reading.load(Ordering::SeqCst))).unwrap();

    spool.append(&[stream(ALLOCATION, vec![])]).unwrap();
    spool
}

fn open(path: &Path) -> EventSpool {
    open_at(path, &clock(1_000))
}

/// The layout files compiled in, then `extra`, each a name and its SQL.
fn layout<'a>(extra: impl IntoIterator<Item = (&'a str, &'a str)>) -> SpoolSchema {
    let files = SPOOL_LAYOUT_FILES.iter().copied().chain(extra);

    SpoolSchema::from_files(files.map(|(name, sql)| (name.to_owned(), sql.to_owned()))).unwrap()
}

/// Only the version-1 layout, which every file starts from.
fn base() -> SpoolSchema {
    SpoolSchema::from_files(SPOOL_LAYOUT_FILES.iter().take(1).map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned()))).unwrap()
}

/// The layout steps in a test fixture directory, read from its files.
fn fixture_steps(fixture: &str) -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture);

    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();

            (path.file_name().unwrap().to_str().unwrap().to_owned(), std::fs::read_to_string(&path).unwrap())
        })
        .collect()
}

/// The layout files compiled in, then the steps of a fixture directory.
fn with_fixture(fixture: &str) -> SpoolSchema {
    let compiled = SPOOL_LAYOUT_FILES.iter().map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned()));

    SpoolSchema::from_files(compiled.chain(fixture_steps(fixture))).unwrap()
}

fn open_schema(path: &Path, log: &Logger, schema: &SpoolSchema) -> Result<EventSpool, String> {
    EventSpool::open_with(path, log, Box::new(|| 1_000), schema).map_err(|error| error.to_string())
}

/// A file as an agent on schema version 1 left it: the baseline, before any migration.
fn version_one() -> File {
    let file = filename();

    drop(open_schema(&file.path, &Logger::null(), &base()).unwrap());
    file
}

type Lines = Arc<Mutex<Vec<(Level, String)>>>;

fn recording() -> (Logger, Lines) {
    let lines: Lines = Arc::default();
    let seen = Arc::clone(&lines);

    (Logger::new("agent:events", Some(Arc::new(move |entry: Entry| seen.lock().unwrap().push((entry.level, entry.message))))), lines)
}

fn messages(lines: &Mutex<Vec<(Level, String)>>) -> Vec<String> {
    lines.lock().unwrap().iter().map(|(_, message)| message.clone()).collect()
}

/// Servers with a stream row, in the order they were first seen.
fn spool_streams(path: &Path) -> Vec<String> {
    let db = Connection::open(path).unwrap();
    let mut statement = db.prepare("SELECT allocation FROM server_streams ORDER BY rowid").unwrap();

    statement.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

fn json_of(frame: &SpoolFrame) -> Value {
    serde_json::from_str(&frame.frame).unwrap()
}

fn seqs(frames: &[SpoolFrame]) -> Vec<i64> {
    frames.iter().map(|frame| frame.seq).collect()
}

#[test]
fn recovers_pending_frames_across_a_restart_and_drops_them_once_delivered() {
    let file = filename();
    let mut spool = open(&file.path);

    assert_eq!(spool.append(&[stream(ALLOCATION, vec![frame(0), frame_for(1, OTHER_MATCH_ID, "event-1", ALLOCATION)])]).unwrap(), [ack(ALLOCATION, 1)]);
    drop(spool);

    let mut recovered = open(&file.path);

    assert_eq!(recovered.position(ALLOCATION).unwrap(), Some(SpoolAck { allocation_id: ALLOCATION.to_owned(), to_seq: 1 }));
    assert_eq!(
        recovered.pending(ALLOCATION).unwrap().iter().map(|row| json_of(row)["event"]["match_id"].clone()).collect::<Vec<_>>(),
        [MATCH_ID, OTHER_MATCH_ID]
    );
    recovered.acknowledge(ALLOCATION, 1).unwrap();
    assert_eq!(recovered.pending(ALLOCATION).unwrap(), []);
}

#[tokio::test]
async fn passes_the_lines_the_spool_logs_on_its_thread_to_the_agents_logger() {
    let file = filename();
    let (log, lines) = recording();
    let client = SpoolClient::start(file.path.clone(), log);

    assert_eq!(client.version().await.unwrap(), SPOOL_SCHEMA_VERSION);
    let _ = client.close().await;
    // A new file is created at the current version, so nothing migrates.
    assert_eq!(messages(&lines), Vec::<String>::new());
}

#[test]
fn creates_a_new_file_at_the_current_schema_version_and_keeps_it_there_on_reopening() {
    let file = filename();
    let mut first = open(&file.path);

    first.append(&[stream(ALLOCATION, vec![frame(0)])]).unwrap();
    drop(first);

    let again = open(&file.path);

    assert_eq!(again.version().unwrap(), SPOOL_SCHEMA_VERSION);
    assert_eq!(std::fs::metadata(&file.path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(seqs(&again.pending(ALLOCATION).unwrap()), [0]);
}

#[test]
fn compiles_in_every_layout_file_and_writes_the_newest() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut on_disk = Vec::new();

    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();

        if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("sql")) {
            on_disk.push(path.file_name().unwrap().to_str().unwrap().to_owned());
        }
    }

    on_disk.sort();

    let listed: Vec<&str> = SPOOL_LAYOUT_FILES.iter().map(|(name, _)| *name).collect();

    assert_eq!(listed, on_disk, "every file in migrations/ needs its line in SPOOL_LAYOUT_FILES, in version order");
    assert_eq!(SpoolSchema::current().unwrap().version, SPOOL_SCHEMA_VERSION, "SPOOL_SCHEMA_VERSION must be the newest file's version");
}

#[test]
fn migrates_a_file_through_the_steps_its_files_name_committing_each_and_creates_new_files_the_same_way() {
    let steps = with_fixture("spool_migrations");
    let file = version_one();
    let (log, lines) = recording();
    let migrated = open_schema(&file.path, &log, &steps).unwrap();

    assert_eq!(migrated.version().unwrap(), 999);
    assert_eq!(messages(&lines), ["[agent:events] event spool migrated from schema 1 to 998", "[agent:events] event spool migrated from schema 998 to 999"]);
    assert!(lines.lock().unwrap().iter().all(|(level, _)| *level == Level::Info));
    Connection::open(&file.path).unwrap().execute("INSERT INTO notes (text, at) VALUES ('x', 1)", []).unwrap();

    let fresh = filename();
    let created = open_schema(&fresh.path, &log, &steps).unwrap();

    assert_eq!(created.version().unwrap(), 999);
    assert_eq!(messages(&lines).len(), 4);
    Connection::open(&fresh.path).unwrap().execute("INSERT INTO notes (text, at) VALUES ('x', 1)", []).unwrap();

    // Reopened at its version, the file runs no step again.
    drop(open_schema(&file.path, &log, &steps).unwrap());
    assert_eq!(messages(&lines).len(), 4);
}

#[test]
fn refuses_layout_files_it_cannot_order() {
    let error = |files: &[(&str, &str)]| SpoolSchema::from_files(files.iter().map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned()))).err().unwrap();

    assert_eq!(error(&[("event_spool_0001.sql", ""), ("spool_0002.sql", "")]), "spool_0002.sql is not named event_spool_NNNN.sql");
    assert_eq!(error(&[("event_spool_0001.sql", ""), ("event_spool_2.sql", "")]), "event_spool_2.sql is not named event_spool_NNNN.sql");
    assert_eq!(error(&[("event_spool_0001.sql", ""), ("event_spool_0002.sql", ""), ("event_spool_0002.sql", "")]), "two files reach version 2");
    assert_eq!(error(&[("event_spool_0002.sql", "")]), "no event_spool_0001.sql, the layout every file starts from");
}

mod allocations_launched {
    use super::*;

    fn nth(index: u64) -> String {
        format!("00000000-0000-4000-8000-{index:012}")
    }

    #[test]
    fn remembers_one_across_a_restart() {
        let file = filename();
        let mut spool = open(&file.path);

        assert!(!spool.allocation_used(ALLOCATION).unwrap());
        spool.record_allocation(ALLOCATION).unwrap();
        drop(spool);
        assert!(open(&file.path).allocation_used(ALLOCATION).unwrap());
    }

    #[test]
    fn keeps_the_newest_forgetting_the_oldest() {
        let file = filename();
        let now = clock(1_000);
        let mut spool = open_at(&file.path, &now);

        for index in 0..=REMEMBERED_ALLOCATIONS {
            spool.record_allocation(&nth(index)).unwrap();
            now.fetch_add(1, Ordering::SeqCst);
        }

        assert!(!spool.allocation_used(&nth(0)).unwrap());
        assert!(spool.allocation_used(&nth(1)).unwrap());
        assert!(spool.allocation_used(&nth(REMEMBERED_ALLOCATIONS)).unwrap());
    }

    #[test]
    fn keeps_the_newest_by_the_order_recorded_when_they_share_a_time() {
        let file = filename();
        let mut spool = open(&file.path);

        for index in 0..=REMEMBERED_ALLOCATIONS {
            spool.record_allocation(&nth(index)).unwrap();
        }

        assert!(!spool.allocation_used(&nth(0)).unwrap());
        assert!(spool.allocation_used(&nth(1)).unwrap());
    }

    #[test]
    fn changes_nothing_when_one_is_recorded_again_so_it_stays_the_oldest() {
        let file = filename();
        let now = clock(1_000);
        let mut spool = open_at(&file.path, &now);

        for index in 0..REMEMBERED_ALLOCATIONS {
            spool.record_allocation(&nth(index)).unwrap();
            now.fetch_add(1, Ordering::SeqCst);
        }

        spool.record_allocation(&nth(0)).unwrap();
        assert!(spool.allocation_used(&nth(0)).unwrap());
        now.fetch_add(1, Ordering::SeqCst);
        spool.record_allocation(&nth(REMEMBERED_ALLOCATIONS)).unwrap();
        assert!(!spool.allocation_used(&nth(0)).unwrap());
        assert!(spool.allocation_used(&nth(1)).unwrap());
    }
}

mod exits_the_controller_has_not_had {
    use super::*;

    #[test]
    fn keeps_them_across_a_restart_oldest_first_each_once_until_the_controller_has_them() {
        let file = filename();
        let now = clock(1_000);
        let mut spool = open_at(&file.path, &now);
        let clean = exit("a", ALLOCATION, ServerExitReason::CleanExit, Some(0));
        let timeout = exit("b", OTHER, ServerExitReason::StartTimeout, None);

        spool.record_exit(&clean).unwrap();
        now.fetch_add(1, Ordering::SeqCst);
        spool.record_exit(&timeout).unwrap();
        spool.record_exit(&exit("a", ALLOCATION, ServerExitReason::IdleEmpty, None)).unwrap();
        drop(spool);

        let mut again = open_at(&file.path, &now);

        assert_eq!(again.exits().unwrap(), [clean, timeout.clone()]);
        again.forget_exits(&[ALLOCATION.to_owned(), "00000000-0000-4000-8000-00000000dead".to_owned()]).unwrap();
        assert_eq!(again.exits().unwrap(), [timeout]);
    }

    /// A reason a newer agent wrote before a rollback hides only its own row.
    #[test]
    fn skips_a_row_with_a_reason_this_agent_does_not_know_and_keeps_the_others() {
        let file = filename();
        let now = clock(1_000);
        let mut spool = open_at(&file.path, &now);
        let kept = exit("b", OTHER, ServerExitReason::StartTimeout, None);

        spool.record_exit(&exit("a", ALLOCATION, ServerExitReason::CleanExit, Some(0))).unwrap();
        spool.record_exit(&kept).unwrap();
        drop(spool);
        Connection::open(&file.path).unwrap().execute("UPDATE exits SET reason = 'from_a_newer_agent' WHERE allocation = ?", [ALLOCATION]).unwrap();

        assert_eq!(open_at(&file.path, &now).exits().unwrap(), [kept]);
    }

    #[test]
    fn drops_one_that_has_waited_a_week_with_the_expired_streams_and_keeps_a_younger_one() {
        let file = filename();
        let now = clock(1_000);
        let mut spool = open_at(&file.path, &now);

        spool.record_exit(&exit("a", ALLOCATION, ServerExitReason::CleanExit, Some(0))).unwrap();
        now.fetch_add(RETENTION, Ordering::SeqCst);
        spool.record_exit(&exit("b", OTHER, ServerExitReason::CleanExit, Some(0))).unwrap();
        now.fetch_add(1, Ordering::SeqCst);
        spool.prune(&[]).unwrap();
        assert_eq!(spool.exits().unwrap().iter().map(|exit| exit.allocation_id.as_str()).collect::<Vec<_>>(), [OTHER]);
    }
}

#[test]
fn leaves_a_file_at_the_last_version_that_committed_when_a_migration_fails_and_says_how_to_recover() {
    let file = version_one();
    let failing = layout([("event_spool_0002.sql", "CREATE TABLE notes (text TEXT)"), ("event_spool_0003.sql", "ALTER TABLE missing ADD COLUMN at INTEGER")]);
    let error = open_schema(&file.path, &Logger::null(), &failing).err().unwrap();
    let pattern =
        regex::Regex::new(r"migration from schema 2 to 3 failed, leaving it at 2: .*missing.*\. Update the agent, or delete .* not yet delivered").unwrap();

    assert!(pattern.is_match(&error), "{error}");

    let version: u32 = Connection::open(&file.path).unwrap().query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();

    assert_eq!(version, 2);
}

#[test]
fn leaves_a_file_at_its_version_when_a_step_read_from_a_file_is_not_valid_sql() {
    let file = version_one();
    let error = open_schema(&file.path, &Logger::null(), &with_fixture("spool_migrations_invalid")).err().unwrap();

    assert!(error.contains("migration from schema 1 to 2 failed, leaving it at 1"), "{error}");

    let version: u32 = Connection::open(&file.path).unwrap().query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();

    assert_eq!(version, 1);
}

#[test]
fn refuses_a_file_a_newer_agent_wrote_saying_how_to_recover_and_leaves_it_as_it_is() {
    let file = filename();
    let newer = SPOOL_SCHEMA_VERSION + 1;

    Connection::open(&file.path).unwrap().execute_batch(&format!("CREATE TABLE future (x INTEGER); PRAGMA user_version = {newer};")).unwrap();

    let error = EventSpool::open(&file.path, &Logger::null()).err().unwrap().to_string();
    let pattern = regex::Regex::new(&format!(
        r"schema mismatch: .* version {newer}, newer than this agent's {SPOOL_SCHEMA_VERSION}\. Update the agent, or delete .* deletes every event not yet delivered"
    ))
    .unwrap();

    assert!(pattern.is_match(&error), "{error}");

    let db = Connection::open(&file.path).unwrap();
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();

    assert_eq!(tables, ["future"]);
}

#[test]
fn skips_a_replay_of_what_the_server_already_sent_whatever_it_carries() {
    let file = filename();
    let mut spool = open(&file.path);

    spool.append(&[stream(ALLOCATION, vec![frame(0)])]).unwrap();
    assert_eq!(spool.append(&[stream(ALLOCATION, vec![frame_for(0, MATCH_ID, "changed", ALLOCATION), frame(1)])]).unwrap(), [ack(ALLOCATION, 1)]);
    assert_eq!(spool.pending(ALLOCATION).unwrap().iter().map(|row| json_of(row)["event"]["message"].clone()).collect::<Vec<_>>(), ["event-0", "event-1"]);
}

#[test]
fn stores_a_new_stream_from_its_first_event_without_calling_any_event_lost() {
    let file = filename();
    let (log, lines) = recording();
    let mut spool = EventSpool::open_with_clock(&file.path, &log, Box::new(|| 1_000)).unwrap();

    assert_eq!(spool.append(&[stream(ALLOCATION, vec![frame(0)])]).unwrap(), [ack(ALLOCATION, 0)]);
    assert!(!messages(&lines).iter().any(|message| message.contains("lost")), "{:?}", messages(&lines));
}

#[test]
fn stores_a_frame_as_received_leaving_its_event_for_the_controller_to_validate() {
    let file = filename();
    let mut spool = open(&file.path);
    let invalid = SpooledFrame { seq: 0, raw: json!({ "allocation_id": ALLOCATION, "seq": 0, "event": { "type": "log" } }).to_string() };

    spool.append(&[stream(ALLOCATION, vec![invalid.clone()])]).unwrap();
    assert_eq!(spool.pending(ALLOCATION).unwrap(), [SpoolFrame { allocation_id: ALLOCATION.to_owned(), seq: 0, frame: invalid.raw }]);
}

#[test]
fn rolls_back_a_noncontiguous_stream_whole_and_commits_the_others_beside_it() {
    let file = filename();
    let mut spool = open(&file.path);

    assert_eq!(
        spool.append(&[stream(ALLOCATION, vec![frame(0), frame(2)]), stream(OTHER, vec![frame_for(0, MATCH_ID, "other", OTHER)])]).unwrap(),
        [Err(format!("missing event {ALLOCATION}/1; received 2")), ack(OTHER, 0)]
    );
    assert_eq!(spool.pending(ALLOCATION).unwrap(), []);
    assert_eq!(spool.position(ALLOCATION).unwrap(), None);
    assert_eq!(seqs(&spool.pending(OTHER).unwrap()), [0]);
}

#[test]
fn keeps_different_servers_independent() {
    let file = filename();
    let mut spool = open(&file.path);

    assert_eq!(spool.append(&[stream(OTHER, vec![])]).unwrap(), [Ok(None)]);
    spool.append(&[stream(ALLOCATION, vec![frame(0)])]).unwrap();
    spool.append(&[stream(OTHER, vec![frame_for(0, MATCH_ID, "other", OTHER)])]).unwrap();
    spool.acknowledge(ALLOCATION, 0).unwrap();
    assert_eq!(spool.pending(OTHER).unwrap().len(), 1);
}

#[test]
fn lists_the_servers_with_undelivered_events_the_one_waiting_longest_first() {
    let file = filename();
    let mut spool = open(&file.path);

    spool.append(&[stream(OTHER, vec![])]).unwrap();
    spool.append(&[stream(OTHER, vec![frame_for(0, MATCH_ID, "other", OTHER)])]).unwrap();
    spool.append(&[stream(ALLOCATION, vec![frame(0), frame(1)])]).unwrap();
    assert_eq!(
        spool.backlog().unwrap(),
        [SpoolBacklog { allocation_id: OTHER.to_owned(), pending: 1 }, SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 2 }]
    );

    spool.acknowledge(OTHER, 0).unwrap();
    assert_eq!(spool.backlog().unwrap(), [SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 2 }]);
}

#[test]
fn bounds_batches_and_refuses_acknowledgements_beyond_durable_receipt() {
    let file = filename();
    let mut spool = open(&file.path);

    spool.append(&[stream(ALLOCATION, (0..129).map(frame).collect())]).unwrap();
    assert_eq!(spool.pending(ALLOCATION).unwrap().len(), 128);
    assert!(spool.acknowledge(ALLOCATION, 129).err().unwrap().to_string().contains("missing"));
    spool.acknowledge(ALLOCATION, 127).unwrap();
    assert_eq!(seqs(&spool.pending(ALLOCATION).unwrap()), [128]);
}

#[test]
fn fills_a_batch_up_to_2_mib_of_frames_the_most_a_receiver_takes_and_no_further() {
    const MIB: usize = 1 << 20;
    let file = filename();
    let mut spool = open(&file.path);
    // A frame of exactly `bytes`, its message padding it out.
    let sized = |seq: i64, bytes: usize| frame_for(seq, MATCH_ID, &"x".repeat(bytes - frame_for(seq, MATCH_ID, "", ALLOCATION).raw.len()), ALLOCATION);

    spool.append(&[stream(ALLOCATION, vec![sized(0, MIB), sized(1, MIB), frame(2)])]).unwrap();
    // The first two make exactly 2 MiB; the third would go past it, so it waits for the next batch.
    assert_eq!(seqs(&spool.pending(ALLOCATION).unwrap()), [0, 1]);
    spool.acknowledge(ALLOCATION, 1).unwrap();
    assert_eq!(seqs(&spool.pending(ALLOCATION).unwrap()), [2]);
}

#[tokio::test]
async fn drains_accepted_writes_before_shutdown_and_recovers_them() {
    let file = filename();
    let client = Arc::new(SpoolClient::start(file.path.clone(), Logger::null()));

    client.append(vec![stream(ALLOCATION, vec![])]).await.unwrap();

    let written = tokio::spawn({
        let client = Arc::clone(&client);

        async move { client.append(vec![stream(ALLOCATION, vec![frame(0)])]).await }
    });

    tokio::task::yield_now().await;

    let closed = tokio::spawn({
        let client = Arc::clone(&client);

        async move { client.close().await }
    });

    tokio::task::yield_now().await;
    assert_eq!(client.append(vec![stream(ALLOCATION, vec![frame(1)])]).await.unwrap_err().to_string(), "event spool is closing");
    written.await.unwrap().unwrap();
    let _ = closed.await.unwrap();
    assert_eq!(open(&file.path).pending(ALLOCATION).unwrap().len(), 1);
}

#[tokio::test]
async fn keeps_exits_on_the_spool_thread_until_they_are_forgotten() {
    let file = filename();
    let client = SpoolClient::start(file.path.clone(), Logger::null());
    let clean = exit("a", ALLOCATION, ServerExitReason::CleanExit, Some(0));

    client.record_exit(clean.clone()).await.unwrap();
    assert_eq!(client.exits().await.unwrap(), [clean]);
    client.forget_exits(vec![ALLOCATION.to_owned()]).await.unwrap();
    assert_eq!(client.exits().await.unwrap(), []);
    assert_eq!(client.prune(vec![]).await.unwrap(), Vec::<SpoolBacklog>::new());
    let _ = client.close().await;
}

#[tokio::test]
async fn remembers_launched_allocations_on_the_spool_thread() {
    let file = filename();
    let client = SpoolClient::start(file.path.clone(), Logger::null());

    assert!(!client.allocation_used(ALLOCATION.to_owned()).await.unwrap());
    client.record_allocation(ALLOCATION.to_owned()).await.unwrap();
    assert!(client.allocation_used(ALLOCATION.to_owned()).await.unwrap());
    let _ = client.close().await;
}

#[tokio::test]
async fn makes_the_node_unavailable_with_the_reason_when_the_spool_file_is_damaged() {
    let file = filename();
    let mut spool = open(&file.path);

    for server in 0..20 {
        let allocation = format!("00000000-0000-4000-8000-0000000001{server:02}");

        spool.append(&[stream(&allocation, (0..20).map(|seq| frame_for(seq, MATCH_ID, &"x".repeat(200), &allocation)).collect())]).unwrap();
    }

    drop(spool);

    // Garbage over a page past the header: the file opens, but its tree no longer holds together.
    let mut damaged = std::fs::OpenOptions::new().write(true).open(&file.path).unwrap();

    damaged.seek(SeekFrom::Start(3 * 4096 + 8)).unwrap();
    damaged.write_all(&[0xff; 64]).unwrap();
    drop(damaged);

    assert_eq!(EventSpool::open(&file.path, &Logger::null()).err().unwrap().to_string(), "event spool integrity check failed");

    let client = SpoolClient::start(file.path.clone(), Logger::null());

    assert_eq!(
        open_spool(&client, &Logger::null()).await,
        Some(NodeUnavailable { code: NodeUnavailableCode::EventStorage, operation: None, detail: "event spool integrity check failed".to_owned() })
    );
    let _ = client.close().await;
}

#[tokio::test]
async fn fails_every_call_on_a_file_a_newer_agent_wrote_so_the_first_carries_the_reason() {
    let file = filename();

    Connection::open(&file.path).unwrap().execute_batch(&format!("PRAGMA user_version = {};", SPOOL_SCHEMA_VERSION + 1)).unwrap();

    let client = SpoolClient::start(file.path.clone(), Logger::null());

    assert!(client.version().await.unwrap_err().to_string().contains("event spool schema mismatch"));
    let _ = client.close().await;
}

mod retention {
    use super::*;

    const DAY: i64 = 24 * 60 * 60 * 1000;

    #[test]
    fn keeps_a_delivered_server_until_the_window_passes_then_drops_its_stream() {
        let file = filename();
        let now = clock(1_000_000);
        let mut spool = open_at(&file.path, &now);

        spool.append(&[stream(ALLOCATION, vec![frame(0), frame(1)])]).unwrap();
        spool.acknowledge(ALLOCATION, 1).unwrap();

        // Delivered, but young enough that the plugin could still be replaying it.
        assert_eq!(spool.prune(&[]).unwrap(), Vec::<SpoolBacklog>::new());
        assert_eq!(spool_streams(&file.path), [ALLOCATION]);

        now.fetch_add(RETENTION, Ordering::SeqCst);
        assert_eq!(spool.prune(&[]).unwrap(), Vec::<SpoolBacklog>::new());
        now.fetch_add(1, Ordering::SeqCst);
        assert_eq!(spool.prune(&[ALLOCATION.to_owned()]).unwrap(), Vec::<SpoolBacklog>::new());
        assert_eq!(spool.prune(&[]).unwrap(), [SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 0 }]);
        assert_eq!(spool_streams(&file.path), Vec::<String>::new());
    }

    #[test]
    fn drops_undelivered_history_left_idle_and_leaves_attached_producers_alone() {
        let file = filename();
        let now = clock(1_000_000);
        let mut spool = open_at(&file.path, &now);

        spool.append(&[stream(OTHER, vec![])]).unwrap();
        spool.append(&[stream(ALLOCATION, vec![frame(0), frame(1)])]).unwrap();
        spool.append(&[stream(OTHER, vec![frame_for(0, MATCH_ID, "other", OTHER)])]).unwrap();
        spool.acknowledge(ALLOCATION, 0).unwrap();
        now.fetch_add(RETENTION + DAY, Ordering::SeqCst);

        // The idle server's event reached no controller within the retention period, so it goes with
        // its stream.
        assert_eq!(spool.prune(&[ALLOCATION.to_owned()]).unwrap(), [SpoolBacklog { allocation_id: OTHER.to_owned(), pending: 1 }]);
        assert!(spool.pending(OTHER).unwrap().is_empty());
        // The attached server keeps its position, so a replay of what it sent is still skipped.
        assert_eq!(spool.append(&[stream(ALLOCATION, vec![frame_for(0, MATCH_ID, "changed", ALLOCATION)])]).unwrap(), [ack(ALLOCATION, 1)]);
        assert_eq!(seqs(&spool.pending(ALLOCATION).unwrap()), [1]);
        assert_eq!(spool_streams(&file.path), [ALLOCATION]);
    }
}

#[tokio::test]
async fn prunes_on_the_spool_thread_and_returns_the_streams_it_dropped() {
    let file = filename();

    // Stored more than the retention period ago, by the spool's clock.
    let mut spool = open_at(&file.path, &clock(1_000));

    spool.append(&[stream(ALLOCATION, vec![frame(0)])]).unwrap();
    spool.close().unwrap();

    let client = SpoolClient::start(file.path.clone(), Logger::null());

    assert_eq!(client.prune(vec![]).await.unwrap(), [SpoolBacklog { allocation_id: ALLOCATION.to_owned(), pending: 1 }]);
    client.close().await.unwrap();
}
