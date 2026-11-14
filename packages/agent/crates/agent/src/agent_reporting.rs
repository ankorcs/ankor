//! The agent's state and update output as the controller receives it: the `hello`, the reporters that send
//! only what the controller does not hold yet, and the feeds that carry slot, plugin and update events on.

use crate::sync::lock;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use ankor_protocol::config::AgentConfig;
use ankor_protocol::constants::{CS2_UPDATE_MESSAGE_MAX_CHARS, MAX_REPORTED_DEMOS, MAX_SLOTS};
use ankor_protocol::helpers::{demo_key, gslt_hint};
use ankor_protocol::types::{
    AgentBackup, AgentCs2UpdateProgress, AgentDemo, AgentDemoRemoved, AgentEventBacklog, AgentHello, AgentInfo, AgentNodeStatus, AgentPhaseChange, AgentScore,
    AgentServerExited, AgentSlotStatus, AgentToControllerMessage, AgentUpdate, ControllerAddress, Cs2State, DemoFile, DemoRemovedReason, EventBacklog,
    HostInfo, NodeSettings, NodeUnavailable, NodeUnavailableCode, ReportedDemoUpload, ReportedDemoUploadHeader, ReportedSlot, ServerExit, SlotStatus,
};
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use indexmap::{IndexMap, IndexSet};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent_update::AgentUpdateState;
use crate::config::ConfigReader;
use crate::contract::types::{PluginToAgentMessage, ServerStatus};
use crate::controller_connection::ControllerConnection;
use crate::demo_uploader::DemoUploader;
use crate::docker::LineHook;
use crate::docker_api::DockerError;
use crate::event_uploader::{EventUploader, nothing_stuck};
use crate::failure::OpError;
use crate::install::{MaintenanceHolder, ProgressHook};
use crate::logger::Logger;
use crate::node_log::NodeLog;
use crate::plugin_relay::RelayHooks;
use crate::public_ip::PublicAddress;
use crate::resources::{OS_RELEASE_FILE, ResourceReporter, read_os_name};
use crate::slot_manager::{SlotEvent, SlotManager};
use crate::spool_client::SpoolPort;

/// The demo upload header's value is a credential the controller set, so the hello carries only its hash.
#[must_use]
pub fn reported_config(config: &AgentConfig) -> NodeSettings {
    NodeSettings {
        slots: config
            .slots
            .iter()
            .map(|slot| ReportedSlot { id: slot.id.clone(), port: slot.port, tv_port: slot.tv_port, gslt_hint: gslt_hint(&slot.gslt) })
            .collect(),
        pinned_ip: config.pinned_ip.clone(),
        controller: ControllerAddress {
            host: config.controller.host.clone(),
            tls: config.controller.tls,
            socket_path: config.controller.socket_path.clone(),
            events_path: config.controller.events_path.clone(),
        },
        auto_update: config.auto_update,
        demo_upload: config.demo_upload.as_ref().map(|upload| ReportedDemoUpload {
            url: upload.url.clone(),
            header: upload
                .header
                .as_ref()
                .map(|header| ReportedDemoUploadHeader { name: header.name.clone(), value_sha256: hex::encode(Sha256::digest(header.value.as_bytes())) }),
            limit_mbit: upload.limit_mbit,
        }),
        memory_mb: config.memory_mb,
    }
}

/// Sends a frame and resolves with whether it left the socket. It is called at once, so the frame
/// keeps its place among the frames around it.
pub type SendFrame<T> = Arc<dyn Fn(T) -> BoxFuture<'static, bool> + Send + Sync>;

struct Reports<T> {
    /// What the controller holds, as far as the agent knows, with the report that set it.
    known: HashMap<String, (T, u64)>,
    /// What the controller has, by frames that left the socket or the hello.
    confirmed: HashMap<String, T>,
    /// Counts hellos, so a send result from before the latest hello is ignored.
    hellos: u64,
    /// Counts reports, so a send result can tell whether a later report replaced its frame.
    sequence: u64,
}

/// The tag of an entry no report set: one a hello carried, or one put back after a send failed. No
/// report has it, so no send result matches it.
const NO_REPORT: u64 = 0;

/// Sends a frame only when it differs from what the controller holds under its key. A hello sets that; a
/// frame that never left the socket puts it back, so one held back before a welcome goes out on the catch-up.
pub struct ChangeReporter<T> {
    key: Arc<dyn Fn(&T) -> String + Send + Sync>,
    send: SendFrame<T>,
    state: Arc<Mutex<Reports<T>>>,
}

impl<T> Clone for ChangeReporter<T> {
    fn clone(&self) -> Self {
        Self { key: Arc::clone(&self.key), send: Arc::clone(&self.send), state: Arc::clone(&self.state) }
    }
}

