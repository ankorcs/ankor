//! The node's `agent.yml`: read and validated, and patched in place with its comments kept.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};

use ankor_protocol::config::{AgentConfig, DemoUpload};
use ankor_protocol::constants::{DEFAULT_EVENTS_PATH, DEFAULT_SOCKET_PATH};
use ankor_protocol::helpers::format_config_error;
use ankor_protocol::schemas::s_agent_config_schema;
use ankor_protocol::types::{ConfiguredSlot, ControllerFields, DemoUploadFields, DemoUploadHeader, RawAgentConfig};
use ankor_protocol::validate::safe_parse;
use serde_json::{Map, Value, json};

use crate::sync::lock;

/// The config the node runs with. A [`ConfigStore`] changes it.
pub type SharedConfig = Arc<Mutex<AgentConfig>>;

/// Writes a patch to `agent.yml`, as [`persist_config_patch`] does.
pub(crate) type Persist = Arc<dyn Fn(&Path, &ConfigPatch) -> Result<(), ConfigError> + Send + Sync>;

/// `agent.yml` and the config in memory, kept as one. Changes run one at a time: each reads the
/// config, writes the file and applies the patch in memory before the next one reads.
#[derive(Clone)]
pub struct ConfigStore {
    path: PathBuf,
    memory: SharedConfig,
    persist: Persist,
    changes: Arc<Mutex<()>>,
}

/// The config in memory of a [`ConfigStore`], which it reads but cannot change.
#[derive(Clone)]
pub struct ConfigReader {
    memory: SharedConfig,
}

impl ConfigReader {
    #[must_use]
    pub fn read(&self) -> AgentConfig {
        lock(&self.memory).clone()
    }
}

/// Writes a change's patch, given to [`ConfigStore::change`].
pub struct ConfigWrite<'a> {
    store: &'a ConfigStore,
}

impl ConfigWrite<'_> {
    /// Writes `patch` to `agent.yml` before applying it in memory.
    ///
    /// # Errors
    ///
    /// Fails as the write fails, and then leaves memory as it was.
    pub fn write(&self, patch: &ConfigPatch) -> Result<AgentConfig, ConfigError> {
        (self.store.persist)(&self.store.path, patch)?;

        let mut memory = lock(&self.store.memory);

        apply_patch(&mut memory, patch);
        Ok(memory.clone())
    }
}

impl ConfigStore {
    #[must_use]
    pub fn new(path: PathBuf, config: AgentConfig) -> Self {
        Self::over(path, Arc::new(Mutex::new(config)))
    }

    /// Shares `memory` with the caller, so a test can read it too.
    #[must_use]
    pub fn over(path: PathBuf, memory: SharedConfig) -> Self {
        Self { path, memory, persist: Arc::new(persist_config_patch), changes: Arc::default() }
    }

    #[must_use]
    pub fn with_persist(&self, persist: Persist) -> Self {
        Self { persist, ..self.clone() }
    }

    #[must_use]
    pub fn at(&self, path: PathBuf) -> Self {
        Self { path, ..self.clone() }
    }

    #[must_use]
    pub fn read(&self) -> AgentConfig {
        lock(&self.memory).clone()
    }

    #[must_use]
    pub fn reader(&self) -> ConfigReader {
        ConfigReader { memory: Arc::clone(&self.memory) }
    }

