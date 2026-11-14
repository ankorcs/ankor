//! Where the agent puts things on its node and inside the containers it creates.

use std::path::{Path, PathBuf};

use crate::contract::constants::{ANKOR_DIR, BACKUP_DIR, CFG_DIR, DEMO_DIR, GAME_INSTALL_DIR, MATCH_CONFIG_FILE, RESTORE_FILE, cfg_names};

pub const INSTALL_DIR: &str = GAME_INSTALL_DIR;

// The game's directories, relative to the install. Macros rather than constants, because
// `concat!` builds the constant paths below from them and takes only literals.
macro_rules! game_dir {
    () => {
        "game/csgo"
    };
}
macro_rules! addons_dir {
    () => {
        concat!(game_dir!(), "/addons")
    };
}
macro_rules! swiftlys2_dir {
    () => {
        concat!(addons_dir!(), "/swiftlys2")
    };
}
macro_rules! game_cfg_dir {
    () => {
        concat!(game_dir!(), "/cfg")
    };
}
macro_rules! ankor_plugin {
    () => {
        "Ankor"
    };
}
macro_rules! plugin_configs_dir {
    () => {
        "configs/plugins"
    };
}
macro_rules! data_dir {
    () => {
        "/srv/ankor"
    };
}

const ADDONS_DIR: &str = addons_dir!();
const GAME_CFG_DIR: &str = game_cfg_dir!();

/// Relative to the install. The plugin runs this cfg on every map load and when a match goes live.
pub(crate) static LAUNCH_CFG_PATH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| format!("{GAME_CFG_DIR}/{}.cfg", cfg_names::LAUNCH));

/// The image ships a default inside the overlay; each launch binds its own over it.
pub(crate) const PLUGIN_CONFIG_PATH: &str = concat!(swiftlys2_dir!(), "/", plugin_configs_dir!(), "/", ankor_plugin!(), "/config.jsonc");

/// Directories kept per slot across launches, each with a trailing `/`. `configs/` is not one,
/// because the image supplies it and an empty shadow would hide it.
#[must_use]
pub(crate) fn shadow_paths() -> [String; 4] {
    [format!("{ANKOR_DIR}/"), format!("{SWIFTLYS2_DIR}/logs/"), format!("{SWIFTLYS2_DIR}/dumps/"), format!("{SWIFTLYS2_DIR}/data/")]
}

/// Inside the ankor shadow, so the agent creates them without mountpoints.
#[must_use]
pub(crate) fn ankor_dirs() -> [String; 2] {
    [format!("{DEMO_DIR}/"), format!("{BACKUP_DIR}/")]
}

/// Writable and shared by every slot (`<data_dir>/shared/<path>`). The engine downloads workshop
/// maps below its binary directory. The install volume is read-only, so without this a workshop
/// map has nowhere to go.
pub(crate) const SHARED_PATHS: [&str; 1] = ["game/bin/linuxsteamrt64/steamapps/"];

const SWIFTLYS2_DIR: &str = swiftlys2_dir!();

/// The image's plugin, which a node plugin of the same name cannot replace.
pub(crate) const ANKOR_PLUGIN: &str = ankor_plugin!();

macro_rules! plugins_dir {
    () => {
        "plugins"
    };
}

/// Where `SwiftlyS2` looks for plugins and their configs, each with whether node plugins mount
/// read-only into it. Each is a tmpfs holding only what is mounted into it. Configs stay writable
/// because `SwiftlyS2` writes a missing config with its defaults.
const PLUGIN_DIRS: [(&str, bool); 2] = [(plugins_dir!(), true), (plugin_configs_dir!(), false)];

const PLUGIN_TMPFS_BYTES: i64 = 64 << 10;

/// The node's own cfgs, mounted read-only as one directory, since a file bind needs a mountpoint
/// in the read-only install.
const NODE_CFG_TARGET: &str = concat!(game_cfg_dir!(), "/node");

/// The files each launch renders and binds read-only over the install.
#[must_use]
pub(crate) fn per_launch_files() -> [String; 4] {
    [LAUNCH_CFG_PATH.clone(), PLUGIN_CONFIG_PATH.to_owned(), MATCH_CONFIG_FILE.to_owned(), RESTORE_FILE.to_owned()]
}

/// The host and agent see the data directory at the same path, because dockerd resolves bind sources.
pub const DATA_DIR: &str = data_dir!();

pub const AGENT_CONFIG_PATH: &str = concat!(data_dir!(), "/agent.yml");

pub(crate) const DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// The shared game volume. Only an install or update mounts it writable.
pub(crate) const DATA_VOLUME: &str = "ankor_cs2_data";

/// Where the agent's container mounts the game volume read-only, to read the installed build.
pub const CS2_INSTALL_MOUNT: &str = "/cs2-install";

