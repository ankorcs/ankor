//! The addon overlay: `SwiftlyS2`, the plugin and the Ankor cfgs, copied out of the cs2 image into
//! `<data_dir>/overlay/<image id>` and bind-mounted read-only into every server. It is keyed by
//! image id, so running servers keep the folder they started from (their `ankor.overlay` label)
//! while new launches take a new image's, and unused folders are collected.

use crate::sync::lock;
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::fs;

use crate::container_spec::ContainerRole;
use crate::contract::constants::IMAGE_OVERLAY_DIR;
use crate::docker::DockerPort;
use crate::docker_api::DockerError;
use crate::helper_container::{HelperSpec, helper_spec, short_id};
use crate::host_layout::{MountpointKind, OVERLAY_ADDONS, overlay_mountpoints};
use crate::logger::Logger;

/// Written last, so a directory without it is a failed extraction.
pub const COMPLETE_MARKER: &str = ".complete";

/// The sibling folder an extraction fills before it is renamed into place.
const TMP_SUFFIX: &str = ".tmp";

#[must_use]
pub(crate) fn overlay_root(data_dir: &Path) -> PathBuf {
    data_dir.join("overlay")
}

/// An image id as a directory name: `sha256:1f70…` becomes `sha256_1f70…`.
#[must_use]
pub(crate) fn overlay_dir_name(image_id: &str) -> String {
    image_id.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
}

/// How the name of every extraction folder for `image_id` starts; an attempt adds `-<attempt>`.
fn attempt_prefix(image_id: &str) -> String {
    format!("{}{TMP_SUFFIX}", overlay_dir_name(image_id))
}

