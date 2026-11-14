//! The agent process: the agent, or the self-update helper when started as `ankor-self-update`.
//! Either runs only after the process has given up root.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ankor_agent::agent::{AgentHost, run_agent};
use ankor_agent::contract::constants::SELF_UPDATE_COMMAND;
use ankor_agent::controller_connection::Signal;
use ankor_agent::cs2_auto_update::HttpSteam;
use ankor_agent::docker::DockerService;
use ankor_agent::docker_api::DockerApi;
use ankor_agent::host_layout::{AGENT_CONFIG_PATH, CS2_INSTALL_MOUNT, DATA_DIR};
use ankor_agent::logger::{Entry, Logger, console_level_from_env, flush_console};
use ankor_agent::node_log::{NodeLog, wall_clock};
use ankor_agent::privileges::{ProcessCalls, drop_privileges};
use ankor_agent::public_ip::{DEFAULT_TIMEOUT, HttpsIpv4Resolver, PUBLIC_IP_URL};
use ankor_agent::registry_tags::HttpRegistry;
use ankor_agent::self_update_helper::start_self_update;
use ankor_agent::steam_logon::log_on_game_server;
use ankor_protocol::constants::EVENT_RELAY_PORT;
use tokio::signal::unix::{SignalKind, signal};

/// How long a token logon may take before it counts as unanswered.
const STEAM_LOGON_TIMEOUT: Duration = Duration::from_secs(15);

/// The relay's port as a socket takes it. The contract's port is a `u64`; this fails the build if it
/// ever stops fitting, instead of falling back to a copy of the number.
#[allow(clippy::cast_possible_truncation, reason = "the assertion above the cast proves it fits")]
const RELAY_PORT: u16 = {
    assert!(EVENT_RELAY_PORT <= u16::MAX as u64, "EVENT_RELAY_PORT does not fit a port");
    EVENT_RELAY_PORT as u16
};

fn main() -> std::process::ExitCode {
    // Handled before anything reads the host. `node-setup.sh` starts the image with `--version` to
    // prove a container with the agent's flags starts, with no socket, data directory or network.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{}", ankor_agent::AGENT_VERSION);
        return std::process::ExitCode::SUCCESS;
    }

    console_level_from_env();
    // Every return from here writes the queued console lines first, and so does a panic before it
    // aborts the process.
    let _flush = FlushConsoleOnExit;
    let previous_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        flush_console(CONSOLE_PANIC_WAIT);
        previous_hook(info);
    }));

    let dropped = drop_privileges(&mut ProcessCalls, &Logger::new("agent:entrypoint", None));

    // The direct writes below, and the self-update helper's, follow the entrypoint's queued lines.
    flush_console(CONSOLE_EXIT_WAIT);

    if let Err(error) = dropped {
        eprintln!("{error}");
        return std::process::ExitCode::FAILURE;
    }

    // One thread is enough, because the agent mostly waits on the network and Docker.
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start the runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let code = if std::env::args().nth(1).as_deref() == SELF_UPDATE_COMMAND.first().copied() {
        std::process::ExitCode::from(u8::try_from(runtime.block_on(start_self_update())).unwrap_or(1))
    } else {
        runtime.block_on(start_agent())
    };

    // Exits once the agent has shut down, rather than wait on blocking work such as a DNS lookup.
    runtime.shutdown_timeout(RUNTIME_EXIT_WAIT);
    code
}

/// Waits for the console thread to write the queued lines when `main` returns, as the shutdown's
/// are usually still queued.
struct FlushConsoleOnExit;

impl Drop for FlushConsoleOnExit {
    fn drop(&mut self) {
        flush_console(CONSOLE_EXIT_WAIT);
    }
}

/// How long the process waits for blocking work still running at exit.
const RUNTIME_EXIT_WAIT: Duration = Duration::from_secs(1);

/// How long the process waits for the console to take its last lines.
const CONSOLE_EXIT_WAIT: Duration = Duration::from_secs(2);

/// How long a panic waits for the console before its message, which goes to stderr directly.
const CONSOLE_PANIC_WAIT: Duration = Duration::from_millis(200);

async fn start_agent() -> std::process::ExitCode {
    let stop = tokio_util::sync::CancellationToken::new();
    // Captures the startup too, before the agent connects.
    let node_log = NodeLog::new(wall_clock(), stop.child_token());
    let sink = node_log.clone();
    let log = Logger::new("agent", Some(Arc::new(move |entry: Entry| sink.agent_line(&entry))));
    // Listened for before boot. As PID 1 without an init, the agent ignores a signal it has no
    // handler for, so a stop during boot would wait for Docker's kill.
    let mut signals = match Signals::new() {
        Ok(signals) => signals,
        Err(error) => {
            log.error(format!("could not listen for signals: {error}"));
            return std::process::ExitCode::FAILURE;
        }
    };
    let host = AgentHost {
        stop: stop.clone(),
        config_path: PathBuf::from(AGENT_CONFIG_PATH),
        data_dir: PathBuf::from(DATA_DIR),
        cs2_install: PathBuf::from(CS2_INSTALL_MOUNT),
        docker: Arc::new(DockerService::new(DockerApi::default(), log.child("docker"))),
        public_ip: Arc::new(HttpsIpv4Resolver::new(PUBLIC_IP_URL, DEFAULT_TIMEOUT)),
        relay_port: RELAY_PORT,
        steam_logon: Arc::new(|token: String| Box::pin(async move { log_on_game_server(&token, None, STEAM_LOGON_TIMEOUT).await })),
        steam_fetch: Arc::new(HttpSteam::new()),
        registry_fetch: Arc::new(HttpRegistry::new()),
        node_log,
        log: log.clone(),
    };
    // A signal during boot ends it where it is. The game servers keep running, and the next boot
    // adopts them as it would after a crash.
    let booted = tokio::select! {
        booted = run_agent(host) => booted,
        signal = signals.next() => {
            log.info(format!("{}: stopping during boot (game servers keep running and are re-adopted on boot)", signal.name()));
            stop.cancel();
            return std::process::ExitCode::SUCCESS;
        }
    };
    let agent = match booted {
        Ok(agent) => agent,
        Err(error) => {
            log.error(error.to_string());
            return std::process::ExitCode::FAILURE;
        }
    };
    let signal = signals.next().await;

    match agent.shutdown(signal).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            log.error(format!("event spool shutdown failed: {error}"));
            std::process::ExitCode::FAILURE
        }
    }
}

/// SIGINT, SIGTERM and SIGQUIT, each handled from the moment this is made.
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
}

impl Signals {
    fn new() -> std::io::Result<Self> {
        Ok(Self { interrupt: signal(SignalKind::interrupt())?, terminate: signal(SignalKind::terminate())?, quit: signal(SignalKind::quit())? })
    }

    async fn next(&mut self) -> Signal {
        tokio::select! {
            _ = self.interrupt.recv() => Signal::Sigint,
            _ = self.terminate.recv() => Signal::Sigterm,
            _ = self.quit.recv() => Signal::Sigquit,
        }
    }
}
