# Headless plugin

`ankor-headless` is a game server process without CS2: the plugin's [runtime](../core/PluginRuntime.cs),
[agent link](../core/AgentLink.cs) and [match](../core/MatchSession.cs), as the plugin runs them,
on the [test engine](../testing/FakeGameHost.cs) in place of the game. The match does not capture,
clear grenades or run work before its shutdown, which need the engine. The agent's
[conformance suite](../../agent/crates/conformance/README.md) runs it as its game servers, so the
suite checks the agent against the plugin that ships. Nothing here ships to a game server.

## The image

[docker/Dockerfile](docker/Dockerfile) lays the host out as an `ankor-cs2` image for the agent: the
labels it reads, the addon overlay it extracts, and an [entrypoint](docker/entrypoint.sh) for both
modes the agent starts the image in.

- **Run mode** starts `ankor-headless` with the launch's environment.
- **Update mode** (`CS2_UPDATE_ONLY=1`) stands in for the real image's steamcmd update. It prints
  the real entrypoint's and steamcmd's lines, then waits for a file at
  `ANKOR_HEADLESS_UPDATE_INSTRUCTION` (default `/tmp/ankor-headless-update.json`) to say how the
  update ends. `{"op":"install","build":"<id>"}` records that build in the install. Any other `op`,
  such as `{"op":"fail","code":<n>}`, fails as the real entrypoint does. It logs `code` as
  steamcmd's exit, 8 when missing, and exits 1.

```sh
# from packages/plugin
docker build -f headless/docker/Dockerfile -t ankor-cs2-headless:latest .
```

## Driving it

Without `ANKOR_HEADLESS_CONTROL_URL`, the server starts by itself: its plugin loads, dials the
relay and loads the start map, and nobody plays. With it, the server dials
`<ANKOR_HEADLESS_CONTROL_URL>?allocation=<id>`, sends reports and takes instructions, one JSON
object per message.

| Instruction                | Does                                                                                                                                                                                                                                                    |
|----------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `connect`                  | Starts the server once, as SwiftlyS2 loads the plugin. Once the plugin has sent its first frame on the relay, the start map loads, Steam logs on as the game server account `[G:1:1]` and two seconds pass. Later, lets the plugin back onto the relay. |
| `hold`                     | Keeps the plugin off the relay: its dials fail until the next `connect`.                                                                                                                                                                                |
| `join`                     | A player (`steam_id`, `name`) connects and joins `side` (`ct`, `t` or `spec`).                                                                                                                                                                          |
| `round_start`, `round_end` | The engine starts a round, writing its backup as `mp_backup_round_auto` does, or scores one for `winner` with the engine's `reason`.                                                                                                                    |
| `advance`                  | Moves the engine's clock by `seconds`, firing the timers that come due.                                                                                                                                                                                 |
| `exit`                     | The process ends with `code`, as a crash does: the plugin does not unload.                                                                                                                                                                              |

The reports are:

- `started`, with `match` true when the launch has a match config file;
- `relay`: `connected`, `closed` with its code and reason, `refused` or `unreachable`;
- each frame on the plugin's socket, as `plugin_frame` or `relay_frame`;
- `recording`, when the engine starts a demo;
- `exiting`, with `code`: `exit`, or `SIGTERM`, `SIGINT` or `quit`, after which the plugin unloads.

An instruction the server cannot carry out is reported as `op_failed` with its error, and one it
does not know as `unknown_op`. The server plays on. The plugin's socket passes through a proxy in
the process, which reads the frames off it without changing a byte.

The engine runs the cfgs as the server does: the plugin's from its source tree, and the launch cfg
the agent mounts at `cfg/ankor_launch.cfg`. A convar a cfg or command sets reads as set.

The engine's clock runs in real time from `ANKOR_HEADLESS_START_MS`, or from now. With
`ANKOR_HEADLESS_CLOCK=manual`, it moves only on `advance` and the two seconds of boot, so a driver
sees the same game times and demo names on every run.