/// The overlay folder that an entry of the overlay root belongs to: its own name, or the one an
/// extraction folder is for.
fn owning_overlay(name: &str) -> &str {
    name.split(TMP_SUFFIX).next().unwrap_or(name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedOverlay {
    /// The image the overlay was extracted from.
    pub image_id: String,
    /// The path as dockerd sees it, ready to be a bind source.
    pub dir: PathBuf,
}

/// An overlay a slot uses. Release it only once the slot is idle, a failed launch included.
#[derive(Debug)]
pub struct AcquiredOverlay {
    pub overlay: ExtractedOverlay,
    references: Arc<Mutex<BTreeMap<String, usize>>>,
    released: AtomicBool,
}

/// An acquisition dropped unreleased gives its reference back, so the overlay can be cleaned up.
impl Drop for AcquiredOverlay {
    fn drop(&mut self) {
        self.release();
    }
}

impl AcquiredOverlay {
    pub fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }

        let mut references = lock(&self.references);

        if let Some(count) = references.get_mut(&self.overlay.image_id) {
            *count -= 1;

            if *count == 0 {
                references.remove(&self.overlay.image_id);
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("cannot extract the addon overlay: {image} is not on this host (pull it first)")]
    NotOnHost { image: String },
    #[error("overlay extraction failed: {0}")]
    Copy(#[source] DockerError),
    #[error("overlay extraction failed: no addons/ in {IMAGE_OVERLAY_DIR}")]
    NoAddons,
}

pub struct OverlayStore {
    /// Resolves image ids, copies the overlay out and lists running overlays.
    docker: Arc<dyn DockerPort>,
    /// The agent's data directory, which holds the overlay root.
    data_dir: PathBuf,
    /// Receives extraction and cleanup progress.
    log: Logger,
    /// One extraction or cleanup at a time, so collection never deletes a folder being filled.
    queue: tokio::sync::Mutex<()>,
    /// Acquisitions held per image id, whose folders collection keeps.
    references: Arc<Mutex<BTreeMap<String, usize>>>,
}

impl OverlayStore {
    #[must_use]
    pub fn new(docker: Arc<dyn DockerPort>, data_dir: impl Into<PathBuf>, log: Logger) -> Self {
        Self { docker, data_dir: data_dir.into(), log, queue: tokio::sync::Mutex::new(()), references: Arc::default() }
    }

    /// The overlay for the image as it is on this host now, extracted first when needed.
    ///
    /// # Errors
    ///
    /// Fails when the image is not on the host or the extraction fails.
    pub async fn ensure(&self, image: &str) -> Result<ExtractedOverlay, OverlayError> {
        let _turn = self.queue.lock().await;

        self.prepare(image).await
    }

    /// Like [`OverlayStore::ensure`], registering the slot's use before another extraction can
    /// collect the overlay.
    ///
    /// # Errors
    ///
    /// Fails as [`OverlayStore::ensure`] does.
    pub async fn acquire(&self, image: &str) -> Result<AcquiredOverlay, OverlayError> {
        let _turn = self.queue.lock().await;
        let overlay = self.prepare(image).await?;

        Ok(AcquiredOverlay::counted(overlay, &self.references))
    }

    /// `<data_dir>/overlay/<image id><suffix>`.
    #[must_use]
    pub fn dir(&self, image_id: &str, suffix: &str) -> PathBuf {
        overlay_root(&self.data_dir).join(format!("{}{suffix}", overlay_dir_name(image_id)))
    }

    async fn prepare(&self, image: &str) -> Result<ExtractedOverlay, OverlayError> {
        let image_id = self.docker.image_id(image).await?.ok_or_else(|| OverlayError::NotOnHost { image: image.to_owned() })?;
        let dir = self.dir(&image_id, "");

        // A stat that fails says nothing, and extracting again would wipe a folder running servers mount.
        if fs::try_exists(dir.join(COMPLETE_MARKER)).await? {
            return Ok(ExtractedOverlay { image_id, dir });
        }

        self.extract(&image_id).await?;
        self.collect_garbage(&image_id).await;

        Ok(ExtractedOverlay { image_id, dir })
    }

    /// The marker is written after the rename, so a crash leaves at most a temp folder that the next
    /// attempt wipes. Each attempt gets its own folder, because an unpack that outlived its timeout
    /// keeps running in a blocking thread. It must never write into a folder that a later attempt
    /// marks complete. The copy container is never started, so nothing runs in the image. The
    /// slot's mountpoints are created before the rename, because Docker cannot create them in a
    /// read-only mount. Symlinks are not checked, because the image comes from the same source as
    /// the agent, so a check could not defend against a poisoned image.
    async fn extract(&self, image_id: &str) -> Result<(), OverlayError> {
        let local = self.dir(image_id, "");
        let tmp = overlay_root(&self.data_dir).join(format!("{}-{}", attempt_prefix(image_id), short_id()));

        self.log.info(format!("extracting the addon overlay for {image_id} into {}", local.display()));
        self.remove_attempts(image_id).await?;
        fs::create_dir_all(&tmp).await?;

        let spec = helper_spec(
            HelperSpec { name: "overlay".to_owned(), image: image_id.to_owned(), role: Some(ContainerRole::Overlay), ..HelperSpec::default() },
            &short_id(),
        );
        let unpacked = tmp.join(Path::new(IMAGE_OVERLAY_DIR).file_name().unwrap_or_default());
        let copied = match self.docker.copy_from_image(&spec, IMAGE_OVERLAY_DIR, &tmp).await {
            Err(error) => Err(OverlayError::Copy(error)),
            Ok(()) if !fs::metadata(unpacked.join(OVERLAY_ADDONS)).await.is_ok_and(|meta| meta.is_dir()) => Err(OverlayError::NoAddons),
            Ok(()) => Ok(()),
        };

        if let Err(error) = copied {
            let _ = remove_all(&tmp).await;
            return Err(error);
        }

        self.log.info(format!("overlay: extracted {} files", count_files(&unpacked).await?));

        for (rel_path, kind) in overlay_mountpoints() {
            let target = unpacked.join(rel_path);

            match kind {
                MountpointKind::Directory => fs::create_dir_all(&target).await?,
                MountpointKind::File => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).await?;
                    }

                    // Opened for append, so a file the image ships (the plugin's config) keeps its contents.
                    fs::OpenOptions::new().create(true).append(true).open(&target).await?;
                }
            }
        }

        remove_all(&local).await?;
        fs::rename(&unpacked, &local).await?;
        remove_all(&tmp).await?;
        fs::write(local.join(COMPLETE_MARKER), format!("{image_id}\n")).await?;
        self.log.info(format!("addon overlay for {image_id} ready"));

        Ok(())
    }

    /// Removes the temp folders earlier attempts at this image left. One an unpack still writes
    /// into fails that unpack, which no later attempt reads.
    async fn remove_attempts(&self, image_id: &str) -> Result<(), OverlayError> {
        let prefix = attempt_prefix(image_id);
        let mut entries = match fs::read_dir(overlay_root(&self.data_dir)).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };

        while let Some(entry) = entries.next_entry().await? {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                remove_all(&entry.path()).await?;
            }
        }

        Ok(())
    }

    /// A kept image keeps its temp folders too. A failure costs disk, not a match, so it is only
    /// logged.
    async fn collect_garbage(&self, current: &str) {
        let root = overlay_root(&self.data_dir);
        let result = async {
            let in_use = self.docker.running_overlay_image_ids().await?;
            let held: Vec<String> = lock(&self.references).keys().cloned().collect();
            let keep: BTreeSet<String> = std::iter::once(current.to_owned()).chain(held).chain(in_use).map(|id| overlay_dir_name(&id)).collect();
            let mut entries = fs::read_dir(&root).await?;

            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let image = owning_overlay(&name);

                if keep.contains(image) {
                    continue;
                }

                self.log.info(format!("removing unused addon overlay {name}"));
                remove_all(&entry.path()).await?;
            }

            Ok::<(), OverlayError>(())
        };

        if let Err(error) = result.await {
            self.log.warn(format!("overlay cleanup failed: {error}"));
        }
    }
}

