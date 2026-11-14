//! Applies the controller's requests with the agent's current components and configuration. One request in,
//! one reply out; an error becomes `reply { ok: false, error }`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ankor_protocol::config::DemoUpload;
use ankor_protocol::constants::{SERVER_STATUS_TIMEOUT, official_images};
use ankor_protocol::helpers::{cs2_behind, duplicate_gslts, wire_name};
use ankor_protocol::types::{
    AgentErrorCode, ConfiguredSlot, ConsoleCommandReply, ControllerFields, ControllerToAgentMessage, Cs2UpdateMode, DeleteDemoReply, DemoUploadEdit,
    DemoUploadHeader, GetServerStatusReply, NodeUnavailable, NodeUnavailableCode, ReserveSlotReply, RestoreRoundReply, ServerState, SlotEdit, StartMatchReply,
    StartRefusal, StopServerReply, UpdateCs2Reply,
};
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::agent_reporting::reported_config;
use crate::config::{ConfigPatch, ConfigStore};
use crate::contract::backup_file_name;
use crate::contract::types::{AgentToPluginMessage, ExecCommand, PluginRestoreRound, PluginStartMatch};
use crate::controller_connection::{Defer, HandlerError, Request};
use crate::cs2_auto_update::Cs2AutoUpdate;
use crate::demo_uploader::{DemoUploader, ScanError};
use crate::failure::{AgentFailure, OpError};
use crate::gslt_keep_alive::GsltKeepAlive;
use crate::host_layout::slot_backup_dir;
use crate::install::{BegunUpdate, MaintenanceGuard};
use crate::logger::Logger;
use crate::plugin_relay::PluginRelay;
use crate::registry_tags::parse_image_reference;
use crate::self_update::{AgentUpdateFactory, Cs2ImageUpdateFactory, Schedule, update_agent, update_cs2_image};
use crate::slot_manager::{SlotError, SlotManager};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RequestHandlerError {
    #[error("unhandled request {0}")]
    Unhandled(String),
}

/// A command for a server's plugin, which it answers with its request id.
pub enum PluginCommandKind {
    Exec(String),
    RestoreRound(i64),
    StartMatch,
}

impl PluginCommandKind {
    fn message(self, request_id: String) -> AgentToPluginMessage {
        match self {
            Self::Exec(command) => AgentToPluginMessage::Exec(ExecCommand { command, request_id }),
            Self::RestoreRound(round) => AgentToPluginMessage::RestoreRound(PluginRestoreRound { round, request_id }),
            Self::StartMatch => AgentToPluginMessage::StartMatch(PluginStartMatch { request_id }),
        }
    }
}

#[async_trait]
pub trait Plugins: Send + Sync {
    async fn request_status(&self, allocation_id: &str, timeout: Duration) -> Result<ServerState, AgentFailure>;
    async fn run_command(&self, allocation_id: &str, command: PluginCommandKind, timeout: Duration) -> Result<(), AgentFailure>;
}

#[async_trait]
impl Plugins for PluginRelay {
    async fn request_status(&self, allocation_id: &str, timeout: Duration) -> Result<ServerState, AgentFailure> {
        PluginRelay::request_status(self, allocation_id, timeout).await
    }

    async fn run_command(&self, allocation_id: &str, command: PluginCommandKind, timeout: Duration) -> Result<(), AgentFailure> {
        PluginRelay::run_command(self, allocation_id, |request_id| command.message(request_id), timeout).await
    }
}

/// The newest public cs2 build, and the automatic update that follows it.
pub trait PublicBuild: Send + Sync {
    /// The newest public build, once read from Steam.
    fn latest(&self) -> Option<String>;

    /// Lets the automatic update catch up, in the background, once `set_config` turns it back on.
    fn catch_up(&self);
}

impl PublicBuild for Cs2AutoUpdate {
    fn latest(&self) -> Option<String> {
        self.latest()
    }

    fn catch_up(&self) {
        let update = self.clone();

        tokio::spawn(async move { update.consider().await });
    }
}

#[async_trait]
pub trait TokenChecks: Send + Sync {
    async fn changed(&self, slot_ids: &[String]);
}

