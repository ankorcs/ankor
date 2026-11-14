//! The node's resources: memory and CPU from the environment's cgroup or `/proc`, the data
//! directory's disk, the host's distribution, and the ticks that carry them to the controller while it
//! watches.

use crate::sync::lock;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use ankor_protocol::constants::{OS_NAME_MAX_CHARS, RESOURCE_TICK, RESOURCE_TICKS_HELD, WATCH_LEASE};
use ankor_protocol::types::{AgentResourceBatch, AgentToControllerMessage, ResourceTick, ResourceTickCpu, ResourceUsage};
use async_trait::async_trait;
use regex::Regex;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::failure::truncate;
use crate::node_log::{Clock, Transport, wall_clock};

/// Two small file reads, twice a second; each tick carries their peak and average.
const CPU_SAMPLE: Duration = Duration::from_millis(500);

/// CPU samples per resource tick; the last one of each is the tick.
#[allow(clippy::cast_possible_truncation, reason = "a handful of samples")]
const SAMPLES_PER_TICK: u64 = (RESOURCE_TICK.as_millis() / CPU_SAMPLE.as_millis()) as u64;

pub(crate) const OS_RELEASE_FILE: &str = "/host/etc/os-release";

/// Memory as it is now and the CPU since the previous report; `cpu` is `None` until two samples exist.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceStatus {
    pub memory: ResourceUsage,
    pub cpu: Option<ResourceTickCpu>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourcePaths {
    /// The environment's cgroup root, which node-setup mounts read-only.
    pub cgroup: PathBuf,
    /// Tried in order; node-setup mounts the environment's meminfo first.
    pub meminfo: Vec<PathBuf>,
    /// `/proc/stat`, read for CPU time when the cgroup has no `usage_usec`.
    pub stat: PathBuf,
}

