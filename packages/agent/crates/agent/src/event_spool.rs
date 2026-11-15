//! The event queue: an SQLite file of each server's events until the controller has them, the server ends the
//! controller has not had, and the allocations launched. The file outlives agent updates, so its layout is
//! versioned in `user_version` and only ever moved forward by migrations.

use std::path::{Path, PathBuf};

use ankor_protocol::constants::{EVENT_BATCH_MAX_BYTES, EVENT_BATCH_MAX_FRAMES, EVENT_SPOOL_RETENTION, REMEMBERED_ALLOCATIONS};
use ankor_protocol::helpers::{from_wire_name, wire_name};
use ankor_protocol::types::{ServerExit, ServerExitReason};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::logger::Logger;

/// Every layout file in `migrations/`, by name, in version order. A new migration adds its line here
/// and moves [`SPOOL_SCHEMA_VERSION`] to its version; the tests fail until both are done.
pub const SPOOL_LAYOUT_FILES: &[(&str, &str)] = &[("event_spool_0001.sql", include_str!("../migrations/event_spool_0001.sql"))];

/// The layout this agent writes: the version of the newest file in [`SPOOL_LAYOUT_FILES`].
pub const SPOOL_SCHEMA_VERSION: u32 = 1;

/// The naming rule for the layout files in `migrations/`: `event_spool_NNNN.sql` takes a file to
/// version `NNNN`, and `event_spool_0001.sql` is the layout every file starts from. `None` for a
/// name that breaks the rule.
fn spool_file_version(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("event_spool_")?.strip_suffix(".sql")?;

    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    digits.parse().ok().filter(|version| *version >= 1)
}

/// The layouts a spool file moves through, read from files named by `spool_file_version`'s rule:
/// the version-1 layout every file starts from, and each later step with the version it reaches.
#[derive(Clone, Debug)]
pub struct SpoolSchema {
    /// The version the newest step reaches.
    pub version: u32,
    /// The version-1 layout that every file starts from.
    base: String,
    /// Each later step with the version it reaches, oldest first.
    migrations: Vec<(u32, String)>,
}

impl SpoolSchema {
    /// The layout this agent writes, from the files compiled in.
    ///
    /// # Errors
    ///
    /// Fails as [`SpoolSchema::from_files`] does, which the tests rule out.
    pub fn current() -> std::result::Result<Self, String> {
        Self::from_files(SPOOL_LAYOUT_FILES.iter().map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned())))
    }

    /// # Errors
    ///
    /// Fails on a name that breaks the rule, two files of one version, or no version-1 file.
    pub fn from_files(files: impl IntoIterator<Item = (String, String)>) -> std::result::Result<Self, String> {
        let mut steps = Vec::new();

        for (name, sql) in files {
            let version = spool_file_version(&name).ok_or_else(|| format!("{name} is not named event_spool_NNNN.sql"))?;

            steps.push((version, sql));
        }

        steps.sort_by_key(|(version, _)| *version);

        let twice = steps.windows(2).find_map(|pair| match pair {
            [first, second] if first.0 == second.0 => Some(first.0),
            _ => None,
        });

        if let Some(version) = twice {
            return Err(format!("two files reach version {version}"));
        }

        let mut steps = steps.into_iter();
        let Some((1, base)) = steps.next() else {
            return Err("no event_spool_0001.sql, the layout every file starts from".to_owned());
        };
        let migrations: Vec<(u32, String)> = steps.collect();

        Ok(Self { version: migrations.last().map_or(1, |(version, _)| *version), base, migrations })
    }
}

/// How far a server's frames are stored; the relay tells that server's plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolAck {
    pub allocation_id: String,
    pub to_seq: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolFrame {
    pub allocation_id: String,
    pub seq: i64,
    pub frame: String,
}

/// A plugin frame as received, its `type` removed; only its `seq` was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpooledFrame {
    pub seq: i64,
    pub raw: String,
}

/// One server's frames for a shared commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolStream {
    pub allocation_id: String,
    pub frames: Vec<SpooledFrame>,
}