impl<T: Clone + PartialEq + Send + 'static> ChangeReporter<T> {
    #[must_use]
    pub fn new(key: impl Fn(&T) -> String + Send + Sync + 'static, send: SendFrame<T>) -> Self {
        Self { key: Arc::new(key), send, state: Arc::new(Mutex::new(Reports { known: HashMap::new(), confirmed: HashMap::new(), hellos: 0, sequence: 0 })) }
    }

    /// What the hello about to go out carries.
    pub fn helloed(&self, frames: &[T]) {
        let mut state = lock(&self.state);

        state.hellos += 1;
        state.known = frames.iter().map(|frame| ((self.key)(frame), (frame.clone(), NO_REPORT))).collect();
        state.confirmed = frames.iter().map(|frame| ((self.key)(frame), frame.clone())).collect();
    }

    /// Written at once, so it keeps its place among the frames around it. The frame is offered in
    /// the same hold that records it, and a send that has already settled, such as one refused
    /// before the welcome, is settled in that hold too. The welcome's catch-up then never sees a
    /// refused frame as held, whichever thread it runs on.
    pub fn report(&self, frame: T) {
        let key = (self.key)(&frame);
        let mut state = lock(&self.state);

        if state.known.get(&key).is_some_and(|(known, _)| *known == frame) {
            return;
        }

        state.sequence += 1;

        let report = state.sequence;

        state.known.insert(key.clone(), (frame.clone(), report));

        let mut sent = (self.send)(frame.clone());

        if let Poll::Ready(sent) = sent.poll_unpin(&mut Context::from_waker(Waker::noop())) {
            settle(&mut state, key, frame, report, sent);
            return;
        }

        let hellos = state.hellos;
        let shared = Arc::clone(&self.state);

        drop(state);
        tokio::spawn(async move {
            let sent = sent.await;
            let mut state = lock(&shared);

            // The hello since then carries what the controller holds.
            if hellos == state.hellos {
                settle(&mut state, key, frame, report, sent);
            }
        });
    }

    /// Drops the last frame sent under this key. A slot added again under a removed slot's id must
    /// still be reported.
    pub fn forget(&self, key: &str) {
        let mut state = lock(&self.state);

        state.known.remove(key);
        state.confirmed.remove(key);
    }
}

/// Records how a report's send ended: a frame that left the socket is what the controller has, and
/// one that never did puts back what it has, unless a later report replaced the frame meanwhile.
fn settle<T: Clone>(state: &mut Reports<T>, key: String, frame: T, report: u64, sent: bool) {
    if sent {
        state.confirmed.insert(key, frame);
    } else if state.known.get(&key).is_some_and(|(_, at)| *at == report) {
        match state.confirmed.get(&key).cloned() {
            None => {
                state.known.remove(&key);
            }
            Some(confirmed) => {
                state.known.insert(key, (confirmed, NO_REPORT));
            }
        }
    }
}

/// One reason at a time: maintenance ends on its own, so it goes first while it lasts.
#[must_use]
pub fn node_unavailable(maintenance: Option<MaintenanceHolder>, spool: Option<NodeUnavailable>) -> Option<NodeUnavailable> {
    match maintenance {
        None => spool,
        Some(holder) => Some(NodeUnavailable { code: NodeUnavailableCode::Maintenance, operation: Some(holder.operation), detail: holder.detail }),
    }
}

pub struct HelloInput {
    pub agent: AgentInfo,
    pub host: HostInfo,
    pub public_ip: Option<String>,
    pub country: Option<String>,
    pub config: AgentConfig,
    pub slot_status: Vec<SlotStatus>,
    pub cs2: Cs2State,
    pub unavailable: Option<NodeUnavailable>,
    pub demos: Vec<DemoFile>,
    pub server_exits: Vec<ServerExit>,
    pub event_backlog: EventBacklog,
    pub agent_update: Option<AgentUpdate>,
}

#[must_use]
pub fn build_hello(input: HelloInput) -> AgentToControllerMessage {
    AgentToControllerMessage::Hello(AgentHello {
        agent: input.agent,
        host: input.host,
        public_ip: input.public_ip,
        country: input.country,
        config: reported_config(&input.config),
        slot_status: input.slot_status,
        cs2: input.cs2,
        unavailable: input.unavailable,
        demos: input.demos,
        server_exits: input.server_exits,
        event_backlog: input.event_backlog,
        agent_update: input.agent_update,
    })
}

/// Puts a frame on the socket without waiting for it.
pub(crate) type Push = Arc<dyn Fn(AgentToControllerMessage) + Send + Sync>;

pub struct UpdateFeed {
    pub on_progress: ProgressHook,
    pub on_log: LineHook,
}

