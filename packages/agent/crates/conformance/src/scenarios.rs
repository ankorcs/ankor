//! The scenarios, each run the same way against every agent.

use ankor_protocol::helpers::wire_name;
use ankor_protocol::types::{
    AgentToControllerMessage, ControllerHelloRefused, ControllerToAgentMessage, Cs2UpdateOutcome, HelloRefusedReason, MatchPhase, Side,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::scenario::{
    ALLOCATIONS, Ctx, GOOD_GSLT, NodeConfig, OTHER_GSLT, REFUSED_GSLT, Slot, TEAM1_PLAYER, TEAM2_PLAYER, agent_message, demo_url, match_launch, plain_launch,
    slot_runs, succeeded,
};
use crate::transcript::{channels, controller_entry};

/// A scenario: its name, what it checks, and the node it starts from.
pub struct Scenario {
    pub name: &'static str,
    pub about: &'static str,
    pub node: fn() -> NodeConfig,
}

/// Every scenario, in the order the runner takes them.
pub const ALL: &[Scenario] = &[
    Scenario { name: "boot", about: "boot, hello and welcome, the first reports, shutdown", node: NodeConfig::default },
    Scenario { name: "launch", about: "start_server, plugin ready, events uploaded, stop_server, server_exited", node: NodeConfig::default },
    Scenario { name: "adoption", about: "an agent restart while a match runs: the new agent adopts it and its plugin reconnects", node: NodeConfig::default },
    Scenario { name: "event_replay", about: "events a plugin sent while the agent was down are replayed to the next agent", node: NodeConfig::default },
    Scenario { name: "set_config", about: "set_config adds a slot, changes a token, pins and unpins the address", node: NodeConfig::default },
    Scenario { name: "refusals", about: "no free slot, unknown allocation, allocation in use and reused", node: NodeConfig::default },
    Scenario { name: "demo", about: "a recorded demo is uploaded once its server stops", node: demo_node },
    Scenario {
        name: "reconnect",
        about: "the controller drops the socket; a server exits meanwhile; the agent redials and replays",
        node: NodeConfig::default,
    },
    Scenario { name: "gslt_refused", about: "Steam refuses one slot's token; launches skip that slot", node: refused_node },
    Scenario {
        name: "cs2_update",
        about: "update_cs2 holds the maintenance guard against launches and a second update, then installs a newer build",
        node: NodeConfig::default,
    },
    Scenario { name: "cs2_update_failed", about: "steamcmd fails an update; the guard is freed and launches run again", node: NodeConfig::default },
    Scenario {
        name: "update_adoption",
        about: "an agent restart during an update: the new agent adopts the update container and reports its end",
        node: NodeConfig::default,
    },
    Scenario { name: "crash", about: "a game server exits with an error mid-match; the slot is free again", node: NodeConfig::default },
    Scenario {
        name: "commands",
        about: "console_command, start_match refused then accepted, a match's phase and score reach the controller",
        node: NodeConfig::default,
    },
    Scenario { name: "reservation", about: "reserve_slot keeps launches off a slot until a new token for it arrives", node: NodeConfig::default },
    Scenario { name: "idle_shutdown", about: "an empty server stops itself once its launch's idle limit passes", node: NodeConfig::default },
    Scenario {
        name: "event_backlog",
        about: "the controller refuses event batches; the backlog is reported, then delivered and cleared",
        node: NodeConfig::default,
    },
    Scenario { name: "demo_retry", about: "a demo upload the controller fails is retried and lands", node: demo_node },
    Scenario { name: "demo_delete", about: "delete_demo removes a demo that waits for its upload", node: demo_node },
    Scenario { name: "hello_refused", about: "the controller refuses the hello; the agent closes and does not dial again", node: NodeConfig::default },
    Scenario {
        name: "controller_misbehaves",
        about: "a controller integration that sends duplicate launches and stops, stops a server before it is ready, and retries a finished allocation",
        node: NodeConfig::default,
    },
];

/// The build the fake update installs, newer than the one the node starts with.
const NEWER_BUILD: &str = "20260102";

fn demo_node() -> NodeConfig {
    NodeConfig { demo_upload: true, ..NodeConfig::default() }
}

fn refused_node() -> NodeConfig {
    NodeConfig {
        slots: vec![Slot { id: "s1", port: 27015, tv_port: 27020, gslt: REFUSED_GSLT }, Slot { id: "s2", port: 27025, tv_port: 27030, gslt: GOOD_GSLT }],
        ..NodeConfig::default()
    }
}

/// Runs the scenario called `name`.
pub async fn run(name: &str, ctx: &mut Ctx<'_>) -> Result<()> {
    match name {
        "boot" => boot(ctx).await,
        "launch" => launch(ctx).await,
        "adoption" => adoption(ctx).await,
        "event_replay" => event_replay(ctx).await,
        "set_config" => set_config(ctx).await,
        "refusals" => refusals(ctx).await,
        "demo" => demo(ctx).await,
        "reconnect" => reconnect(ctx).await,
        "gslt_refused" => gslt_refused(ctx).await,
        "cs2_update" => cs2_update(ctx).await,
        "cs2_update_failed" => cs2_update_failed(ctx).await,
        "update_adoption" => update_adoption(ctx).await,
        "crash" => crash(ctx).await,
        "commands" => commands(ctx).await,
        "reservation" => reservation(ctx).await,
        "idle_shutdown" => idle_shutdown(ctx).await,
        "event_backlog" => event_backlog(ctx).await,
        "demo_retry" => demo_retry(ctx).await,
        "demo_delete" => demo_delete(ctx).await,
        "hello_refused" => hello_refused(ctx).await,
        "controller_misbehaves" => controller_misbehaves(ctx).await,
        other => anyhow::bail!("no scenario called {other}"),
    }
}

fn stop(allocation: &str) -> Value {
    json!({ "type": "stop_server", "allocation_id": allocation })
}

/// Launches `launch` as `allocation` and waits until its slot runs.
async fn launch_running(ctx: &mut Ctx<'_>, allocation: &str, launch: Value) -> Result<()> {
    Ctx::expect_ok(&ctx.launch(allocation, launch).await?)?;
    ctx.ready(allocation).await
}

/// Stops the server of `allocation` and waits until its exit is reported. Returns the mark from
/// before the stop, for what the stop sets off.
async fn stop_server(ctx: &mut Ctx<'_>, allocation: &str) -> Result<usize> {
    let mark = ctx.mark();
    Ctx::expect_ok(&ctx.request(stop(allocation)).await?)?;
    wait_exit(ctx, mark, allocation).await?;
    Ok(mark)
}

async fn wait_exit(ctx: &Ctx<'_>, mark: usize, allocation: &str) -> Result<()> {
    ctx.wait_agent(mark, |frame| match agent_message(frame) {
        Some(AgentToControllerMessage::ServerExited(exited)) => exited.allocation_id == allocation,
        Some(AgentToControllerMessage::Hello(hello)) => hello.server_exits.iter().any(|exit| exit.allocation_id == allocation),
        _ => false,
    })
    .await
    .context("the server's exit was never reported")?;
    ctx.step(&format!("  server_exited reported for {}", allocation.chars().take(8).collect::<String>()));
    ctx.settle().await;
    Ok(())
}

async fn boot(ctx: &mut Ctx<'_>) -> Result<()> {
    ctx.boot().await?;
    ctx.snapshot("booted").await?;
    ctx.stop_agent().await?;
    ctx.snapshot("stopped").await
}

async fn launch(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    let reply = ctx.launch(allocation, match_launch(true, false)).await?;
    Ctx::expect_ok(&reply)?;
    ctx.snapshot("launched").await?;
    ctx.ready(allocation).await?;
    // map_start is event 0, sent as the map loaded. The players join in warmup, which sends no events.
    ctx.join(allocation, TEAM1_PLAYER, "alpha", Side::Ct).await?;
    ctx.join(allocation, TEAM2_PLAYER, "bravo", Side::T).await?;
    ctx.request(json!({ "type": "get_server_status", "allocation_id": allocation })).await?;
    stop_server(ctx, allocation).await?;
    ctx.snapshot("stopped").await?;
    ctx.stop_agent().await
}

async fn adoption(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, match_launch(false, false)).await?;
    // The plugin dials the new agent only when told, so the hello finds it offline every run.
    ctx.plugin(allocation, json!({ "op": "hold" }))?;
    ctx.stop_agent().await?;
    ctx.snapshot("agent_stopped").await?;
    let mark = ctx.mark();
    ctx.boot().await?;
    ctx.plugin(allocation, json!({ "op": "connect" }))?;
    ctx.wait_agent(mark, |frame| slot_runs(frame, allocation)).await.context("the adopted server never ran again")?;
    ctx.settle().await;
    ctx.snapshot("adopted").await?;
    // The controller reads the match's phase from the plugin, as no phase_change is replayed.
    Ctx::expect_ok(&ctx.request(json!({ "type": "get_server_status", "allocation_id": allocation })).await?)?;
    stop_server(ctx, allocation).await?;
    ctx.stop_agent().await
}