/// A stream's outcome within a shared commit: how far its server is received, or why it was
/// rolled back.
pub type SpoolAppend = std::result::Result<Option<SpoolAck>, String>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolBacklog {
    pub allocation_id: String,
    pub pending: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    /// SQLite refused an operation.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// Creating the spool's directory or setting the file's permissions failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A newer agent wrote the file, so this agent cannot use its layout.
    #[error("event spool schema mismatch: {} is at version {version}, newer than this agent's {supported}. {remedy}", .file.display())]
    Newer { file: PathBuf, version: u32, supported: u32, remedy: String },
    /// A migration step failed and was rolled back.
    #[error("event spool migration from schema {from} to {to} failed, leaving it at {from}: {source}. {remedy}")]
    Migration {
        from: u32,
        to: u32,
        #[source]
        source: Box<SpoolError>,
        remedy: String,
    },
    /// `PRAGMA quick_check` found the file damaged.
    #[error("event spool integrity check failed")]
    Integrity,
    /// The controller acknowledged events past those the spool received.
    #[error("backend acknowledged events missing from the local spool")]
    AcknowledgedMissing,
    /// A stream skipped a sequence number after the last one stored.
    #[error("missing event {allocation}/{expected}; received {received}")]
    MissingEvent { allocation: String, expected: i64, received: i64 },
    /// Too many calls wait on the spool's thread.
    #[error("event spool is busy; retry without discarding events")]
    Busy,
    /// The spool is closing and takes no new calls.
    #[error("event spool is closing")]
    Closing,
    /// The spool's thread ended before it answered.
    #[error("event spool thread stopped")]
    Stopped,
    /// The file did not open, and no reason was recorded.
    #[error("the event spool did not open")]
    NotOpened,
    /// The file did not open or the thread did not start, for this reason.
    #[error("{0}")]
    Unavailable(String),
}

type Result<T> = std::result::Result<T, SpoolError>;

pub struct EventSpool {
    db: Connection,
    now: Box<dyn Fn() -> i64 + Send>,
    log: Logger,
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn reason_text(reason: ServerExitReason) -> String {
    wire_name(reason)
}

impl EventSpool {
    /// Opens the file, creating or migrating it as needed.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be opened, is newer than this agent, cannot be migrated or fails
    /// its integrity check.
    pub fn open(filename: &Path, log: &Logger) -> Result<Self> {
        Self::open_with_clock(filename, log, Box::new(now_ms))
    }

    /// # Errors
    ///
    /// Fails as [`EventSpool::open`] does.
    pub fn open_with_clock(filename: &Path, log: &Logger, now: Box<dyn Fn() -> i64 + Send>) -> Result<Self> {
        Self::open_with(filename, log, now, &SpoolSchema::current().map_err(SpoolError::Unavailable)?)
    }

    /// # Errors
    ///
    /// Fails as [`EventSpool::open`] does.
    pub fn open_with(filename: &Path, log: &Logger, now: Box<dyn Fn() -> i64 + Send>, schema: &SpoolSchema) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

        let dir = filename.parent().unwrap_or(Path::new("."));

        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;

        let db = Connection::open(filename)?;