impl Default for ResourcePaths {
    fn default() -> Self {
        Self { cgroup: "/host/sys/fs/cgroup".into(), meminfo: vec!["/host/proc/meminfo".into(), "/proc/meminfo".into()], stat: "/proc/stat".into() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meminfo {
    pub total_bytes: i64,
    pub available_bytes: i64,
}

fn read_text(file: &Path) -> Option<String> {
    std::fs::read_to_string(file).ok()
}

fn digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    text.parse().ok()
}

fn read_number(file: &Path) -> Option<i64> {
    digits(read_text(file)?.trim())
}

#[allow(clippy::expect_used, reason = "fixed patterns, which the tests compile")]
static MEM_TOTAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^MemTotal:\s+([0-9]+) kB$").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "fixed patterns, which the tests compile")]
static MEM_AVAILABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^MemAvailable:\s+([0-9]+) kB$").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "fixed patterns, which the tests compile")]
static INACTIVE_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^inactive_file ([0-9]+)$").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "fixed patterns, which the tests compile")]
static USAGE_USEC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^usage_usec ([0-9]+)$").expect("valid pattern"));

fn field(text: &str, pattern: &Regex) -> Option<i64> {
    pattern.captures(text)?.get(1)?.as_str().parse().ok()
}

#[must_use]
pub fn parse_meminfo(text: &str) -> Option<Meminfo> {
    let total = field(text, &MEM_TOTAL)?;
    let available = field(text, &MEM_AVAILABLE)?;

    Some(Meminfo { total_bytes: total.saturating_mul(1024), available_bytes: available.saturating_mul(1024) })
}

/// The kernel's figures, when no meminfo file is readable.
#[cfg(target_os = "linux")]
fn system_memory() -> Meminfo {
    nix::sys::sysinfo::sysinfo().map_or(Meminfo { total_bytes: 0, available_bytes: 0 }, |info| Meminfo {
        total_bytes: i64::try_from(info.ram_total()).unwrap_or(i64::MAX),
        available_bytes: i64::try_from(info.ram_unused()).unwrap_or(i64::MAX),
    })
}

#[cfg(not(target_os = "linux"))]
fn system_memory() -> Meminfo {
    Meminfo { total_bytes: 0, available_bytes: 0 }
}

/// A number as `/proc/stat` writes it; `NaN` for anything else.
fn stat_number(text: &str) -> f64 {
    if text.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-')) { text.parse().unwrap_or(f64::NAN) } else { f64::NAN }
}

/// What the CPU readings and reports need, so tests can count them.
#[async_trait]
pub trait Sampler: Send {
    /// One CPU reading; the report averages them.
    async fn sample_cpu(&mut self, now: i64);
    /// Memory as it is now and the CPU window since the previous report, which this empties.
    async fn report(&mut self, now: i64) -> ResourceStatus;
}

/// The files one reading takes, read together on a blocking thread: through lxcfs, which renders
/// meminfo in an LXC, a read can hang, and on the runtime thread it would stop the agent.
#[derive(Default)]
struct Readings {
    cpu_stat: Option<String>,
    cpu_max: Option<String>,
    stat: Option<String>,
    memory: Option<MemoryFiles>,
}

struct MemoryFiles {
    current: Option<i64>,
    stat: Option<String>,
    max: Option<i64>,
    meminfo: Meminfo,
}

fn read_files(paths: &ResourcePaths, memory: bool) -> Readings {
    let (cpu_stat, cpu_max, stat) = (read_text(&paths.cgroup.join("cpu.stat")), read_text(&paths.cgroup.join("cpu.max")), read_text(&paths.stat));
    let memory = memory.then(|| MemoryFiles {
        current: read_number(&paths.cgroup.join("memory.current")),
        stat: read_text(&paths.cgroup.join("memory.stat")),
        max: read_number(&paths.cgroup.join("memory.max")),
        meminfo: paths.meminfo.iter().find_map(|file| read_text(file).and_then(|text| parse_meminfo(&text))).unwrap_or_else(system_memory),
    });

    Readings { cpu_stat, cpu_max, stat, memory }
}

fn memory_usage(files: &MemoryFiles) -> ResourceUsage {
    let meminfo = files.meminfo;

    if let (Some(current), Some(stat)) = (files.current, &files.stat) {
        // What the platform's gauge counts: resident pages, less the file cache it can drop.
        let used = current - field(stat, &INACTIVE_FILE).unwrap_or(0);

        return ResourceUsage { cap: files.max.unwrap_or(meminfo.total_bytes), used: used.max(0) };
    }

    ResourceUsage { cap: meminfo.total_bytes, used: (meminfo.total_bytes - meminfo.available_bytes).max(0) }
}

#[derive(Clone, Copy)]
struct CpuSample {
    at: i64,
    busy: f64,
    total: f64,
}

/// Samples what the environment has and uses, the way its tools report it. In an LXC the root
/// cgroup carries the limit and the usage (lxcfs renders meminfo per reader, so a container inside
/// it sees only itself); on bare metal the root cgroup has no `memory.current`, and meminfo and
/// `/proc/stat`, both the host's inside a container, answer instead.
pub struct ResourceSampler {
    /// The files each reading takes.
    paths: ResourcePaths,
    /// The CPU count, used when the cgroup sets no quota.
    cpus: f64,
    /// The last CPU sample, which the next one is measured against.
    previous: Option<CpuSample>,
    /// Busy percentages of the samples taken since the last report.
    window: Vec<f64>,
}

impl Default for ResourceSampler {
    fn default() -> Self {
        Self::new(ResourcePaths::default(), std::thread::available_parallelism().map_or(1, std::num::NonZero::get))
    }
}

impl ResourceSampler {
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "a CPU count")]
    pub fn new(paths: ResourcePaths, cpus: usize) -> Self {
        Self { paths, cpus: cpus as f64, previous: None, window: Vec::new() }
    }

    async fn read(&self, memory: bool) -> Readings {
        let paths = self.paths.clone();

        tokio::task::spawn_blocking(move || read_files(&paths, memory)).await.unwrap_or_default()
    }

    /// Busy time over the interval, per CPU the environment may use.
    fn cpu(&mut self, now: i64, readings: &Readings) -> Option<f64> {
        let sample = self.cpu_sample(now, readings)?;
        let previous = self.previous.replace(sample)?;

        if sample.total <= previous.total {
            return None;
        }

        let percent = (sample.busy - previous.busy) / (sample.total - previous.total) * 100.0;

        if percent.is_nan() {
            return None;
        }

        Some(percent.round().clamp(0.0, 100.0))
    }