    /// Runs `change` after every earlier change has written the file and memory.
    ///
    /// `change` runs under a std mutex on the calling thread, and its write syncs `agent.yml` to
    /// disk there. Config changes are rare, so the wait is accepted. `change` must not call
    /// [`ConfigStore::change`] again, because the mutex is not reentrant and the call would hang.
    /// `set_config` takes the slot lock inside `change`, so no holder of the slot lock may call
    /// `change`.
    ///
    /// # Errors
    ///
    /// Fails as `change` does.
    pub fn change<T, E>(&self, change: impl FnOnce(&AgentConfig, &ConfigWrite<'_>) -> Result<T, E>) -> Result<T, E> {
        let _changing = lock(&self.changes);
        let current = self.read();

        change(&current, &ConfigWrite { store: self })
    }
}

fn apply_patch(config: &mut AgentConfig, patch: &ConfigPatch) {
    let ConfigPatch { controller, cs2_image, agent_image, pinned_ip, auto_update, demo_upload, memory_mb, slots } = patch.clone();

    if let Some(controller) = controller {
        config.controller = controller;
    }

    if let Some(image) = cs2_image {
        config.cs2_image = image;
    }

    if let Some(image) = agent_image {
        config.agent_image = image;
    }

    if let Some(ip) = pinned_ip {
        config.pinned_ip = ip;
    }

    if let Some(auto) = auto_update {
        config.auto_update = auto;
    }

    if let Some(upload) = demo_upload {
        config.demo_upload = upload;
    }

    if let Some(memory) = memory_mb {
        config.memory_mb = memory;
    }

    if let Some(slots) = slots {
        config.slots = slots;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Reading or writing `agent.yml` failed while saving a patch.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// `agent.yml` could not be read, so the agent has no config to run with.
    #[error("{}", unreadable(path, source))]
    Read { path: PathBuf, source: std::io::Error },
    /// `agent.yml`, or the file a patch would make, is not valid YAML or does not match the schema.
    #[error("{0}")]
    Invalid(String),
}

/// Why `agent.yml` could not be read, with what fixes the two causes a node meets.
fn unreadable(path: &Path, error: &std::io::Error) -> String {
    let path = path.display();

    match error.kind() {
        std::io::ErrorKind::NotFound => {
            format!("no agent config at {path}: node-setup.sh writes it, so run it on this node, or mount the node's data directory into the agent container")
        }
        // node-setup.sh writes it owned by the agent's user, mode 0600, as it holds the node token.
        std::io::ErrorKind::PermissionDenied => {
            let id = crate::privileges::AGENT_ID;

            format!("cannot read {path}: permission denied. The agent reads it as uid {id}; give that user the file again with chown {id}:{id} {path}")
        }
        _ => format!("cannot read {path}: {error}"),
    }
}

fn parse_yaml(path: &Path, text: &str) -> Result<Value, ConfigError> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }

    let invalid = |error: &dyn std::fmt::Display| ConfigError::Invalid(format!("invalid {}:\n{error}", path.display()));
    // Read as YAML's own value first, which refuses a key given twice instead of keeping the last.
    let yaml: yaml_serde::Value = yaml_serde::from_str(text).map_err(|error| invalid(&error))?;

    serde_json::to_value(yaml).map_err(|error| invalid(&error))
}

fn validate(path: &Path, raw: Value) -> Result<AgentConfig, ConfigError> {
    let parsed = safe_parse(s_agent_config_schema, raw)
        .map_err(|issues| ConfigError::Invalid(format!("invalid {}:\n{}", path.display(), format_config_error(&issues))))?;

    serde_json::from_value(parsed).map_err(|error| ConfigError::Invalid(format!("invalid {}:\n{error}", path.display())))
}

/// Reads `agent.yml`, calling `on_unsupported` with each key the agent does not read.
///
/// # Errors
///
/// Fails when the file cannot be read, is not YAML, or does not match the schema.
pub fn load_agent_config(path: &Path, mut on_unsupported: impl FnMut(&str)) -> Result<AgentConfig, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read { path: path.to_owned(), source })?;
    let raw = parse_yaml(path, &text)?;

    for key in unsupported_agent_yml_keys(&raw) {
        on_unsupported(&key);
    }

    validate(path, raw)
}

