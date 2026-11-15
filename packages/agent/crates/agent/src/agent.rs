//! Starts the agent: connects its components, says hello to the controller and keeps the node reported, and
//! stops it on a signal. Game servers outlive the agent and are adopted again on the next boot.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ankor_protocol::constants::{CS2_BUILD_CHECK_INTERVAL, CS2_BUILD_POLL_INTERVAL, IMAGE_UPDATE_POLL_INTERVAL};
use ankor_protocol::helpers::{OriginKind, agent_ws_url, controller_origin, controller_tls_by_default};
use ankor_protocol::types::{
    AgentEventBacklog, AgentInfo, AgentNodeStatus, AgentSlotStatus, AgentToControllerMessage, Cs2State, Cs2UpdateMode, MaintenanceOperation, NodeUnavailable,
};
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent_reporting::{
    ChangeReporter, Cs2StateRead, DemoReporter, ExitReporter, ListDemos, NodeReporter, NodeReporterDeps, SendFrame, SlotFeed, backlog_frame, node_unavailable,
    plugin_feed, update_feed,
};
use crate::agent_update::AgentUpdateState;
use crate::config::{ConfigReader, ConfigStore, load_agent_config};
use crate::container_spec::{GAME_CPU_WEIGHT, cpu_shares_for_weight};
use crate::contract::types::{PluginCommand, PluginGetServerStatus};
use crate::controller_connection::{ControllerConnection, Defer, HandlerError, Hooks, Request, Signal, Timings, Watch};
use crate::cs2_auto_update::{Cs2AutoUpdate, NodeCs2, SteamFetch};
use crate::cs2_install::InstalledBuild;
use crate::demo_uploader::{DemoUploader, DemoUploaderOptions};
use crate::disk_space::{DISK_CHECK, DiskSpace};
use crate::docker::DockerPort;
use crate::event_spool::SpoolError;
use crate::event_uploader::EventsTarget;
use crate::events::{Events, start_events};
use crate::failure::{BoxError, OpError};
use crate::gslt_keep_alive::{GsltKeepAlive, GsltSlotsPort, SteamLogon};
use crate::host_layout::{agent_update_failure_path, refused_gslts_path, spool_path};
use crate::image_auto_update::{ImageAutoUpdate, NodeImages};
use crate::install::{Cs2UpdateState, InstallOptions, MaintenanceGuard, MaintenanceGuardOptions, begin_cs2_update};
use crate::logger::Logger;
use crate::node_log::{NodeLog, Transport};
use crate::overlay::{OverlayError, OverlayPort, OverlayStore};
use crate::public_ip::{PublicIpResolver, resolve_public_address};
use crate::refused_gslts::RefusedGslts;
use crate::registry_tags::RegistryFetch;
use crate::request_handler::{RequestHandlerDeps, handle};
use crate::resources::{ResourceReporter, ResourceReporterOptions};
use crate::self_update::{AgentUpdateDeps, AgentUpdateFactory, Cs2ImageUpdateDeps, Cs2ImageUpdateFactory, Cs2ImageUpdateState, prune_superseded_images};
use crate::slot_files::SlotFiles;
use crate::slot_manager::{SlotEvent, SlotManager, SlotManagerDeps};
use crate::spool_client::{SPOOL_CHECK, SpoolClient, SpoolPort, SpoolWatch};

/// How often game servers are asked for their players and checked for staying empty. A failed stop
/// is retried on the same tick.
const GAME_CHECK: Duration = Duration::from_secs(30);
const DEMO_SCAN: Duration = Duration::from_secs(30);

/// What the agent runs on: its configuration, its data directory, Docker and the services it calls.
pub struct AgentHost {
    /// The root of every component's stop, which shutdown cancels. The node log, built before the
    /// agent to capture its startup, follows a child of it.
    pub stop: CancellationToken,
    /// The node's `agent.yml`.
    pub config_path: PathBuf,
    /// The node's data directory, which holds the slots, the event spool and the agent's state files.
    pub data_dir: PathBuf,
    /// The game volume as the agent sees it, read-only, from which it reads the installed build.
    pub cs2_install: PathBuf,
    /// The Docker Engine that runs the agent, its game servers and helper containers.
    pub docker: Arc<dyn DockerPort>,
    /// Finds the node's public address and country, which are reported to the controller.
    pub public_ip: Arc<dyn PublicIpResolver>,
    /// The port the plugin relay listens on for game servers.
    pub relay_port: u16,
    /// Logs game server login tokens on to Steam, which keeps them from expiring.
    pub steam_logon: SteamLogon,
    /// Reads the newest public CS2 build.
    pub steam_fetch: Arc<dyn SteamFetch>,
    /// Reads the image repositories' tags from their registries.
    pub registry_fetch: Arc<dyn RegistryFetch>,
    /// Holds the agent's lines from its first, so the controller can be shown the startup too.
    pub node_log: NodeLog,
    /// The agent's root logger.
    pub log: Logger,
}