    #[allow(clippy::cast_precision_loss, reason = "microseconds and milliseconds stay far below 2^53")]
    fn cpu_sample(&self, now: i64, readings: &Readings) -> Option<CpuSample> {
        let usage = readings.cpu_stat.as_deref().and_then(|text| field(text, &USAGE_USEC));

        if let Some(usage) = usage {
            let quota = readings.cpu_max.clone().unwrap_or_default();
            let quota: Vec<&str> = quota.split_whitespace().collect();
            let cores = match (quota.first().and_then(|limit| digits(limit)), quota.get(1)) {
                (Some(limit), Some(period)) => limit as f64 / stat_number(period),
                _ => self.cpus,
            };
            // Wall time in microseconds times the cores available is the budget the usage is measured against.
            let wall = self.previous.map_or(0.0, |previous| (now - previous.at) as f64 * 1000.0 * cores);

            return Some(CpuSample { at: now, busy: usage as f64, total: self.previous.map_or(0.0, |previous| previous.total) + wall });
        }

        let stat = readings.stat.as_deref()?;
        let fields: Vec<f64> = stat.split('\n').next().unwrap_or_default().split_whitespace().skip(1).map(stat_number).collect();

        if fields.len() < 4 || fields.iter().any(|field| field.is_nan()) {
            return None;
        }

        let idle = fields.get(3).copied().unwrap_or(0.0) + fields.get(4).copied().unwrap_or(0.0);
        // `guest` and `guest_nice`, after the first 8, are already counted in `user` and `nice`.
        let total: f64 = fields.iter().take(8).sum();

        Some(CpuSample { at: now, busy: total - idle, total })
    }
}