#[async_trait]
impl TokenChecks for GsltKeepAlive {
    async fn changed(&self, slot_ids: &[String]) {
        GsltKeepAlive::changed(self, slot_ids).await;
    }
}

#[async_trait]
pub trait DemoStore: Send + Sync {
    async fn remove(&self, slot_id: &str, file_name: &str) -> Result<(), OpError>;
    /// Looks for new demos; a failure is only logged.
    fn scan_all(&self) -> BoxFuture<'static, Result<(), ScanError>>;
}

#[async_trait]
impl DemoStore for DemoUploader {
    async fn remove(&self, slot_id: &str, file_name: &str) -> Result<(), OpError> {
        DemoUploader::remove(self, slot_id, file_name).await
    }

    fn scan_all(&self) -> BoxFuture<'static, Result<(), ScanError>> {
        Box::pin(DemoUploader::scan_all(self))
    }
}

pub type BuildRead = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;
/// Tells another part to act now, with nothing to pass or wait for.
pub type Nudge = Arc<dyn Fn() + Send + Sync>;
/// Brings a reading up to date, and settles once it is.
pub type Refresh = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
pub(crate) type BeginUpdate = Arc<dyn Fn(Cs2UpdateMode) -> BoxFuture<'static, Result<BegunUpdate, OpError>> + Send + Sync>;

#[derive(Clone)]
pub struct RequestHandlerDeps {
    /// `agent.yml` and the config in memory; `set_config` and the image updates change both.
    pub config: ConfigStore,
    /// The slots that launches, stops and slot edits act on.
    pub slots: SlotManager,
    /// Carries status requests and commands to each server's plugin.
    pub plugins: Arc<dyn Plugins>,
    /// The token checks, told when `set_config` changes a slot's token.
    pub gslts: Arc<dyn TokenChecks>,
    /// The demos on the node, which `delete_demo` removes and `set_config` rescans.
    pub demos: Arc<dyn DemoStore>,
    /// The one guard for the host; boot builds it before adopting any running update.
    pub updates: MaintenanceGuard,
    /// Starts a CS2 install or update, as the automatic update does; `run` does the work.
    pub begin_cs2_update: BeginUpdate,
    /// Where the slots keep their round backups.
    pub data_dir: PathBuf,
    /// What `update_agent` runs with.
    pub agent_update: AgentUpdateFactory,
    /// What `update_cs2_image` runs with.
    pub cs2_image_update: Cs2ImageUpdateFactory,
    /// Where deferred work logs its outcome.
    pub log: Logger,
    /// The installed build, as the next hello reports it.
    pub installed_build: BuildRead,
    /// The newest public build, which a launch must be on, and the automatic update that follows it.
    pub public_build: Arc<dyn PublicBuild>,
    /// Lets the automatic image update catch up once `set_config` turns it back on.
    pub poll_images: Nudge,
    /// Why the node cannot run games, as the next hello reports it.
    pub unavailable: Arc<dyn Fn() -> Option<NodeUnavailable> + Send + Sync>,
    /// Asks the disk for its free space, so a launch's admission reads a fresh figure.
    pub check_disk: Refresh,
    /// Switches the socket to the controller address in the config; runs once the reply has left.
    pub redial: Nudge,
}

fn failure(code: AgentErrorCode, detail: impl Into<String>) -> HandlerError {
    OpError::Refused(AgentFailure::new(code, detail))
}

/// Refuses an image outside `official`, its kind's repository, while the node's operator has the node
/// run only official images. A controller cannot turn that off, so it cannot run an image of its own.
fn allowed_image(config: &ConfigStore, image: &str, official: &str) -> Result<(), HandlerError> {
    if !config.read().official_images_only || parse_image_reference(image).name == official {
        return Ok(());
    }

    Err(failure(AgentErrorCode::ImageNotAllowed, format!("{image} is not from {official}, and this node runs only official images")))
}

fn reply(value: &impl serde::Serialize) -> Result<Value, HandlerError> {
    Ok(serde_json::to_value(value)?)
}