/// Serves an update whether this agent started it or inherited it.
#[must_use]
pub fn update_feed(push: Push, update_line: LineHook) -> UpdateFeed {
    UpdateFeed {
        on_progress: Arc::new(move |progress: AgentCs2UpdateProgress| {
            push(AgentToControllerMessage::Cs2UpdateProgress(AgentCs2UpdateProgress {
                message: crate::failure::truncate(&progress.message, CS2_UPDATE_MESSAGE_MAX_CHARS),
                ..progress
            }));
        }),
        on_log: update_line,
    }
}

/// Implemented by a [`ChangeReporter`] keyed by slot id.
pub trait StatusReports: Send + Sync {
    fn report(&self, frame: AgentSlotStatus);
    fn forget(&self, key: &str);
}

impl StatusReports for ChangeReporter<AgentSlotStatus> {
    fn report(&self, frame: AgentSlotStatus) {
        Self::report(self, frame);
    }

    fn forget(&self, key: &str) {
        Self::forget(self, key);
    }
}

#[must_use]
pub fn slot_status(status: SlotStatus) -> AgentSlotStatus {
    AgentSlotStatus {
        slot_id: status.slot_id,
        state: status.state,
        allocation_id: status.allocation_id,
        match_id: status.match_id,
        plugin_online: status.plugin_online,
        gslt_state: status.gslt_state,
        reserved: status.reserved,
        server_steam_id: status.server_steam_id,
    }
}

fn server_exited(exit: ServerExit) -> AgentServerExited {
    AgentServerExited { slot_id: exit.slot_id, allocation_id: exit.allocation_id, reason: exit.reason, exit_code: exit.exit_code, detail: exit.detail }
}

fn kept_exit(exit: &AgentServerExited) -> ServerExit {
    ServerExit {
        slot_id: exit.slot_id.clone(),
        allocation_id: exit.allocation_id.clone(),
        reason: exit.reason,
        exit_code: exit.exit_code,
        detail: exit.detail.clone(),
    }
}

#[derive(Clone)]
pub struct SlotFeed {
    statuses: Arc<dyn StatusReports>,
    exits: ExitReporter,
    node_log: NodeLog,
}

impl SlotFeed {
    #[must_use]
    pub fn new(statuses: Arc<dyn StatusReports>, exits: ExitReporter, node_log: NodeLog) -> Self {
        Self { statuses, exits, node_log }
    }

    /// Other events are left to the caller.
    pub fn handle(&self, event: &SlotEvent) {
        match event {
            SlotEvent::ServerExited(exit) => {
                self.node_log.game_ended(&exit.allocation_id);
                self.exits.exited(server_exited(exit.clone()));
            }
            SlotEvent::SlotState(status) => self.statuses.report(slot_status(status.clone())),
            SlotEvent::SlotRemoved(slot_id) => self.statuses.forget(slot_id),
            SlotEvent::ContainerLog { allocation_id, line } => self.node_log.game_line(allocation_id, line),
            SlotEvent::SlotIdle { .. } | SlotEvent::TokenUsed(_) => {}
        }
    }
}

#[derive(Default)]
struct Exits {
    /// Each exit the controller may not have yet, in the order it ended.
    pending: IndexMap<u64, AgentServerExited>,
    /// The exits the hello about to go out carries.
    carried: Vec<u64>,
    /// The id given to the last added exit.
    next: u64,
}

impl Exits {
    fn add(&mut self, exit: AgentServerExited) -> u64 {
        self.next += 1;
        self.pending.insert(self.next, exit);
        self.next
    }
}

/// Keeps each `server_exited` until the controller has it: a slot's status shows it idle, but not which game
/// ended or why. One held back before a welcome goes in the next hello, which the controller settles first.
#[derive(Clone)]
pub struct ExitReporter {
    send: SendFrame<AgentServerExited>,
    store: Option<Arc<dyn SpoolPort>>,
    log: Logger,
    state: Arc<Mutex<Exits>>,
}

impl ExitReporter {
    /// `store` keeps each exit until the controller has it, so an agent restart in between does not lose it.
    #[must_use]
    pub fn new(send: SendFrame<AgentServerExited>, store: Option<Arc<dyn SpoolPort>>, log: Logger) -> Self {
        Self { send, store, log, state: Arc::default() }
    }

    /// The exits a previous run kept, read before the first hello.
    pub fn restore(&self, kept: &[ServerExit]) {
        let mut state = lock(&self.state);

        for exit in kept {
            state.add(server_exited(exit.clone()));
        }
    }

