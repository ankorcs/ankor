//! Container identity, networking and resource restrictions shared by games and helpers.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// The label every container the agent creates carries; its value says what the container is for.
pub(crate) const CONTAINER_ROLE_LABEL: &str = "ankor.role";

/// The agent finds its own container by `role_filter(ContainerRole::Agent)`. `node-setup.sh` sets
/// that label on it.
#[must_use]
pub(crate) fn role_filter(role: ContainerRole) -> String {
    format!("{CONTAINER_ROLE_LABEL}={}", role.as_str())
}

pub mod labels {
    pub const ALLOCATION: &str = "ankor.allocation";
    /// The match a game container plays, fixed for the container's life.
    pub(crate) const MATCH: &str = "ankor.match";
    /// The launch's `idle_shutdown_seconds`, which an adopted game is stopped by too.
    pub(crate) const IDLE_SHUTDOWN: &str = "ankor.idle-shutdown";
    pub(crate) const SLOT: &str = "ankor.slot";
    /// The key a game's plugin dials the relay with, read back when the game is adopted.
    pub(crate) const RELAY_KEY: &str = "ankor.relay-key";
    /// The token a game runs on, read back when the game is adopted, as the slot may have a new one.
    pub(crate) const GSLT: &str = "ankor.gslt";
    pub const ROLE: &str = super::CONTAINER_ROLE_LABEL;
    /// Pins the mounted overlay, by image id, while the container runs.
    pub(crate) const OVERLAY: &str = "ankor.overlay";
    pub const UPDATE_MODE: &str = "ankor.update-mode";
    /// `true` on an update container the agent started to reach the newest public build.
    pub const UPDATE_AUTOMATIC: &str = "ankor.update-automatic";
    pub const UPDATE_ID: &str = "ankor.update-id";
}

/// Set by both image builds. The image sweep lists by it, as a label outlives the tag.
pub(crate) const IMAGE_LABEL: &str = "ankor.image";
/// What [`IMAGE_LABEL`] says on the agent's image.
pub(crate) const AGENT_IMAGE: &str = "agent";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContainerRole {
    Game,
    Update,
    Overlay,
    Probe,
    SelfUpdate,
    Agent,
}

impl ContainerRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Update => "update",
            Self::Overlay => "overlay",
            Self::Probe => "probe",
            Self::SelfUpdate => "self-update",
            Self::Agent => "agent",
        }
    }

    /// Every role, so that `parse` knows each one `as_str` names.
    pub const ALL: [Self; 6] = [Self::Game, Self::Update, Self::Overlay, Self::Probe, Self::SelfUpdate, Self::Agent];

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == text)
    }
}

/// One-shot helpers a boot removes once they have exited, or were created and never started, as when
/// an agent stopped between the two. A running update is adopted instead. A self-update helper waits
/// for the next swap.
pub(crate) const SWEPT_ROLES: [ContainerRole; 3] = [ContainerRole::Overlay, ContainerRole::Probe, ContainerRole::Update];

/// Docker's `local` log driver rotates and compresses by itself. Non-blocking mode drops lines when
/// the buffer is full, so a slow log driver never blocks a console write. `node-setup.sh` starts
/// the agent with the same options.
pub(crate) fn log_config() -> Value {
    json!({ "Type": "local", "Config": { "mode": "non-blocking", "max-buffer-size": "4m" } })
}

pub(crate) const PIDS_LIMIT: i64 = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContainerHardening {
    /// Linux capabilities the container gives up.
    pub cap_drop: Vec<String>,
    /// Security options, which forbid privilege escalation.
    pub security_opt: Vec<String>,
    /// The most processes the container may run.
    pub pids_limit: i64,
    /// `HostConfig.Memory` in bytes, or `None` for no limit.
    pub memory_bytes: Option<i64>,
}

/// `memory_mb` comes from `agent.yml`, which `set_config` can change.
#[must_use]
pub(crate) fn container_hardening(memory_mb: Option<i64>) -> ContainerHardening {
    ContainerHardening {
        cap_drop: vec!["ALL".to_owned()],
        security_opt: vec!["no-new-privileges=true".to_owned()],
        pids_limit: PIDS_LIMIT,
        // The contract keeps the value within MAX_MEMORY_MB; saturating keeps any other value safe.
        memory_bytes: memory_mb.map(|mb| mb.saturating_mul(1024 * 1024)),
    }
}

const CPU_SHARES_MIN: i64 = 2;
const CPU_SHARES_MAX: i64 = 262_144;

/// The cgroup v2 weight every game slot runs at under CPU contention. It reserves no CPU time.
pub const GAME_CPU_WEIGHT: i64 = 1000;

/// Docker CPU shares for a cgroup v2 weight, rounded up so the kernel's conversion back
/// (`1 + (shares - 2) * 9999 / 262142`) does not undershoot.
#[must_use]
pub fn cpu_shares_for_weight(weight: i64) -> i64 {
    // A weight of 1 or less is the least Docker takes.
    let scaled = (weight - 1).max(0) * 262_142;
    let shares = CPU_SHARES_MIN + (scaled + 9_998) / 9_999;

    shares.clamp(CPU_SHARES_MIN, CPU_SHARES_MAX)
}

#[derive(Clone, Debug, Default)]
pub struct LabelInput<'a> {
    pub slot_id: Option<&'a str>,
    pub overlay_image_id: Option<&'a str>,
    pub allocation_id: Option<&'a str>,
    pub match_id: Option<&'a str>,
    pub idle_shutdown_seconds: Option<i64>,
    pub relay_key: Option<&'a str>,
    pub gslt: Option<&'a str>,
}