/// Fills in kept tokens; the result has every token once.
///
/// # Errors
///
/// Refuses a new slot without a token, and tokens that repeat another slot's.
pub(crate) fn resolve_slot_tokens(edits: &[SlotEdit], current: &[ConfiguredSlot]) -> Result<Vec<ConfiguredSlot>, AgentFailure> {
    let mut slots = Vec::with_capacity(edits.len());

    for edit in edits {
        let gslt = edit.gslt.clone().or_else(|| current.iter().find(|slot| slot.id == edit.id).map(|slot| slot.gslt.clone()));
        let Some(gslt) = gslt else {
            return Err(AgentFailure::new(AgentErrorCode::SlotTokenRequired, format!("slot {} is new and needs a token", edit.id)));
        };

        slots.push(ConfiguredSlot { id: edit.id.clone(), port: edit.port, tv_port: edit.tv_port, gslt });
    }

    let tokens: Vec<Option<&str>> = slots.iter().map(|slot| Some(slot.gslt.as_str())).collect();
    let duplicates = duplicate_gslts(&tokens);

    if !duplicates.is_empty() {
        let ids: Vec<&str> = duplicates.iter().filter_map(|index| slots.get(*index)).map(|slot| slot.id.as_str()).collect();

        return Err(AgentFailure::new(AgentErrorCode::InvalidRequest, format!("each slot needs its own token: {} repeat another slot's", ids.join(", "))));
    }

    Ok(slots)
}

/// Fills in a kept header value from the node's current header of that name.
///
/// # Errors
///
/// Refuses a header that is new without its value.
pub(crate) fn resolve_demo_upload(edit: Option<&DemoUploadEdit>, current: Option<&DemoUpload>) -> Result<Option<DemoUpload>, AgentFailure> {
    let Some(edit) = edit else {
        return Ok(None);
    };
    let header = match &edit.header {
        None => None,
        Some(header) => match &header.value {
            Some(value) => Some(DemoUploadHeader { name: header.name.clone(), value: value.clone() }),
            None => match current.and_then(|current| current.header.clone()).filter(|kept| kept.name == header.name) {
                Some(kept) => Some(kept),
                None => {
                    return Err(AgentFailure::new(
                        AgentErrorCode::DemoHeaderValueRequired,
                        format!("demo upload header {} is new and needs its value", header.name),
                    ));
                }
            },
        },
    };

    Ok(Some(DemoUpload { url: edit.url.clone(), header, limit_mbit: edit.limit_mbit }))
}

fn refused(code: StartRefusal, detail: Option<String>) -> SlotError {
    SlotError::Refused(AgentFailure::start_refused(code, detail))
}

fn node_refusal(updates: &MaintenanceGuard, unavailable: &(dyn Fn() -> Option<NodeUnavailable> + Send + Sync)) -> Option<SlotError> {
    if updates.running() {
        return Some(refused(StartRefusal::MaintenanceRunning, None));
    }

    unavailable().map(|unavailable| refused(StartRefusal::NodeUnavailable, Some(format!("{}: {}", wire_name(unavailable.code), unavailable.detail))))
}

fn schedule_on(defer: &Defer) -> Schedule {
    let defer = defer.clone();

    Arc::new(move |work| defer.push(work))
}

