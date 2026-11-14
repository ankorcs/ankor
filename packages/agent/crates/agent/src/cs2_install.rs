//! The installed CS2 build, read from the install's records on the game volume, which the agent's
//! container mounts read-only.

use crate::sync::lock;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use ankor_protocol::schemas::s_build_id;
use ankor_protocol::validate::safe_parse;
use futures_util::FutureExt as _;
use futures_util::future::{BoxFuture, Shared};
use regex::Regex;
use serde_json::Value;

use crate::logger::Logger;

/// CS2's Steam app id. It names the app manifest, the entry in steamcmd's app info and the app a
/// login token logs on to.
pub(crate) const CS2_APP_ID: u32 = 730;

/// The key of the build in the update's completion record.
pub(crate) const INSTALLED_BUILD_KEY: &str = "buildid";

/// Where the image's update mode records the build it installed, once steamcmd is done, relative to
/// the install.
const INSTALLED_RECORD: &str = ".ankor/installed.json";

/// Steam's app manifest, which steamcmd writes as a download starts, relative to the install.
fn app_manifest() -> String {
    format!("steamapps/appmanifest_{CS2_APP_ID}.acf")
}

/// The completion record as an update container sees it, which the test doubles write.
#[cfg(any(test, feature = "testing"))]
#[must_use]
pub(crate) fn installed_record_path() -> String {
    format!("{}/{INSTALLED_RECORD}", crate::host_layout::INSTALL_DIR)
}

/// How long the install's records may take to read. Two small local files take far less, so a read
/// this slow means the disk has failed, and launches and status reports must not wait on it.
const BUILD_READ_LIMIT: Duration = Duration::from_secs(5);

/// The `StateFlags` of an app Steam has fully installed.
const STATE_FULLY_INSTALLED: &str = "4";
/// The `buildid` of an app Steam has not installed.
const BUILD_NOT_INSTALLED: &str = "0";

#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static STATE_FLAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)"StateFlags"\s+"(\d+)""#).expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static BUILD_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)"buildid"\s+"(\d+)""#).expect("valid pattern"));

fn from_installed_json(text: Option<&str>) -> Option<String> {
    let record: Value = serde_json::from_str(text?.trim()).ok()?;
    let build = record.get(INSTALLED_BUILD_KEY)?.clone();

    safe_parse(s_build_id, build).ok()?.as_str().map(str::to_owned)
}

/// The completion record's build, else the app manifest's. steamcmd writes the manifest as soon as
/// a download starts, with build 0 and `StateFlags` other than 4, so only a finished install
/// names a build. A build id does not verify the files or rule out an update in progress.
fn parse_cs2_build_id(installed: Option<&str>, manifest: Option<&str>) -> Option<String> {
    if let Some(build) = from_installed_json(installed) {
        return Some(build);
    }

    let manifest = manifest.unwrap_or_default();
    let flags = STATE_FLAGS.captures(manifest).and_then(|captures| captures.get(1)).map(|flags| flags.as_str().to_owned());
    let build = BUILD_ID.captures(manifest).and_then(|captures| captures.get(1)).map(|build| build.as_str().to_owned())?;

    (build != BUILD_NOT_INSTALLED && flags.is_none_or(|flags| flags == STATE_FULLY_INSTALLED)).then_some(build)
}

/// A file the install does not have yet is not an error.
async fn read_record(path: &Path, log: &Logger) -> Option<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            log.warn(format!("could not read {}: {error}", path.display()));
            None
        }
    }
}

async fn read_cs2_install(install: &Path, log: &Logger) -> Option<String> {
    let read = async {
        let installed = read_record(&install.join(INSTALLED_RECORD), log).await;
        let manifest = read_record(&install.join(app_manifest()), log).await;

        parse_cs2_build_id(installed.as_deref(), manifest.as_deref())
    };

    if let Ok(build) = tokio::time::timeout(BUILD_READ_LIMIT, read).await {
        return build;
    }

    log.warn(format!("the CS2 install at {} did not answer within {} s", install.display(), BUILD_READ_LIMIT.as_secs()));
    None
}

type BuildRead = Shared<BoxFuture<'static, Option<String>>>;

/// The installed build's reads, numbered so a read that found none clears only its own entry.
#[derive(Default)]
struct BuildReads {
    /// The number the next read starts under.
    next: u64,
    /// The read under way or done, with the number it was started under.
    current: Option<(u64, BuildRead)>,
}

/// The installed build, read once until an update container's end may have changed it. A read that
/// found none is not kept.
#[derive(Clone)]
pub(crate) struct InstalledBuild {
    /// Where the agent's container mounts the game volume.
    install: Arc<PathBuf>,
    /// Where a record that cannot be read is logged.
    log: Logger,
    /// The read under way or kept.
    reads: Arc<Mutex<BuildReads>>,
}