/// Does not follow symlinks.
async fn count_files(dir: &Path) -> std::io::Result<usize> {
    let mut pending = vec![dir.to_owned()];
    let mut files = 0;

    while let Some(next) = pending.pop() {
        let mut entries = fs::read_dir(&next).await?;

        while let Some(entry) = entries.next_entry().await? {
            let kind = entry.file_type().await?;

            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files += 1;
            }
        }
    }

    Ok(files)
}

/// Removes a folder with its contents, or a file, as `rm -rf` would.
async fn remove_all(path: &Path) -> std::io::Result<()> {
    let removed = match fs::symlink_metadata(path).await {
        Ok(info) if info.is_dir() => fs::remove_dir_all(path).await,
        Ok(_) => fs::remove_file(path).await,
        Err(error) => Err(error),
    };

    match removed {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The overlay operations launches and updates need, so they can run against a fake.
#[async_trait::async_trait]
pub trait OverlayPort: Send + Sync {
    async fn ensure(&self, image: &str) -> Result<ExtractedOverlay, OverlayError>;
    async fn acquire(&self, image: &str) -> Result<AcquiredOverlay, OverlayError>;
}

#[async_trait::async_trait]
impl OverlayPort for OverlayStore {
    async fn ensure(&self, image: &str) -> Result<ExtractedOverlay, OverlayError> {
        OverlayStore::ensure(self, image).await
    }

    async fn acquire(&self, image: &str) -> Result<AcquiredOverlay, OverlayError> {
        OverlayStore::acquire(self, image).await
    }
}

impl AcquiredOverlay {
    /// An overlay counted in `references` until released, so its folder stays while it is held.
    #[must_use]
    pub fn counted(overlay: ExtractedOverlay, references: &Arc<Mutex<BTreeMap<String, usize>>>) -> Self {
        *lock(references).entry(overlay.image_id.clone()).or_default() += 1;

        Self { overlay, references: Arc::clone(references), released: AtomicBool::new(false) }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
mod tests {
    use super::*;
    use crate::container_spec::labels;
    use crate::testing::FakeDocker;

    const IMAGE: &str = "ghcr.io/example/ankor-cs2:latest";
    const IMAGE_ID: &str = "sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    const CAFE: &str = "sha256:cafecafecafecafecafecafecafecafecafecafecafecafecafecafecafecafe";

    struct Extraction {
        /// The fake that the store copies from.
        docker: Arc<FakeDocker>,
        /// The store under test.
        store: OverlayStore,
        /// The data directory.
        dir: tempfile::TempDir,
        /// Whether the image's overlay folder has `addons/`.
        addons: Arc<Mutex<bool>>,
    }

    /// A Docker whose copy unpacks the files the image's overlay carries, as the archive puts them:
    /// under the folder's own name.
    fn extraction() -> Extraction {
        let dir = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        let addons = Arc::new(Mutex::new(true));
        let with_addons = Arc::clone(&addons);

        *docker.default_image_id.lock().unwrap() = Some(IMAGE_ID.to_owned());
        *docker.on_copy.lock().unwrap() = Some(Arc::new(move |path: &str, target: &Path| {
            let root = target.join(Path::new(path).file_name().unwrap());

            if *with_addons.lock().unwrap() {
                std::fs::create_dir_all(root.join("addons/swiftlys2/bin/linuxsteamrt64")).unwrap();
                std::fs::write(root.join("addons/swiftlys2/bin/linuxsteamrt64/swiftlys2.so"), "elf").unwrap();
                std::fs::create_dir_all(root.join("addons/swiftlys2/configs/plugins/Ankor")).unwrap();
                std::fs::write(root.join("addons/swiftlys2/configs/plugins/Ankor/config.jsonc"), "// from the image\n").unwrap();
            }

            std::fs::create_dir_all(root.join("cfg/ankor")).unwrap();
            std::fs::write(root.join("cfg/ankor/base.cfg"), "mp_maxrounds 24\n").unwrap();
        }));

        let store = OverlayStore::new(Arc::clone(&docker) as Arc<dyn DockerPort>, dir.path(), Logger::null());

        Extraction { docker, store, dir, addons }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).map(|entries| entries.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();

        names.sort();
        names
    }

    fn extractions(docker: &FakeDocker) -> Vec<crate::docker::StartContainerSpec> {
        docker.copied.lock().unwrap().iter().map(|(spec, _)| spec.clone()).collect()
    }

    #[tokio::test]
    async fn extracts_once_per_image_id_and_answers_from_the_marker_after_that() {
        let setup = extraction();
        let first = setup.store.ensure(IMAGE).await.unwrap();

        assert_eq!(first, ExtractedOverlay { image_id: IMAGE_ID.to_owned(), dir: setup.dir.path().join("overlay").join(overlay_dir_name(IMAGE_ID)) });
        assert_eq!(setup.store.ensure(IMAGE).await.unwrap(), first);
        assert_eq!(extractions(&setup.docker).len(), 1);
        assert!(first.dir.join(COMPLETE_MARKER).exists());
        assert!(std::fs::read_to_string(first.dir.join("cfg/ankor/base.cfg")).unwrap().contains("mp_maxrounds"));
        assert!(!setup.store.dir(IMAGE_ID, TMP_SUFFIX).exists());
    }

    #[tokio::test]
    async fn extracts_again_when_the_image_id_moves() {
        let setup = extraction();

        setup.store.ensure(IMAGE).await.unwrap();
        *setup.docker.default_image_id.lock().unwrap() = Some(CAFE.to_owned());

        let moved = setup.store.ensure(IMAGE).await.unwrap();

        assert_eq!(moved.image_id, CAFE);
        assert_eq!(extractions(&setup.docker).len(), 2);
    }

    #[tokio::test]
    async fn copies_the_overlay_folder_from_a_hardened_container_that_runs_nothing() {
        let setup = extraction();

        setup.store.ensure(IMAGE).await.unwrap();

        let (spec, path) = setup.docker.copied.lock().unwrap()[0].clone();

        assert_eq!(path, IMAGE_OVERLAY_DIR);
        assert_eq!(spec.image, IMAGE_ID);
        assert_eq!(spec.labels[labels::ROLE], "overlay");
        assert!(spec.binds.is_empty() && spec.volumes.is_empty());
        assert_eq!((spec.entrypoint, spec.cmd), (None, None));
        assert_eq!((spec.cap_drop.as_slice(), spec.pids_limit, spec.memory_bytes, spec.cpu_shares), (&["ALL".to_owned()][..], 1024, None, None));
        assert!(setup.docker.specs().is_empty(), "no container was started");
    }

    #[tokio::test]
    async fn leaves_nothing_usable_behind_when_the_copy_fails_and_recovers() {
        let setup = extraction();

        *setup.docker.copy_error.lock().unwrap() = Some("no space left on device".to_owned());

        let error = setup.store.ensure(IMAGE).await.unwrap_err().to_string();

        assert_eq!(error, "overlay extraction failed: no space left on device");
        // The copy had unpacked files before it failed; none are left.
        assert!(names(&overlay_root(setup.dir.path())).is_empty());

        *setup.docker.copy_error.lock().unwrap() = None;
        assert_eq!(setup.store.ensure(IMAGE).await.unwrap().image_id, IMAGE_ID);
        assert_eq!(setup.docker.removed.lock().unwrap().len(), 2, "removed whether it worked or not");
    }

    #[tokio::test]
    async fn refuses_an_image_whose_overlay_has_no_addons() {
        let setup = extraction();

        *setup.addons.lock().unwrap() = false;
        assert_eq!(setup.store.ensure(IMAGE).await.unwrap_err().to_string(), "overlay extraction failed: no addons/ in /opt/ankor/overlay");
        assert!(names(&overlay_root(setup.dir.path())).is_empty());
    }

    #[tokio::test]
    async fn refuses_an_image_the_host_lacks_naming_it() {
        let setup = extraction();

        *setup.docker.no_images.lock().unwrap() = true;

        let error = setup.store.ensure(IMAGE).await.unwrap_err().to_string();

        assert_eq!(error, format!("cannot extract the addon overlay: {IMAGE} is not on this host (pull it first)"));
        assert!(extractions(&setup.docker).is_empty());
    }

    #[tokio::test]
    async fn creates_the_mountpoints_inside_the_overlay_keeping_shipped_files() {
        let setup = extraction();
        let local = setup.store.ensure(IMAGE).await.unwrap().dir;

        for (rel_path, kind) in overlay_mountpoints() {
            let meta = std::fs::metadata(local.join(&rel_path)).unwrap();

            assert_eq!(if meta.is_dir() { MountpointKind::Directory } else { MountpointKind::File }, kind, "{rel_path}");
        }

        assert_eq!(std::fs::read_to_string(local.join("addons/swiftlys2/configs/plugins/Ankor/config.jsonc")).unwrap(), "// from the image\n");
    }

    /// An unpack that outlived its timeout keeps writing into its own folder, which no later
    /// attempt reuses or marks complete.
    #[tokio::test]
    async fn unpacks_each_attempt_into_a_folder_of_its_own() {
        let setup = extraction();
        let targets: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let seen = Arc::clone(&targets);
        let hook = lock(&setup.docker.on_copy).clone();

        *lock(&setup.docker.on_copy) = Some(Arc::new(move |path: &str, target: &Path| {
            lock(&seen).push(target.to_owned());

            if let Some(hook) = &hook {
                hook(path, target);
            }
        }));
        *lock(&setup.docker.copy_error) = Some("timed out".to_owned());
        assert!(setup.store.ensure(IMAGE).await.is_err());
        *lock(&setup.docker.copy_error) = None;
        setup.store.ensure(IMAGE).await.unwrap();

        let targets = lock(&targets).clone();

        assert_eq!(targets.len(), 2);
        assert_ne!(targets[0], targets[1]);
        assert!(!targets[0].exists() && !targets[1].exists());
    }

    /// An overlay folder it cannot list may hold an attempt that an unpack still writes into, so
    /// the extraction fails rather than go on beside it.
    #[tokio::test]
    async fn fails_the_extraction_when_it_cannot_list_earlier_attempts() {
        use std::os::unix::fs::PermissionsExt as _;

        let setup = extraction();
        let root = overlay_root(setup.dir.path());

        if nix::unistd::geteuid().is_root() {
            return;
        }

        // Writable and searchable, so a new attempt could be made, but not listable.
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o300)).unwrap();

        let result = setup.store.ensure(IMAGE).await;

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err(), "{result:?}");
        assert!(extractions(&setup.docker).is_empty());
    }

    #[tokio::test]
    async fn extracts_over_what_a_crash_left() {
        let setup = extraction();
        let local = setup.store.dir(IMAGE_ID, "");
        let tmp = setup.store.dir(IMAGE_ID, TMP_SUFFIX);

        std::fs::create_dir_all(tmp.join("addons")).unwrap();
        std::fs::write(tmp.join("addons/half-copied.so"), "elf").unwrap();
        std::fs::create_dir_all(local.join("cfg")).unwrap();
        std::fs::write(local.join("cfg/stale.cfg"), "from before the crash\n").unwrap();

        setup.store.ensure(IMAGE).await.unwrap();

        assert_eq!(extractions(&setup.docker).len(), 1);
        assert!(local.join(COMPLETE_MARKER).exists());
        assert!(!local.join("cfg/stale.cfg").exists());
        assert!(!tmp.exists());
    }

    #[tokio::test]
    async fn an_acquisition_dropped_unreleased_gives_its_overlay_back() {
        let setup = extraction();
        let acquired = setup.store.acquire(IMAGE).await.unwrap();
        let dir = acquired.overlay.dir.clone();

        drop(acquired);
        *setup.docker.default_image_id.lock().unwrap() = Some("sha256:second".to_owned());
        setup.store.ensure(IMAGE).await.unwrap();

        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn keeps_an_overlay_until_every_acquisition_is_released() {
        let setup = extraction();
        let first = setup.store.acquire(IMAGE).await.unwrap();
        let second = setup.store.acquire(IMAGE).await.unwrap();

        for (next, release) in [("sha256:second", None), ("sha256:third", Some(&first)), ("sha256:fourth", Some(&second))] {
            if let Some(acquired) = release {
                acquired.release();
                acquired.release();
            }

            *setup.docker.default_image_id.lock().unwrap() = Some(next.to_owned());
            setup.store.ensure(IMAGE).await.unwrap();

            assert_eq!(first.overlay.dir.exists(), next != "sha256:fourth", "after {next}");
        }
    }

    #[tokio::test]
    async fn deletes_an_unreferenced_old_overlay_and_keeps_one_a_running_container_mounts() {
        let setup = extraction();
        let root = overlay_root(setup.dir.path());

        setup.store.ensure(IMAGE).await.unwrap();
        std::fs::create_dir_all(root.join("sha256_a03f/addons")).unwrap();
        std::fs::write(root.join("sha256_a03f/addons/swiftlys2.so"), "elf").unwrap();
        std::fs::create_dir_all(root.join("sha256_321e.tmp")).unwrap();
        std::fs::create_dir_all(setup.store.dir(IMAGE_ID, TMP_SUFFIX)).unwrap();
        *setup.docker.overlays_in_use.lock().unwrap() = vec![IMAGE_ID.to_owned()];
        *setup.docker.default_image_id.lock().unwrap() = Some(CAFE.to_owned());
        setup.store.ensure(IMAGE).await.unwrap();

        let kept = overlay_dir_name(IMAGE_ID);

        assert_eq!(names(&root), [overlay_dir_name(CAFE), kept.clone(), format!("{kept}.tmp")]);

        setup.docker.overlays_in_use.lock().unwrap().clear();
        *setup.docker.default_image_id.lock().unwrap() = Some("sha256:beef".to_owned());
        setup.store.ensure(IMAGE).await.unwrap();

        assert_eq!(names(&root), ["sha256_beef"]);
    }

    #[tokio::test]
    async fn removes_a_stray_file_with_the_unused_overlays() {
        let setup = extraction();
        let root = overlay_root(setup.dir.path());

        setup.store.ensure(IMAGE).await.unwrap();
        std::fs::write(root.join("aaa-notes.txt"), "an operator's file").unwrap();
        std::fs::create_dir_all(root.join("sha256_a03f/addons")).unwrap();
        *setup.docker.default_image_id.lock().unwrap() = Some("sha256:beef".to_owned());
        setup.store.ensure(IMAGE).await.unwrap();

        assert_eq!(names(&root), ["sha256_beef"]);
    }

    #[tokio::test]
    async fn keeps_the_new_overlay_when_the_daemon_will_not_answer() {
        let setup = extraction();

        setup.store.ensure(IMAGE).await.unwrap();
        *setup.docker.overlays_error.lock().unwrap() = Some("daemon is having a moment".to_owned());
        *setup.docker.default_image_id.lock().unwrap() = Some(CAFE.to_owned());

        assert_eq!(setup.store.ensure(IMAGE).await.unwrap().image_id, CAFE);
        assert_eq!(names(&overlay_root(setup.dir.path())), [overlay_dir_name(CAFE), overlay_dir_name(IMAGE_ID)]);
    }

    /// Slot files whose first preparation says it began and then waits for `resume`.
    struct HeldFiles {
        files: crate::testing::FakeSlotFiles,
        prepared: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resume: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl crate::slot_files::SlotFilesPort for HeldFiles {
        async fn prepare(
            &self,
            slot_id: &str,
            allocation_id: &str,
            cfg_files: &[crate::slot_files::CfgFile],
        ) -> std::io::Result<crate::slot_files::PreparedSlot> {
            let prepared = self.prepared.lock().unwrap().take();

            if let Some(prepared) = prepared {
                prepared.send(()).unwrap();

                let resume = self.resume.lock().await.take().unwrap();

                resume.await.unwrap();
            }

            self.files.prepare(slot_id, allocation_id, cfg_files).await
        }
    }

    #[tokio::test]
    async fn keeps_a_preparing_launch_consistent_across_a_plugin_update() {
        use crate::install::{MaintenanceGuard, MaintenanceGuardOptions};
        use crate::self_update::{Cs2ImageUpdateDeps, Cs2ImageUpdateState, update_cs2_image};
        use crate::testing::fixtures::{agent_config, launch_config};
        use crate::testing::slots::{launch_server, make_slots_with};

        for same_tag in [false, true] {
            let setup = extraction();
            let docker = Arc::clone(&setup.docker);
            let store: Arc<OverlayStore> = Arc::new(setup.store);
            let config = Arc::new(Mutex::new(agent_config(&serde_json::json!({}))));
            let cs2_image = config.lock().unwrap().cs2_image.clone();
            let (prepared, preparing) = tokio::sync::oneshot::channel();
            let (resume, held) = tokio::sync::oneshot::channel();
            let files = Arc::new(HeldFiles {
                files: crate::testing::FakeSlotFiles::default(),
                prepared: Mutex::new(Some(prepared)),
                resume: tokio::sync::Mutex::new(Some(held)),
            });
            let read = Arc::clone(&config);
            let base = make_slots_with(agent_config(&serde_json::json!({})), |deps| {
                deps.config = Arc::new(move || read.lock().unwrap().clone());
                deps.docker = Arc::clone(&docker) as Arc<dyn DockerPort>;
                deps.overlay = Arc::clone(&store) as Arc<dyn OverlayPort>;
                deps.files = files;
            });
            let next_image = if same_tag { cs2_image.clone() } else { "ankor-cs2:next".to_owned() };

            docker.images.lock().unwrap().insert(cs2_image.clone(), IMAGE_ID.to_owned());
            docker.pull_ids.lock().unwrap().insert(next_image.clone(), "sha256:next-image".to_owned());

            let allocation_id = "0199a0b4-5c00-7000-8000-0000000000b1".to_owned();
            let slots = base.slots.clone();
            let first = allocation_id.clone();
            let launch = tokio::spawn(async move { launch_server(&slots, launch_config(&serde_json::json!({})), &first).await });

            preparing.await.unwrap();

            // The switch runs after the reply; it has to be done before the next launch is checked.
            let owed: Arc<Mutex<Vec<futures_util::future::BoxFuture<'static, ()>>>> = Arc::default();
            let queue = Arc::clone(&owed);
            let deps = Cs2ImageUpdateDeps {
                config: crate::config::ConfigStore::over("/tmp/agent.yml".into(), Arc::clone(&config)).with_persist(Arc::new(|_, _| Ok(()))),
                docker: Arc::clone(&docker) as Arc<dyn DockerPort>,
                overlay: Arc::clone(&store) as Arc<dyn OverlayPort>,
                updates: MaintenanceGuard::new(MaintenanceGuardOptions::new(Arc::clone(&docker) as Arc<dyn DockerPort>)),
                log: Logger::null(),
                state: Arc::new(Cs2ImageUpdateState::new(Arc::new(|| {}))),
                schedule: Arc::new(move |work| queue.lock().unwrap().push(work)),
                held_images: Arc::new(Vec::new),
            };

            update_cs2_image(&next_image, true, &deps).await.unwrap();

            let work: Vec<_> = owed.lock().unwrap().drain(..).collect();

            for work in work {
                work.await;
            }

            assert!(store.dir(IMAGE_ID, "").exists());
            assert_eq!(extractions(&docker).iter().map(|spec| spec.image.clone()).collect::<Vec<_>>(), [IMAGE_ID, "sha256:next-image"]);

            resume.send(()).unwrap();
            launch.await.unwrap().unwrap();

            let spec = docker.specs().last().unwrap().clone();

            assert_eq!(spec.image, IMAGE_ID);
            assert_eq!(spec.labels[labels::OVERLAY], IMAGE_ID);
            assert!(spec.binds.iter().any(|bind| Path::new(&bind.source) == store.dir(IMAGE_ID, "").join("addons")));

            launch_server(&base.slots, launch_config(&serde_json::json!({})), "0199a0b4-5c00-7000-8000-0000000000b2").await.unwrap();
            assert_eq!(docker.specs().last().unwrap().image, "sha256:next-image");
            base.slots.stop_allocation(&allocation_id).await.unwrap();
            docker.images.lock().unwrap().insert("ankor-cs2:third".to_owned(), "sha256:third-image".to_owned());
            store.ensure("ankor-cs2:third").await.unwrap();
            assert!(!store.dir(IMAGE_ID, "").exists(), "same tag: {same_tag}");
            assert!(store.dir("sha256:next-image", "").exists(), "same tag: {same_tag}");
        }
    }

    #[test]
    fn turns_an_image_id_into_a_directory_name() {
        assert_eq!(overlay_dir_name(IMAGE_ID), IMAGE_ID.replace(':', "_"));
        assert!(!overlay_dir_name("a/../../b").contains('/'));
    }

    #[tokio::test]
    async fn names_the_copy_container_for_the_overlay() {
        let setup = extraction();

        setup.store.ensure(IMAGE).await.unwrap();

        let name = extractions(&setup.docker)[0].name.clone();

        assert!(name.starts_with("ankor-overlay-") && name.len() > "ankor-overlay-".len(), "{name}");
    }

    #[tokio::test]
    async fn counts_the_regular_files_under_a_folder_at_every_depth() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(dir.path().join("addons/swiftlys2/bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("cfg/ankor")).unwrap();

        for file in ["addons/swiftlys2/bin/swiftlys2.so", "addons/swiftlys2/core.json", "cfg/ankor/base.cfg"] {
            std::fs::write(dir.path().join(file), "x").unwrap();
        }

        std::os::unix::fs::symlink(dir.path().join("cfg"), dir.path().join("addons/linked")).unwrap();

        assert_eq!(count_files(dir.path()).await.unwrap(), 3);
        assert_eq!(count_files(&dir.path().join("cfg")).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn removes_a_folder_or_a_file_takes_a_missing_path_as_removed_and_passes_on_other_failures() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("sha256_a03f");
        let file = dir.path().join("notes.txt");

        std::fs::create_dir_all(folder.join("addons")).unwrap();
        std::fs::write(folder.join("addons/swiftlys2.so"), "elf").unwrap();
        std::fs::write(&file, "an operator's file").unwrap();

        remove_all(&folder).await.unwrap();
        remove_all(&file).await.unwrap();
        remove_all(&dir.path().join("missing")).await.unwrap();
        assert!(names(dir.path()).is_empty());

        // A path through a file is no folder, which is another failure than a missing path.
        std::fs::write(&file, "x").unwrap();
        assert_eq!(remove_all(&file.join("child")).await.unwrap_err().kind(), ErrorKind::NotADirectory);
    }
}
