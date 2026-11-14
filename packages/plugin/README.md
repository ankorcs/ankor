# CS2 plugin

The plugin runs inside CS2. It locks the roster, moves the match through its phases, records demos
and reports gameplay facts.
[CS2 image](docker/README.md) covers the image, and [Event uploads](https://docs.ankor.sh/protocol/events/)
covers how its events reach the controller.

## Parts of the plugin

- **Core** (`core/`): every match decision and its state, tested without the game engine.
  [PluginRuntime](core/PluginRuntime.cs) is the plugin's life on a server: its load, map loads,
  Steam logon and unload. [AgentLink](core/AgentLink.cs) is the socket to the agent and the answers
  to its frames, and [MatchSession](core/MatchSession.cs) wires a loaded match's parts together. A
  host without the engine runs all three as the plugin does.
- **Native adapter:** `AnkorPlugin`, `LoadedMatch` and [GameHost](GameHost). It reads CS2 state into
  plain values for core and carries out core's calls through the host interface.
- **Headless** ([headless](headless/README.md)): the plugin's core on the test engine, as a game
  server process without CS2, which the agent's conformance suite runs as its game servers.
- **Test engine** ([testing](testing)): [FakeGameHost](testing/FakeGameHost.cs) plays the engine
  for core, with its clock, timers, console and players, and `Fixture` holds the shared test
  values. The tests and the headless host build on it.
- **Replay** ([testing](testing), [replay](replay)): a test scenario records a live run's engine
  reads and [`GameplayInput`](core/GameplayInput.cs) values, then feeds them back through the same
  handlers without the engine ([GameplayScenario.cs](testing/GameplayScenario.cs)). The frames the
  replay sends are committed as `tests/fixtures/plugin-replay.jsonl`, which controllers can use as
  test input.
- **Diagnostics** ([diagnostics](diagnostics)): `DebugCapture` writes a launch's debug capture,
  and `GameplayRecording` records the engine reads that replay feeds back. `CaptureNames` holds the
  names both write, which older captures depend on.
- **Configuration and text:** the cfgs in [cfg](cfg) and [configs](configs), and player-facing text
  in the [translation catalogue](resources/translations/en.jsonc).

## During a match

- The plugin does nothing unless the launch sets `ANKOR_ALLOCATION_ID`
  ([container contract](../protocol/src/container.ts)), which identifies its server's events.
- The plugin also does nothing when SwiftlyS2 hot-reloads it on a running server
  ([AnkorPlugin.cs](AnkorPlugin.cs)). Each gameplay event carries a sequence number, `seq`, that
  counts from 0 for the allocation ([`PluginEventFrame`](../protocol/src/plugin-socket.ts)). A
  second load would count from 0 again, and the agent would drop those events as ones it already
  has. Recreate the container instead.
- After each map load's cfgs, the plugin adds the allocation id without its dashes to `sv_tags`
  ([ServerTags.cs](core/ServerTags.cs)). Steam's server list shows the tags, and the server status
  reports the server's SteamID once it is ready. A controller can use both to check that the
  server runs.
- It loads the map, enforces the roster, runs the [knife round](core/KnifeRound.cs) and side choice,
  and starts live play. [Demo recording](core/DemoRecorder.cs) starts with live play. A server plays
  its one match, waits for GOTV to flush the demo ([MatchShutdown.cs](core/MatchShutdown.cs)), then
  quits.
- `ready_mode` ends warmup as [`ReadyMode`](../protocol/src/launch.ts) describes.
  [Manual ready-up](core/ReadyUp.cs) shows `READY`/`NOT READY` clan tags.
- Without `send_statistics`, the plugin sends no gameplay events, only its
  [`phase_change`, `score` and `backup`](../protocol/src/plugin-socket.ts) reports.
- A workshop launch turns the server's [`ready`](core/ServerReadiness.cs) on the first map after
  `host_workshop_map` ([WorkshopMap.cs](core/WorkshopMap.cs)), so a failed download leaves the
  server unready.
- A restore plays on from a round backup: the launch's `match.restore`
  ([launch.ts](../protocol/src/launch.ts)), or the controller's `restore_round` mid-match
  ([plugin-socket.ts](../protocol/src/plugin-socket.ts)). [MatchState.cs](core/MatchState.cs)
  describes how the plugin follows the engine's load and when it refuses one.
- Rosters hold players and spectators, and the game's `coach` command is blocked.
  [Decisions](../../ADR.md#coaching) says why coaching was dropped.

## Game rules

The plugin's cfgs ship under `cfg/ankor/`.
[When the cfgs run](https://docs.ankor.sh/protocol/#when-the-cfgs-run) lists when each one runs,
beside the game mode's cfg and the agent's `ankor_launch.cfg`. `reset.cfg` puts what warmup and the
knife round change back to the engine's defaults. It runs ahead of the mode's cfg, so a mode's
own value, such as casual's `mp_round_restart_delay 10`, survives going live. `base.cfg` holds
Ankor's settings for every mode.

`applied.cfg` holds no settings. It is a marker that runs `ankor_cfg_applied`, a command that the
adapter registers. To act once cfgs have run, the plugin execs `ankor/applied` after them
([AfterCommandsQueue.cs](core/AfterCommandsQueue.cs)). CS2 runs a console line's `exec` after the
rest of its command buffer, so the marker runs once every earlier line has. A player who types
the command changes nothing.

## Adapter and core

- Put a decision in core even when only the adapter calls it: a name or code mapping, a parse, a
  rule that picks between values, or state kept between callbacks.
  [GrenadeAttribution.cs](core/GrenadeAttribution.cs) is an example.
- Pass core plain values. Pass a read that can throw, or that only one path needs, as a delegate
  that core calls when its rule needs the value.
- Leave a decision in the adapter only when it is interleaved with engine reads, as in
  `SwiftlyGameHost.ReadInventoryItems`.
- Test each core helper in `tests/`. Replay does not cover helpers that the adapter calls before it
  builds a `GameplayInput`.

## Working with engine data

- Read gameplay state on the game thread, and move background messages onto it first.
- Copy values while native objects are valid. Do not keep those objects for background work.
- Keep failed reads as failures, not zero or empty values.
- Order gameplay by engine time, including fractional ticks.
- Handle delayed and nested callbacks before closing their round or map.
- Keep live hooks and replay on the same handlers.
- When a roster check cannot be read safely, reject the connection with a retry message. Other read
  failures must not kick players.
- Check [SwiftlyS2's documentation](https://swiftlys2.net/docs) before adding native API calls.

## Timers and shutdown

- Give each timer an owner through [OwnedTimer](core/OwnedTimer.cs), and close phase and match
  timers when their owner finishes. Set whether each timer survives map changes. The exception is
  `TeamLock`'s per-slot Steam verification timers, which a dictionary owns because each slot holds
  at most one.
- Repeat work with `ScheduleRepeating`, never with a one-shot timer that schedules itself again
  from its callback. Each one-shot fires a frame late, and a chain of them adds the lateness up, so
  a countdown of one-second timers runs seconds behind the engine's clocks. Read time left from
  the engine's `CurrentTime` against a fixed end rather than by counting ticks.
  [TimerConventionTests](tests/TimerConventionTests.cs) fails on a method that schedules itself.
- On unload, finish [captures](#capture-a-development-match) and give outgoing messages a bounded
  time to drain. Never block unload on work queued to the game thread, because SwiftlyS2 cancels
  the plugin's timers and next-tick callbacks once unload returns.

## Making and testing a change

1. Add the scenario to the core class's test in `tests/`, and run it from the repository root:

   ```sh
   dotnet test packages/plugin/tests --filter FullyQualifiedName~KnifeRoundTests
   ```

2. If the tests report that the committed replay fixture is out of date, the change altered what the
   plugin sends. Fix the change, or, when it is meant, regenerate the fixture with
   `scripts/plugin.sh replay` and review its diff. A controller that uses it takes a copy, which
   `scripts/plugin.sh replay <path>` writes directly.
3. Check that the tests catch a wrong change with `scripts/plugin.sh mutants`. It mutates core and
   reports, under `coverage/plugin-mutants`, the mutants that no test fails on.
4. If you changed a value the plugin copies from the protocol
   ([Plugin copies](../../DEVELOPMENT.md#plugin-copies)), run `scripts/plugin.sh contract`. It fails on any value
   the two packages disagree on, and on a replay fixture frame that the contract refuses.
5. If you changed `PluginRuntime`, `AgentLink`, `MatchSession` or `headless/`, run the agent's
   conformance suite with `scripts/conformance.sh`. It runs the agent against the headless host, so
   a change to what the plugin sends the agent shows up as a golden diff.
6. Test native engine reads on a development game server.
7. Run the full [checks](../../DEVELOPMENT.md#checks).

## Capture a development match

A launch with `debug_capture` and `send_statistics` ([launch.ts](../protocol/src/launch.ts))
records the plugin's callbacks on the node, in the format replay reads.

1. Launch such a match on a development node and reproduce the behaviour.
2. List the captures, then print one by its file name without `.jsonl`:

   ```sh
   DOCKER_HOST=ssh://root@game-node scripts/plugin.sh captures example/ankor-cs2:TAG
   DOCKER_HOST=ssh://root@game-node scripts/plugin.sh captures example/ankor-cs2:TAG LAUNCH_ID > capture.jsonl
   ```

Overflow or a crash can leave a capture incomplete. Check for its final completion record before
using it as a replay fixture.

## License

The plugin is [GPL-3.0](LICENSE). The CS2 image carries that text in `/usr/share/licenses/ankor-cs2`, with SwiftlyS2's
license and a link to the source of the release it ships.