/// Keys the file carries that the agent does not read, as dotted paths.
#[must_use]
pub(crate) fn unsupported_agent_yml_keys(raw: &Value) -> Vec<String> {
    fn unknown(value: Option<&Value>, known: &[&str], prefix: &str) -> Vec<String> {
        value
            .and_then(Value::as_object)
            .map_or_else(Vec::new, |object| object.keys().filter(|key| !known.contains(&key.as_str())).map(|key| format!("{prefix}{key}")).collect())
    }

    let mut keys = unknown(Some(raw), RawAgentConfig::FIELDS, "");

    if let Some(slots) = raw.get("slots").and_then(Value::as_array) {
        for (index, slot) in slots.iter().enumerate() {
            keys.extend(unknown(Some(slot), ConfiguredSlot::FIELDS, &format!("slots.{index}.")));
        }
    }

    keys.extend(unknown(raw.get("controller"), ControllerFields::FIELDS, "controller."));
    keys.extend(unknown(raw.get("demo_upload"), DemoUploadFields::FIELDS, "demo_upload."));
    keys.extend(unknown(raw.get("demo_upload").and_then(|upload| upload.get("header")), DemoUploadHeader::FIELDS, "demo_upload.header."));

    keys
}

/// The keys the controller may change. `None` leaves a key alone; `Some(None)` removes it where it may be
/// absent.
#[derive(Clone, Debug, Default)]
pub struct ConfigPatch {
    pub controller: Option<ControllerFields>,
    pub cs2_image: Option<String>,
    pub agent_image: Option<String>,
    pub pinned_ip: Option<Option<String>>,
    pub auto_update: Option<bool>,
    pub demo_upload: Option<Option<DemoUpload>>,
    pub memory_mb: Option<Option<i64>>,
    pub slots: Option<Vec<ConfiguredSlot>>,
}

/// The patch under its `agent.yml` keys, in file order. `None` removes the key; a value at its
/// default is removed too, so the file only states what differs.
#[must_use]
pub(crate) fn agent_yml_entries(patch: &ConfigPatch) -> Vec<(&'static str, Option<Value>)> {
    let mut entries = Vec::new();

    if let Some(controller) = &patch.controller {
        let mut fields = Map::new();

        fields.insert("host".to_owned(), json!(controller.host));
        fields.insert("tls".to_owned(), json!(controller.tls));

        if controller.socket_path != DEFAULT_SOCKET_PATH {
            fields.insert("socket_path".to_owned(), json!(controller.socket_path));
        }

        if controller.events_path != DEFAULT_EVENTS_PATH {
            fields.insert("events_path".to_owned(), json!(controller.events_path));
        }

        entries.push(("controller", Some(Value::Object(fields))));
    }

    if let Some(image) = &patch.cs2_image {
        entries.push(("cs2_image", Some(json!(image))));
    }

    if let Some(image) = &patch.agent_image {
        entries.push(("agent_image", Some(json!(image))));
    }

    if let Some(ip) = &patch.pinned_ip {
        entries.push(("pinned_ip", ip.as_ref().map(|ip| json!(ip))));
    }

    if let Some(memory) = patch.memory_mb {
        entries.push(("memory_mb", memory.map(|mb| json!(mb))));
    }

    if let Some(auto) = patch.auto_update {
        entries.push(("auto_update", (!auto).then(|| json!(false))));
    }

    if let Some(upload) = &patch.demo_upload {
        entries.push((
            "demo_upload",
            upload.as_ref().map(|upload| {
                let mut fields = Map::new();

                fields.insert("url".to_owned(), json!(upload.url));

                if let Some(header) = &upload.header {
                    fields.insert("header".to_owned(), json!({ "name": header.name, "value": header.value }));
                }

                if let Some(limit) = upload.limit_mbit {
                    fields.insert("limit_mbit".to_owned(), json!(limit));
                }

                Value::Object(fields)
            }),
        ));
    }

    if let Some(slots) = &patch.slots {
        let slots = slots.iter().map(|slot| json!({ "id": slot.id, "port": slot.port, "tv_port": slot.tv_port, "gslt": slot.gslt })).collect();

        entries.push(("slots", Some(Value::Array(slots))));
    }

    entries
}