async fn event_replay(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, match_launch(true, false)).await?;
    go_live(ctx, allocation).await?;
    ctx.plugin(allocation, json!({ "op": "hold" }))?;
    ctx.stop_agent().await?;
    // The match plays on without its agent; the plugin keeps the round's events until one acknowledges them.
    let mark = ctx.mark();
    ctx.plugin(allocation, json!({ "op": "round_end", "winner": Side::Ct, "reason": 8 }))?;
    ctx.settle().await;
    ctx.boot().await?;
    // The plugin dials again once let go, after its backoff, and sends what was not acknowledged.
    let connected = ctx.mark();
    ctx.plugin(allocation, json!({ "op": "connect" }))?;
    ctx.wait_on(connected, &channels::plugin(allocation), |report| report["relay"] == "connected").await.context("the plugin never dialled the new agent")?;
    ctx.settle().await;
    ctx.events_delivered(allocation, mark).await.context("the replayed events never arrived")?;
    stop_server(ctx, allocation).await?;
    ctx.stop_agent().await
}

async fn set_config(ctx: &mut Ctx<'_>) -> Result<()> {
    ctx.boot().await?;
    let reply = ctx
        .request(json!({ "type": "set_config", "config": {
            "slots": [
                { "id": "s1", "port": 27015, "tv_port": 27020 },
                { "id": "s2", "port": 27025, "tv_port": 27030, "gslt": OTHER_GSLT },
            ],
            "pinned_ip": "198.51.100.7",
        } }))
        .await?;
    Ctx::expect_ok(&reply)?;
    ctx.snapshot("slot_added").await?;
    let reply = ctx
        .request(json!({ "type": "set_config", "config": { "slots": [
        { "id": "s1", "port": 27015, "tv_port": 27020, "gslt": "GOODTOKEN000000000000000000000003" },
        { "id": "s2", "port": 27025, "tv_port": 27030 },
    ] } }))
        .await?;
    Ctx::expect_ok(&reply)?;
    ctx.snapshot("token_changed").await?;
    let reply = ctx.request(json!({ "type": "set_config", "config": { "pinned_ip": null, "auto_update": false, "demo_upload": { "url": demo_url(), "header": null, "limit_mbit": 50 } } })).await?;
    Ctx::expect_ok(&reply)?;
    // A slot layout that reuses a port is refused.
    ctx.request(json!({ "type": "set_config", "config": { "slots": [
        { "id": "s1", "port": 27015, "tv_port": 27020 },
        { "id": "s2", "port": 27015, "tv_port": 27030 },
    ] } }))
    .await?;
    ctx.snapshot("unpinned").await?;
    ctx.stop_agent().await
}