        std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600))?;

        let mut spool = Self { db, now, log: log.clone() };

        spool.initialize(filename, log, schema)?;
        Ok(spool)
    }

    fn initialize(&mut self, filename: &Path, log: &Logger, schema: &SpoolSchema) -> Result<()> {
        let mut version = self.version()?;
        let dir = filename.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        let remedy = format!(
            "Update the agent, or delete {} on the host and restart it to start a new spool, which deletes every event not yet delivered to the controller",
            dir.display()
        );

        self.db.pragma_update(None, "journal_mode", "WAL")?;
        self.db.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;

        if version == 0 {
            self.transaction(|db| {
                db.execute_batch(&schema.base)?;
                db.execute_batch("PRAGMA user_version = 1")?;
                Ok(())
            })?;
            version = 1;
        }

        if version > schema.version {
            return Err(SpoolError::Newer { file: filename.to_path_buf(), version, supported: schema.version, remedy });
        }

        let start = version;

        for (target, migration) in schema.migrations.iter().filter(|(target, _)| *target > start) {
            let target = *target;
            let migrated = self.transaction(|db| {
                db.execute_batch(migration)?;
                db.execute_batch(&format!("PRAGMA user_version = {target}"))?;
                Ok(())
            });

            if let Err(error) = migrated {
                return Err(SpoolError::Migration { from: version, to: target, source: Box::new(error), remedy });
            }

            log.info(format!("event spool migrated from schema {version} to {target}"));
            version = target;
        }

        let check: String = self.db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;

        if check != "ok" {
            return Err(SpoolError::Integrity);
        }

        Ok(())
    }

    fn transaction<T>(&mut self, work: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;

        let result = work(&self.db).and_then(|value| Ok(self.db.execute_batch("COMMIT").map(|()| value)?));

        // A failed COMMIT can leave the transaction open; every later BEGIN would fail. The caller
        // gets the error that failed the work, as a failed rollback says nothing about it.
        if result.is_err()
            && !self.db.is_autocommit()
            && let Err(error) = self.db.execute_batch("ROLLBACK")
        {
            self.log.error(format!("event spool could not roll back a failed commit: {error}"));
        }

        result
    }

    /// Closes the file, which checkpoints the write-ahead log and removes it.
    ///
    /// # Errors
    ///
    /// Fails when SQLite cannot close the file.
    pub fn close(self) -> Result<()> {
        self.db.close().map_err(|(_, error)| SpoolError::Sqlite(error))
    }

    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn version(&self) -> Result<u32> {
        Ok(self.db.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// How far this server's events have been received, or `None` before its first event.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn position(&self, allocation_id: &str) -> Result<Option<SpoolAck>> {
        let received: Option<i64> = self
            .db
            .prepare_cached("SELECT received FROM server_streams WHERE allocation = ? AND received >= 0")?
            .query_row([allocation_id], |row| row.get(0))
            .optional()?;

        Ok(received.map(|to_seq| SpoolAck { allocation_id: allocation_id.to_owned(), to_seq }))
    }

    /// Stores every stream in one commit and one disk sync, each in its own savepoint, so a broken
    /// stream is rolled back alone.
    ///
    /// # Errors
    ///
    /// Fails when the whole commit does, as on a full disk.
    pub fn append(&mut self, streams: &[SpoolStream]) -> Result<Vec<SpoolAppend>> {
        let (now, log) = ((self.now)(), self.log.clone());

        self.transaction(|db| {
            let mut outcomes = Vec::with_capacity(streams.len());

            for stream in streams {
                db.execute_batch("SAVEPOINT stream")?;

                match append_stream(db, stream, now, &log) {
                    Ok(ack) => {
                        db.execute_batch("RELEASE stream")?;
                        outcomes.push(Ok(ack));
                    }
                    Err(error) => {
                        // Disk full or an I/O error ends the whole transaction.
                        if db.is_autocommit() {
                            return Err(error);
                        }

                        db.execute_batch("ROLLBACK TO stream; RELEASE stream")?;
                        outcomes.push(Err(error.to_string()));
                    }
                }
            }

            Ok(outcomes)
        })
    }

    /// The server's oldest events, within one batch's frame and byte limits.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn pending(&self, allocation_id: &str) -> Result<Vec<SpoolFrame>> {
        let mut statement = self.db.prepare_cached("SELECT seq, frame FROM server_events WHERE allocation = ? ORDER BY seq LIMIT ?")?;
        let rows = statement.query_map(params![allocation_id, i64::try_from(EVENT_BATCH_MAX_FRAMES).unwrap_or(i64::MAX)], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut batch = Vec::new();
        let mut bytes = 0;

        for row in rows {
            let (seq, frame) = row?;

            if bytes + frame.len() as u64 > EVENT_BATCH_MAX_BYTES {
                break;
            }

            bytes += frame.len() as u64;
            batch.push(SpoolFrame { allocation_id: allocation_id.to_owned(), seq, frame });
        }

        Ok(batch)
    }

    /// Servers with events to deliver and how many, the one whose oldest event came first leading.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn backlog(&self) -> Result<Vec<SpoolBacklog>> {
        let mut statement = self.db.prepare_cached("SELECT allocation, count(*) FROM server_events GROUP BY allocation ORDER BY min(id)")?;
        let rows = statement.query_map([], |row| Ok(SpoolBacklog { allocation_id: row.get(0)?, pending: row.get(1)? }))?;

        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether the node launched this allocation, among the last it remembers.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn allocation_used(&self, allocation_id: &str) -> Result<bool> {
        Ok(self.db.prepare_cached("SELECT 1 FROM allocations WHERE allocation = ?")?.exists([allocation_id])?)
    }

    /// Remembers a launched allocation, forgetting the oldest past the limit. Oldest goes by the
    /// order of recording, not by the clock, which a host that boots before its time is set reads wrong.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written.
    pub fn record_allocation(&mut self, allocation_id: &str) -> Result<()> {
        let now = (self.now)();

        self.transaction(|db| {
            db.prepare_cached("INSERT INTO allocations (allocation, launchedAt) VALUES (?, ?) ON CONFLICT (allocation) DO NOTHING")?
                .execute(params![allocation_id, now])?;
            db.prepare_cached("DELETE FROM allocations WHERE allocation NOT IN (SELECT allocation FROM allocations ORDER BY rowid DESC LIMIT ?)")?
                .execute([i64::try_from(REMEMBERED_ALLOCATIONS).unwrap_or(i64::MAX)])?;
            Ok(())
        })
    }

    /// Keeps a server's end until the controller has it; recording it again changes nothing.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written.
    pub fn record_exit(&mut self, exit: &ServerExit) -> Result<()> {
        let now = (self.now)();

        self.db
            .prepare_cached(
                "INSERT INTO exits (allocation, slot, reason, exitCode, detail, endedAt) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (allocation) DO NOTHING",
            )?
            .execute(params![exit.allocation_id, exit.slot_id, reason_text(exit.reason), exit.exit_code, exit.detail, now])?;

        Ok(())
    }

    /// The ends the controller has not had, oldest first. A row with a reason this agent does not know, as a
    /// newer agent may have written before a rollback, is skipped and logged, so it hides no other.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn exits(&self) -> Result<Vec<ServerExit>> {
        let mut statement = self.db.prepare_cached("SELECT slot, allocation, reason, exitCode, detail FROM exits ORDER BY endedAt, rowid")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<i64>>(3)?, row.get::<_, Option<String>>(4)?))
        })?;
        let mut exits = Vec::new();

        for row in rows {
            let (slot_id, allocation_id, reason, exit_code, detail) = row?;
            let Some(reason) = from_wire_name::<ServerExitReason>(&reason) else {
                self.log.warn(format!("server {allocation_id}: its exit has a reason this agent does not know ({reason}), so the controller is not told it"));
                continue;
            };

            exits.push(ServerExit { slot_id, allocation_id, reason, exit_code, detail });
        }

        Ok(exits)
    }

    /// The controller has these.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written.
    pub fn forget_exits(&mut self, allocation_ids: &[String]) -> Result<()> {
        self.transaction(|db| {
            for allocation_id in allocation_ids {
                db.prepare_cached("DELETE FROM exits WHERE allocation = ?")?.execute([allocation_id])?;
            }

            Ok(())
        })
    }

    /// The controller has this server's events up to `to_seq`.
    ///
    /// # Errors
    ///
    /// Fails when the controller acknowledges more than the spool received.
    pub fn acknowledge(&mut self, allocation_id: &str, to_seq: i64) -> Result<()> {
        let now = (self.now)();

        self.transaction(|db| {
            let received: Option<i64> =
                db.prepare_cached("SELECT received FROM server_streams WHERE allocation = ?")?.query_row([allocation_id], |row| row.get(0)).optional()?;

            if received.is_none_or(|received| to_seq > received) {
                return Err(SpoolError::AcknowledgedMissing);
            }

            db.prepare_cached("UPDATE server_streams SET updatedAt = ? WHERE allocation = ?")?.execute(params![now, allocation_id])?;
            db.prepare_cached("DELETE FROM server_events WHERE allocation = ? AND seq <= ?")?.execute(params![allocation_id, to_seq])?;
            Ok(())
        })
    }

    /// Drops streams idle for the retention period, with their undelivered events, and ends kept as
    /// long. Idle means the server sent nothing and the controller took nothing. A server that the
    /// controller keeps refusing so cannot keep its events forever. An attached server keeps its
    /// stream, because its plugin can still replay into it.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written.
    pub fn prune(&mut self, active_allocations: &[String]) -> Result<Vec<SpoolBacklog>> {
        let cutoff = (self.now)() - i64::try_from(EVENT_SPOOL_RETENTION.as_millis()).unwrap_or(i64::MAX);

        self.transaction(|db| {
            db.prepare_cached("DELETE FROM exits WHERE endedAt < ?")?.execute([cutoff])?;

            let streams: Vec<(String, i64)> = db
                .prepare_cached("SELECT allocation, updatedAt FROM server_streams")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut pruned = Vec::new();

            for (allocation, updated_at) in streams {
                if active_allocations.contains(&allocation) || updated_at >= cutoff {
                    continue;
                }

                // What it still held never reached the controller, so the caller can say how much was lost.
                let pending = db.prepare_cached("DELETE FROM server_events WHERE allocation = ?")?.execute([&allocation])?;

                db.prepare_cached("DELETE FROM server_streams WHERE allocation = ?")?.execute([&allocation])?;
                pruned.push(SpoolBacklog { allocation_id: allocation, pending: i64::try_from(pending).unwrap_or(i64::MAX) });
            }

            Ok(pruned)
        })
    }
}

