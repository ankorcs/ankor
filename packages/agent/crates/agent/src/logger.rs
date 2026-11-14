//! Scoped log lines on the console, copied to a sink that keeps them for the controller.

use std::io::Write as _;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Ordered from least to most severe, so a threshold is a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Never in an `Entry`: the contract has no debug level, so debug lines stay on the console.
    Debug,
    /// Routine progress, on the console by default and always in the sink.
    Info,
    /// A problem the agent recovers from, written to stderr.
    Warn,
    /// A failure, written to stderr.
    Error,
}

impl Level {
    const ALL: [Self; 4] = [Self::Debug, Self::Info, Self::Warn, Self::Error];

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.name() == name.trim().to_ascii_lowercase())
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Applies to the console only. The sink receives every line from `info` up, because the controller
/// reads the node's log there.
static CONSOLE_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Names the console level. An unset or unknown value means `info`.
pub const LEVEL_ENV: &str = "ANKOR_LOG_LEVEL";

pub fn set_console_level(level: Level) {
    CONSOLE_LEVEL.store(level as u8, Ordering::SeqCst);
}

pub fn console_level_from_env() {
    set_console_level(std::env::var(LEVEL_ENV).ok().as_deref().and_then(Level::parse).unwrap_or(Level::Info));
}

#[must_use]
pub fn console_level() -> Level {
    let stored = CONSOLE_LEVEL.load(Ordering::SeqCst);

    Level::ALL.into_iter().find(|level| *level as u8 == stored).unwrap_or(Level::Info)
}

/// Lines beyond this many waiting for the console thread are dropped.
const CONSOLE_QUEUE: usize = 4096;

/// `Flush` answers once every line queued before it is written.
enum ConsoleJob {
    Line { error: bool, line: String },
    Flush(SyncSender<()>),
}

/// A write blocks while dockerd does not read the agent's console. Lines are therefore written on a
/// separate thread, so the caller never waits on the console. `None` when the thread could not start,
/// and the caller then writes. The first line may start the thread before the entrypoint drops root.
/// The thread only writes to the inherited console.
static CONSOLE: OnceLock<Option<SyncSender<ConsoleJob>>> = OnceLock::new();

/// Reported by the console thread with its next line.
static CONSOLE_DROPPED: AtomicU64 = AtomicU64::new(0);

fn console() -> Option<&'static SyncSender<ConsoleJob>> {
    CONSOLE
        .get_or_init(|| {
            let (jobs, queue) = sync_channel(CONSOLE_QUEUE);

            std::thread::Builder::new().name("console".to_owned()).spawn(move || write_console(&queue)).ok().map(|_| jobs)
        })
        .as_ref()
}

fn write_console(queue: &Receiver<ConsoleJob>) {
    while let Ok(job) = queue.recv() {
        match job {
            ConsoleJob::Line { error, line } => {
                report_dropped();
                write_line(error, &line);
            }
            ConsoleJob::Flush(done) => {
                report_dropped();
                let _ = done.send(());
            }
        }
    }
}

/// The dropped lines were queued after those still waiting, so the report calls them later lines.
fn report_dropped() {
    let dropped = CONSOLE_DROPPED.swap(0, Ordering::SeqCst);

    if dropped > 0 {
        let message = format!("{dropped} later console lines were dropped while the console was not read");

        write_line(true, &console_line(jiff::Timestamp::now(), "agent:console", Level::Warn, &message));
    }
}

/// A console that cannot be written to loses the line, where `println!` would panic.
fn write_line(error: bool, line: &str) {
    let _ = if error { writeln!(std::io::stderr().lock(), "{line}") } else { writeln!(std::io::stdout().lock(), "{line}") };
}

fn queue_line(error: bool, line: String) {
    let Some(jobs) = console() else {
        write_line(error, &line);
        return;
    };

    if let Err(TrySendError::Full(_)) = jobs.try_send(ConsoleJob::Line { error, line }) {
        CONSOLE_DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

/// Called as the process exits. Returns whether the queue drained within `limit`, and true when no
/// console thread runs.
pub fn flush_console(limit: Duration) -> bool {
    let Some(jobs) = CONSOLE.get().and_then(Option::as_ref) else {
        return true;
    };
    let deadline = Instant::now() + limit;
    let (done, written) = sync_channel(1);
    let mut job = ConsoleJob::Flush(done);

    loop {
        match jobs.try_send(job) {
            Ok(()) => break,
            Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                job = back;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return false,
        }
    }

    written.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_ok()
}

/// A line as the sink receives it, with the `[prefix]` in `message`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub at_ms: i64,
    pub level: Level,
    pub message: String,
}

pub(crate) type Sink = Arc<dyn Fn(Entry) + Send + Sync>;

#[derive(Clone)]
pub struct Logger {
    prefix: Option<Arc<str>>,
    sink: Option<Sink>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger").field("prefix", &self.prefix).finish_non_exhaustive()
    }
}