    /// Sent at once, ahead of whatever the caller sends next.
    pub fn exited(&self, exit: AgentServerExited) {
        let id = lock(&self.state).add(exit.clone());

        if let Some(store) = &self.store {
            let (store, log, kept) = (Arc::clone(store), self.log.clone(), kept_exit(&exit));
            let allocation_id = kept.allocation_id.clone();
            let recording = crate::spool_client::queue_now(async move { store.record_exit(kept).await });

            tokio::spawn(async move {
                if let Err(error) = recording.await {
                    log.warn(format!("could not keep the exit of {allocation_id}: {error}"));
                }
            });
        }

        self.send_one(id, exit);
    }

    /// What the hello about to go out carries; the rest wait for the welcome.
    #[must_use]
    pub fn helloed(&self) -> Vec<ServerExit> {
        let mut state = lock(&self.state);
        let carried: Vec<(u64, ServerExit)> =
            state.pending.iter().take(usize::try_from(MAX_SLOTS).unwrap_or(usize::MAX)).map(|(id, exit)| (*id, kept_exit(exit))).collect();

        state.carried = carried.iter().map(|(id, _)| *id).collect();
        carried.into_iter().map(|(_, exit)| exit).collect()
    }

    /// The controller has the welcomed hello's exits; the others go out now, ahead of the slot statuses.
    pub fn welcomed(&self) {
        let (delivered, rest) = {
            let mut state = lock(&self.state);
            let carried = std::mem::take(&mut state.carried);
            let delivered: Vec<AgentServerExited> = carried.iter().filter_map(|id| state.pending.shift_remove(id)).collect();
            let rest: Vec<(u64, AgentServerExited)> = state.pending.iter().map(|(id, exit)| (*id, exit.clone())).collect();

            (delivered, rest)
        };

        self.delivered(&delivered);

        for (id, exit) in rest {
            self.send_one(id, exit);
        }
    }

    fn send_one(&self, id: u64, exit: AgentServerExited) {
        let sent = (self.send)(exit);
        let this = self.clone();

        tokio::spawn(async move {
            if sent.await {
                let removed = lock(&this.state).pending.shift_remove(&id);

                if let Some(exit) = removed {
                    this.delivered(&[exit]);
                }
            }
        });
    }

    fn delivered(&self, exits: &[AgentServerExited]) {
        let Some(store) = &self.store else {
            return;
        };

        if exits.is_empty() {
            return;
        }

        let (store, log, ids) = (Arc::clone(store), self.log.clone(), exits.iter().map(|exit| exit.allocation_id.clone()).collect());
        let forgetting = crate::spool_client::queue_now(async move { store.forget_exits(ids).await });

        tokio::spawn(async move {
            if let Err(error) = forgetting.await {
                log.warn(format!("could not drop delivered exits: {error}"));
            }
        });
    }
}

pub trait PluginSlots: Send + Sync {
    fn plugin_online_changed(&self, allocation_id: &str, online: bool);
    fn plugin_status(&self, allocation_id: &str, status: &ServerStatus);
    fn plugin_quitting(&self, allocation_id: &str);
}

impl PluginSlots for SlotManager {
    fn plugin_online_changed(&self, allocation_id: &str, online: bool) {
        Self::plugin_online_changed(self, allocation_id, online);
    }

    fn plugin_status(&self, allocation_id: &str, status: &ServerStatus) {
        Self::plugin_status(self, allocation_id, status);
    }

    fn plugin_quitting(&self, allocation_id: &str) {
        Self::plugin_quitting(self, allocation_id);
    }
}

#[must_use]
pub fn plugin_feed(slots: Arc<dyn PluginSlots>, send: Push) -> RelayHooks {
    let online = Arc::clone(&slots);

    RelayHooks {
        on_plugin_online: Arc::new(move |allocation_id, connected| online.plugin_online_changed(allocation_id, connected)),
        on_plugin_message: Arc::new(move |allocation_id, message| match message {
            PluginToAgentMessage::ServerStatus(status) => slots.plugin_status(allocation_id, &status),
            PluginToAgentMessage::Quitting(_) => slots.plugin_quitting(allocation_id),
            // Sent once: the controller reads the current phase and score with get_server_status after a gap.
            PluginToAgentMessage::PhaseChange(change) => {
                send(AgentToControllerMessage::PhaseChange(AgentPhaseChange { allocation_id: allocation_id.to_owned(), phase: change.phase }));
            }
            PluginToAgentMessage::Score(score) => send(AgentToControllerMessage::Score(AgentScore {
                allocation_id: allocation_id.to_owned(),
                team1_score: score.team1_score,
                team2_score: score.team2_score,
            })),
            PluginToAgentMessage::Backup(backup) => {
                send(AgentToControllerMessage::Backup(AgentBackup { allocation_id: allocation_id.to_owned(), backup: backup.backup }));
            }
            // The relay settles the request that waits for a command result, and events go to the spool.
            PluginToAgentMessage::CommandResult(_) | PluginToAgentMessage::Event(_) => {}
        }),
    }
}