/// `source` with the patch applied, keeping its comments and key order.
///
/// # Errors
///
/// Fails when `source` is not a YAML mapping, or the patched file would not be a valid `agent.yml`.
pub fn patch_config_document(path: &Path, source: &str, patch: &ConfigPatch) -> Result<String, ConfigError> {
    let invalid = |error: &dyn std::fmt::Display| ConfigError::Invalid(format!("invalid {}:\n{error}", path.display()));
    let document = yaml_edit::Document::from_str(source).map_err(|error| invalid(&error))?;
    let root = document.as_mapping().ok_or_else(|| invalid(&"not a mapping"))?;

    for (key, value) in agent_yml_entries(patch) {
        match value {
            None => {
                root.remove(key);
            }
            // yaml-edit writes the maps it builds as `!!omap`, so a nested value is rendered by
            // yaml_serde and spliced in.
            Some(value @ (Value::Object(_) | Value::Array(_))) => {
                let rendered = yaml_serde::to_string(&value).map_err(|error| invalid(&error))?;
                let node = yaml_edit::Document::from_str(&rendered).map_err(|error| invalid(&error))?;

                match (node.as_mapping(), node.as_sequence()) {
                    (Some(mapping), _) => root.set(key, mapping),
                    (_, Some(sequence)) => root.set(key, sequence),
                    _ => return Err(invalid(&format!("could not render {key}"))),
                }
            }
            Some(Value::String(text)) => root.set(key, text.as_str()),
            Some(Value::Bool(flag)) => root.set(key, flag),
            // As a number, not a string, which `memory_mb` would refuse.
            Some(Value::Number(number)) if let Some(whole) = number.as_i64() => root.set(key, whole),
            Some(other) => root.set(key, other.to_string().as_str()),
        }
    }

    let patched = document.to_string();

    validate(path, parse_yaml(path, &patched)?)?;

    Ok(patched)
}

/// Replaces the file at `path` so a reader or a crash finds the old file or the new one, never half.
/// The temporary file is created beside it with mode `0600`.
///
/// The directory is synced after the rename, so the rename survives a power loss. A failed directory
/// sync is not reported, because the change is already visible to every reader.
///
/// # Errors
///
/// Fails as creating, writing or renaming the file fails, and then leaves the old file.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(directory)?;

    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;

    let _ = File::open(directory).and_then(|directory| directory.sync_all());

    Ok(())
}