pub(crate) const DEBUG_CAPTURE_VOLUME: &str = "ankor_dev_event_captures";

pub(crate) const DEBUG_CAPTURE_DIR: &str = "/home/steam/ankor-event-captures";

#[must_use]
pub(crate) fn slots_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("slots")
}

/// The slot's writable shadows and per-launch files, laid out as the install is.
#[must_use]
pub(crate) fn slot_root(data_dir: &Path, slot_id: &str) -> PathBuf {
    slots_dir(data_dir).join(slot_id)
}

/// The engine's round backups of the slot's current launch.
#[must_use]
pub(crate) fn slot_backup_dir(data_dir: &Path, slot_id: &str) -> PathBuf {
    slot_root(data_dir, slot_id).join(BACKUP_DIR)
}

/// The slot's recordings, one directory per launch below it.
#[must_use]
pub(crate) fn slot_demo_dir(data_dir: &Path, slot_id: &str) -> PathBuf {
    slot_root(data_dir, slot_id).join(DEMO_DIR)
}

#[must_use]
pub(crate) fn shared_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("shared")
}

/// The node's own `SwiftlyS2` plugins and their configs.
#[must_use]
pub(crate) fn swiftly_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("swiftlys2")
}

/// The node's own cfgs, which a game runs as `exec node/<name>`.
#[must_use]
pub(crate) fn node_cfg_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cfg")
}

/// A failed agent swap and why. A swap that holds removes it, and so does `node-setup.sh`.
#[must_use]
pub(crate) fn agent_update_failure_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent-update-failed.json")
}

/// The event spool's database, which keeps every server's events until the controller takes them.
#[must_use]
pub(crate) fn spool_path(data_dir: &Path) -> PathBuf {
    data_dir.join("events").join("streams.sqlite")
}

/// The tokens Steam refused, by hash.
#[must_use]
pub(crate) fn refused_gslts_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gslt-refused.json")
}

/// The node's own `SwiftlyS2` plugins, one directory each, which every launch reads.
#[must_use]
pub(crate) fn node_plugins_dir(data_dir: &Path) -> PathBuf {
    swiftly_dir(data_dir).join(plugins_dir!())
}

/// A node plugin's directory and config directory, relative to [`swiftly_dir`].
#[must_use]
pub(crate) fn node_plugin_dirs(plugin: &str) -> Vec<String> {
    PLUGIN_DIRS.iter().map(|(dir, _)| node_plugin_dir(dir, plugin)).collect()
}

/// `plugin`'s directory in the plugin directory `dir`, relative to [`swiftly_dir`].
fn node_plugin_dir(dir: &str, plugin: &str) -> String {
    format!("{dir}/{plugin}")
}

/// The image overlay's directory that holds the plugin and `SwiftlyS2`, mounted over the install's
/// addons. An image whose overlay lacks it is not a CS2 image.
pub(crate) const OVERLAY_ADDONS: &str = "addons";

/// Each overlay directory and the install path it is mounted over.
fn overlay_mounts() -> [(String, String); 2] {
    [(OVERLAY_ADDONS.to_owned(), ADDONS_DIR.to_owned()), (format!("cfg/{CFG_DIR}"), format!("{GAME_CFG_DIR}/{CFG_DIR}"))]
}