/// The newest `MAX_REPORTED_DEMOS` by `modified_at`: what a `hello` lists.
#[must_use]
pub fn newest_demos(demos: &[DemoFile]) -> Vec<DemoFile> {
    let mut newest = demos.to_vec();

    newest.sort_by_key(|demo| std::cmp::Reverse(demo.modified_at));
    newest.truncate(usize::try_from(MAX_REPORTED_DEMOS).unwrap_or(usize::MAX));
    newest
}

fn key_of(demo: &DemoFile) -> String {
    demo_key(&demo.slot_id, &demo.file_name)
}

/// How long the hello waits for the demo listing. Well inside the controller's hello deadline, so a hung
/// disk leaves the demos out of the hello instead of holding it back.
const HELLO_LISTING_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a node report whose read failed waits before it reads again.
const NODE_REPORT_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

pub type ListDemos = Arc<dyn Fn() -> BoxFuture<'static, io::Result<Vec<DemoFile>>> + Send + Sync>;

/// How many demos the target took are kept for their removal's reason, the oldest going first. The
/// controller hears of an uploaded demo's removal on the next report. Entries for demos it never
/// heard of, or whose removal a hello covered, are never consumed and leave by this limit, unless the
/// uploader says it could not remove the demo.
const UPLOADED_KEPT: usize = 1024;

#[derive(Default)]
struct KnownDemos {
    /// The demos the controller holds, set by the hello and advanced by each frame that left the socket.
    known: IndexMap<String, DemoFile>,
    /// The demos the target took, oldest first, until the removal of each leaves the socket, the
    /// uploader says it could not remove one, or [`UPLOADED_KEPT`] pushes one out. A removal goes out
    /// as `uploaded` for one of them and as `deleted` for any other.
    uploaded: IndexSet<String>,
}

/// Reports each demo that differs from what the controller holds. A hello sets that; a frame advances it only
/// once it has left the socket, so one held back before a welcome goes out on the next report. The controller
/// hears of a new demo while it is among the newest, keeps hearing of one it holds, and is told one
/// is removed, and why, only once it has left the node.
#[derive(Clone)]
pub struct DemoReporter {
    /// Sends a frame and resolves with whether it left the socket.
    send: SendFrame<AgentToControllerMessage>,
    /// The demos the controller holds and the ones the target took.
    state: Arc<Mutex<KnownDemos>>,
    /// Reports run one after another.
    queue: Arc<tokio::sync::Mutex<()>>,
}

fn index_by_key<K: Hash + Eq, V>(entries: impl IntoIterator<Item = (K, V)>) -> IndexMap<K, V> {
    let mut map = IndexMap::new();

    for (key, value) in entries {
        map.insert(key, value);
    }

    map
}

impl DemoReporter {
    #[must_use]
    pub fn new(send: SendFrame<AgentToControllerMessage>) -> Self {
        Self { send, state: Arc::default(), queue: Arc::default() }
    }

    /// The target took a demo. The uploader tells it before it removes the file, so a report that
    /// finds the demo gone already knows the removal is `uploaded`.
    pub fn uploaded(&self, slot_id: &str, file_name: &str) {
        let mut state = lock(&self.state);

        state.uploaded.insert(demo_key(slot_id, file_name));

        if state.uploaded.len() > UPLOADED_KEPT {
            state.uploaded.shift_remove_index(0);
        }
    }

    /// The uploader could not remove a demo the target took, so the file stays. A `delete_demo` that
    /// removes it later goes out as `deleted`.
    pub fn not_removed(&self, slot_id: &str, file_name: &str) {
        lock(&self.state).uploaded.shift_remove(&demo_key(slot_id, file_name));
    }

    /// What the hello about to go out carries.
    pub fn helloed(&self, demos: &[DemoFile]) {
        lock(&self.state).known = index_by_key(demos.iter().map(|demo| (key_of(demo), demo.clone())));
    }

    /// Reports what `list` finds, listed in this report's turn; reports run one after another, and a
    /// failed one leaves the next to catch up.
    ///
    /// # Errors
    ///
    /// Fails when the demos cannot be listed.
    pub async fn report(&self, list: &ListDemos) -> io::Result<()> {
        let _turn = self.queue.lock().await;

        self.send_changes(list).await
    }

