# Conformance suite

A black-box suite that runs scenarios against the agent from its image and records what it does on
its external interfaces. Each scenario's normalised transcript is kept in
[golden/](golden) as `<scenario>.json`, and a scenario passes when its run matches that file. A
change to the agent's behaviour therefore shows up as a diff of a golden transcript, which is
reviewed with the change. The suite does not use the agent crate, so it cannot share a bug with
the code it tests.

## Principles

- **The agent runs as it ships.** It runs from its image with `node-setup.sh`'s run command, and
  it has no code or switches for tests. The fakes sit where the agent already looks: the controller
  host in `agent.yml`, the Docker socket, the relay port, and the public hosts it calls.
- **Nothing leaves the sandbox.** The agent runs in a Docker-in-Docker container on a Docker
  network with no way out. The suite joins that network under the names of the hosts it fakes, so
  the agent reaches the fakes by name and nothing else. No image is pulled during a run.
- **The plugin runs as it ships.** A game container runs the plugin's core from
  `packages/plugin`, on the plugin's test engine instead of CS2. Only the engine is played: players
  join, rounds start and end, the clock moves. The suite has no scenario for something the shipped
  plugin cannot do, because the plugin and the image ship together.
- **Runs are deterministic.** The fakes hold answers until the scenario releases them (Steam's
  build and logons, a plugin's dial, a demo upload's answer), and the game's clock moves only when
  a scenario moves it. Each step waits for the agent's answer before the next one starts.
- **What a step causes is compared, not its order.** Frames within one step arrive in whatever
  order the agent's tasks run. Once each step goes quiet, the suite marks the controller channel
  with the step's name, and normalisation sorts the entries between two markers. The controller
  channel therefore reads as the scenario, step by step. The other channels are sorted whole.
  Normalisation also removes ids, clock readings, the agent's build, and entries whose number
  depends on timers.
- **The agent's values are copied, once each.** The suite takes the wire contract from
  `ankor-protocol`. Values that only the agent crate defines are copied into the suite, with one
  copy each, mostly in `sandbox.rs`. These include the role label, the data and install paths and
  Steam's app id. The relay's `ack` and its key are in `scenario.rs`.

## How a run is put together

```
workstation's Docker
├─ the suite, in the agent's checks container (scripts/run.sh --conformance)
│  ├─ fake controller  port 80, HTTP: agent socket, event and demo uploads, plugin control
│  └─ stubs            port 443, TLS from a CA made per run: ipinfo, steamcmd, Steam, registries
├─ network ankor-conformance, internal: the suite joins it as controller.conformance.test,
│  ghcr.io, ipinfo.io and the other faked hosts
└─ ankor-conformance-dind (docker:29.9-dind), on that network only
   ├─ dockerd, the agent's real daemon
   ├─ /srv/ankor, the node's data directory, at the same path for the daemon and the agent
   └─ ankor-agent, and the game and helper containers it starts
```

For each scenario the suite:

1. resets the node: no containers or volumes, a game volume holding the build the stubs call
   newest, a fresh `/srv/ankor` and the scenario's `agent.yml`;
2. starts the agent, with the test CA added through `SSL_CERT_FILE`;
3. runs the scenario's steps through the fake controller and the game servers, recording every
   frame, upload, Docker event and stub request, and snapshots of the containers and the data
   directory;
4. stops the agent, normalises the transcript and compares it with the golden one.

Game and update containers run the plugin's [headless image](../../../plugin/headless/README.md),
which describes how a scenario drives a server and how the image stands in for the update mode. The
suite's image, [`fake-cs2.Dockerfile`](docker/fake-cs2.Dockerfile), adds only env: where the
control socket and the update instruction are, and the manual clock with its start time. The agent
starts the containers, so the suite cannot add env to them. An update container runs on the bridge
network, which the sandbox keeps from the fake controller, so the scenario writes the update's
instruction into it with `docker exec`.

| Code                                                                                               | Does                                                                                  |
|----------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------|
| [`runner.rs`](src/runner.rs)                                                                       | Starts the fakes and the sandbox, and runs each scenario.                             |
| [`golden.rs`](src/golden.rs)                                                                       | Compares a transcript with its golden file, or writes it.                             |
| [`sandbox.rs`](src/sandbox.rs)                                                                     | The sandbox: its network, images, node reset, the agent's container, snapshots.       |
| [`controller.rs`](src/controller.rs), [`stubs.rs`](src/stubs.rs), [`tls.rs`](src/tls.rs)           | The fake controller, the stubbed outside services and the CA made per run.            |
| [`scenario.rs`](src/scenario.rs), [`scenarios.rs`](src/scenarios.rs)                               | The node config, the steps scenarios use, and the scenarios.                          |
| [`transcript.rs`](src/transcript.rs), [`normalise.rs`](src/normalise.rs), [`diff.rs`](src/diff.rs) | Recording, normalisation and comparison.                                              |
| [`fake-cs2.Dockerfile`](docker/fake-cs2.Dockerfile)                                                | The game server's image: the plugin's headless image, pointed at the suite.           |

## Running it

The suite needs only Docker. The repository's
[`scripts/conformance.sh`](../../../../scripts/conformance.sh) pulls the sandbox image and builds
the images it tests on the workstation's daemon: the plugin's headless image, the game image on top
of it, and the agent's image. It then runs the suite in the agent's checks container, with the Docker socket, through
[`scripts/run.sh --conformance`](../../scripts/run.sh). Arguments go to the suite:

```sh
# from the repository root
scripts/conformance.sh --list                        # the scenarios
scripts/conformance.sh                               # all of them
scripts/conformance.sh --scenario launch --keep      # one, leaving the sandbox running
```

`packages/agent/scripts/check.sh` runs only the suite's unit tests. Each scenario writes the raw
and normalised transcripts, the diff, the agent's log and `steps.log`, the steps it took as the
terminal shows them, to `packages/agent/target/conformance/<scenario>/`. The sandbox's daemon logs
only warnings and errors.

A scenario reports one of:

- **match**: the normalised transcript is the same as the golden one.
- **DIFFER**: the agent finished but did something else. The diff names the channel, with the
  golden version as `-` and this run's as `+`. It is a regression, an intended change whose golden
  transcript needs updating, or noise that normalisation should remove.
- **MISSING GOLDEN**: the scenario has no golden transcript yet.
- **ERROR**: a step failed or timed out. The message names the step, and the agent's log shows
  why.

## Updating the golden transcripts

When a change to the agent alters what a scenario records on purpose, rewrite the golden files and
commit them with the change:

```sh
scripts/conformance.sh --update                      # every scenario
scripts/conformance.sh --update --scenario launch    # one
```

Run the suite again without `--update` to confirm that every scenario matches, which also shows the
run is deterministic. In review, read the diff of `golden/` as the change in the agent's external
behaviour: each line added or removed is a frame, an upload, a Docker call or a file the agent now
produces differently.