impl InstalledBuild {
    #[must_use]
    pub fn new(install: PathBuf, log: Logger) -> Self {
        Self { install: Arc::new(install), log, reads: Arc::default() }
    }

    /// The read runs in a spawned task, so it ends and is kept even when every caller stops waiting.
    pub fn get(&self) -> BoxFuture<'static, Option<String>> {
        let mut reads = lock(&self.reads);

        if let Some((_, pending)) = reads.current.as_ref() {
            return pending.clone().boxed();
        }

        let ticket = reads.next;
        reads.next = ticket.wrapping_add(1);
        let (install, log, cache) = (Arc::clone(&self.install), self.log.clone(), Arc::clone(&self.reads));
        let pending = async move {
            let build = read_cs2_install(&install, &log).await;
            let mut cache = lock(&cache);

            // Clear only its own read, because one a forget started since may have found the build.
            if build.is_none() && cache.current.as_ref().is_some_and(|(kept, _)| *kept == ticket) {
                cache.current = None;
            }

            build
        }
        .boxed()
        .shared();

        reads.current = Some((ticket, pending.clone()));
        drop(reads);
        tokio::spawn(pending.clone());
        pending.boxed()
    }

    /// Reads again on the next call, as after an update container's end.
    pub fn forget(&self) {
        lock(&self.reads).current = None;
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
mod tests {
    use super::*;

    fn app_manifest_text(build_id: &str, state_flags: &str) -> String {
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"730\"\n\t\"Universe\"\t\t\"1\"\n\t\"StateFlags\"\t\t\"{state_flags}\"\n\t\"buildid\"\t\t\"{build_id}\"\n\t\"LastUpdated\"\t\t\"1757000000\"\n}}\n"
        )
    }

    fn installed_json(fields: &serde_json::Value) -> String {
        format!("{}\n", serde_json::to_string_pretty(fields).unwrap())
    }

    /// An install folder holding the given records, as the game volume would.
    fn install(installed: Option<&str>, manifest: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        if let Some(installed) = installed {
            let path = dir.path().join(INSTALLED_RECORD);

            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, installed).unwrap();
        }

        if let Some(manifest) = manifest {
            let path = dir.path().join(app_manifest());

            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, manifest).unwrap();
        }

        dir
    }

    async fn read(installed: Option<&str>, manifest: Option<&str>) -> Option<String> {
        let dir = install(installed, manifest);

        read_cs2_install(dir.path(), &Logger::null()).await
    }

    #[tokio::test]
    async fn reads_the_build_update_mode_recorded() {
        let installed = installed_json(&serde_json::json!({ "buildid": "20037731", "installed_at": "2026-09-09T10:11:12Z" }));

        assert_eq!(read(Some(&installed), None).await.as_deref(), Some("20037731"));
    }

    #[tokio::test]
    async fn falls_back_to_the_app_manifest() {
        assert_eq!(read(None, Some(&app_manifest_text("19999888", "4"))).await.as_deref(), Some("19999888"));
    }

    #[tokio::test]
    async fn prefers_the_completion_record() {
        let installed = installed_json(&serde_json::json!({ "buildid": "20037731" }));

        assert_eq!(read(Some(&installed), Some(&app_manifest_text("19999888", "4"))).await.as_deref(), Some("20037731"));
    }

    #[tokio::test]
    async fn falls_back_when_the_record_names_no_usable_build() {
        for installed in [
            installed_json(&serde_json::json!({ "installed_at": "2026-09-09T10:11:12Z" })),
            installed_json(&serde_json::json!({ "buildid": "unknown" })),
            "{ this is not json".to_owned(),
        ] {
            assert_eq!(read(Some(&installed), Some(&app_manifest_text("19999888", "4"))).await.as_deref(), Some("19999888"), "{installed}");
        }
    }

    #[tokio::test]
    async fn reports_nothing_without_either_file() {
        assert_eq!(read(None, None).await, None);
    }

    /// Steam names the manifest after the app, and steamcmd writes it under `steamapps`.
    #[tokio::test]
    async fn reads_the_manifest_where_steam_writes_it() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(dir.path().join("steamapps")).unwrap();
        std::fs::write(dir.path().join("steamapps/appmanifest_730.acf"), app_manifest_text("19999888", "4")).unwrap();

        assert_eq!(read_cs2_install(dir.path(), &Logger::null()).await.as_deref(), Some("19999888"));
    }

    /// An install that has neither record yet is normal before the first update, so nothing is logged.
    #[tokio::test]
    async fn logs_nothing_for_records_the_install_does_not_have_yet() {
        let dir = install(None, None);
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let log = Logger::new("test", Some(Arc::new(move |entry: crate::logger::Entry| seen.lock().unwrap().push(entry.message))));

        assert_eq!(read_cs2_install(dir.path(), &log).await, None);
        assert!(lines.lock().unwrap().is_empty(), "{:?}", lines.lock().unwrap());
    }

    #[tokio::test]
    async fn names_no_build_from_a_manifest_still_downloading() {
        for (build, flags) in [("0", "1026"), ("25492732", "6"), ("25492732", "1026"), ("0", "4")] {
            assert_eq!(read(None, Some(&app_manifest_text(build, flags))).await, None, "{build} {flags}");
        }
    }

    #[tokio::test]
    async fn takes_the_build_of_a_manifest_without_state_flags() {
        let manifest: String = app_manifest_text("25492732", "4").lines().filter(|line| !line.contains("StateFlags")).collect::<Vec<_>>().join("\n");

        assert_eq!(read(None, Some(&manifest)).await.as_deref(), Some("25492732"));
    }

    /// A record that exists but cannot be read is logged, and the read goes on as if it were missing.
    #[tokio::test]
    async fn logs_a_record_it_cannot_read_and_falls_back() {
        let dir = install(None, Some(&app_manifest_text("19999888", "4")));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let log = Logger::new("test", Some(Arc::new(move |entry: crate::logger::Entry| seen.lock().unwrap().push(entry.message))));

        // A folder in the record's place, which a file read refuses.
        std::fs::create_dir_all(dir.path().join(INSTALLED_RECORD)).unwrap();

        assert_eq!(read_cs2_install(dir.path(), &log).await.as_deref(), Some("19999888"));
        assert_eq!(lines.lock().unwrap().len(), 1);
        assert!(lines.lock().unwrap()[0].contains("could not read"), "{:?}", lines.lock().unwrap());
    }

    /// A record that never answers, a named pipe with no writer here, ends the read after the limit
    /// with no build, which is logged and not kept. On the real clock, because a paused one does
    /// not advance while the stuck read holds a blocking thread.
    #[tokio::test]
    async fn gives_up_on_a_read_that_does_not_answer() {
        let dir = install(None, None);
        let pipe = dir.path().join(INSTALLED_RECORD);
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let log = Logger::new("test", Some(Arc::new(move |entry: crate::logger::Entry| seen.lock().unwrap().push(entry.message))));

        std::fs::create_dir_all(pipe.parent().unwrap()).unwrap();
        nix::unistd::mkfifo(&pipe, nix::sys::stat::Mode::S_IRWXU).unwrap();

        let build = InstalledBuild::new(dir.path().to_owned(), log);

        assert_eq!(build.get().await, None);
        assert!(lines.lock().unwrap().iter().any(|line| line.contains("did not answer within 5 s")), "{:?}", lines.lock().unwrap());
        assert!(lock(&build.reads).current.is_none(), "a read that found no build is not kept");

        // Opening the writer lets the stuck read end, so the runtime can shut down.
        drop(std::fs::OpenOptions::new().write(true).open(&pipe).unwrap());
    }

    #[tokio::test]
    async fn finishes_and_keeps_a_read_whose_caller_stopped_waiting() {
        let dir = install(Some(&installed_json(&serde_json::json!({ "buildid": "19000" }))), None);
        let build = InstalledBuild::new(dir.path().to_owned(), Logger::null());

        drop(build.get());

        // The read runs on a blocking thread, so wait for it on the real clock.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);

        while lock(&build.reads).current.as_ref().is_none_or(|(_, read)| read.peek().is_none()) {
            assert!(std::time::Instant::now() < deadline, "the read nobody waited on never finished");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Kept, so a later caller does not read the files again.
        std::fs::remove_file(dir.path().join(INSTALLED_RECORD)).unwrap();
        assert_eq!(build.get().await.as_deref(), Some("19000"));
    }

    /// A read that found nothing clears only itself. One a forget started since may have found the
    /// build, and the next caller should get it.
    #[tokio::test]
    async fn keeps_a_newer_read_when_an_older_one_finds_nothing() {
        let dir = install(None, None);
        let build = InstalledBuild::new(dir.path().to_owned(), Logger::null());
        let older = build.get();

        build.forget();

        let newer = futures_util::future::ready(Some("19000".to_owned())).boxed().shared();

        lock(&build.reads).current = Some((99, newer));
        assert_eq!(older.await, None);
        assert_eq!(lock(&build.reads).current.as_ref().map(|(ticket, _)| *ticket), Some(99));
        assert_eq!(build.get().await.as_deref(), Some("19000"));
    }
}