async fn refusals(ctx: &mut Ctx<'_>) -> Result<()> {
    let [first, second, third] = [ALLOCATIONS[0], ALLOCATIONS[1], ALLOCATIONS[2]];
    ctx.boot().await?;
    launch_running(ctx, first, plain_launch()).await?;
    // The only slot is taken.
    ctx.request(json!({ "type": "start_server", "allocation_id": second, "launch": plain_launch() })).await?;
    // Nothing runs as this allocation.
    ctx.request(stop(third)).await?;
    ctx.request(json!({ "type": "get_server_status", "allocation_id": third })).await?;
    ctx.request(json!({ "type": "console_command", "allocation_id": third, "command": "status" })).await?;
    // A server launched without a match has none to start.
    ctx.request(json!({ "type": "start_match", "allocation_id": first })).await?;
    // The running allocation again.
    ctx.request(json!({ "type": "start_server", "allocation_id": first, "launch": plain_launch() })).await?;
    stop_server(ctx, first).await?;
    // An allocation that ran before cannot run again.
    ctx.request(json!({ "type": "start_server", "allocation_id": first, "launch": plain_launch() })).await?;
    // A reservation of a slot the node does not have.
    ctx.request(json!({ "type": "reserve_slot", "slot_id": "nope" })).await?;
    ctx.stop_agent().await
}

