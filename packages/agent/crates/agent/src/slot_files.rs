//! Per-slot writable shadows for the read-only game install. The slot root itself is not mounted.
//! Bind sources are host paths, because dockerd resolves them.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tokio::fs;

use crate::host_layout::{
    ANKOR_PLUGIN, SHARED_PATHS, ankor_dirs, node_cfg_dir, node_plugin_dirs, node_plugins_dir, shadow_paths, shared_dir, slot_backup_dir, slot_demo_dir,
    slot_root, swiftly_dir,
};

/// A per-launch file to write; `None` removes it, as an earlier launch may have left one.
#[derive(Clone, Debug)]
pub struct CfgFile {
    pub rel_path: String,
    pub contents: Option<String>,
}

/// Host paths as dockerd sees them, so a launch mounts what was prepared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSlot {
    /// The slot's root, which holds its shadows and per-launch files.
    pub root: PathBuf,
    /// The writable paths every slot on the node shares.
    pub shared_root: PathBuf,
    /// The node's `SwiftlyS2` directory, which holds its plugins and their configs.
    pub swiftly_root: PathBuf,
    /// The node's cfg directory, which a game runs as `exec node/<name>`.
    pub cfg_dir: PathBuf,
    /// This launch's demo directory, `demos/<allocation_id>` in the slot.
    pub demo_dir: PathBuf,
    /// The node's own `SwiftlyS2` plugins, each with its config directory in place.
    pub node_plugins: Vec<String>,
    /// The per-launch files written.
    pub files: Vec<String>,
}

/// The slot preparation a launch needs, so the slot manager can run against a fake.
#[async_trait::async_trait]
pub trait SlotFilesPort: Send + Sync {
    async fn prepare(&self, slot_id: &str, allocation_id: &str, cfg_files: &[CfgFile]) -> std::io::Result<PreparedSlot>;
}

#[async_trait::async_trait]
impl SlotFilesPort for SlotFiles {
    async fn prepare(&self, slot_id: &str, allocation_id: &str, cfg_files: &[CfgFile]) -> std::io::Result<PreparedSlot> {
        SlotFiles::prepare(self, slot_id, allocation_id, cfg_files).await
    }
}

#[derive(Clone, Debug)]
pub struct SlotFiles {
    data_dir: PathBuf,
}

impl SlotFiles {
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into() }
    }

    /// # Errors
    ///
    /// Fails as the file system does.
    pub async fn prepare(&self, slot_id: &str, allocation_id: &str, cfg_files: &[CfgFile]) -> std::io::Result<PreparedSlot> {
        let root = slot_root(&self.data_dir, slot_id);

        // A backup belongs to the launch that wrote it, and the plugin has sent it on as an event.
        remove_all(&slot_backup_dir(&self.data_dir, slot_id)).await?;

        for dir in shadow_paths().iter().chain(ankor_dirs().iter()) {
            fs::create_dir_all(root.join(dir)).await?;
        }

        let demo_dir = launch_demo_dir(&slot_demo_dir(&self.data_dir, slot_id), allocation_id).await?;
        let shared_root = shared_dir(&self.data_dir);
        let cfg_dir = node_cfg_dir(&self.data_dir);

        fs::create_dir_all(&cfg_dir).await?;

        for shared in SHARED_PATHS {
            fs::create_dir_all(shared_root.join(shared)).await?;
        }

        for cfg in cfg_files {
            let path = root.join(&cfg.rel_path);

            match &cfg.contents {
                None => remove_file(&path).await?,
                Some(contents) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).await?;
                    }

                    write_new(&path, contents).await?;
                }
            }
        }

        let files = cfg_files.iter().filter(|cfg| cfg.contents.is_some()).map(|cfg| cfg.rel_path.clone()).collect();

        Ok(PreparedSlot { root, shared_root, swiftly_root: swiftly_dir(&self.data_dir), cfg_dir, demo_dir, node_plugins: self.node_plugins().await?, files })
    }

    /// Read at each launch, so a new plugin needs no restart.
    async fn node_plugins(&self) -> std::io::Result<Vec<String>> {
        let root = swiftly_dir(&self.data_dir);
        let plugins = node_plugins_dir(&self.data_dir);

        fs::create_dir_all(&plugins).await?;

        let mut names = Vec::new();
        let mut entries = fs::read_dir(&plugins).await?;

        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();

            if entry.file_type().await?.is_dir() && !name.starts_with('.') && name != ANKOR_PLUGIN {
                names.push(name);
            }
        }

        names.sort();

        for name in &names {
            for dir in node_plugin_dirs(name) {
                fs::create_dir_all(root.join(dir)).await?;
            }
        }

        Ok(names)
    }
}

/// Named for the allocation, so a recording says which server made it across crashes and restarts.
/// Removing earlier empty directories is safe because the slot is idle, so none of them is mounted.
async fn launch_demo_dir(demos: &Path, allocation_id: &str) -> std::io::Result<PathBuf> {
    let mut entries = fs::read_dir(demos).await?;

    while let Some(entry) = entries.next_entry().await? {
        // Fails on a file and on a directory that still holds a recording, and both stay.
        let _ = fs::remove_dir(entry.path()).await;
    }

    let dir = demos.join(allocation_id);

    fs::create_dir_all(&dir).await?;
    Ok(dir)
}