#[must_use]
pub fn container_labels(role: ContainerRole, input: &LabelInput) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([(labels::ROLE.to_owned(), role.as_str().to_owned())]);
    let mut put = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            labels.insert(key.to_owned(), value);
        }
    };

    put(labels::ALLOCATION, input.allocation_id.map(str::to_owned));
    put(labels::SLOT, input.slot_id.map(str::to_owned));
    put(labels::MATCH, input.match_id.map(str::to_owned));
    put(labels::IDLE_SHUTDOWN, input.idle_shutdown_seconds.map(|seconds| seconds.to_string()));
    put(labels::OVERLAY, input.overlay_image_id.map(str::to_owned));
    put(labels::RELAY_KEY, input.relay_key.map(str::to_owned));
    put(labels::GSLT, input.gslt.map(str::to_owned));

    labels
}

#[must_use]
pub(crate) fn container_name(slot_id: &str, role: ContainerRole, short_id: &str) -> String {
    format!("ankor-{slot_id}-{}-{short_id}", role.as_str())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::overlay::ExtractedOverlay;
    use crate::slot_manager::build_server_launch;
    use crate::testing::fixtures::{ALLOCATION_ID, agent_config, prepared_slot, server_launch_config};

    #[test]
    fn labels_containers_so_recovery_can_find_them() {
        let labels = container_labels(ContainerRole::Game, &LabelInput { slot_id: Some("a"), ..LabelInput::default() });

        assert_eq!(labels, BTreeMap::from([("ankor.slot".to_owned(), "a".to_owned()), ("ankor.role".to_owned(), "game".to_owned())]));
        assert_eq!(container_name("a", ContainerRole::Game, "ab12cd"), "ankor-a-game-ab12cd");
    }

    #[test]
    fn leaves_out_every_label_its_input_has_none_for() {
        assert_eq!(container_labels(ContainerRole::Update, &LabelInput::default()), BTreeMap::from([("ankor.role".to_owned(), "update".to_owned())]));

        let full = container_labels(
            ContainerRole::Game,
            &LabelInput {
                slot_id: Some("a"),
                allocation_id: Some(ALLOCATION_ID),
                match_id: Some("m1"),
                idle_shutdown_seconds: Some(0),
                overlay_image_id: Some("sha256:deadbeef"),
                relay_key: Some("0123456789abcdef"),
                gslt: Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001"),
            },
        );

        assert_eq!(full["ankor.idle-shutdown"], "0");
        assert_eq!(full["ankor.match"], "m1");
        assert_eq!(full["ankor.overlay"], "sha256:deadbeef");
        assert_eq!(full["ankor.relay-key"], "0123456789abcdef");
        assert_eq!(full["ankor.gslt"], "AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001");
        assert_eq!(full.len(), 8);
    }

    /// Docker's conversion, which [`cpu_shares_for_weight`] inverts. Its integer division is why the
    /// inverse rounds up.
    fn cpu_weight_for_shares(shares: i64) -> i64 {
        1 + (shares - 2) * 9999 / 262_142
    }

    #[test]
    fn round_trips_every_weight_through_dockers_conversion() {
        for weight in [1, 2, 10, 39, 100, 500, 1000, 2500, 9999, 10000] {
            assert_eq!(cpu_weight_for_shares(cpu_shares_for_weight(weight)), weight, "weight {weight}");
        }
    }

    #[test]
    fn maps_the_documented_values_and_clamps_to_dockers_range() {
        assert_eq!(cpu_shares_for_weight(100), 2598);
        assert_eq!(cpu_shares_for_weight(GAME_CPU_WEIGHT), 26193);
        assert_eq!(cpu_shares_for_weight(1), 2);
        assert_eq!(cpu_shares_for_weight(10000), 262_144);
        assert_eq!(cpu_shares_for_weight(0), 2);
        assert_eq!(cpu_shares_for_weight(-5), 2);
        assert_eq!(cpu_shares_for_weight(20000), 262_144);
    }

    #[test]
    fn drops_every_capability_forbids_escalation_and_caps_pids() {
        let mib: i64 = 1024 * 1024;

        assert_eq!(
            container_hardening(None),
            ContainerHardening {
                cap_drop: vec!["ALL".to_owned()],
                security_opt: vec!["no-new-privileges=true".to_owned()],
                pids_limit: PIDS_LIMIT,
                memory_bytes: None
            }
        );
        assert_eq!(container_hardening(Some(4096)).memory_bytes, Some(4096 * mib));
        assert_eq!(container_hardening(Some(i64::MAX / 2)).memory_bytes, Some(i64::MAX));
    }

    #[test]
    fn hardens_a_server_container_with_the_hosts_memory_cap() {
        let config = agent_config(&json!({ "memory_mb": 6144 }));
        let overlay = ExtractedOverlay { image_id: "sha256:deadbeef".to_owned(), dir: "/srv/ankor/overlay/sha256_deadbeef".into() };
        let spec = build_server_launch(
            &config,
            &config.slots[0],
            &server_launch_config(&json!({})),
            "ws://127.0.0.1:4500/ws/plugin?slot_id=a",
            "0123456789abcdef",
            "ab12cd",
            &prepared_slot(),
            &overlay,
            ALLOCATION_ID,
        );

        assert_eq!(spec.cap_drop, ["ALL"]);
        assert_eq!(spec.security_opt, ["no-new-privileges=true"]);
        assert_eq!(spec.pids_limit, PIDS_LIMIT);
        assert_eq!(spec.memory_bytes, Some(6144 * 1024 * 1024));
    }
}