async fn demo(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, match_launch(false, true)).await?;
    go_live(ctx, allocation).await?;
    recording(ctx, allocation).await?;
    // The upload starts as the slot turns idle, alongside the stop's reply; its answer waits
    // until that reply is in, so the two arrive in the same order for every agent.
    ctx.controller.demo_gate().close();
    let mark = stop_server(ctx, allocation).await?;
    ctx.controller.demo_gate().open();
    ctx.wait_on(mark, channels::DEMOS, |_| true).await.context("the demo was never uploaded")?;
    ctx.wait_agent(mark, |frame| matches!(agent_message(frame), Some(AgentToControllerMessage::DemoRemoved(_)))).await?;
    ctx.settle().await;
    ctx.snapshot("uploaded").await?;
    ctx.stop_agent().await
}

async fn reconnect(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, plain_launch()).await?;
    ctx.controller.set_refuse(true);
    ctx.controller.drop_agent()?;
    ctx.settle().await;
    // The server ends while the controller cannot hear about it.
    let mark = ctx.mark();
    ctx.plugin(allocation, json!({ "op": "exit", "code": 0 }))?;
    ctx.wait_on(mark, channels::DOCKER, |event| event["action"] == "die").await?;
    ctx.settle().await;
    let mark = ctx.mark();
    ctx.controller.set_refuse(false);
    ctx.wait_agent(mark, |frame| matches!(agent_message(frame), Some(AgentToControllerMessage::Hello(_)))).await.context("the agent never came back")?;
    wait_exit(ctx, mark, allocation).await?;
    ctx.stop_agent().await
}

async fn gslt_refused(ctx: &mut Ctx<'_>) -> Result<()> {
    let [first, second] = [ALLOCATIONS[0], ALLOCATIONS[1]];
    ctx.boot().await?;
    ctx.snapshot("refused").await?;
    launch_running(ctx, first, plain_launch()).await?;
    // The other slot's token was refused, so it takes no game.
    ctx.request(json!({ "type": "start_server", "allocation_id": second, "launch": plain_launch() })).await?;
    stop_server(ctx, first).await?;
    ctx.stop_agent().await?;
    // A restart does not log the refused token on again.
    ctx.boot().await?;
    ctx.stop_agent().await
}

fn update_cs2() -> Value {
    json!({ "type": "update_cs2", "mode": "update" })
}