    async fn send_changes(&self, list: &ListDemos) -> io::Result<()> {
        let on_disk = list().await?;
        let newest: HashSet<String> = newest_demos(&on_disk).iter().map(key_of).collect();
        let listed = index_by_key(on_disk.iter().map(|demo| (key_of(demo), demo.clone())));
        let changes: Vec<(DemoFile, bool)> = {
            let state = lock(&self.state);
            let changed = listed
                .iter()
                .filter(|(key, demo)| (newest.contains(*key) || state.known.contains_key(*key)) && state.known.get(*key) != Some(*demo))
                .map(|(_, demo)| (demo.clone(), false));
            let removed = state.known.iter().filter(|(key, _)| !listed.contains_key(*key)).map(|(_, demo)| (demo.clone(), true));

            changed.chain(removed).collect()
        };

        for (demo, removed) in changes {
            let key = key_of(&demo);
            let frame = if removed {
                let reason = if lock(&self.state).uploaded.contains(&key) { DemoRemovedReason::Uploaded } else { DemoRemovedReason::Deleted };

                AgentToControllerMessage::DemoRemoved(AgentDemoRemoved { slot_id: demo.slot_id.clone(), file_name: demo.file_name.clone(), reason })
            } else {
                AgentToControllerMessage::Demo(AgentDemo {
                    slot_id: demo.slot_id.clone(),
                    file_name: demo.file_name.clone(),
                    size_bytes: demo.size_bytes,
                    modified_at: demo.modified_at,
                    recording: demo.recording,
                    uploading: demo.uploading,
                    upload_error: demo.upload_error.clone(),
                })
            };

            let left = (self.send)(frame).await;
            let mut state = lock(&self.state);

            if left {
                if removed {
                    state.uploaded.shift_remove(&key);
                    state.known.shift_remove(&key);
                } else {
                    state.known.insert(key, demo);
                }
            }
        }

        Ok(())
    }
}

#[must_use]
pub(crate) fn backlog_frame(backlog: EventBacklog) -> AgentEventBacklog {
    AgentEventBacklog { servers: backlog.servers, server_count: backlog.server_count, event_count: backlog.event_count, reason: backlog.reason }
}

pub(crate) type Cs2StateRead = Arc<dyn Fn() -> BoxFuture<'static, Result<Cs2State, DockerError>> + Send + Sync>;

/// What the hello and `node_status` read, and where they go.
pub(crate) struct NodeReporterDeps {
    /// Reads the CS2 install and image state that the hello and `node_status` carry.
    pub cs2_state: Cs2StateRead,
    /// Why the node takes no launches now, if it takes none.
    pub unavailable: Arc<dyn Fn() -> Option<NodeUnavailable> + Send + Sync>,
    /// The agent update under way or the last one that failed.
    pub agent_update: AgentUpdateState,
    /// The node's settings, which the hello carries.
    pub config: ConfigReader,
    /// The slots, whose statuses the hello and the welcome's catch-up carry.
    pub slots: SlotManager,
    /// The demos on the node, which the hello lists.
    pub demos: DemoUploader,
    /// Counts the event backlog for the hello and the welcome.
    pub uploader: EventUploader,
    /// The server exits the hello carries and the welcome sends.
    pub exits: ExitReporter,
    /// Slot statuses, reset by each hello.
    pub slot_reports: ChangeReporter<AgentSlotStatus>,
    /// The node status, reset by each hello.
    pub node_reports: ChangeReporter<AgentNodeStatus>,
    /// The event backlog, reset by each hello.
    pub backlog_reports: ChangeReporter<AgentEventBacklog>,
    /// Demo reports, reset by each hello.
    pub demo_reports: DemoReporter,
    /// Reports the demos after the welcome, the same way a change of them is reported.
    pub report_demos: Arc<dyn Fn(&DemoUploader) + Send + Sync>,
    /// Resource usage, sent again after the welcome.
    pub resources: ResourceReporter,
    /// The node log, sent again after the welcome.
    pub node_log: NodeLog,
    /// The controller connection the hello goes out on.
    pub connection: ControllerConnection,
    /// The agent's version and image, which the hello carries.
    pub agent: AgentInfo,
    /// The node's public address and country, which the hello carries.
    pub address: PublicAddress,
    /// Where the reporter logs, such as a backlog that could not be counted.
    pub log: Logger,
    /// Where a demo listing that failed is logged.
    pub demo_log: Logger,
}

struct NodeReporterInner {
    /// What the reporter reads and where it sends.
    deps: NodeReporterDeps,
    /// The newest read's number; an older read that finishes later is dropped.
    reads: AtomicU64,
}

/// Says hello on each socket and reports the node with `node_status`.
#[derive(Clone)]
pub(crate) struct NodeReporter {
    inner: Arc<NodeReporterInner>,
}

impl NodeReporter {
    #[must_use]
    pub fn new(deps: NodeReporterDeps) -> Self {
        Self { inner: Arc::new(NodeReporterInner { deps, reads: AtomicU64::new(0) }) }
    }