/// Stores a stream's new frames. A replay of what the server already sent is skipped, as the controller has
/// the first copy; a gap after what is stored fails the stream.
fn append_stream(db: &Connection, stream: &SpoolStream, now: i64, log: &Logger) -> Result<Option<SpoolAck>> {
    let allocation = &stream.allocation_id;

    db.prepare_cached("INSERT INTO server_streams (allocation, updatedAt) VALUES (?, ?) ON CONFLICT (allocation) DO NOTHING")?
        .execute(params![allocation, now])?;

    let mut received: i64 = db.prepare_cached("SELECT received FROM server_streams WHERE allocation = ?")?.query_row([allocation], |row| row.get(0))?;

    // A stream that holds nothing yet starts where its server's events do. One that began past 0
    // lost its history here, as with a deleted spool, and its plugin resends only what this spool
    // never acknowledged; holding out for the rest would refuse the server's every later event.
    if received < 0
        && let Some(first) = stream.frames.first().filter(|first| first.seq > 0)
    {
        log.warn(format!("server {allocation}: events 0 to {} are not in the spool, so they are lost; storing from {}", first.seq - 1, first.seq));
        received = first.seq - 1;
    }

    for frame in &stream.frames {
        if frame.seq <= received {
            continue;
        }

        if frame.seq != received + 1 {
            return Err(SpoolError::MissingEvent { allocation: allocation.to_owned(), expected: received + 1, received: frame.seq });
        }

        db.prepare_cached("INSERT INTO server_events (allocation, seq, frame) VALUES (?, ?, ?)")?.execute(params![allocation, frame.seq, frame.raw])?;
        received = frame.seq;
    }

    db.prepare_cached("UPDATE server_streams SET received = ?, updatedAt = ? WHERE allocation = ?")?.execute(params![received, now, allocation])?;

    Ok((received >= 0).then(|| SpoolAck { allocation_id: allocation.clone(), to_seq: received }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    const A: &str = "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e";
    const B: &str = "7c2d3e4f-1a2b-4c5d-8e9f-0a1b2c3d4e5f";

    fn frames(seqs: &[i64]) -> Vec<SpooledFrame> {
        seqs.iter().map(|&seq| SpooledFrame { seq, raw: format!(r#"{{"seq":{seq}}}"#) }).collect()
    }

    fn open(dir: &Path) -> (EventSpool, Arc<AtomicI64>) {
        let clock = Arc::new(AtomicI64::new(1_760_000_000_000));
        let reading = Arc::clone(&clock);
        let spool = EventSpool::open_with_clock(&dir.join("events/streams.sqlite"), &Logger::null(), Box::new(move || reading.load(Ordering::SeqCst))).unwrap();

        (spool, clock)
    }

    #[test]
    fn a_failed_commit_rolls_back_so_the_spool_keeps_working() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spool, _) = open(dir.path());

        // A deferred foreign key fails at COMMIT rather than at the INSERT.
        let failed = spool.transaction(|db| {
            db.execute_batch("PRAGMA defer_foreign_keys = ON; INSERT INTO server_events (allocation, seq, frame) VALUES ('none', 0, '{}')")?;
            Ok(())
        });

        assert!(failed.is_err());
        assert!(spool.db.is_autocommit());

        let outcomes = spool.append(&[SpoolStream { allocation_id: A.to_owned(), frames: frames(&[0]) }]).unwrap();

        assert_eq!(outcomes[0], Ok(Some(SpoolAck { allocation_id: A.to_owned(), to_seq: 0 })));
    }

    #[test]
    fn a_failed_transaction_that_ended_itself_returns_its_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spool, _) = open(dir.path());

        let failed = spool.transaction(|db| -> Result<()> {
            db.execute_batch("ROLLBACK")?;
            Err(SpoolError::Integrity)
        });

        assert!(matches!(failed, Err(SpoolError::Integrity)), "{failed:?}");
        assert!(spool.db.is_autocommit());
    }

    #[test]
    fn a_failed_transaction_still_open_is_rolled_back_and_returns_its_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spool, _) = open(dir.path());

        let failed = spool.transaction(|db| -> Result<()> {
            db.execute_batch("INSERT INTO allocations (allocation, launchedAt) VALUES ('x', 1)")?;
            Err(SpoolError::Integrity)
        });

        assert!(matches!(failed, Err(SpoolError::Integrity)), "{failed:?}");
        assert!(spool.db.is_autocommit());
        assert!(!spool.allocation_used("x").unwrap());
    }

    #[test]
    fn stamps_with_the_wall_clock() {
        let before = jiff::Timestamp::now().as_millisecond();
        let stamped = now_ms();

        assert!((before..before + 1_000).contains(&stamped), "{before} {stamped}");
    }

    #[test]
    fn appends_replays_gaps_and_acknowledgements() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spool, _) = open(dir.path());

        assert_eq!(spool.version().unwrap(), SPOOL_SCHEMA_VERSION);

        let outcomes = spool
            .append(&[
                SpoolStream { allocation_id: A.to_owned(), frames: frames(&[0, 1, 2]) },
                SpoolStream { allocation_id: B.to_owned(), frames: frames(&[1]) },
            ])
            .unwrap();

        assert_eq!(outcomes[0], Ok(Some(SpoolAck { allocation_id: A.to_owned(), to_seq: 2 })));
        // A stream with nothing stored starts at its first event, as after a deleted spool.
        assert_eq!(outcomes[1], Ok(Some(SpoolAck { allocation_id: B.to_owned(), to_seq: 1 })));

        let gap = spool.append(&[SpoolStream { allocation_id: A.to_owned(), frames: frames(&[5]) }]).unwrap();

        assert_eq!(gap[0], Err("missing event 5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e/3; received 5".to_owned()));

        let replay = spool.append(&[SpoolStream { allocation_id: A.to_owned(), frames: frames(&[1, 2, 3]) }]).unwrap();

        assert_eq!(replay[0], Ok(Some(SpoolAck { allocation_id: A.to_owned(), to_seq: 3 })));
        assert_eq!(spool.pending(A).unwrap().iter().map(|frame| frame.seq).collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert_eq!(
            spool.backlog().unwrap(),
            [SpoolBacklog { allocation_id: A.to_owned(), pending: 4 }, SpoolBacklog { allocation_id: B.to_owned(), pending: 1 }]
        );

        spool.acknowledge(A, 1).unwrap();
        assert_eq!(spool.pending(A).unwrap().len(), 2);
        assert!(spool.acknowledge(A, 9).is_err());
    }

    #[test]
    fn exits_and_allocations_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spool, _) = open(dir.path());
        let exit = ServerExit { slot_id: "s1".to_owned(), allocation_id: A.to_owned(), reason: ServerExitReason::Crashed, exit_code: Some(139), detail: None };

        spool.record_exit(&exit).unwrap();
        spool.record_exit(&exit).unwrap();
        spool.record_allocation(A).unwrap();
        drop(spool);

        let (mut spool, _) = open(dir.path());

        assert_eq!(spool.exits().unwrap(), [exit]);
        assert!(spool.allocation_used(A).unwrap());
        assert!(!spool.allocation_used(B).unwrap());
        spool.forget_exits(&[A.to_owned()]).unwrap();
        assert_eq!(spool.exits().unwrap().len(), 0);
    }

    #[test]
    fn prune_drops_idle_streams_with_their_undelivered_events_and_keeps_attached_ones() {
        let dir = tempfile::tempdir().unwrap();
        let (mut spool, clock) = open(dir.path());

        spool
            .append(&[SpoolStream { allocation_id: A.to_owned(), frames: frames(&[0]) }, SpoolStream { allocation_id: B.to_owned(), frames: frames(&[0]) }])
            .unwrap();
        spool.acknowledge(B, 0).unwrap();
        clock.fetch_add(i64::try_from(EVENT_SPOOL_RETENTION.as_millis()).unwrap() + 1, Ordering::SeqCst);

        assert_eq!(spool.prune(&[A.to_owned()]).unwrap(), [SpoolBacklog { allocation_id: B.to_owned(), pending: 0 }]);
        assert_eq!(spool.pending(A).unwrap().len(), 1, "an attached server keeps its undelivered events");
        assert_eq!(spool.prune(&[]).unwrap(), [SpoolBacklog { allocation_id: A.to_owned(), pending: 1 }]);
        assert!(spool.pending(A).unwrap().is_empty());
        assert!(spool.backlog().unwrap().is_empty());
    }

    #[test]
    fn a_newer_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events/streams.sqlite");

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 99;").unwrap();

        let error = EventSpool::open(&path, &Logger::null()).err().unwrap().to_string();

        assert!(error.contains(&format!("is at version 99, newer than this agent's {SPOOL_SCHEMA_VERSION}")), "{error}");
    }
}