/// Waits until the node reports the update ended as `outcome`.
async fn update_ended(ctx: &Ctx<'_>, mark: usize, outcome: Cs2UpdateOutcome) -> Result<()> {
    let name = wire_name(outcome);
    ctx.wait_agent(mark, |frame| {
        matches!(agent_message(frame), Some(AgentToControllerMessage::NodeStatus(status)) if status.cs2.update.as_ref().and_then(|update| update.outcome) == Some(outcome))
    })
    .await
    .with_context(|| format!("the update never ended as {name}"))?;
    ctx.step(&format!("  the update ended as {name}"));
    ctx.settle().await;
    Ok(())
}

/// Launches, readies and stops a server, as the node does once it takes games again.
async fn play(ctx: &mut Ctx<'_>, allocation: &str) -> Result<()> {
    launch_running(ctx, allocation, plain_launch()).await?;
    stop_server(ctx, allocation).await?;
    Ok(())
}

async fn cs2_update(ctx: &mut Ctx<'_>) -> Result<()> {
    ctx.boot().await?;
    Ctx::expect_ok(&ctx.request(update_cs2()).await?)?;
    // The update holds the guard: no launch, and no second update.
    ctx.request(json!({ "type": "start_server", "allocation_id": ALLOCATIONS[0], "launch": plain_launch() })).await?;
    ctx.request(update_cs2()).await?;
    let mark = ctx.mark();
    ctx.finish_update(json!({ "op": "install", "build": NEWER_BUILD })).await?;
    update_ended(ctx, mark, Cs2UpdateOutcome::Completed).await?;
    ctx.snapshot("updated").await?;
    play(ctx, ALLOCATIONS[1]).await?;
    ctx.stop_agent().await
}

async fn cs2_update_failed(ctx: &mut Ctx<'_>) -> Result<()> {
    ctx.boot().await?;
    Ctx::expect_ok(&ctx.request(update_cs2()).await?)?;
    let mark = ctx.mark();
    // steamcmd's exit 8; the entrypoint dies with 1 after its three attempts.
    ctx.finish_update(json!({ "op": "fail", "code": 8 })).await?;
    update_ended(ctx, mark, Cs2UpdateOutcome::Failed).await?;
    play(ctx, ALLOCATIONS[0]).await?;
    ctx.stop_agent().await
}

async fn update_adoption(ctx: &mut Ctx<'_>) -> Result<()> {
    ctx.boot().await?;
    Ctx::expect_ok(&ctx.request(update_cs2()).await?)?;
    ctx.stop_agent().await?;
    ctx.snapshot("agent_stopped").await?;
    ctx.boot().await?;
    // The update an earlier agent started still holds the guard.
    ctx.request(json!({ "type": "start_server", "allocation_id": ALLOCATIONS[0], "launch": plain_launch() })).await?;
    let mark = ctx.mark();
    ctx.finish_update(json!({ "op": "install", "build": NEWER_BUILD })).await?;
    update_ended(ctx, mark, Cs2UpdateOutcome::Completed).await?;
    play(ctx, ALLOCATIONS[1]).await?;
    ctx.stop_agent().await
}

async fn crash(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, plain_launch()).await?;
    let mark = ctx.mark();
    // A CS2 that segfaults, as Docker reports one: 128 + SIGSEGV.
    ctx.plugin(allocation, json!({ "op": "exit", "code": 139 }))?;
    wait_exit(ctx, mark, allocation).await?;
    play(ctx, ALLOCATIONS[1]).await?;
    ctx.stop_agent().await
}

async fn commands(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, match_launch(false, false)).await?;
    Ctx::expect_ok(&ctx.request(json!({ "type": "console_command", "allocation_id": allocation, "command": "status" })).await?)?;
    // Nobody has joined, so the match cannot start; once both teams are in, it can.
    ctx.request(json!({ "type": "start_match", "allocation_id": allocation })).await?;
    go_live(ctx, allocation).await?;
    // The first live round: team 1, on CT, wins it by eliminating the other side.
    ctx.plugin(allocation, json!({ "op": "round_end", "winner": Side::Ct, "reason": 8 }))?;
    ctx.settle().await;
    Ctx::expect_ok(&ctx.request(json!({ "type": "get_server_status", "allocation_id": allocation })).await?)?;
    stop_server(ctx, allocation).await?;
    ctx.stop_agent().await
}

