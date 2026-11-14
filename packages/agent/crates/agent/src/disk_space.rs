//! Whether the disk under the data directory has room for a match: its events, logs and demo. A
//! node short of space refuses launches instead of losing a match's records part way through.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::constants::DISK_SPACE_MIN_BYTES;
use ankor_protocol::types::{NodeUnavailable, NodeUnavailableCode, ResourceUsage};
use futures_util::FutureExt as _;

use crate::resources::{FsStats, disk_usage};
use crate::sync::{SharedRun, join_or_spawn, lock};

/// How often the disk is asked, so a node that frees space takes launches again soon after.
pub const DISK_CHECK: Duration = Duration::from_secs(30);
/// A `statvfs` answers immediately, so a read past this limit means the disk hangs. Launches then
/// use the last known figure instead of waiting.
pub const DISK_READ_LIMIT: Duration = Duration::from_secs(5);

pub type ReadFs = Arc<dyn Fn(&Path) -> std::io::Result<FsStats> + Send + Sync>;

/// Called with the new state on each change, `None` once there is room again.
pub type SpaceChanged = Arc<dyn Fn(Option<&NodeUnavailable>) + Send + Sync>;

type Read = SharedRun<()>;

#[derive(Default)]
struct State {
    /// The disk last had less than `DISK_SPACE_MIN_BYTES` free.
    short: bool,
    /// `None` when the disk could not be read.
    usage: Option<ResourceUsage>,
    /// The read under way, which every caller joins, so a hung disk holds one blocking thread.
    reading: Option<Read>,
}

struct Inner {
    dir: PathBuf,
    read: ReadFs,
    changed: SpaceChanged,
    state: Mutex<State>,
}

/// A launch reads the last known space instead of asking the disk, because a hung disk would hold
/// the thread that asks. The disk is asked on a blocking thread every [`DISK_CHECK`] and before each
/// launch.
#[derive(Clone)]
pub struct DiskSpace {
    inner: Arc<Inner>,
    read_limit: Duration,
}

impl DiskSpace {
    /// `changed` is called on every change of [`Self::unavailable`].
    #[must_use]
    pub fn new(dir: PathBuf, read: ReadFs, changed: SpaceChanged) -> Self {
        Self { inner: Arc::new(Inner { dir, read, changed, state: Mutex::default() }), read_limit: DISK_READ_LIMIT }
    }

    #[must_use]
    pub fn with_read_limit(&self, limit: Duration) -> Self {
        Self { read_limit: limit, ..self.clone() }
    }

    /// `None` while there was room, and before the disk was first asked.
    #[must_use]
    pub fn unavailable(&self) -> Option<NodeUnavailable> {
        lock(&self.inner.state).short.then(|| self.short())
    }

    /// The resource ticks report this figure without asking the disk themselves.
    #[must_use]
    pub fn usage(&self) -> Option<ResourceUsage> {
        lock(&self.inner.state).usage.clone()
    }

    /// The read runs in a separate task, so it applies its answer even when every caller stopped
    /// waiting. A disk that cannot be read counts as having room, so an unreadable figure refuses
    /// no launch. A disk that does not answer in time leaves the last figure as it was.
    pub async fn refresh(&self) {
        let reading = join_or_spawn(&self.inner.state, |state| &mut state.reading, || self.clone().ask().boxed());
        let _ = tokio::time::timeout(self.read_limit, reading).await;
    }

    async fn ask(self) {
        let (dir, read) = (self.inner.dir.clone(), Arc::clone(&self.inner.read));
        let stats = tokio::task::spawn_blocking(move || read(&dir)).await.ok().and_then(Result::ok);
        let short = stats.as_ref().is_some_and(|stats| stats.bavail.saturating_mul(stats.frsize) < DISK_SPACE_MIN_BYTES);
        let changed = {
            let mut state = lock(&self.inner.state);

            state.reading = None;
            state.usage = stats.as_ref().map(disk_usage);
            std::mem::replace(&mut state.short, short) != short
        };

        if changed {
            (self.inner.changed)(short.then(|| self.short()).as_ref());
        }
    }