async fn remove_all(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(path).await {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Some files live where the slot's last game could write, so the path is unlinked first and created
/// exclusively. A symlink left there is removed, never written through. One recreated in between
/// fails the launch instead.
async fn write_new(path: &Path, contents: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    remove_file(path).await?;

    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(path).await?;

    file.write_all(contents.as_bytes()).await?;
    file.flush().await
}

async fn remove_file(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path).await {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALLOCATION: &str = "11111111-1111-4111-8111-111111111111";

    fn cfg(rel_path: &str, contents: Option<&str>) -> CfgFile {
        CfgFile { rel_path: rel_path.to_owned(), contents: contents.map(str::to_owned) }
    }

    fn launch_files() -> Vec<CfgFile> {
        vec![
            cfg("game/csgo/cfg/ankor_launch.cfg", Some("hostname \"test\"\n")),
            cfg("game/csgo/addons/swiftlys2/configs/plugins/Ankor/config.jsonc", Some("{}\n")),
        ]
    }

    fn with(extra: CfgFile) -> Vec<CfgFile> {
        let mut files = launch_files();

        files.push(extra);
        files
    }

    async fn names(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        let mut entries = fs::read_dir(dir).await.unwrap();

        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }

        names.sort();
        names
    }

    #[tokio::test]
    async fn creates_every_shadow_dir_and_both_rendered_files() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let prepared = SlotFiles::new(data).prepare("a", ALLOCATION, &launch_files()).await.unwrap();
        let root = data.join("slots/a");

        assert_eq!(
            prepared,
            PreparedSlot {
                root: root.clone(),
                shared_root: data.join("shared"),
                swiftly_root: data.join("swiftlys2"),
                cfg_dir: data.join("cfg"),
                demo_dir: root.join("game/csgo/ankor/demos").join(ALLOCATION),
                node_plugins: Vec::new(),
                files: launch_files().into_iter().map(|cfg| cfg.rel_path).collect(),
            }
        );

        for shadow in shadow_paths().iter().filter(|dir| *dir != "game/csgo/ankor/") {
            assert!(names(&root.join(shadow)).await.is_empty(), "{shadow}");
        }

        assert_eq!(names(&root.join("game/csgo/ankor")).await, ["backups", "demos"]);
        assert_eq!(fs::read_to_string(root.join("game/csgo/cfg/ankor_launch.cfg")).await.unwrap(), "hostname \"test\"\n");
        assert_eq!(fs::read_to_string(root.join("game/csgo/addons/swiftlys2/configs/plugins/Ankor/config.jsonc")).await.unwrap(), "{}\n");
    }

    #[tokio::test]
    async fn takes_a_first_launch_without_a_match_and_lays_down_the_shared_directories() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = SlotFiles::new(dir.path()).prepare("a", ALLOCATION, &with(cfg("game/csgo/ankor/match.json", None))).await.unwrap();

        assert_eq!(prepared.files, launch_files().into_iter().map(|cfg| cfg.rel_path).collect::<Vec<_>>());

        for shared in SHARED_PATHS {
            assert!(shared_dir(dir.path()).join(shared).is_dir());
        }
    }

    #[tokio::test]
    async fn removes_an_earlier_match_file_and_mounts_only_what_it_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let files = SlotFiles::new(dir.path());
        let ankor = dir.path().join("slots/a/game/csgo/ankor");

        files.prepare("a", ALLOCATION, &with(cfg("game/csgo/ankor/match.json", Some("{}")))).await.unwrap();
        assert!(names(&ankor).await.contains(&"match.json".to_owned()));

        let prepared = files.prepare("a", ALLOCATION, &with(cfg("game/csgo/ankor/match.json", None))).await.unwrap();

        assert!(!names(&ankor).await.contains(&"match.json".to_owned()));
        assert!(!prepared.files.contains(&"game/csgo/ankor/match.json".to_owned()));
    }

    #[tokio::test]
    async fn writes_a_restore_file_and_removes_it_for_the_next_launch() {
        let dir = tempfile::tempdir().unwrap();
        let files = SlotFiles::new(dir.path());
        let restore = "game/csgo/ankor/restore.txt";
        let contents = "\"SaveFile\"\n{\n\t\"round\"\t\"14\"\n}";
        let restored = files.prepare("a", ALLOCATION, &with(cfg(restore, Some(contents)))).await.unwrap();

        assert_eq!(fs::read_to_string(dir.path().join("slots/a").join(restore)).await.unwrap(), contents);
        assert!(restored.files.contains(&restore.to_owned()));

        let next = files.prepare("a", ALLOCATION, &with(cfg(restore, None))).await.unwrap();

        assert!(!names(&dir.path().join("slots/a/game/csgo/ankor")).await.contains(&"restore.txt".to_owned()));
        assert!(!next.files.contains(&restore.to_owned()));
    }

    #[tokio::test]
    async fn fails_when_an_earlier_launch_file_it_would_remove_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ankor = dir.path().join("slots/a/game/csgo/ankor");

        fs::create_dir_all(ankor.join("match.json")).await.unwrap();
        assert!(SlotFiles::new(dir.path()).prepare("a", ALLOCATION, &with(cfg("game/csgo/ankor/match.json", None))).await.is_err());
    }

    #[tokio::test]
    async fn fails_when_the_last_launchs_backups_cannot_be_removed() {
        use std::os::unix::fs::PermissionsExt as _;

        // Root removes what permissions forbid, so there is nothing to refuse it.
        if nix::unistd::geteuid().is_root() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join("slots/a/game/csgo/ankor/backups");

        fs::create_dir_all(backups.join("locked")).await.unwrap();
        fs::write(backups.join("locked/ankor_round03.txt"), "old").await.unwrap();
        fs::set_permissions(backups.join("locked"), std::fs::Permissions::from_mode(0o500)).await.unwrap();

        let result = SlotFiles::new(dir.path()).prepare("a", ALLOCATION, &launch_files()).await;

        fs::set_permissions(backups.join("locked"), std::fs::Permissions::from_mode(0o700)).await.unwrap();
        assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn empties_the_last_launchs_backups_and_keeps_its_demos() {
        let dir = tempfile::tempdir().unwrap();
        let files = SlotFiles::new(dir.path());
        let ankor = dir.path().join("slots/a/game/csgo/ankor");

        files.prepare("a", ALLOCATION, &launch_files()).await.unwrap();
        fs::write(ankor.join("backups/ankor_round03.txt"), "old").await.unwrap();
        fs::write(ankor.join("demos/pending.dem"), "demo").await.unwrap();
        files.prepare("a", ALLOCATION, &launch_files()).await.unwrap();

        assert!(names(&ankor.join("backups")).await.is_empty());
        assert_eq!(names(&ankor.join("demos")).await, [ALLOCATION, "pending.dem"]);
    }

    #[tokio::test]
    async fn gives_each_launch_a_demo_directory_and_drops_earlier_empty_ones() {
        let dir = tempfile::tempdir().unwrap();
        let files = SlotFiles::new(dir.path());
        let demos = dir.path().join("slots/a/game/csgo/ankor/demos");
        let third = "33333333-3333-4333-8333-333333333333";

        files.prepare("a", ALLOCATION, &launch_files()).await.unwrap();
        fs::write(demos.join(ALLOCATION).join("recorded.dem"), "demo").await.unwrap();
        files.prepare("a", "22222222-2222-4222-8222-222222222222", &launch_files()).await.unwrap();

        let next = files.prepare("a", third, &launch_files()).await.unwrap();

        assert_eq!(names(&demos).await, [ALLOCATION, third]);
        assert_eq!(next.demo_dir, demos.join(third));
        assert_eq!(names(&demos.join(ALLOCATION)).await, ["recorded.dem"]);
    }

    #[tokio::test]
    async fn lists_the_node_plugins_and_gives_each_a_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let swiftly = dir.path().join("swiftlys2");

        for plugin in ["Zeta", "Alpha", "Ankor", ".hidden"] {
            fs::create_dir_all(swiftly.join("plugins").join(plugin)).await.unwrap();
        }

        fs::write(swiftly.join("plugins/notes.txt"), "not a plugin").await.unwrap();

        let prepared = SlotFiles::new(dir.path()).prepare("a", ALLOCATION, &launch_files()).await.unwrap();

        assert_eq!(prepared.node_plugins, ["Alpha", "Zeta"]);
        assert!(names(&dir.path().join("cfg")).await.is_empty());
        assert_eq!(names(&swiftly.join("configs/plugins")).await, ["Alpha", "Zeta"]);
    }

    #[tokio::test]
    async fn creates_no_game_side_event_journal() {
        let dir = tempfile::tempdir().unwrap();

        SlotFiles::new(dir.path()).prepare("b", ALLOCATION, &[]).await.unwrap();

        assert_eq!(names(&dir.path().join("slots/b")).await, ["game"]);
    }

    /// A launch without a match leaves `match.json` unmounted in the slot's writable shadow, so its
    /// game can leave a link there. The next launch with a match must replace the link, not write
    /// through it.
    #[tokio::test]
    async fn writes_a_launch_file_in_place_of_a_link_the_last_game_left() {
        let root = tempfile::tempdir().unwrap();
        let victim = root.path().join("agent.yml");
        let ankor = slot_root(root.path(), "a").join("game/csgo/ankor");

        std::fs::write(&victim, b"token: node-secret-token\n").unwrap();
        std::fs::create_dir_all(&ankor).unwrap();
        std::os::unix::fs::symlink(&victim, ankor.join("match.json")).unwrap();

        SlotFiles::new(root.path()).prepare("a", ALLOCATION, &with(cfg("game/csgo/ankor/match.json", Some("{}\n")))).await.unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"token: node-secret-token\n");
        assert!(!std::fs::symlink_metadata(ankor.join("match.json")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(ankor.join("match.json")).unwrap(), "{}\n");
    }
}