async fn reservation(ctx: &mut Ctx<'_>) -> Result<()> {
    ctx.boot().await?;
    Ctx::expect_ok(&ctx.request(json!({ "type": "reserve_slot", "slot_id": "s1" })).await?)?;
    // The only slot is reserved for its next token.
    ctx.request(json!({ "type": "start_server", "allocation_id": ALLOCATIONS[0], "launch": plain_launch() })).await?;
    Ctx::expect_ok(
        &ctx.request(json!({ "type": "set_config", "config": { "slots": [{ "id": "s1", "port": 27015, "tv_port": 27020, "gslt": OTHER_GSLT }] } })).await?,
    )?;
    play(ctx, ALLOCATIONS[1]).await?;
    ctx.stop_agent().await
}

async fn idle_shutdown(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    let mut launch = plain_launch();
    launch["idle_shutdown_seconds"] = json!(1);
    ctx.boot().await?;
    Ctx::expect_ok(&ctx.launch(allocation, launch).await?)?;
    let mark = ctx.mark();
    ctx.ready(allocation).await?;
    // Nobody joins; the agent's game check finds it empty past its limit.
    wait_exit(ctx, mark, allocation).await?;
    ctx.stop_agent().await
}

async fn event_backlog(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, match_launch(true, false)).await?;
    go_live(ctx, allocation).await?;
    ctx.controller.answer_events([(503, json!({ "error": "busy" })), (503, json!({ "error": "busy" }))]);
    let mark = ctx.mark();
    ctx.plugin(allocation, json!({ "op": "round_end", "winner": Side::Ct, "reason": 8 }))?;
    ctx.wait_agent(mark, |frame| matches!(agent_message(frame), Some(AgentToControllerMessage::EventBacklog(backlog)) if backlog.event_count > 0))
        .await
        .context("the backlog was never reported")?;
    ctx.wait_agent(mark, |frame| matches!(agent_message(frame), Some(AgentToControllerMessage::EventBacklog(backlog)) if backlog.event_count == 0))
        .await
        .context("the backlog never cleared")?;
    ctx.settle().await;
    stop_server(ctx, allocation).await?;
    ctx.stop_agent().await
}

/// Has both rostered players join and the controller start the match. Its countdown runs on the
/// engine's clock, then the engine restarts into the first live round, as a match that records a
/// demo must before it records one.
async fn go_live(ctx: &mut Ctx<'_>, allocation: &str) -> Result<()> {
    ctx.join(allocation, TEAM1_PLAYER, "alpha", Side::Ct).await?;
    ctx.join(allocation, TEAM2_PLAYER, "bravo", Side::T).await?;
    Ctx::expect_ok(&ctx.request(json!({ "type": "start_match", "allocation_id": allocation })).await?)?;
    let mark = ctx.mark();
    ctx.plugin(allocation, json!({ "op": "advance", "seconds": COUNTDOWN_SECONDS }))?;
    ctx.plugin(allocation, json!({ "op": "round_start" }))?;
    ctx.wait_agent(mark, |frame| frame["type"] == "phase_change" && frame["phase"] == wire_name(MatchPhase::Live))
        .await
        .context("the match never went live")?;
    ctx.settle().await;
    Ok(())
}

/// The plugin's countdown between `start_match` and the match, and a little more.
const COUNTDOWN_SECONDS: u64 = 16;

/// The demo the server of `allocation` records, from when the plugin started it as the match went
/// live: its file name.
async fn recording(ctx: &Ctx<'_>, allocation: &str) -> Result<String> {
    let report = ctx.wait_on(0, &channels::plugin(allocation), |report| report.get("recording").is_some()).await.context("the demo never started")?;
    let path = report["recording"].as_str().unwrap_or_default();
    Ok(path.rsplit('/').next().unwrap_or_default().to_owned())
}