    /// The controller welcomed the hello. The exits it did not carry go out first, then every slot's status
    /// and the node; once the controller has the node, the backlog, the demos and the streams the controller
    /// watches.
    pub fn welcomed(&self) -> BoxFuture<'static, Result<(), OpError>> {
        let deps = &self.inner.deps;

        deps.exits.welcomed();

        for status in deps.slots.statuses() {
            deps.slot_reports.report(slot_status(status));
        }

        let node = self.report();
        let this = Arc::clone(&self.inner);

        Box::pin(async move {
            node.await?;

            let deps = &this.deps;
            let (uploader, backlog_reports, log) = (deps.uploader.clone(), deps.backlog_reports.clone(), deps.log.clone());

            // A backlog held back before the welcome goes now, not at the next upload pass.
            tokio::spawn(async move {
                match uploader.backlog().await {
                    Ok(backlog) => backlog_reports.report(backlog_frame(backlog)),
                    Err(error) => log.warn(format!("could not count the event backlog: {error}")),
                }
            });
            (deps.report_demos)(&deps.demos);
            deps.resources.resend();
            deps.node_log.resend();
            Ok(())
        })
    }

    /// Reads the node's state and reports it, unless a newer read started meanwhile.
    ///
    /// A report is a snapshot of now, so an older read that finishes later is dropped. A hello needs no
    /// part in this, because only answers leave before the welcome, and the welcome reports afresh. If
    /// the newest read fails, nothing is sent here; [`NodeReporter::follow`] reads again.
    pub fn report(&self) -> BoxFuture<'static, Result<(), OpError>> {
        self.numbered_report().1
    }

    /// [`NodeReporter::report`], with the number of its read.
    fn numbered_report(&self) -> (u64, BoxFuture<'static, Result<(), OpError>>) {
        let inner = &self.inner;
        let deps = &inner.deps;
        let read = inner.reads.fetch_add(1, Ordering::SeqCst) + 1;
        let (cs2, unavailable, agent_update, this) = ((deps.cs2_state)(), (deps.unavailable)(), deps.agent_update.get(), Arc::clone(inner));

        (
            read,
            Box::pin(async move {
                let cs2 = cs2.await?;

                if read == this.reads.load(Ordering::SeqCst) {
                    this.deps.node_reports.report(AgentNodeStatus { cs2, unavailable, agent_update });
                }

                Ok(())
            }),
        )
    }

    /// Reports each node change. Reports are sent only on a change, so one whose read fails is read
    /// again every [`NODE_REPORT_RETRY`] until it goes out, or until a newer change reports instead.
    pub fn follow(&self, tasks: &TaskTracker, stop: &CancellationToken, mut changes: mpsc::UnboundedReceiver<&'static str>) {
        let (this, stop) = (self.clone(), stop.clone());

        tasks.spawn(async move {
            while let Some(what) = stop.run_until_cancelled(changes.recv()).await.flatten() {
                let (this, stop, (mut read, mut report)) = (this.clone(), stop.clone(), this.numbered_report());

                tokio::spawn(async move {
                    while let Err(error) = report.await {
                        this.inner.deps.log.warn(format!("could not report {what}: {error}; reading again in {} s", NODE_REPORT_RETRY.as_secs()));

                        if stop.run_until_cancelled(tokio::time::sleep(NODE_REPORT_RETRY)).await.is_none() || this.inner.reads.load(Ordering::SeqCst) != read {
                            break;
                        }

                        (read, report) = this.numbered_report();
                    }
                });
            }
        });
    }

    /// Reads the node and queues the hello, which tells each reporter what the controller holds from here.
    pub fn hello(&self) -> BoxFuture<'static, Result<(), OpError>> {
        let this = Arc::clone(&self.inner);
        let cs2 = (this.deps.cs2_state)();

        Box::pin(async move {
            let deps = &this.deps;
            let cs2 = cs2.await?;
            // A demo directory that cannot be read, or a disk that does not answer, must not hold up
            // the hello; the next report fills it in.
            let listing = match tokio::time::timeout(HELLO_LISTING_LIMIT, deps.demos.list()).await {
                Ok(listing) => listing,
                Err(_) => Err(std::io::Error::other(format!("the disk did not answer within {} s", HELLO_LISTING_LIMIT.as_secs()))),
            };
            let listed = newest_demos(&listing.unwrap_or_else(|error| {
                deps.demo_log.warn(format!("could not list demos: {error}"));
                Vec::new()
            }));
            // Nor must a spool that cannot be read; the next upload pass reports the backlog. A spool
            // the node reports unavailable is not asked, because a stuck spool would hold the hello.
            // Maintenance hides that reason, so a spool that hangs during it holds the hello. That is
            // accepted, because nothing on that disk works meanwhile.
            let spool_down = (deps.unavailable)().is_some_and(|unavailable| unavailable.code == NodeUnavailableCode::EventStorage);
            let backlog = if spool_down {
                nothing_stuck()
            } else {
                deps.uploader.backlog().await.unwrap_or_else(|error| {
                    deps.log.warn(format!("could not count the event backlog: {error}"));
                    nothing_stuck()
                })
            };
            let os_name = read_os_name(Path::new(OS_RELEASE_FILE)).await;
            let slot_status_now = deps.slots.statuses();
            let node = AgentNodeStatus { cs2: cs2.clone(), unavailable: (deps.unavailable)(), agent_update: deps.agent_update.get() };

            deps.slot_reports.helloed(&slot_status_now.iter().cloned().map(slot_status).collect::<Vec<_>>());
            deps.node_reports.helloed(std::slice::from_ref(&node));
            deps.backlog_reports.helloed(&[backlog_frame(backlog.clone())]);
            deps.demo_reports.helloed(&listed);

            // Reads live update state on every hello, including reconnects during an install.
            let hello = build_hello(HelloInput {
                agent: deps.agent.clone(),
                host: HostInfo { cpu_count: cpu_count(), os_name, kernel: kernel_release() },
                public_ip: deps.address.public_ip.clone(),
                country: deps.address.country.clone(),
                config: deps.config.read(),
                slot_status: slot_status_now,
                cs2: node.cs2,
                unavailable: node.unavailable,
                demos: listed,
                server_exits: deps.exits.helloed(),
                event_backlog: backlog,
                agent_update: node.agent_update,
            });

            // Queued before push returns; nothing waits for its ack.
            drop(deps.connection.push(&hello));

            Ok(())
        })
    }
}