/// Runs one request and returns its reply's `data`.
///
/// # Errors
///
/// Fails as the request fails; an `AgentFailure` keeps its code in the reply.
pub async fn handle(deps: &RequestHandlerDeps, request: Request, defer: Defer) -> Result<Value, HandlerError> {
    match request.message {
        ControllerToAgentMessage::StartServer(start) => start_server(deps, start).await,
        ControllerToAgentMessage::StopServer(stop) => {
            deps.slots.stop_allocation(&stop.allocation_id).await?;

            reply(&StopServerReply { allocation_id: stop.allocation_id })
        }
        ControllerToAgentMessage::GetServerStatus(get) => {
            if !deps.slots.holds(&get.allocation_id) {
                return Err(OpError::Refused(AgentFailure::code(AgentErrorCode::UnknownAllocation)));
            }

            let ServerState { map_name, r#match, players, config_error, ready, server_steam_id } =
                deps.plugins.request_status(&get.allocation_id, SERVER_STATUS_TIMEOUT).await?;

            reply(&GetServerStatusReply { map_name, r#match, players, config_error, ready, server_steam_id, allocation_id: get.allocation_id })
        }
        ControllerToAgentMessage::ConsoleCommand(command) => {
            if !deps.slots.holds(&command.allocation_id) {
                return Err(OpError::Refused(AgentFailure::code(AgentErrorCode::UnknownAllocation)));
            }

            deps.plugins.run_command(&command.allocation_id, PluginCommandKind::Exec(command.command), SERVER_STATUS_TIMEOUT).await?;

            reply(&ConsoleCommandReply { allocation_id: command.allocation_id })
        }
        ControllerToAgentMessage::RestoreRound(restore) => {
            let Some(slot_id) = deps.slots.slot_of(&restore.allocation_id) else {
                return Err(OpError::Refused(AgentFailure::code(AgentErrorCode::UnknownAllocation)));
            };
            // The engine wrote the file into the slot's shadow as the round started; the plugin reads the same file.
            // The contract caps `round` at 1000, so it always fits.
            let file = backup_file_name(u32::try_from(restore.round - 1).unwrap_or(0));

            if tokio::fs::metadata(slot_backup_dir(&deps.data_dir, &slot_id).join(&file)).await.is_err() {
                return Err(failure(AgentErrorCode::NoBackup, format!("no backup of round {}: {file} is not on the node", restore.round)));
            }

            deps.plugins.run_command(&restore.allocation_id, PluginCommandKind::RestoreRound(restore.round), SERVER_STATUS_TIMEOUT).await?;

            reply(&RestoreRoundReply { allocation_id: restore.allocation_id, round: restore.round })
        }
        ControllerToAgentMessage::StartMatch(start) => {
            if deps.slots.slot_of(&start.allocation_id).is_none() {
                return Err(OpError::Refused(AgentFailure::code(AgentErrorCode::UnknownAllocation)));
            }

            deps.plugins.run_command(&start.allocation_id, PluginCommandKind::StartMatch, SERVER_STATUS_TIMEOUT).await?;

            reply(&StartMatchReply { allocation_id: start.allocation_id })
        }
        ControllerToAgentMessage::UpdateCs2(update) => {
            let BegunUpdate { update_id, run } = (deps.begin_cs2_update)(update.mode).await?;
            let log = deps.log.clone();
            let installed_build = Arc::clone(&deps.installed_build);
            let mode = wire_name(update.mode);

            // The update runs after the reply; cs2.update reports how it ends.
            defer.push(async move {
                match run.await {
                    Ok(()) => log.info(format!("cs2 {mode}: installed build {}", installed_build().await.unwrap_or_else(|| "unknown".to_owned()))),
                    Err(error) => log.warn(format!("cs2 {mode} failed: {error}")),
                }
            });

            reply(&UpdateCs2Reply { update_id })
        }
        ControllerToAgentMessage::ReserveSlot(reserve) => {
            deps.slots.reserve(&reserve.slot_id)?;

            reply(&ReserveSlotReply { slot_id: reserve.slot_id })
        }
        ControllerToAgentMessage::DeleteDemo(delete) => {
            deps.demos.remove(&delete.slot_id, &delete.file_name).await?;

            reply(&DeleteDemoReply { slot_id: delete.slot_id, file_name: delete.file_name })
        }
        // Both reply immediately with what follows; the work runs after the reply, and node_status reports how it ended.
        ControllerToAgentMessage::UpdateAgent(update) => {
            allowed_image(&deps.config, &update.image, official_images::AGENT)?;
            let update_deps = (deps.agent_update)(schedule_on(&defer));

            reply(&update_agent(&update.image, update.pull, &update_deps).await?)
        }
        ControllerToAgentMessage::UpdateCs2Image(update) => {
            allowed_image(&deps.config, &update.image, official_images::CS2)?;
            let update_deps = (deps.cs2_image_update)(schedule_on(&defer));

            reply(&update_cs2_image(&update.image, update.pull, &update_deps).await?)
        }
        ControllerToAgentMessage::SetConfig(set) => set_config(deps, set.config, &defer),
        other => Err(OpError::failed(RequestHandlerError::Unhandled(serde_json::to_value(&other).map(|value| value.to_string()).unwrap_or_default()))),
    }
}

async fn start_server(deps: &RequestHandlerDeps, start: ankor_protocol::types::ControllerStartServer) -> Result<Value, HandlerError> {
    // Asked again in admit; this first look refuses before the launch queues a spool call, which a
    // stuck spool would never answer. The disk is left out: its figure here may be up to a check
    // old, and admit reads it again after asking the disk.
    let without_disk = || (deps.unavailable)().filter(|unavailable| unavailable.code != NodeUnavailableCode::DiskSpace);

    if let Some(refusal) = node_refusal(&deps.updates, &without_disk) {
        return Err(refusal.into());
    }

    let latest = deps.public_build.latest();
    // Read in admit, not now: a set_config during the build read counts.
    let config = deps.config.clone();
    let updates = deps.updates.clone();
    let unavailable = Arc::clone(&deps.unavailable);
    // The disk's answer is read in admit, from a check just before; asking under the slot lock would
    // hold every slot on a hung disk.
    let installed = {
        let (check, read) = ((deps.check_disk)(), (deps.installed_build)());

        async move {
            check.await;
            read.await
        }
    };
    // The build read is part of the launch: a stop that comes during it waits for the slot pick.
    let admit = move |installed: Option<String>| {
        // Runs with the slot pick: an update that took the guard during the read is seen, and one after it sees the slot busy.
        if let Some(refusal) = node_refusal(&updates, unavailable.as_ref()) {
            return Err(refusal);
        }

        // A server on an empty game volume would only exit at once.
        let Some(installed) = installed else {
            return Err(refused(StartRefusal::Cs2NotInstalled, Some("CS2 is not installed on this node; update_cs2 installs it".to_owned())));
        };

        // Players on the new client cannot join an old build, and every launch keeps the node from going idle to update.
        let auto_update = config.read().auto_update;

        if auto_update && cs2_behind(Some(&installed), latest.as_deref()) {
            return Err(refused(
                StartRefusal::Cs2Outdated,
                Some(format!("cs2 build {installed} is behind the public {}; launches resume once the node has updated", latest.unwrap_or_default())),
            ));
        }

        Ok(())
    };

    let started = deps.slots.start_with(start.launch, &start.allocation_id, installed, admit).await?;

    reply(&started)
}

fn set_config(deps: &RequestHandlerDeps, edit: ankor_protocol::types::NodeSettingsEdit, defer: &Defer) -> Result<Value, HandlerError> {
    // One change of the config at a time: no other one reads the config this edit starts from.
    deps.config.change(|config, file| {
        let pinned_ip = edit.pinned_ip.unwrap_or_else(|| config.pinned_ip.clone());
        let controller = edit.controller.map_or_else(
            || config.controller.clone(),
            |controller| ControllerFields {
                host: controller.host,
                tls: controller.tls,
                socket_path: controller.socket_path,
                events_path: controller.events_path,
            },
        );
        let demo_upload = match edit.demo_upload {
            None => config.demo_upload.clone(),
            Some(upload) => resolve_demo_upload(upload.as_ref(), config.demo_upload.as_ref())?,
        };
        let auto_update = edit.auto_update.unwrap_or(config.auto_update);
        // Read at each launch, so a new cap applies to the servers launched after it.
        let memory_mb = edit.memory_mb.unwrap_or(config.memory_mb);
        let auto_update_resumed = auto_update && !config.auto_update;
        let controller_changed = controller != config.controller;
        let wanted = match &edit.slots {
            None => config.slots.clone(),
            Some(slots) => resolve_slot_tokens(slots, &config.slots)?,
        };

        let unchanged = (&wanted, &pinned_ip, &controller, auto_update, &demo_upload, memory_mb)
            == (&config.slots, &config.pinned_ip, &config.controller, config.auto_update, &config.demo_upload, config.memory_mb);
        let patch = ConfigPatch {
            slots: Some(wanted.clone()),
            pinned_ip: Some(pinned_ip),
            controller: Some(controller),
            auto_update: Some(auto_update),
            // The uploader starts over on its own when the target changed.
            demo_upload: Some(demo_upload),
            memory_mb: Some(memory_mb),
            ..ConfigPatch::default()
        };
        let mut applied = None;

        // Checked, written and applied in one hold of the slot lock: no launch takes a slot in between.
        let changed = deps.slots.change_slots(
            &wanted,
            crate::failure::OpError::from,
            |busy| {
                if controller_changed && busy {
                    Err(failure(AgentErrorCode::SlotsHeld, "Pause the node and wait for its games to exit before changing its controller address."))
                } else {
                    Ok(())
                }
            },
            || {
                if unchanged {
                    return Ok(false);
                }

                applied = Some(file.write(&patch)?);
                Ok(true)
            },
        )?;
        let (Some(new_tokens), Some(applied)) = (changed, applied) else {
            return reply(&reported_config(config));
        };

        // The reply names the layout first, so the controller knows each slot before its status arrives.
        let slots = deps.slots.clone();
        let gslts = Arc::clone(&deps.gslts);
        let demos = Arc::clone(&deps.demos);
        let log = deps.log.clone();

        defer.push(async move {
            slots.announce(&new_tokens);
            tokio::spawn(async move { gslts.changed(&new_tokens).await });

            let scan = demos.scan_all();

            tokio::spawn(async move {
                if let Err(error) = scan.await {
                    log.warn(format!("demo scan failed: {error}"));
                }
            });
        });

        // A node behind the public build or an image release would otherwise wait for the next poll.
        if auto_update_resumed {
            let (cs2, poll) = (Arc::clone(&deps.public_build), Arc::clone(&deps.poll_images));

            defer.push(async move {
                cs2.catch_up();
                poll();
            });
        }

        // A new address gets a new socket, which says hello there.
        if controller_changed {
            let redial = Arc::clone(&deps.redial);

            defer.push(async move { redial() });
        }

        reply(&reported_config(&applied))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test fails by crashing")]
mod adapter_tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::demo_uploader::DemoUploaderOptions;
    use crate::gslt_keep_alive::{GsltSlotsPort, Release, SlotToken};
    use crate::refused_gslts::RefusedGslts;

    struct OneSlot;

    impl GsltSlotsPort for OneSlot {
        fn tokens(&self) -> Vec<SlotToken> {
            vec![SlotToken { slot_id: "a".to_owned(), gslt: "TOKEN".to_owned(), gslt_state: ankor_protocol::types::GsltState::Unchecked }]
        }

        fn running_tokens(&self) -> std::collections::HashSet<String> {
            std::collections::HashSet::new()
        }

        fn set_gslt_state(&self, _slot_id: &str, _gslt: &str, _state: ankor_protocol::types::GsltState) {}

        fn hold_for_logon(&self, _slot_id: &str, _gslt: &str) -> Option<Release> {
            Some(Release::new(|_| {}))
        }
    }

    #[tokio::test]
    async fn the_token_checks_log_a_changed_slots_token_on() {
        let calls = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&calls);
        let keep_alive = GsltKeepAlive::new(
            Arc::new(OneSlot),
            Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(crate::steam_logon::eresult::OK) })
            }),
            RefusedGslts::new(None, Logger::null()),
            Logger::null(),
            tokio_util::sync::CancellationToken::new(),
        );

        TokenChecks::changed(&keep_alive, &["a".to_owned()]).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_demo_store_deletes_the_named_recording() {
        let data = tempfile::tempdir().unwrap();
        let demo = data.path().join("slots/a/game/csgo/ankor/demos/11111111-1111-4111-8111-111111111111/a.dem");

        std::fs::create_dir_all(demo.parent().unwrap()).unwrap();
        std::fs::write(&demo, "gotv").unwrap();

        let uploader = DemoUploader::new(DemoUploaderOptions::new(data.path().to_owned(), Arc::new(|| None), Arc::new(|_| None), Arc::new(|| false)));

        DemoStore::remove(&uploader, "a", "a.dem").await.unwrap();
        assert!(!demo.exists());
    }
}