/// # Errors
///
/// Fails as reading, patching or writing fails.
pub fn persist_config_patch(path: &Path, patch: &ConfigPatch) -> Result<(), ConfigError> {
    let patched = patch_config_document(path, &fs::read_to_string(path)?, patch)?;

    write_atomic(path, &patched)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ankor_protocol::constants::{MAX_MEMORY_MB, MIN_MEMORY_MB};

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz01";
    const GSLT: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn file() -> String {
        format!(
            "# written by setup\ntoken: {TOKEN} # keep secret\ncontroller:\n  host: cs.example.com\n  tls: true\ncs2_image: ghcr.io/ankorcs/cs2:1\nagent_image: ghcr.io/ankorcs/agent:1\nauto_update: false\nslots:\n  - id: s1\n    port: 27015\n    tv_port: 27020\n    gslt: {GSLT}\nextra_key: 1\n"
        )
    }

    fn write(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yml");

        fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn names_the_file_and_the_fix_when_agent_yml_is_missing_or_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("agent.yml");
        let error = load_agent_config(&missing, |_| {}).unwrap_err().to_string();

        assert_eq!(
            error,
            format!(
                "no agent config at {}: node-setup.sh writes it, so run it on this node, or mount the node's data directory into the agent container",
                missing.display()
            )
        );

        let (_dir, path) = write(&file());

        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let error = load_agent_config(&path, |_| {}).unwrap_err().to_string();
        let shown = path.display();
        let id = crate::privileges::AGENT_ID;

        assert_eq!(
            error,
            format!("cannot read {shown}: permission denied. The agent reads it as uid {id}; give that user the file again with chown {id}:{id} {shown}")
        );
    }

    #[test]
    fn names_the_file_and_the_read_error_when_agent_yml_is_not_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yml");

        fs::write(&path, [0xff]).unwrap();

        let error = load_agent_config(&path, |_| {}).unwrap_err().to_string();

        assert_eq!(error, format!("cannot read {}: stream did not contain valid UTF-8", path.display()));
    }

    #[test]
    fn loads_and_reports_unsupported_keys() {
        let (_dir, path) = write(&file());
        let mut unsupported = Vec::new();
        let config = load_agent_config(&path, |key| unsupported.push(key.to_owned())).unwrap();

        assert_eq!(unsupported, ["extra_key"]);
        assert!(!config.auto_update);
        assert_eq!(config.pinned_ip, None);
        assert_eq!(config.demo_upload, None);
        assert_eq!(config.controller.socket_path, DEFAULT_SOCKET_PATH);
        assert_eq!(config.slots[0].port, 27015);
    }

    #[test]
    fn invalid_files_name_their_issues() {
        let (_dir, path) = write("token: short\n");
        let error = load_agent_config(&path, |_| {}).unwrap_err().to_string();

        assert!(error.starts_with(&format!("invalid {}:\n  token: ", path.display())), "{error}");
        assert!(error.contains("  controller: Invalid input: expected object, received undefined"), "{error}");
    }

    #[test]
    fn patches_keep_comments_and_plain_image_lines() {
        let (_dir, path) = write(&file());
        let patch = ConfigPatch {
            cs2_image: Some("ghcr.io/ankorcs/cs2@sha256:0123".to_owned()),
            auto_update: Some(true),
            pinned_ip: Some(Some("10.0.0.2".to_owned())),
            demo_upload: Some(Some(DemoUpload {
                url: "https://up.example.com/d".to_owned(),
                header: Some(DemoUploadHeader { name: "Authorization".to_owned(), value: "Bearer: x # y".to_owned() }),
                limit_mbit: None,
            })),
            ..ConfigPatch::default()
        };

        persist_config_patch(&path, &patch).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let config = load_agent_config(&path, |_| {}).unwrap();

        assert!(text.contains("# written by setup\n") && text.contains("# keep secret"), "{text}");
        assert!(text.contains("\ncs2_image: ghcr.io/ankorcs/cs2@sha256:0123\n"), "{text}");
        assert!(!text.contains("auto_update"), "{text}");
        assert!(config.auto_update);
        assert_eq!(config.pinned_ip.as_deref(), Some("10.0.0.2"));
        assert_eq!(config.demo_upload.unwrap().header.unwrap().value, "Bearer: x # y");
    }

    const GOOD_YAML: &str = "
token: 0123456789abcdef0123456789abcdef
controller:
  host: cs.example.com
  tls: true
pinned_ip: 203.0.113.7
cs2_image: ghcr.io/example/ankor-cs2:latest
agent_image: ghcr.io/example/ankor-agent:latest
slots:
  - id: a
    port: 27015
    tv_port: 27020
    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001
  - id: b
    port: 27016
    tv_port: 27021
    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0002