async fn demo_retry(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, match_launch(false, true)).await?;
    go_live(ctx, allocation).await?;
    recording(ctx, allocation).await?;
    ctx.controller.answer_demos([500]);
    let mark = stop_server(ctx, allocation).await?;
    // The first upload fails; the retry after the backoff lands.
    ctx.wait_agent(mark, |frame| matches!(agent_message(frame), Some(AgentToControllerMessage::DemoRemoved(_))))
        .await
        .context("the retried demo was never uploaded")?;
    ctx.settle().await;
    ctx.stop_agent().await
}

async fn demo_delete(ctx: &mut Ctx<'_>) -> Result<()> {
    let allocation = ALLOCATIONS[0];
    ctx.boot().await?;
    launch_running(ctx, allocation, match_launch(false, true)).await?;
    go_live(ctx, allocation).await?;
    let demo = recording(ctx, allocation).await?;
    // Every upload fails, so the demo waits on the node for its retry.
    ctx.controller.answer_demos([500, 500, 500]);
    let mark = stop_server(ctx, allocation).await?;
    ctx.wait_on(mark, channels::DEMOS, |_| true).await.context("the demo was never tried")?;
    ctx.settle().await;
    Ctx::expect_ok(&ctx.request(json!({ "type": "delete_demo", "slot_id": "s1", "file_name": demo })).await?)?;
    ctx.snapshot("deleted").await?;
    ctx.stop_agent().await
}

async fn hello_refused(ctx: &mut Ctx<'_>) -> Result<()> {
    ctx.controller.set_auto_welcome(false);
    ctx.boot().await?;
    let mark = ctx.mark();
    ctx.controller.send(json!(ControllerToAgentMessage::HelloRefused(ControllerHelloRefused {
        reason: HelloRefusedReason::InvalidHello,
        detail: "this node's hello does not match the contract".to_owned(),
    })))?;
    ctx.wait_on(mark, channels::CONTROLLER, |entry| entry.get(controller_entry::AGENT_CLOSED).is_some() || entry.get(controller_entry::AGENT_GONE).is_some())
        .await
        .context("the agent never closed")?;
    // The agent's first reconnect comes after about a second, so a second hello would show here.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    ctx.settle().await;
    ctx.stop_agent().await
}

fn start(allocation: &str) -> Value {
    json!({ "type": "start_server", "allocation_id": allocation, "launch": plain_launch() })
}

async fn controller_misbehaves(ctx: &mut Ctx<'_>) -> Result<()> {
    let [first, second] = [ALLOCATIONS[0], ALLOCATIONS[1]];
    ctx.boot().await?;

    // Two launches of one allocation at once: one runs, the other is refused while it does.
    let mark = ctx.mark();
    let replies = ctx.requests_together(vec![start(first), start(first)]).await?;
    if replies.iter().filter(|reply| succeeded(reply)).count() != 1 {
        anyhow::bail!("two launches of one allocation did not run exactly once: {replies:?}");
    }
    let channel = channels::plugin(first);
    ctx.wait_on(mark, &channel, |report| report["started"] == true).await.context("the game server never started")?;
    let mark = ctx.mark();
    ctx.plugin(first, json!({ "op": "connect" }))?;
    ctx.wait_on(mark, &channel, |report| report["relay"] == "connected").await.context("the plugin never reached the relay")?;
    ctx.ready(first).await?;

    // Two stops of it at once: both answered, one exit.
    let mark = ctx.mark();
    ctx.requests_together(vec![stop(first), stop(first)]).await?;
    wait_exit(ctx, mark, first).await?;

    // A retried launch of the allocation that has run.
    ctx.request(start(first)).await?;

    // A stop before the server is ready: its container runs, its plugin has not connected.
    let mark = ctx.mark();
    Ctx::expect_ok(&ctx.request(start(second)).await?)?;
    ctx.wait_on(mark, &channels::plugin(second), |report| report["started"] == true).await.context("the game server never started")?;
    stop_server(ctx, second).await?;
    ctx.stop_agent().await
}