impl Logger {
    #[must_use]
    pub fn new(prefix: &str, sink: Option<Sink>) -> Self {
        Self { prefix: Some(prefix.into()), sink }
    }

    /// Writes nothing anywhere.
    #[must_use]
    pub fn null() -> Self {
        Self { prefix: None, sink: None }
    }

    #[must_use]
    pub fn child(&self, prefix: &str) -> Self {
        Self { prefix: self.prefix.as_ref().map(|parent| format!("{parent}:{prefix}").into()), sink: self.sink.clone() }
    }

    /// Goes to the console only, because the contract has no debug level.
    pub fn debug(&self, message: impl AsRef<str>) {
        self.emit(Level::Debug, message.as_ref());
    }

    pub fn info(&self, message: impl AsRef<str>) {
        self.emit(Level::Info, message.as_ref());
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.emit(Level::Warn, message.as_ref());
    }

    pub fn error(&self, message: impl AsRef<str>) {
        self.emit(Level::Error, message.as_ref());
    }

    fn emit(&self, level: Level, message: &str) {
        let Some(prefix) = &self.prefix else {
            return;
        };
        let now = jiff::Timestamp::now();

        if level >= console_level() {
            queue_line(level >= Level::Warn, console_line(now, prefix, level, message));
        }

        if level >= Level::Info
            && let Some(sink) = &self.sink
        {
            sink(Entry { at_ms: now.as_millisecond(), level, message: format!("[{prefix}] {message}") });
        }
    }
}