/// An install-relative path inside the overlay, or `None` outside its mounts.
#[must_use]
pub(crate) fn overlay_rel_path(install_rel_path: &str) -> Option<String> {
    let rel = install_rel_path.trim_end_matches('/');

    overlay_mounts().into_iter().find_map(|(source, target)| {
        if rel == target {
            Some(source)
        } else {
            rel.strip_prefix(target.as_str()).and_then(|rest| rest.strip_prefix('/')).map(|rest| format!("{source}/{rest}"))
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountpointKind {
    Directory,
    File,
}

/// Mountpoints to create in the overlay before it becomes read-only, relative to it.
#[must_use]
pub fn overlay_mountpoints() -> Vec<(String, MountpointKind)> {
    shadow_paths()
        .into_iter()
        .chain(per_launch_files())
        .filter_map(|rel| {
            let kind = if rel.ends_with('/') { MountpointKind::Directory } else { MountpointKind::File };

            overlay_rel_path(&rel).map(|path| (path, kind))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeMount {
    pub volume: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmpfsMount {
    pub target: String,
    pub size_bytes: i64,
    pub mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mounts {
    pub volumes: Vec<VolumeMount>,
    pub binds: Vec<BindMount>,
    pub tmpfs: Vec<TmpfsMount>,
}

/// Host-visible paths a game container's mounts come from.
pub(crate) struct MountSources<'a> {
    /// `<data_dir>/slots/<slot_id>`.
    pub slot_root: &'a str,
    /// `<data_dir>/shared`, which holds the [`SHARED_PATHS`].
    pub shared_root: &'a str,
    /// The extracted overlay directory.
    pub overlay_dir: &'a str,
    /// The node's `SwiftlyS2` directory, from [`swiftly_dir`].
    pub swiftly_root: &'a str,
    /// The node's cfg directory, from [`node_cfg_dir`].
    pub cfg_dir: &'a str,
    /// The node's own plugin ids, each with both its directories under `swiftly_root`.
    pub node_plugins: &'a [String],
    /// The per-launch files written for this launch.
    pub per_launch_files: &'a [String],
    /// This launch's demo directory, bound over the ankor shadow's `demos/`.
    pub demo_dir: &'a str,
}

fn join(root: &str, rel: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), rel.trim_end_matches('/'))
}

/// The shared install and overlay read-only, with per-slot writable shadows. A nested mount comes
/// after its parent. Per-launch files, for example, sit inside the overlay, the plugin tmpfs and
/// the ankor shadow.
///
/// # Panics
///
/// Never, because the plugin directories lie inside the overlay.
#[must_use]
pub(crate) fn resolve_mounts(sources: &MountSources) -> Mounts {
    let mut binds = Vec::new();
    let mut tmpfs = Vec::new();
    let bind = |source: String, target: String, read_only: bool| BindMount { source, target, read_only };

    for (source, target) in overlay_mounts() {
        binds.push(bind(join(sources.overlay_dir, &source), join(INSTALL_DIR, &target), true));
    }

    binds.push(bind(sources.cfg_dir.trim_end_matches('/').to_owned(), join(INSTALL_DIR, NODE_CFG_TARGET), true));

    for (dir, read_only) in PLUGIN_DIRS {
        let target = join(INSTALL_DIR, &format!("{SWIFTLYS2_DIR}/{dir}"));
        #[allow(clippy::expect_used, reason = "the plugin directories are inside the overlay by construction, which a test checks")]
        let ankor = overlay_rel_path(&format!("{SWIFTLYS2_DIR}/{dir}/{ANKOR_PLUGIN}")).expect("plugin directories are in the overlay");

        tmpfs.push(TmpfsMount { target: target.clone(), size_bytes: PLUGIN_TMPFS_BYTES, mode: 0o755 });
        binds.push(bind(join(sources.overlay_dir, &ankor), format!("{target}/{ANKOR_PLUGIN}"), true));

        for plugin in sources.node_plugins {
            binds.push(bind(join(sources.swiftly_root, &node_plugin_dir(dir, plugin)), format!("{target}/{plugin}"), read_only));
        }
    }

    for shadow in shadow_paths() {
        binds.push(bind(join(sources.slot_root, &shadow), join(INSTALL_DIR, &shadow), false));
    }

    binds.push(bind(sources.demo_dir.trim_end_matches('/').to_owned(), join(INSTALL_DIR, DEMO_DIR), false));

    for rel in sources.per_launch_files {
        binds.push(bind(join(sources.slot_root, rel), join(INSTALL_DIR, rel), true));
    }

    for shared in SHARED_PATHS {
        binds.push(bind(join(sources.shared_root, shared), join(INSTALL_DIR, shared), false));
    }

    Mounts { volumes: vec![VolumeMount { volume: DATA_VOLUME.to_owned(), target: INSTALL_DIR.to_owned(), read_only: true }], binds, tmpfs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::backup_file_name;

    #[test]
    fn names_a_backup_by_the_rounds_played_before_it() {
        assert_eq!(backup_file_name(0), "ankor_round00.txt");
        assert_eq!(backup_file_name(13), "ankor_round13.txt");
        assert_eq!(backup_file_name(100), "ankor_round100.txt");
        assert_eq!(slot_backup_dir(Path::new("/srv/ankor"), "a"), Path::new("/srv/ankor/slots/a/game/csgo/ankor/backups"));
    }

    #[test]
    fn keeps_the_refused_tokens_in_the_data_directory() {
        assert_eq!(refused_gslts_path(Path::new("/var/lib/ankor")), Path::new("/var/lib/ankor/gslt-refused.json"));
    }

    #[test]
    fn mounts_the_shared_install_read_only_and_shadows_every_writable_path() {
        let plugins = vec!["Other".to_owned()];
        let files = per_launch_files().to_vec();
        let mounts = resolve_mounts(&MountSources {
            slot_root: "/srv/ankor/slots/a",
            shared_root: "/srv/ankor/shared",
            overlay_dir: "/srv/ankor/overlay/sha256_deadbeef",
            swiftly_root: "/srv/ankor/swiftlys2",
            cfg_dir: "/srv/ankor/cfg",
            node_plugins: &plugins,
            per_launch_files: &files,
            demo_dir: "/srv/ankor/slots/a/game/csgo/ankor/demos/11111111-1111-4111-8111-111111111111",
        });
        let install = "/home/steam/cs2-dedicated";
        let bind = |source: &str, target: &str, read_only: bool| BindMount { source: source.to_owned(), target: format!("{install}/{target}"), read_only };

        assert_eq!(mounts.volumes, [VolumeMount { volume: "ankor_cs2_data".to_owned(), target: install.to_owned(), read_only: true }]);
        assert_eq!(
            mounts.binds,
            [
                bind("/srv/ankor/overlay/sha256_deadbeef/addons", "game/csgo/addons", true),
                bind("/srv/ankor/overlay/sha256_deadbeef/cfg/ankor", "game/csgo/cfg/ankor", true),
                bind("/srv/ankor/cfg", "game/csgo/cfg/node", true),
                bind("/srv/ankor/overlay/sha256_deadbeef/addons/swiftlys2/plugins/Ankor", "game/csgo/addons/swiftlys2/plugins/Ankor", true),
                bind("/srv/ankor/swiftlys2/plugins/Other", "game/csgo/addons/swiftlys2/plugins/Other", true),
                bind("/srv/ankor/overlay/sha256_deadbeef/addons/swiftlys2/configs/plugins/Ankor", "game/csgo/addons/swiftlys2/configs/plugins/Ankor", true),
                bind("/srv/ankor/swiftlys2/configs/plugins/Other", "game/csgo/addons/swiftlys2/configs/plugins/Other", false),
                bind("/srv/ankor/slots/a/game/csgo/ankor", "game/csgo/ankor", false),
                bind("/srv/ankor/slots/a/game/csgo/addons/swiftlys2/logs", "game/csgo/addons/swiftlys2/logs", false),
                bind("/srv/ankor/slots/a/game/csgo/addons/swiftlys2/dumps", "game/csgo/addons/swiftlys2/dumps", false),
                bind("/srv/ankor/slots/a/game/csgo/addons/swiftlys2/data", "game/csgo/addons/swiftlys2/data", false),
                bind("/srv/ankor/slots/a/game/csgo/ankor/demos/11111111-1111-4111-8111-111111111111", "game/csgo/ankor/demos", false),
                bind("/srv/ankor/slots/a/game/csgo/cfg/ankor_launch.cfg", "game/csgo/cfg/ankor_launch.cfg", true),
                bind(
                    "/srv/ankor/slots/a/game/csgo/addons/swiftlys2/configs/plugins/Ankor/config.jsonc",
                    "game/csgo/addons/swiftlys2/configs/plugins/Ankor/config.jsonc",
                    true
                ),
                bind("/srv/ankor/slots/a/game/csgo/ankor/match.json", "game/csgo/ankor/match.json", true),
                bind("/srv/ankor/slots/a/game/csgo/ankor/restore.txt", "game/csgo/ankor/restore.txt", true),
                bind("/srv/ankor/shared/game/bin/linuxsteamrt64/steamapps", "game/bin/linuxsteamrt64/steamapps", false),
            ]
        );
        assert_eq!(
            mounts.tmpfs,
            [
                TmpfsMount { target: format!("{install}/game/csgo/addons/swiftlys2/plugins"), size_bytes: 65536, mode: 0o755 },
                TmpfsMount { target: format!("{install}/game/csgo/addons/swiftlys2/configs/plugins"), size_bytes: 65536, mode: 0o755 },
            ]
        );
    }

    #[test]
    fn maps_install_relative_paths_into_the_overlay_and_only_those_under_it() {
        assert_eq!(overlay_rel_path("game/csgo/addons/swiftlys2/logs/").as_deref(), Some("addons/swiftlys2/logs"));
        assert_eq!(overlay_rel_path("game/csgo/cfg/ankor/base.cfg").as_deref(), Some("cfg/ankor/base.cfg"));
        assert_eq!(overlay_rel_path("game/csgo/addons").as_deref(), Some("addons"));
        assert_eq!(overlay_rel_path("game/csgo/ankor/"), None);
        assert_eq!(overlay_rel_path("game/csgo/cfg/ankor_launch.cfg"), None);
        assert_eq!(overlay_rel_path("game/bin/linuxsteamrt64/anything.log"), None);
    }

    #[test]
    fn derives_the_mountpoints_the_agent_creates_inside_the_overlay() {
        assert_eq!(
            overlay_mountpoints(),
            [
                ("addons/swiftlys2/logs".to_owned(), MountpointKind::Directory),
                ("addons/swiftlys2/dumps".to_owned(), MountpointKind::Directory),
                ("addons/swiftlys2/data".to_owned(), MountpointKind::Directory),
                ("addons/swiftlys2/configs/plugins/Ankor/config.jsonc".to_owned(), MountpointKind::File),
            ]
        );
    }
}