fn cpu_count() -> i64 {
    std::thread::available_parallelism().map_or(1, |count| i64::try_from(count.get()).unwrap_or(i64::MAX))
}

fn kernel_release() -> String {
    nix::sys::utsname::uname().map(|name| name.release().to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ankor_protocol::constants::QUIT_TIMEOUT;
    use ankor_protocol::types::{ServerExitReason, SlotState};

    use super::*;
    use crate::testing::fixtures::{agent_config, launch_config, ready_status};
    use crate::testing::slots::{launch_server, make_slots};

    #[tokio::test]
    async fn forgets_the_oldest_demo_the_target_took_past_the_limit() {
        use futures_util::FutureExt as _;

        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let record = Arc::clone(&sent);
        let reporter = DemoReporter::new(Arc::new(move |frame| {
            lock(&record).push(frame);

            async { true }.boxed()
        }));
        let demo = |file_name: &str| DemoFile {
            slot_id: "a".to_owned(),
            file_name: file_name.to_owned(),
            size_bytes: 1,
            modified_at: 1,
            recording: false,
            uploading: false,
            upload_error: None,
        };
        let nothing: ListDemos = Arc::new(|| async { Ok(Vec::new()) }.boxed());

        reporter.helloed(&[demo("0.dem"), demo("1.dem")]);

        for index in 0..=UPLOADED_KEPT {
            reporter.uploaded("a", &format!("{index}.dem"));
        }

        reporter.report(&nothing).await.unwrap();

        let reasons: Vec<_> = lock(&sent)
            .iter()
            .filter_map(|frame| match frame {
                AgentToControllerMessage::DemoRemoved(removed) => Some((removed.file_name.clone(), removed.reason)),
                _ => None,
            })
            .collect();

        assert_eq!(reasons, [("0.dem".to_owned(), DemoRemovedReason::Deleted), ("1.dem".to_owned(), DemoRemovedReason::Uploaded)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_plugin_saying_it_quits_reaches_the_slot_through_the_plugin_port() {
        let base = make_slots(agent_config(&serde_json::json!({})));
        let allocation_id = "11111111-1111-4111-8111-111111111111";

        launch_server(&base.slots, launch_config(&serde_json::json!({})), allocation_id).await.unwrap();

        let port: &dyn PluginSlots = &base.slots;

        // Ready first, so only the quit timeout can stop the server, not the start timeout.
        port.plugin_status(allocation_id, &ready_status());
        port.plugin_quitting(allocation_id);
        tokio::time::sleep(QUIT_TIMEOUT + Duration::from_secs(1)).await;
        crate::testing::until("the quit timeout", || base.status("a").state == SlotState::Idle).await;
        assert_eq!(base.exits().iter().map(|(exit, _)| exit.reason).collect::<Vec<_>>(), [ServerExitReason::QuitTimeout]);
    }
}