#[async_trait]
impl Sampler for ResourceSampler {
    async fn sample_cpu(&mut self, now: i64) {
        let readings = self.read(false).await;

        if let Some(percent) = self.cpu(now, &readings) {
            self.window.push(percent);
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, reason = "percentages from 0 to 100")]
    async fn report(&mut self, now: i64) -> ResourceStatus {
        let readings = self.read(true).await;

        if let Some(percent) = self.cpu(now, &readings) {
            self.window.push(percent);
        }

        let memory = readings
            .memory
            .as_ref()
            .map_or_else(|| memory_usage(&MemoryFiles { current: None, stat: None, max: None, meminfo: system_memory() }), memory_usage);
        let samples = std::mem::take(&mut self.window);
        let cpu = (!samples.is_empty()).then(|| ResourceTickCpu {
            max: samples.iter().copied().fold(f64::NEG_INFINITY, f64::max) as i64,
            average: (samples.iter().sum::<f64>() / samples.len() as f64).round() as i64,
        });

        ResourceStatus { memory, cpu }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Flushed {
    pub ticks: Vec<ResourceTick>,
    pub reset: bool,
}

/// The last `held` ticks. While a lease runs, each tick sends the ticks since the last frame, or the
/// whole ring after a lease starts, a replay or a new connection; nothing goes out otherwise. Every
/// `watch_resources` restarts the one lease, so any number of dashboards, or a burst of frames, count
/// as one watcher; silence ends it by itself.
pub struct ResourceBuffer {
    /// How many ticks the ring keeps.
    held: usize,
    /// How long a `watch_resources` keeps the lease running.
    lease: Duration,
    /// The ring, oldest first.
    ticks: Vec<ResourceTick>,
    /// When the lease ends, in wall-clock milliseconds.
    watched_until: i64,
    /// Ticks pushed since the last flush; `None` until the next flush sends the whole ring. A count,
    /// not the last tick's time, because the wall clock can step back, and a tick from before the
    /// step is still new.
    unsent: Option<usize>,
}

impl ResourceBuffer {
    #[must_use]
    pub fn new(held: usize, lease: Duration) -> Self {
        Self { held, lease, ticks: Vec::new(), watched_until: 0, unsent: None }
    }

    #[must_use]
    pub fn watched(&self, now: i64) -> bool {
        now < self.watched_until
    }

    /// Renews the lease; true when it was not running, which is when the ring should leave at once.
    pub fn watch(&mut self, now: i64) -> bool {
        let started = !self.watched(now);

        self.watched_until = now.saturating_add(i64::try_from(self.lease.as_millis()).unwrap_or(i64::MAX));
        started
    }

    pub fn push(&mut self, tick: ResourceTick) {
        self.ticks.push(tick);

        let excess = self.ticks.len().saturating_sub(self.held);

        self.ticks.drain(..excess);

        if let Some(unsent) = &mut self.unsent {
            *unsent += 1;
        }
    }

    /// While a lease runs, or when forced: the whole ring when forced or after [`Self::resend`], else
    /// the ticks since the last flush. `None` when there is nothing to send.
    pub fn flush(&mut self, now: i64, force: bool) -> Option<Flushed> {
        if !force && !self.watched(now) {
            return None;
        }

        let unsent = if force { None } else { self.unsent };
        let ticks = match unsent {
            None => self.ticks.clone(),
            Some(unsent) => self.ticks.get(self.ticks.len().saturating_sub(unsent)..).unwrap_or_default().to_vec(),
        };

        if ticks.is_empty() {
            return None;
        }

        self.unsent = Some(0);

        Some(Flushed { ticks, reset: unsent.is_none() })
    }

    /// A new connection holds nothing, so the next flush sends the whole ring.
    pub fn resend(&mut self) {
        self.unsent = None;
    }
}

/// The disk figure as last read, which a tick takes without asking the disk: a hung disk would hold
/// the thread that asks.
pub type ReadDisk = Arc<dyn Fn() -> Option<ResourceUsage> + Send + Sync>;

pub struct ResourceReporterOptions {
    /// Nothing is flushed while it is not connected.
    pub transport: Arc<dyn Transport>,
    /// Takes the CPU and memory readings.
    pub sampler: Box<dyn Sampler>,
    /// The disk figure each tick carries.
    pub read_disk: ReadDisk,
    /// The wall clock that stamps each tick.
    pub now: Clock,
    /// Ends the sampling once cancelled.
    pub stop: CancellationToken,
}

impl ResourceReporterOptions {
    /// The real sampler and clock.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>, read_disk: ReadDisk) -> Self {
        Self { transport, sampler: Box::new(ResourceSampler::default()), read_disk, now: wall_clock(), stop: CancellationToken::new() }
    }
}

struct ReporterState {
    /// Taken by the sampling task, which owns it, so a reading that waits on the disk holds no lock.
    sampler: Option<Box<dyn Sampler>>,
    /// The ticks held for the controller's watch lease.
    buffer: ResourceBuffer,
    /// The sampling task.
    timer: Option<JoinHandle<()>>,
    /// CPU samples taken; every `SAMPLES_PER_TICK`th one is a tick.
    samples: u64,
}

struct ReporterInner {
    transport: Arc<dyn Transport>,
    read_disk: ReadDisk,
    now: Clock,
    stop: CancellationToken,
    state: Mutex<ReporterState>,
}

/// The node's resource ticks: a CPU sample every 500 ms, a tick every `RESOURCE_TICK`, and a
/// `resource_batch` each tick while the controller's watch lease runs.
#[derive(Clone)]
pub struct ResourceReporter {
    inner: Arc<ReporterInner>,
}

impl ResourceReporter {
    #[must_use]
    pub fn new(options: ResourceReporterOptions) -> Self {
        let buffer = ResourceBuffer::new(usize::try_from(RESOURCE_TICKS_HELD).unwrap_or(usize::MAX), WATCH_LEASE);

        Self {
            inner: Arc::new(ReporterInner {
                transport: options.transport,
                read_disk: options.read_disk,
                now: options.now,
                stop: options.stop,
                state: Mutex::new(ReporterState { sampler: Some(options.sampler), buffer, timer: None, samples: 0 }),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, ReporterState> {
        lock(&self.inner.state)
    }

    pub fn start(&self) {
        let mut state = self.state();

        let Some(mut sampler) = state.sampler.take() else {
            return;
        };
        let (weak, stop) = (Arc::downgrade(&self.inner), self.inner.stop.clone());

        state.timer = Some(tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(Instant::now() + CPU_SAMPLE, CPU_SAMPLE);

            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);

            while stop.run_until_cancelled(ticks.tick()).await.is_some() {
                let Some(inner) = weak.upgrade() else {
                    return;
                };

                ResourceReporter { inner }.sample(sampler.as_mut()).await;
            }
        }));
    }

    /// A `watch_resources` from the controller renews the lease; one that starts it, or a `replay`, gets the
    /// whole ring at once.
    pub fn watch(&self, replay: bool) {
        let now = (self.inner.now)();
        let started = self.state().buffer.watch(now);

        if started || replay {
            self.flush(true);
        }
    }

    /// A new connection holds nothing, so the next frame carries the whole ring.
    pub fn resend(&self) {
        self.state().buffer.resend();
    }

    async fn sample(&self, sampler: &mut dyn Sampler) {
        let now = (self.inner.now)();
        let tick = {
            let mut state = self.state();

            state.samples += 1;
            state.samples.is_multiple_of(SAMPLES_PER_TICK)
        };

        if !tick {
            sampler.sample_cpu(now).await;
            return;
        }

        let status = sampler.report(now).await;
        let disk = (self.inner.read_disk)();

        self.state().buffer.push(ResourceTick { at: now, disk, memory: status.memory, cpu: status.cpu });
        self.flush(false);
    }

    fn flush(&self, force: bool) {
        if !self.inner.transport.connected() {
            return;
        }

        let now = (self.inner.now)();
        let flushed = self.state().buffer.flush(now, force);

        if let Some(Flushed { ticks, reset }) = flushed {
            self.inner.transport.send(AgentToControllerMessage::ResourceBatch(AgentResourceBatch { ticks, reset }));
        }
    }
}

/// A filesystem's size, in blocks of `frsize` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsStats {
    /// The unit the block counts are in: the fragment size, not the preferred I/O size, which some
    /// filesystems report larger.
    pub frsize: u64,
    /// The filesystem's size in blocks.
    pub blocks: u64,
    /// Free blocks, root's reserve included.
    pub bfree: u64,
    /// Free blocks a process without root's reserve may use.
    pub bavail: u64,
}

/// Counts blocks in the fragment size, as `df` does.
///
/// # Errors
///
/// Fails when the directory cannot be read.
#[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions, reason = "the field types differ between platforms")]
pub(crate) fn statvfs(dir: &Path) -> std::io::Result<FsStats> {
    let stat = nix::sys::statvfs::statvfs(dir)?;

    Ok(FsStats {
        frsize: u64::try_from(stat.fragment_size()).unwrap_or(0),
        blocks: u64::try_from(stat.blocks()).unwrap_or(0),
        bfree: u64::try_from(stat.blocks_free()).unwrap_or(0),
        bavail: u64::try_from(stat.blocks_available()).unwrap_or(0),
    })
}

/// The filesystem as `df` counts it.
#[must_use]
pub fn disk_usage(stat: &FsStats) -> ResourceUsage {
    let bytes = |blocks: u64| i64::try_from(blocks.saturating_mul(stat.frsize)).unwrap_or(i64::MAX);

    ResourceUsage { cap: bytes(stat.blocks), used: bytes(stat.blocks.saturating_sub(stat.bfree)) }
}

/// `PRETTY_NAME`, or `NAME`, out of an os-release file; `None` when it names neither.
#[must_use]
pub fn parse_os_release(text: &str) -> Option<String> {
    let field = |key: &str| -> Option<String> {
        let pattern = Regex::new(&format!(r"(?m)^{key}=(.*)$")).ok()?;
        let value = pattern.captures(text)?.get(1)?.as_str().trim();
        let unquoted = value
            .chars()
            .next()
            .filter(|quote| matches!(quote, '"' | '\'') && value.len() >= 2 && value.ends_with(*quote))
            .and_then(|_| value.get(1..value.len() - 1))
            .unwrap_or(value);

        (!unquoted.is_empty()).then(|| unquoted.to_owned())
    };

    field("PRETTY_NAME").or_else(|| field("NAME")).map(|name| truncate(&name, OS_NAME_MAX_CHARS))
}

/// The host's distribution, from the os-release node-setup mounts; the container's names the image.
pub async fn read_os_name(file: &Path) -> Option<String> {
    parse_os_release(&tokio::fs::read_to_string(file).await.ok()?)
}