";

    fn load(text: &str) -> Result<AgentConfig, String> {
        let (_dir, path) = write(text);

        load_agent_config(&path, |_| {}).map_err(|error| error.to_string())
    }

    fn issues(text: &str) -> Vec<String> {
        load(text).err().map(|error| error.lines().skip(1).map(|line| line.trim().to_owned()).collect()).unwrap_or_default()
    }

    #[test]
    fn accepts_a_minimal_file_and_fills_in_the_optional_keys() {
        let config = load(GOOD_YAML).unwrap();

        assert_eq!(config.slots.len(), 2);
        assert_eq!(config.slots[0], ConfiguredSlot { id: "a".to_owned(), port: 27015, tv_port: 27020, gslt: "AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001".to_owned() });
        assert_eq!(config.memory_mb, None);
        assert!(config.auto_update);
        assert!(config.official_images_only);
        assert_eq!((config.controller.socket_path.as_str(), config.controller.events_path.as_str()), (DEFAULT_SOCKET_PATH, DEFAULT_EVENTS_PATH));
        assert_eq!(load(&GOOD_YAML.replace("pinned_ip: 203.0.113.7\n", "")).unwrap().pinned_ip, None);
    }

    #[test]
    fn takes_a_memory_cap_and_refuses_nonsense_for_it() {
        assert_eq!(load(&format!("{GOOD_YAML}memory_mb: 6144\n")).unwrap().memory_mb, Some(6144));

        let (min, max) = (i64::try_from(MIN_MEMORY_MB).unwrap(), i64::try_from(MAX_MEMORY_MB).unwrap());

        assert_eq!(load(&format!("{GOOD_YAML}memory_mb: {min}\n")).unwrap().memory_mb, Some(min));
        assert_eq!(load(&format!("{GOOD_YAML}memory_mb: {max}\n")).unwrap().memory_mb, Some(max));

        for value in ["0", "-512", "2048.5", "'2048'", &(min - 1).to_string(), &(max + 1).to_string()] {
            assert!(load(&format!("{GOOD_YAML}memory_mb: {value}\n")).is_err(), "{value}");
        }
    }

    #[test]
    fn runs_only_official_images_unless_official_images_only_is_false() {
        assert!(load(&format!("{GOOD_YAML}official_images_only: true\n")).unwrap().official_images_only);
        assert!(!load(&format!("{GOOD_YAML}official_images_only: false\n")).unwrap().official_images_only);
        assert!(load(&format!("{GOOD_YAML}official_images_only: 'no'\n")).is_err());
    }

    #[test]
    fn keeps_itself_up_to_date_unless_auto_update_is_false() {
        assert!(!load(&format!("{GOOD_YAML}auto_update: false\n")).unwrap().auto_update);
        assert!(load(&format!("{GOOD_YAML}auto_update: 'no'\n")).is_err());
    }

    #[test]
    fn takes_the_paths_the_file_names_and_refuses_bad_ones() {
        let config = load(&GOOD_YAML.replace("  tls: true\n", "  tls: true\n  socket_path: /ankor/agent\n  events_path: /ankor/node%20events\n")).unwrap();

        assert_eq!((config.controller.socket_path.as_str(), config.controller.events_path.as_str()), ("/ankor/agent", "/ankor/node%20events"));

        for path in ["ws/agent", "''", "/ws/agent?x=1", "'/ws/agent#top'", "'/ws agent'", "/ws/%zz", "'/ws\\agent'"] {
            for key in ["socket_path", "events_path"] {
                assert!(load(&GOOD_YAML.replace("  tls: true\n", &format!("  tls: true\n  {key}: {path}\n"))).is_err(), "{key}: {path}");
            }
        }

        assert!(load(&GOOD_YAML.replace("  tls: true\n", &format!("  tls: true\n  socket_path: /{}\n", "a".repeat(255)))).is_err());
    }

    #[test]
    fn reads_a_file_with_no_slots() {
        let text =
            "token: 0123456789abcdef0123456789abcdef\ncontroller:\n  host: cs.example.com\n  tls: true\ncs2_image: img\nagent_image: img-agent\nslots: []\n";

        assert_eq!(load(text).unwrap().slots.len(), 0);
    }

    #[allow(clippy::format_collect, reason = "test YAML, built for reading")]
    fn slots(entries: &[(&str, i64, i64, Option<&str>)]) -> String {
        let slots: String = entries
            .iter()
            .map(|(id, port, tv_port, gslt)| {
                format!("  - id: {id}\n    port: {port}\n    tv_port: {tv_port}\n{}", gslt.map(|gslt| format!("    gslt: {gslt}\n")).unwrap_or_default())
            })
            .collect();

        format!(
            "token: 0123456789abcdef0123456789abcdef\ncontroller:\n  host: cs.example.com\n  tls: true\ncs2_image: img\nagent_image: img-agent\nslots:\n{slots}"
        )
    }

    #[test]
    fn rejects_duplicate_slot_ids_and_reused_ports() {
        let found =
            issues(&slots(&[("a", 27015, 27020, Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001")), ("a", 27020, 27021, Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAA0002"))]));

        assert!(found.iter().any(|issue| issue.contains("duplicate slot id")), "{found:?}");
        assert!(found.iter().any(|issue| issue.contains("used by more than one slot")), "{found:?}");
        assert!(load(&slots(&[("\"slot one!\"", 27015, 27020, Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001"))])).is_err());
    }

    #[test]
    fn requires_each_slots_own_token_in_steams_alphabet() {
        let a = "AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001";
        let b = "AAAAAAAAAAAAAAAAAAAAAAAAAAAA0002";

        assert_eq!(issues(&slots(&[("s0", 27015, 27016, Some(a)), ("s1", 27017, 27018, Some(b))])).len(), 0);

        let missing = issues(&slots(&[("s0", 27015, 27016, Some(a)), ("s1", 27017, 27018, None)]));

        assert_eq!(missing.len(), 1);
        assert!(missing[0].starts_with("slots.1.gslt: "), "{missing:?}");
        assert!(issues(&slots(&[("s0", 27015, 27016, Some("not-a-token!"))])).contains(&"slots.0.gslt: a token is 10 to 64 letters and digits".to_owned()));
        assert_eq!(issues(&slots(&[("s0", 27015, 27016, Some(a)), ("s1", 27017, 27018, Some(a))])), ["slots.1.gslt: each slot needs its own token"]);
    }

    #[test]
    fn rejects_a_missing_token_an_unusable_host_and_an_out_of_range_port() {
        let found = issues(
            "controller:\n  host: https://cs.example.com/lobby\n  tls: true\ncs2_image: img\nagent_image: img-agent\nslots:\n  - id: a\n    port: 99999\n    tv_port: 27020\n",
        );
        let paths: Vec<&str> = found.iter().map(|issue| issue.split(':').next().unwrap_or_default()).collect();

        assert!(paths.contains(&"token") && paths.contains(&"controller.host") && paths.contains(&"slots.0.port"), "{found:?}");
    }

    #[test]
    fn names_the_keys_it_ignores_nested_ones_included() {
        let text = format!("{GOOD_YAML}docker_socket: /var/run/docker.sock\ndemo_upload:\n  url: https://demos.example.com\n  retries: 3\n")
            .replace("    tv_port: 27020\n", "    tv_port: 27020\n    rcon: secret\n")
            .replace("  tls: true\n", "  tls: true\n  port: 443\n");
        let raw: Value = yaml_serde::from_str(&text).unwrap();

        assert!(load(&text).is_ok());
        assert_eq!(unsupported_agent_yml_keys(&raw), ["docker_socket", "slots.0.rcon", "controller.port", "demo_upload.retries"]);
        assert_eq!(unsupported_agent_yml_keys(&yaml_serde::from_str(GOOD_YAML).unwrap()).len(), 0);
    }

    #[test]
    fn requires_the_controller_block_and_knows_no_top_level_controller_keys() {
        let text = GOOD_YAML.replace("controller:\n  host: cs.example.com\n  tls: true\n", "controller_host: cs.example.com\ncontroller_tls: true\n");
        let found = issues(&text);

        assert_eq!(found.iter().map(|issue| issue.split(':').next().unwrap_or_default()).collect::<Vec<_>>(), ["controller"]);
        assert_eq!(unsupported_agent_yml_keys(&yaml_serde::from_str(&text).unwrap()), ["controller_host", "controller_tls"]);
    }

    #[test]
    fn refuses_a_key_given_twice_rather_than_guessing() {
        let twice = format!("{GOOD_YAML}cs2_image: ghcr.io/example/ankor-cs2:v2\n");
        let slot_twice = GOOD_YAML.replace(
            "    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001\n",
            "    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0001\n    gslt: AAAAAAAAAAAAAAAAAAAAAAAAAAAA0009\n",
        );

        for text in [twice, slot_twice] {
            let (_dir, path) = write(&text);
            let error = load_agent_config(&path, |_| {}).unwrap_err().to_string();

            assert!(error.starts_with(&format!("invalid {}:", path.display())), "{error}");
        }
    }

    #[test]
    fn replaces_agent_yml_whole_when_it_saves_a_change() {
        use std::os::unix::fs::MetadataExt as _;

        let (dir, path) = write(GOOD_YAML);
        let before = fs::metadata(&path).unwrap().ino();

        persist_config_patch(&path, &ConfigPatch { pinned_ip: Some(Some("203.0.113.10".to_owned())), ..ConfigPatch::default() }).unwrap();

        assert_eq!(load_agent_config(&path, |_| {}).unwrap().pinned_ip.as_deref(), Some("203.0.113.10"));
        assert_ne!(fs::metadata(&path).unwrap().ino(), before);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn controller(socket_path: &str, events_path: &str) -> ControllerFields {
        ControllerFields { host: "nodes.example.dk".to_owned(), tls: true, socket_path: socket_path.to_owned(), events_path: events_path.to_owned() }
    }

    #[test]
    fn entries_write_a_path_only_when_it_differs_from_the_default() {
        let entries = |controller: ControllerFields| agent_yml_entries(&ConfigPatch { controller: Some(controller), ..ConfigPatch::default() });

        assert_eq!(entries(controller(DEFAULT_SOCKET_PATH, DEFAULT_EVENTS_PATH)), [("controller", Some(json!({ "host": "nodes.example.dk", "tls": true })))]);
        assert_eq!(
            entries(controller("/ankor/agent", DEFAULT_EVENTS_PATH)),
            [("controller", Some(json!({ "host": "nodes.example.dk", "tls": true, "socket_path": "/ankor/agent" })))]
        );
    }

    #[test]
    fn entries_write_the_demo_header_and_limit_only_when_set() {
        let entry = |upload: DemoUpload| agent_yml_entries(&ConfigPatch { demo_upload: Some(Some(upload)), ..ConfigPatch::default() }).remove(0).1;
        let url = "https://demos.example.com/put".to_owned();

        assert_eq!(entry(DemoUpload { url: url.clone(), header: None, limit_mbit: None }), Some(json!({ "url": url })));
        assert_eq!(entry(DemoUpload { url: url.clone(), header: None, limit_mbit: Some(40) }), Some(json!({ "url": url, "limit_mbit": 40 })));
        assert_eq!(
            entry(DemoUpload { url: url.clone(), header: Some(DemoUploadHeader { name: "X-Demo-Key".to_owned(), value: "k".to_owned() }), limit_mbit: None }),
            Some(json!({ "url": url, "header": { "name": "X-Demo-Key", "value": "k" } }))
        );
        assert_eq!(
            agent_yml_entries(&ConfigPatch { demo_upload: Some(None), auto_update: Some(true), ..ConfigPatch::default() }),
            [("auto_update", None), ("demo_upload", None)]
        );
    }

    #[test]
    fn a_replace_leaves_the_new_file_and_no_temporary_one() {
        let (dir, path) = write("old\n");

        write_atomic(&path, "new\n").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A directory the agent can write and search but not open stands in for a directory sync that
    /// fails after the rename. Root would ignore the mode; the checks run as a non-root user.
    #[test]
    fn a_directory_sync_that_fails_after_the_rename_still_counts_as_written() {
        use std::os::unix::fs::PermissionsExt as _;

        let (dir, path) = write("old\n");

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o300)).unwrap();

        let written = write_atomic(&path, "new\n");

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        written.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[test]
    fn a_failed_rename_keeps_the_old_file_and_removes_the_temporary_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yml");

        // A non-empty directory where the file goes makes the rename fail.
        fs::create_dir_all(path.join("inside")).unwrap();

        assert!(write_atomic(&path, "new\n").is_err());
        assert!(path.join("inside").is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_patch_that_breaks_the_file_is_refused() {
        let (_dir, path) = write(&file());
        let patch = ConfigPatch { pinned_ip: Some(Some("not an ip".to_owned())), ..ConfigPatch::default() };

        assert!(persist_config_patch(&path, &patch).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), file());
    }
}