    fn short(&self) -> NodeUnavailable {
        NodeUnavailable {
            code: NodeUnavailableCode::DiskSpace,
            operation: None,
            detail: format!("less than {} MiB free under {}", DISK_SPACE_MIN_BYTES >> 20, self.inner.dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const BLOCK: u64 = 4096;

    type Seen = Arc<Mutex<Vec<Option<String>>>>;

    /// A disk whose free blocks the test sets, or that cannot be read while they are `u64::MAX`.
    fn disk() -> (DiskSpace, Arc<AtomicU64>, Seen) {
        let free = Arc::new(AtomicU64::new(DISK_SPACE_MIN_BYTES / BLOCK));
        let reading = Arc::clone(&free);
        let seen: Seen = Arc::default();
        let record = Arc::clone(&seen);
        let space = DiskSpace::new(
            PathBuf::from("/srv/ankor"),
            Arc::new(move |_| match reading.load(Ordering::SeqCst) {
                u64::MAX => Err(std::io::Error::other("no statvfs")),
                blocks => Ok(FsStats { frsize: BLOCK, blocks: 1 << 30, bfree: blocks + 100, bavail: blocks }),
            }),
            Arc::new(move |state: Option<&NodeUnavailable>| lock(&record).push(state.map(|state| state.detail.clone()))),
        );

        (space, free, seen)
    }

    #[tokio::test]
    async fn refuses_below_the_minimum_counting_only_space_without_roots_reserve_and_reports_each_change_once() {
        let (space, free, seen) = disk();

        space.refresh().await;
        assert_eq!(space.unavailable(), None);

        free.store(DISK_SPACE_MIN_BYTES / BLOCK - 1, Ordering::SeqCst);
        space.refresh().await;

        let short = space.unavailable().unwrap();

        assert_eq!(short.code, NodeUnavailableCode::DiskSpace);
        assert_eq!(short.detail, "less than 1024 MiB free under /srv/ankor");
        space.refresh().await;
        assert!(space.unavailable().is_some());

        free.store(DISK_SPACE_MIN_BYTES / BLOCK, Ordering::SeqCst);
        space.refresh().await;
        assert_eq!(space.unavailable(), None);
        assert_eq!(*lock(&seen), [Some(short.detail), None]);
    }

    #[tokio::test]
    async fn takes_launches_when_the_disk_cannot_be_asked() {
        let (space, free, seen) = disk();

        free.store(0, Ordering::SeqCst);
        space.refresh().await;
        assert!(space.unavailable().is_some());
        free.store(u64::MAX, Ordering::SeqCst);
        space.refresh().await;
        assert_eq!(space.unavailable(), None);
        assert_eq!(lock(&seen).len(), 2);
    }

    #[tokio::test]
    async fn reads_the_last_known_space_without_asking_the_disk() {
        let (space, free, seen) = disk();

        free.store(0, Ordering::SeqCst);
        assert_eq!(space.unavailable(), None);
        space.refresh().await;
        free.store(u64::MAX - 1, Ordering::SeqCst);
        assert!(space.unavailable().is_some());
        assert_eq!(lock(&seen).len(), 1);
    }

    #[tokio::test]
    async fn keeps_the_last_figure_while_the_disk_hangs_and_applies_the_read_once_it_answers() {
        let free = Arc::new(AtomicU64::new(0));
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        let asked = Arc::new(AtomicU64::new(0));
        let (reading, waiting, counted) = (Arc::clone(&free), Arc::clone(&released), Arc::clone(&asked));
        let space = DiskSpace::new(
            PathBuf::from("/srv/ankor"),
            Arc::new(move |_| {
                if counted.fetch_add(1, Ordering::SeqCst) == 1 {
                    let _ = lock(&waiting).recv();
                }

                Ok(FsStats { frsize: BLOCK, blocks: 1 << 30, bfree: 100, bavail: reading.load(Ordering::SeqCst) })
            }),
            Arc::new(|_: Option<&NodeUnavailable>| {}),
        );
        // Only the reads meant to hang are cut short, so a slow machine cannot fail the first.
        let impatient = space.with_read_limit(Duration::from_millis(20));

        space.refresh().await;
        assert!(space.unavailable().is_some());

        free.store(DISK_SPACE_MIN_BYTES / BLOCK, Ordering::SeqCst);

        let started = tokio::time::Instant::now();

        impatient.refresh().await;
        impatient.refresh().await;
        assert!(started.elapsed() < DISK_READ_LIMIT, "the shorter read limit applies");
        assert!(space.unavailable().is_some(), "a hung read keeps the last figure");
        assert_eq!(asked.load(Ordering::SeqCst), 2, "a hung disk is not asked again");

        release.send(()).unwrap();
        while lock(&space.inner.state).reading.is_some() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(space.unavailable(), None, "the read applies once the disk answers");
    }

    #[tokio::test]
    async fn keeps_the_disk_figure_of_the_last_read_for_the_resource_ticks() {
        let (space, free, _) = disk();

        assert_eq!(space.usage(), None);
        space.refresh().await;

        let blocks = 1_i64 << 30;
        let bfree = i64::try_from(DISK_SPACE_MIN_BYTES / BLOCK + 100).unwrap();
        let block = i64::try_from(BLOCK).unwrap();

        assert_eq!(space.usage(), Some(ResourceUsage { cap: blocks * block, used: (blocks - bfree) * block }));
        free.store(u64::MAX, Ordering::SeqCst);
        space.refresh().await;
        assert_eq!(space.usage(), None);
    }
}