pub struct RunningAgent {
    /// The agent's root logger.
    log: Logger,
    /// Once cancelled, no loop goes on, no timer runs out and no update or poll starts.
    stop: CancellationToken,
    /// The agent's own loops, which shutdown waits for.
    tasks: TaskTracker,
    /// The socket to the controller, which shutdown closes with its signal.
    controller_connection: ControllerConnection,
    /// The event pipeline, which shutdown closes after the socket.
    events: Arc<Events>,
    /// Why the spool did not close at shutdown, if it did not. A second call gets the first one's answer.
    done: tokio::sync::OnceCell<Result<(), Arc<SpoolError>>>,
}

impl RunningAgent {
    /// Game servers keep running. A second call waits for the first and fails as it did.
    ///
    /// # Errors
    ///
    /// Fails when the event spool cannot close its file.
    pub async fn shutdown(&self, signal: Signal) -> Result<(), Arc<SpoolError>> {
        self.done
            .get_or_init(|| async {
                self.log.info(format!("{}: shutting down (game servers keep running and are re-adopted on boot)", signal.name()));
                self.stop.cancel();

                // The pipeline closes after the requests that run, so a stop among them still stores
                // its server's exit and last events. Both waits together fit Docker's stop timeout.
                self.controller_connection.shutdown(signal).await;

                let closed = self.events.close().await;

                self.tasks.close();
                self.tasks.wait().await;
                closed.map_err(Arc::new)
            })
            .await
            .clone()
    }
}

fn warn_cleartext(log: &Logger, host: &str, tls: bool) {
    if !tls && controller_tls_by_default(host) {
        log.warn(format!("controller.host {host} is not a local address and controller.tls is off: the node token crosses the network in the clear"));
    }
}

/// Queues a frame on the socket, keeping the order frames are given in.
fn push_on(connection: &ControllerConnection) -> Arc<dyn Fn(AgentToControllerMessage) + Send + Sync> {
    let connection = connection.clone();

    Arc::new(move |frame| {
        // The frame is queued before push returns; nothing waits for its ack.
        drop(connection.push(&frame));
    })
}

fn send_on<T: Send + 'static>(connection: &ControllerConnection, wrap: fn(T) -> AgentToControllerMessage) -> SendFrame<T> {
    let connection = connection.clone();

    Arc::new(move |frame| connection.push(&wrap(frame)).boxed())
}

struct SocketTransport {
    connection: ControllerConnection,
    push: Arc<dyn Fn(AgentToControllerMessage) + Send + Sync>,
}

impl Transport for SocketTransport {
    fn connected(&self) -> bool {
        self.connection.welcomed()
    }

    fn send(&self, frame: AgentToControllerMessage) {
        (self.push)(frame);
    }
}

struct AgentHooks {
    config: ConfigReader,
    handler: RequestHandlerDeps,
    log: Logger,
    reporter: NodeReporter,
    on_failed: Arc<dyn Fn() + Send + Sync>,
    on_watch: Arc<dyn Fn(Watch) + Send + Sync>,
    on_check: Arc<dyn Fn() + Send + Sync>,
}

impl Hooks for AgentHooks {
    fn url(&self) -> String {
        let controller = self.config.read().controller;

        agent_ws_url(&controller.host, controller.tls, &controller.socket_path)
    }

    fn on_open(&self, _connection: ControllerConnection) -> BoxFuture<'static, Result<(), OpError>> {
        self.reporter.hello()
    }

    // Replays once the controller has taken the hello, so it can file the history under this node.
    fn on_welcome(&self, _connection: ControllerConnection) -> BoxFuture<'static, Result<(), OpError>> {
        self.reporter.welcomed()
    }

    fn on_close(&self, code: u16, reason: &str) {
        let reason = if reason.is_empty() { String::new() } else { format!(" {reason}") };

        self.log.warn(format!("disconnected ({code}{reason})"));
    }

    fn on_watch(&self, watch: Watch) {
        (self.on_watch)(watch);
    }

    fn on_failed(&self, _reason: &str) {
        (self.on_failed)();
    }

    fn on_check_cs2_build(&self) {
        (self.on_check)();
    }

    fn handle(&self, request: Request, defer: Defer) -> BoxFuture<'static, Result<Value, HandlerError>> {
        let deps = self.handler.clone();

        Box::pin(async move { handle(&deps, request, defer).await })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BootError {
    #[error(
        "{} agent containers are running ({}); a node runs one. Remove the extra one with docker rm -f, or re-run node-setup.sh --update to start over with one",
        .0.len(),
        .0.join(", ")
    )]
    ExtraAgents(Vec<String>),
    #[error("no container carries the agent's label; the agent runs as the container node-setup.sh creates")]
    NoOwnContainer,
}