#[must_use]
pub(crate) fn console_line(at: jiff::Timestamp, prefix: &str, level: Level, message: &str) -> String {
    let stamp = at.strftime("%Y-%m-%dT%H:%M:%S%.3fZ");

    match level {
        Level::Debug => format!("{stamp} [{prefix}] DEBUG {message}"),
        Level::Info => format!("{stamp} [{prefix}] {message}"),
        Level::Warn => format!("{stamp} [{prefix}] WARN {message}"),
        Level::Error => format!("{stamp} [{prefix}] ERROR {message}"),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn console_lines_are_written_by_the_console_thread_and_a_flush_waits_for_them() {
        let log = Logger::new("test", None);

        log.debug("a line below the console's level, which is not queued");
        log.warn("a line the console thread writes");

        assert!(flush_console(Duration::from_secs(5)), "the console thread did not drain its queue");
        assert!(CONSOLE.get().is_some_and(Option::is_some), "the console thread did not start");
    }

    #[test]
    fn writes_a_timestamped_line_under_its_tag() {
        let line = console_line(jiff::Timestamp::now(), "agent", Level::Info, "x");
        let pattern = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\S+Z \[agent\] x$").unwrap();

        assert!(pattern.is_match(&line), "{line}");
        assert!(console_line(jiff::Timestamp::now(), "agent", Level::Warn, "x").ends_with("[agent] WARN x"));
        assert!(console_line(jiff::Timestamp::now(), "agent", Level::Debug, "x").ends_with("[agent] DEBUG x"));
    }

    #[test]
    fn keeps_debug_lines_from_the_sink() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let log = Logger::new("agent", Some(Arc::new(move |entry: Entry| seen.lock().unwrap().push(entry))));

        set_console_level(Level::Debug);
        log.debug("detail");
        set_console_level(Level::Info);

        assert!(lines.lock().unwrap().is_empty());
    }

    #[test]
    fn reads_a_level_by_name_and_orders_levels_by_severity() {
        assert_eq!(Level::parse(" DEBUG "), Some(Level::Debug));
        assert_eq!(Level::parse("verbose"), None);
        assert!(Level::Debug < Level::Info && Level::Info < Level::Warn && Level::Warn < Level::Error);
    }

    /// Set in a process that [`child`] starts, so the test runs its child half there.
    const CHILD: &str = "ANKOR_LOGGER_TEST_CHILD";

    fn in_child() -> bool {
        std::env::var_os(CHILD).is_some()
    }

    /// Runs `test` of this module in a process of its own. Its console is a pipe that the test reads
    /// when it chooses, and its console level and thread start fresh.
    fn child(test: &str) -> std::process::Command {
        let module = module_path!().split_once("::").map_or(module_path!(), |(_, module)| module);
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());

        command
            .args([&format!("{module}::{test}"), "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env_remove(LEVEL_ENV)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    }

    /// Logs until the console queue is full, which happens once the pipe behind the console is
    /// full and the console thread waits on it.
    fn fill_the_console_queue() {
        let log = Logger::new("test", None);

        for index in 0..1_000_000 {
            if CONSOLE_DROPPED.load(Ordering::SeqCst) > 0 {
                return;
            }

            log.info(format!("line {index}"));
        }

        std::process::exit(2);
    }

    /// Waits for `child` to exit, and kills it after `limit`.
    fn exit_within(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + limit;

        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }

            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = child.kill();
        None
    }

    #[test]
    fn writes_debug_lines_to_the_console_once_the_environment_names_debug() {
        if in_child() {
            console_level_from_env();
            assert_eq!(console_level(), Level::Debug);
            Logger::new("test", None).debug("detail");
            assert!(flush_console(Duration::from_secs(5)));
            return;
        }

        let output = child("writes_debug_lines_to_the_console_once_the_environment_names_debug").env(LEVEL_ENV, " Debug ").output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(output.status.success(), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
        assert!(stdout.contains("[test] DEBUG detail"), "{stdout}");
    }

    /// The console's pipe is never read, so the console thread never takes the flush.
    #[test]
    fn a_flush_gives_up_once_the_console_stays_unread_for_its_whole_limit() {
        if in_child() {
            fill_the_console_queue();

            let started = Instant::now();
            let drained = flush_console(Duration::from_millis(300));
            let waited = started.elapsed();

            // The console thread holds the console's lock, so the test harness could not report.
            std::process::exit(i32::from(drained || waited < Duration::from_millis(300)));
        }

        let mut process = child("a_flush_gives_up_once_the_console_stays_unread_for_its_whole_limit").spawn().unwrap();
        let unread = process.stdout.take();
        let status = exit_within(&mut process, Duration::from_secs(10));

        drop(unread);
        assert!(status.is_some_and(|status| status.success()), "{status:?}");
    }

    /// Lines dropped while the console was not read are reported once, by the console thread.
    #[test]
    fn a_flush_waits_for_a_full_queue_while_the_console_is_read_again_and_reports_the_dropped_lines() {
        if in_child() {
            fill_the_console_queue();
            eprintln!("full");

            let drained = flush_console(Duration::from_secs(10));

            std::process::exit(i32::from(!drained));
        }

        let mut process = child("a_flush_waits_for_a_full_queue_while_the_console_is_read_again_and_reports_the_dropped_lines").spawn().unwrap();
        let mut stdout = process.stdout.take().unwrap();
        let mut stderr = std::io::BufReader::new(process.stderr.take().unwrap());
        let mut line = String::new();

        while !line.contains("full") {
            line.clear();
            assert_ne!(std::io::BufRead::read_line(&mut stderr, &mut line).unwrap(), 0, "the child ended before its queue was full");
        }

        // The flush finds the queue full, then waits for room.
        std::thread::sleep(Duration::from_millis(300));

        let reader = std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
        let mut rest = String::new();

        std::io::Read::read_to_string(&mut stderr, &mut rest).unwrap();
        reader.join().unwrap().unwrap();

        let status = exit_within(&mut process, Duration::from_secs(10));

        assert!(status.is_some_and(|status| status.success()), "{status:?} {rest}");
        assert_eq!(rest.matches("later console lines were dropped").count(), 1, "{rest}");
    }

    #[test]
    fn the_null_logger_stays_null_in_its_children() {
        assert!(format!("{:?}", Logger::null().child("x")).contains("prefix: None"));
    }

    #[test]
    fn children_extend_the_prefix_and_share_the_sink() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let log = Logger::new("agent", Some(Arc::new(move |entry: Entry| seen.lock().unwrap().push(entry))));

        log.child("ws").warn("lost");
        Logger::null().error("dropped");

        let lines = lines.lock().unwrap();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].level, Level::Warn);
        assert_eq!(lines[0].message, "[agent:ws] lost");
    }
}