/// # Errors
///
/// Fails when the configuration cannot be read, Docker is unusable, another agent runs, the relay
/// cannot listen, or the running servers cannot be adopted.
#[allow(clippy::too_many_lines, reason = "the one place every component is built and connected, in the order boot needs")]
pub async fn run_agent(host: AgentHost) -> Result<RunningAgent, BoxError> {
    let AgentHost { stop, config_path, data_dir, cs2_install, docker, public_ip, relay_port, steam_logon, steam_fetch, registry_fetch, node_log, log } = host;
    let path_text = config_path.display().to_string();
    let warn_log = log.clone();
    let config =
        ConfigStore::new(config_path.clone(), load_agent_config(&config_path, |key| warn_log.warn(format!("{path_text}: unsupported key {key} is ignored")))?);
    let initial = config.read();
    // Every part but set_config and the image updates only reads the config.
    let config_reader = config.reader();

    log.info(format!("@ankorcs/agent {} with {} slot(s)", crate::AGENT_VERSION, initial.slots.len()));
    warn_cleartext(&log, &initial.controller.host, initial.controller.tls);
    log.info(format!("Docker Engine {}", docker.require_supported_engine().await?));

    // Two agents would adopt, update and sweep the same containers. A swap stops and renames the old
    // agent before it starts the new one; the rename waits until Docker lists the old one as stopped,
    // so whichever agent finds another running is the extra one.
    let agents = docker.running_agent_containers().await?;

    if agents.len() > 1 {
        return Err(BootError::ExtraAgents(agents).into());
    }

    // Only update containers write the game volume, and each one's end forgets the build read before it.
    let build = InstalledBuild::new(cs2_install, log.child("cs2"));
    let files = Arc::new(SlotFiles::new(data_dir.clone()));
    let overlay: Arc<dyn OverlayPort> = Arc::new(OverlayStore::new(Arc::clone(&docker), data_dir.clone(), log.child("overlay")));
    // Exists from here, so every part that reports to the controller holds it; it dials once the hooks exist.
    let (connection, driver) = ControllerConnection::new(initial.token.clone(), Timings::default(), log.child("ws"));
    let push = push_on(&connection);
    let transport: Arc<dyn Transport> = Arc::new(SocketTransport { connection: connection.clone(), push: Arc::clone(&push) });

    let tasks = TaskTracker::new();

    node_log.attach(Arc::clone(&transport));

    let slot_reports = ChangeReporter::new(|frame: &AgentSlotStatus| frame.slot_id.clone(), send_on(&connection, AgentToControllerMessage::SlotStatus));
    let node_reports = ChangeReporter::new(|_: &AgentNodeStatus| "node".to_owned(), send_on(&connection, AgentToControllerMessage::NodeStatus));
    let backlog_reports = ChangeReporter::new(|_: &AgentEventBacklog| "backlog".to_owned(), send_on(&connection, AgentToControllerMessage::EventBacklog));

    log.info(format!("game servers run at cpu weight {GAME_CPU_WEIGHT} (cpu shares {})", cpu_shares_for_weight(GAME_CPU_WEIGHT)));

    let spool_client = Arc::new(SpoolClient::start(spool_path(&data_dir), log.child("events")));
    let spool: Arc<dyn SpoolPort> = Arc::clone(&spool_client) as Arc<dyn SpoolPort>;
    let target_config = config_reader.clone();
    let backlog_sink = backlog_reports.clone();
    let (events, relay_listener) = start_events(
        Arc::clone(&spool),
        relay_port,
        &initial.token,
        Arc::new(move || {
            let controller = target_config.read().controller;

            EventsTarget { host: controller.host, tls: controller.tls, events_path: controller.events_path }
        }),
        Arc::new(move |backlog| backlog_sink.report(backlog_frame(backlog))),
        &log,
    )
    .await?;
    let events = Arc::new(events);
    let relay = events.relay.clone();
    let uploader = events.uploader.clone();
    let spool_unavailable = events.unavailable.clone();
    // Undelivered exits live in the spool, so an agent restart before the controller has them keeps them; one
    // that did not open keeps them in memory alone.
    let exit_store = spool_unavailable.is_none().then(|| Arc::clone(&spool));
    let exits = ExitReporter::new(send_on(&connection, AgentToControllerMessage::ServerExited), exit_store.clone(), log.child("exits"));

    exits.restore(&match &exit_store {
        Some(store) => store.exits().await.unwrap_or_else(|error| {
            log.warn(format!("could not read the exits a previous run kept: {error}"));
            Vec::new()
        }),
        None => Vec::new(),
    });

    let slot_config = config_reader.clone();
    let (url_relay, poll_relay) = (relay.clone(), relay.clone());
    // What the controller is told and the node logs, queued with each change, so frames keep its order.
    let feed = SlotFeed::new(Arc::new(slot_reports.clone()), exits.clone(), node_log.clone());
    let (slots, slot_events) = SlotManager::new(SlotManagerDeps {
        config: Arc::new(move || slot_config.read()),
        docker: Arc::clone(&docker),
        files,
        overlay: Arc::clone(&overlay),
        plugin_url: Arc::new(move |slot_id, allocation_id, key| url_relay.prepare(slot_id, allocation_id, key)),
        allocations: exit_store,
        poll_plugin: Arc::new(move |allocation_id| {
            let _ = poll_relay.command(allocation_id, &PluginCommand::GetServerStatus(PluginGetServerStatus { request_id: None }));
        }),
        report: Some(Arc::new(move |event| feed.handle(event))),
        log: log.child("slots"),
        stop: stop.child_token(),
    });

    // The hooks hold the slots, which hold the relay that holds the hooks: a cycle for the life of
    // the process, which shutdown ends by closing the relay.
    relay_listener.serve(plugin_feed(Arc::new(slots.clone()), Arc::clone(&push)));

    let demo_log = log.child("demos");
    let demo_reports = DemoReporter::new(Arc::new({
        let connection = connection.clone();

        move |frame| connection.push(&frame).boxed()
    }));
    // Reports what the uploader it is given lists, so the reporter and the uploader need not hold each other.
    let report_demos: Arc<dyn Fn(&DemoUploader) + Send + Sync> = {
        let (reports, log) = (demo_reports.clone(), demo_log.clone());

        Arc::new(move |demos: &DemoUploader| {
            let (reports, log, demos) = (reports.clone(), log.clone(), demos.clone());
            let list: ListDemos = Arc::new(move || {
                let demos = demos.clone();

                Box::pin(async move { demos.list().await })
            });

            tokio::spawn(async move {
                if let Err(error) = reports.report(&list).await {
                    log.warn(format!("could not report demos: {error}"));
                }
            });
        })
    };
    let demos = {
        let (target, running, busy, uploaded_reports, progress_push) =
            (config_reader.clone(), slots.clone(), slots.clone(), demo_reports.clone(), Arc::clone(&push));
        let mut options = DemoUploaderOptions::new(
            data_dir.clone(),
            Arc::new(move || target.read().demo_upload),
            Arc::new(move |slot_id| running.running_allocation(slot_id)),
            Arc::new(move || busy.any_busy()),
        );

        options.log = demo_log.clone();
        options.on_change = Some(Arc::clone(&report_demos));
        options.on_progress = Some(Arc::new(move |progress| progress_push(AgentToControllerMessage::DemoProgress(progress))));
        report_uploads(&mut options, &uploaded_reports);
        DemoUploader::new(options)
    };

    let scan_demos: Arc<dyn Fn() + Send + Sync> = {
        let (demos, log) = (demos.clone(), demo_log.clone());

        Arc::new(move || {
            let (scan, log) = (demos.scan_all(), log.clone());

            tokio::spawn(async move {
                if let Err(error) = scan.await {
                    log.warn(format!("demo scan failed: {error}"));
                }
            });
        })
    };

    // Made before adoption, whose servers may report ready; it logs nothing on until start.
    let gslt_log = log.child("gslt");
    let gslts = GsltKeepAlive::new(
        Arc::new(slots.clone()) as Arc<dyn GsltSlotsPort>,
        steam_logon,
        RefusedGslts::new(Some(refused_gslts_path(&data_dir)), gslt_log.clone()),
        gslt_log,
        stop.child_token(),
    );

    let update_log = log.child("update");
    // Progress and lines of an update, whether this agent started it or inherited it.
    let update_output = {
        let node_log = node_log.clone();

        update_feed(Arc::clone(&push), Arc::new(move |line| node_log.update_line(&line)))
    };
    // What changed in the node's state, for the node report task, which reads and reports it.
    let (node_changes, node_changed) = tokio::sync::mpsc::unbounded_channel::<&'static str>();
    let reporting = |what: &'static str| -> Arc<dyn Fn() + Send + Sync> {
        let (node_changes, connection) = (node_changes.clone(), connection.clone());

        // Nothing is read for the controller before the socket exists.
        Arc::new(move || {
            if connection.started() {
                let _ = node_changes.send(what);
            }
        })
    };
    // What node_status reports as cs2.update; an inherited update fills it before the first hello.
    // An update's end drops the build read before it is reported, so the report reads the new one.
    let update_state = {
        let (ended_build, ended_log, started_log) = (build.clone(), node_log.clone(), node_log.clone());

        Cs2UpdateState::new(
            reporting("the cs2 update"),
            Arc::new(move || {
                ended_build.forget();
                ended_log.update_ended();
            }),
            Arc::new(move |id| started_log.update_started(id)),
        )
    };
    // What node_status reports as cs2.image_update: the switch of the cs2 image that runs or last ended.
    let image_update_state = Cs2ImageUpdateState::new(reporting("the cs2 image switch"));
    // Adopts any surviving update before serving requests; a claim looks again before every update.
    let updates = {
        let mut options = MaintenanceGuardOptions::new(Arc::clone(&docker));

        options.stop = stop.child_token();

        options.log = update_log.clone();
        options.state = Some(update_state.clone());
        options.config = Some(config_reader.clone());
        options.on_progress = Arc::clone(&update_output.on_progress);
        options.on_log = Some(Arc::clone(&update_output.on_log));
        MaintenanceGuard::new(options)
    };
    // The node report follows the guard. The boot-time sweep takes and releases it before there is
    // a socket; the first hello reads afresh.
    follow_guard(&tasks, &stop, updates.subscribe(), {
        let report = reporting("the maintenance guard");

        move |_| report()
    });
    // update_cs2 and the automatic update run one and the same update.
    let install: Arc<dyn Fn(Cs2UpdateMode, bool) -> InstallOptions + Send + Sync> = {
        let (config, docker, overlay, slots, updates, update_output, update_log, update_state) = (
            config_reader.clone(),
            Arc::clone(&docker),
            Arc::clone(&overlay),
            slots.clone(),
            updates.clone(),
            update_output,
            update_log.clone(),
            update_state.clone(),
        );

        Arc::new(move |mode, automatic| {
            let slots = slots.clone();

            InstallOptions {
                config: config.clone(),
                docker: Arc::clone(&docker),
                on_progress: Arc::clone(&update_output.on_progress),
                on_log: Some(Arc::clone(&update_output.on_log)),
                log: update_log.clone(),
                any_busy: Arc::new(move || slots.any_busy()),
                updates: updates.clone(),
                mode,
                automatic,
                state: Some(update_state.clone()),
                overlay: Arc::clone(&overlay),
                short_id: None,
            }
        })
    };
    let cs2 = Cs2AutoUpdate::new(
        Arc::new(NodeCs2 {
            config: config_reader.clone(),
            build: build.clone(),
            slots: slots.clone(),
            updates: updates.clone(),
            install: Arc::clone(&install),
            on_latest: reporting("the newest public build"),
        }),
        steam_fetch,
        log.child("cs2"),
        CS2_BUILD_POLL_INTERVAL,
        CS2_BUILD_CHECK_INTERVAL,
        stop.child_token(),
    );

    // An update this agent did not start may have ended without installing the newest build.
    on_guard_free(&tasks, &stop, updates.subscribe(), {
        let cs2 = cs2.clone();

        move || {
            let cs2 = cs2.clone();

            tokio::spawn(async move { cs2.consider().await });
        }
    });

    // What an idle slot or a used token sets off. None of it depends on when it runs: the relay
    // entry is released only if it still belongs to the launch that ended.
    {
        let (relay, scan_demos, cs2, gslts, stop) = (relay.clone(), Arc::clone(&scan_demos), cs2.clone(), gslts.clone(), stop.clone());
        let mut slot_events = slot_events;

        tasks.spawn(async move {
            while let Some(event) = stop.run_until_cancelled(slot_events.recv()).await.flatten() {
                match event {
                    SlotEvent::SlotIdle { slot_id, allocation_id } => {
                        if let Some(allocation_id) = allocation_id {
                            relay.release_allocation(&slot_id, &allocation_id);
                        }

                        scan_demos();

                        let cs2 = cs2.clone();

                        tokio::spawn(async move { cs2.consider().await });
                    }
                    SlotEvent::TokenUsed(gslt) => gslts.used(&gslt),
                    _ => {}
                }
            }
        });
    }

    let address = resolve_public_address(public_ip.as_ref(), &log.child("public-ip")).await;

    log.info(format!("public ip: {}, country: {}", address.public_ip.as_deref().unwrap_or("unknown"), address.country.as_deref().unwrap_or("unknown")));
    slots.adopt().await?;

    {
        let gslts = gslts.clone();

        tokio::spawn(async move { gslts.start().await });
    }

    scan_demos();

    tasks.spawn({
        let (scan_demos, stop) = (Arc::clone(&scan_demos), stop.clone());

        async move {
            let mut interval = every(DEMO_SCAN);

            while stop.run_until_cancelled(interval.tick()).await.is_some() {
                scan_demos();
            }
        }
    });
    tasks.spawn({
        let (slots, stop) = (slots.clone(), stop.clone());

        async move {
            let mut interval = every(GAME_CHECK);

            while stop.run_until_cancelled(interval.tick()).await.is_some() {
                slots.check_games(tokio::time::Instant::now());
            }
        }
    });

    // Sweeps stopped helper containers left by interrupted operations; a failure costs nothing.
    match docker.sweep_helper_containers().await {
        Ok(removed) => log.info(format!("swept {} helper container(s)", removed.len())),
        Err(error) => log.warn(format!("helper sweep failed: {error}")),
    }

    let disk_space = {
        let (report, disk_log) = (reporting("the disk space"), log.child("disk"));

        DiskSpace::new(
            data_dir.clone(),
            Arc::new(|dir: &std::path::Path| crate::resources::statvfs(dir)),
            Arc::new(move |short: Option<&NodeUnavailable>| {
                match short {
                    Some(short) => disk_log.error(format!("{}; launches are refused until space is freed", short.detail)),
                    None => disk_log.info("enough disk space again; launches are taken"),
                }

                report();
            }),
        )
    };

    // The ticks report the disk as the space check last read it, so no tick asks a disk that may hang.
    let resources = {
        let disk_space = disk_space.clone();
        let mut options = ResourceReporterOptions::new(Arc::clone(&transport), Arc::new(move || disk_space.usage()));

        options.stop = stop.child_token();
        ResourceReporter::new(options)
    };

    resources.start();

    disk_space.refresh().await;

    let spool_watch = SpoolWatch::new(spool_client, reporting("the event spool"), log.child("events"));
    let unavailable: Arc<dyn Fn() -> Option<NodeUnavailable> + Send + Sync> = {
        let (updates, disk_space, spool_watch) = (updates.clone(), disk_space.clone(), spool_watch.clone());

        // The disk stays last: a launch's first look leaves a disk reason out, as its figure may be
        // a check old, so a spool reason must never hide behind it.
        Arc::new(move || {
            node_unavailable(updates.holder(), spool_unavailable.clone().or_else(|| spool_watch.unavailable()).or_else(|| disk_space.unavailable()))
        })
    };

    // The spool's thread is looked at, never asked, so the check itself cannot wait on a hung disk.
    tasks.spawn({
        let stop = stop.clone();

        async move {
            let mut interval = every(SPOOL_CHECK);

            while stop.run_until_cancelled(interval.tick()).await.is_some() {
                spool_watch.check();
            }
        }
    });

    // Asked on its own too, so a change reaches the controller without a launch to ask.
    tasks.spawn({
        let (disk_space, stop) = (disk_space.clone(), stop.clone());

        async move {
            let mut interval = every(DISK_CHECK);

            while stop.run_until_cancelled(interval.tick()).await.is_some() {
                stop.run_until_cancelled(disk_space.refresh()).await;
            }
        }
    });

    updates.adopt_running().await?;

    // The container is the truth about what runs; agent.yml can name the image a rolled-back swap was heading for.
    let own = docker.inspect_agent_container().await?.ok_or(BootError::NoOwnContainer)?;

    log.info(format!("running as {} ({})", own.image, own.image_id));

    // The last swap's helper says how it went, before the first hello; one still proving is followed.
    let agent_update =
        AgentUpdateState::new(Arc::clone(&docker), agent_update_failure_path(&data_dir), own.image.clone(), log.child("update"), reporting("the agent update"));

    if let Err(error) = agent_update.load().await {
        log.warn(format!("could not read the last agent update: {error}"));
    }

    // The swap helper keeps the replaced container and its image until the new agent proves it stays
    // up, and holds the guard until then. So the sweep waits for the guard rather than skipping. An
    // inherited CS2 update sweeps itself before it frees the guard.
    match updates.holder().map(|holder| holder.operation) {
        Some(MaintenanceOperation::AgentUpdate) => {
            tasks.spawn({
                let (config, docker, updates, log, stop) = (config_reader.clone(), Arc::clone(&docker), updates.clone(), update_log.clone(), stop.clone());

                async move {
                    let mut running = updates.subscribe();

                    // The guard's borrow ends here, before the sweep awaits anything.
                    let freed = stop.run_until_cancelled(running.wait_for(|held| !held)).await.is_some_and(|waited| waited.is_ok());

                    // A sweep cut short frees the guard as its claim drops; the next boot sweeps again.
                    if freed {
                        stop.run_until_cancelled(prune_superseded_images(&config, docker.as_ref(), &updates, &log)).await;
                    }
                }
            });
        }
        Some(_) => {}
        None => prune_superseded_images(&config_reader, docker.as_ref(), &updates, &update_log).await,
    }

    // A fresh node extracts the addon overlay here instead of on its first launch, because the
    // controller sends no update to a node already on the current build.
    {
        let (overlay, image, log) = (Arc::clone(&overlay), config.read().cs2_image, log.clone());

        tokio::spawn(async move {
            match overlay.ensure(&image).await {
                Ok(_) => {}
                Err(OverlayError::NotOnHost { .. }) => log.info(format!("{image} is not on this host yet; the addon overlay is extracted once it is pulled")),
                Err(error) => log.warn(format!("addon overlay not prepared: {error}; the first launch extracts it")),
            }
        });
    }

    // What update_agent and update_cs2_image run with, for requests under the update log and for
    // the releases the registries list under the images log.
    let image_update_factories = |log: Logger| -> (AgentUpdateFactory, Cs2ImageUpdateFactory) {
        let agent: AgentUpdateFactory = {
            let (config, docker, data_dir, agent_update, updates, log) =
                (config.clone(), Arc::clone(&docker), data_dir.clone(), agent_update.clone(), updates.clone(), log.clone());

            Arc::new(move |schedule| AgentUpdateDeps {
                config: config.clone(),
                docker: Arc::clone(&docker),
                data_dir: data_dir.clone(),
                agent_update: Arc::new(agent_update.clone()),
                updates: updates.clone(),
                log: log.clone(),
                schedule,
            })
        };
        let cs2: Cs2ImageUpdateFactory = {
            let (config, docker, overlay, updates, state, slots) =
                (config.clone(), Arc::clone(&docker), Arc::clone(&overlay), updates.clone(), image_update_state.clone(), slots.clone());

            Arc::new(move |schedule| Cs2ImageUpdateDeps {
                config: config.clone(),
                docker: Arc::clone(&docker),
                overlay: Arc::clone(&overlay),
                updates: updates.clone(),
                log: log.clone(),
                state: Arc::new(state.clone()),
                schedule,
                held_images: {
                    let slots = slots.clone();

                    Arc::new(move || slots.held_images())
                },
            })
        };

        (agent, cs2)
    };
    let image_log = log.child("images");
    let (agent_update_deps, cs2_image_update_deps) = image_update_factories(image_log.clone());
    let images = ImageAutoUpdate::new(
        Arc::new(NodeImages {
            config: config_reader.clone(),
            docker: Arc::clone(&docker),
            registry: Arc::clone(&registry_fetch),
            own_image_id: own.image_id.clone(),
            data_dir: data_dir.clone(),
            agent_update: agent_update_deps,
            cs2_image_update: cs2_image_update_deps,
        }),
        registry_fetch,
        image_log,
        IMAGE_UPDATE_POLL_INTERVAL,
        stop.child_token(),
    );
    // An update may have named another image, or an image update may have waited for this one.
    on_guard_free(&tasks, &stop, updates.subscribe(), {
        let images = images.clone();

        move || {
            tokio::spawn(images.maintenance_ended());
        }
    });

    let (agent_update_deps, cs2_image_update_deps) = image_update_factories(update_log.clone());
    let handler = RequestHandlerDeps {
        config: config.clone(),
        slots: slots.clone(),
        plugins: Arc::new(relay.clone()),
        gslts: Arc::new(gslts.clone()),
        demos: Arc::new(demos.clone()),
        updates: updates.clone(),
        begin_cs2_update: {
            let install = Arc::clone(&install);

            Arc::new(move |mode| begin_cs2_update(install(mode, false)).boxed())
        },
        data_dir: data_dir.clone(),
        agent_update: agent_update_deps,
        cs2_image_update: cs2_image_update_deps,
        log: update_log.clone(),
        installed_build: {
            let build = build.clone();

            Arc::new(move || build.get())
        },
        public_build: Arc::new(cs2.clone()),
        poll_images: {
            let images = images.clone();

            Arc::new(move || {
                tokio::spawn(images.poll());
            })
        },
        unavailable: Arc::clone(&unavailable),
        check_disk: {
            let disk_space = disk_space.clone();

            Arc::new(move || {
                let disk_space = disk_space.clone();

                Box::pin(async move { disk_space.refresh().await })
            })
        },
        redial: {
            let (config, connection, log) = (config_reader.clone(), connection.clone(), log.clone());

            Arc::new(move || {
                let controller = config.read().controller;

                log.info(format!("the controller is now at {}; reconnecting there", controller_origin(&controller.host, controller.tls, OriginKind::Http)));
                warn_cleartext(&log, &controller.host, controller.tls);

                connection.redial();
            })
        },
    };

    let cs2_state: Cs2StateRead = {
        let (build, config, docker, cs2, update_state, image_update_state) =
            (build.clone(), config_reader.clone(), Arc::clone(&docker), cs2.clone(), update_state.clone(), image_update_state.clone());

        Arc::new(move || {
            let (build, image, docker, latest, update, image_update) =
                (build.get(), config.read().cs2_image, Arc::clone(&docker), cs2.latest(), update_state.get(), image_update_state.get());

            Box::pin(async move {
                let build_id = build.await;
                let image_id = docker.image_id(&image).await?;

                Ok(Cs2State { build_id, latest_build_id: latest, image, image_id, update, image_update })
            })
        })
    };
    let reporter = NodeReporter::new(NodeReporterDeps {
        cs2_state,
        unavailable: Arc::clone(&unavailable),
        agent_update: agent_update.clone(),
        config: config_reader.clone(),
        slots: slots.clone(),
        demos: demos.clone(),
        uploader: uploader.clone(),
        exits: exits.clone(),
        slot_reports: slot_reports.clone(),
        node_reports,
        backlog_reports: backlog_reports.clone(),
        demo_reports,
        report_demos: Arc::clone(&report_demos),
        resources: resources.clone(),
        node_log: node_log.clone(),
        connection: connection.clone(),
        agent: AgentInfo { version: crate::AGENT_VERSION.to_owned(), image: own.image.clone(), image_id: own.image_id.clone() },
        address,
        log: log.clone(),
        demo_log,
    });

    reporter.follow(&tasks, &stop, node_changed);

    let hooks = AgentHooks {
        config: config_reader.clone(),
        handler,
        log: log.child("ws"),
        reporter,
        on_failed: {
            let uploader = uploader.clone();

            Arc::new(move || uploader.close_upstream())
        },
        on_watch: {
            let (node_log, resources) = (node_log.clone(), resources.clone());

            Arc::new(move |watch| match watch {
                Watch::Logs(watch) => node_log.watch(&watch.source, watch.replay_id.as_deref()),
                Watch::Resources(watch) => resources.watch(watch.replay),
            })
        },
        on_check: {
            let cs2 = cs2.clone();

            Arc::new(move || {
                let cs2 = cs2.clone();

                tokio::spawn(async move { cs2.check().await });
            })
        },
    };
    driver.start(Arc::new(hooks));

    let controller_connection = connection.clone();

    cs2.start();
    images.start();

    Ok(RunningAgent { log: log.clone(), stop, tasks, controller_connection, events, done: tokio::sync::OnceCell::new() })
}

/// The first tick is a whole period from now, because boot has done the work once already.
fn every(period: Duration) -> tokio::time::Interval {
    tokio::time::interval_at(tokio::time::Instant::now() + period, period)
}

fn on_guard_free(tasks: &TaskTracker, stop: &CancellationToken, running: tokio::sync::watch::Receiver<bool>, ended: impl Fn() + Send + 'static) {
    follow_guard(tasks, stop, running, move |held| {
        if !held {
            ended();
        }
    });
}

/// Sees only changes after `running` was subscribed. Changes that come together arrive as the latest one.
fn follow_guard(tasks: &TaskTracker, stop: &CancellationToken, mut running: tokio::sync::watch::Receiver<bool>, changed: impl Fn(bool) + Send + 'static) {
    let stop = stop.clone();

    tasks.spawn(async move {
        while let Some(Ok(())) = stop.run_until_cancelled(running.changed()).await {
            let held = *running.borrow_and_update();

            changed(held);
        }
    });
}

/// Tells `reports` of each demo the target takes, and of each such demo the uploader then could not
/// remove, which decide the reason its removal goes out with.
fn report_uploads(options: &mut DemoUploaderOptions, reports: &DemoReporter) {
    let (uploaded, kept) = (reports.clone(), reports.clone());

    options.on_uploaded = Some(Arc::new(move |slot_id, file_name| uploaded.uploaded(slot_id, file_name)));
    options.on_not_removed = Some(Arc::new(move |slot_id, file_name| kept.not_removed(slot_id, file_name)));
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[tokio::test]
    async fn the_uploaders_hooks_reach_the_demo_reporter() {
        let sent: Arc<Mutex<Vec<AgentToControllerMessage>>> = Arc::default();
        let record = Arc::clone(&sent);
        let reports = DemoReporter::new(Arc::new(move |frame| {
            record.lock().unwrap().push(frame);

            async { true }.boxed()
        }));
        let nothing: ListDemos = Arc::new(|| async { Ok(Vec::new()) }.boxed());
        let mut options = DemoUploaderOptions::new(std::env::temp_dir(), Arc::new(|| None), Arc::new(|_| None), Arc::new(|| false));
        let demo = |file_name: &str| ankor_protocol::types::DemoFile {
            slot_id: "a".to_owned(),
            file_name: file_name.to_owned(),
            size_bytes: 1,
            modified_at: 1,
            recording: false,
            uploading: false,
            upload_error: None,
        };

        report_uploads(&mut options, &reports);
        reports.helloed(&[demo("taken.dem"), demo("kept.dem")]);

        let (on_uploaded, on_not_removed) = (options.on_uploaded.unwrap(), options.on_not_removed.unwrap());

        on_uploaded("a", "taken.dem");
        on_uploaded("a", "kept.dem");
        on_not_removed("a", "kept.dem");
        reports.report(&nothing).await.unwrap();

        let reasons: Vec<_> = sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|frame| match frame {
                AgentToControllerMessage::DemoRemoved(removed) => Some((removed.file_name.clone(), removed.reason)),
                _ => None,
            })
            .collect();

        assert_eq!(
            reasons,
            [
                ("taken.dem".to_owned(), ankor_protocol::types::DemoRemovedReason::Uploaded),
                ("kept.dem".to_owned(), ankor_protocol::types::DemoRemovedReason::Deleted)
            ]
        );
    }

    fn recorded() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&lines);

        (Logger::new("agent", Some(Arc::new(move |entry: crate::logger::Entry| seen.lock().unwrap().push(entry.message)))), lines)
    }

    #[test]
    fn warns_of_a_cleartext_controller_address_only_off_this_host_and_without_tls() {
        let warned = |host: &str, tls: bool| {
            let (log, lines) = recorded();

            warn_cleartext(&log, host, tls);
            lines.lock().unwrap().clone()
        };

        assert_eq!(
            warned("controller.example.com", false),
            [
                "[agent] controller.host controller.example.com is not a local address and controller.tls is off: the node token crosses the network in the clear"
            ]
        );
        assert!(warned("controller.example.com", true).is_empty());
        assert!(warned("127.0.0.1", false).is_empty());
    }

    #[tokio::test]
    async fn the_socket_transport_is_connected_only_once_the_controller_has_welcomed_a_socket() {
        let (connection, _driver) = ControllerConnection::new("token".to_owned(), Timings::default(), Logger::null());
        let transport = SocketTransport { connection: connection.clone(), push: push_on(&connection) };

        assert!(!transport.connected());
    }

    #[tokio::test(start_paused = true)]
    async fn ticks_first_a_whole_period_from_now_and_then_every_period() {
        let start = tokio::time::Instant::now();
        let mut ticks = every(Duration::from_secs(30));

        assert_eq!(ticks.tick().await - start, Duration::from_secs(30));
        assert_eq!(ticks.tick().await - start, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn runs_its_hook_each_time_the_guard_turns_free_and_never_when_it_is_taken() {
        let (held, running) = tokio::sync::watch::channel(false);
        let (tasks, stop) = (TaskTracker::new(), CancellationToken::new());
        let freed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&freed);
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        };

        on_guard_free(&tasks, &stop, running, move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        held.send_replace(true);
        settle().await;
        assert_eq!(freed.load(std::sync::atomic::Ordering::SeqCst), 0);
        held.send_replace(false);
        settle().await;
        assert_eq!(freed.load(std::sync::atomic::Ordering::SeqCst), 1);
        stop.cancel();
    }
}
