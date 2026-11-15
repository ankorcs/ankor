# Agent

The Ankor game-host agent in Rust, published as the `ghcr.io/ankorcs/ankor-agent` image. It runs a
node's CS2 game servers for an Ankor controller, over the controller socket, the plugin relay, the
Docker containers it creates and the files it keeps on the node.

Read [CONVENTIONS.md](CONVENTIONS.md) for how the agent runs (threads, tasks, shared state,
shutdown) and the rules for changing its code.

## Layout

| Path                      | Holds                                                                                                               |
|---------------------------|---------------------------------------------------------------------------------------------------------------------|
| `crates/agent`            | The agent: one binary, and a library its tests use.                                                                 |
| `crates/agent/migrations` | The event spool's layout, one `event_spool_NNNN.sql` per step, listed in `SPOOL_LAYOUT_FILES` in `event_spool.rs`.  |
| `crates/protocol`         | The wire contract: validators, types and constants generated from `@ankorcs/protocol`, and helpers written by hand. |
| `crates/conformance`      | The [conformance suite](crates/conformance/README.md), a black-box check against golden transcripts.                |
| `crates/fake-plugin`      | The plugin's behaviour on the agent's relay, which the fuzz test plays.                                             |
| `codegen`                 | The generator and corpus writer for the contract in `crates/protocol` and `crates/agent/src/contract`.              |
| `docker`                  | The agent's image and the checks' toolchain image.                                                                  |
| `scripts`                 | The [scripts](#scripts) that run the checks and sync the contract.                                                  |
| `about.toml`, `about.hbs` | cargo-about's config and template for `THIRD-PARTY-LICENSES.txt`, the notices of the crates the binary bundles.     |

## Scripts

Each script runs itself in the pinned Linux toolchain of [docker/ci.Dockerfile](docker/ci.Dockerfile),
built for the workstation's architecture. A script therefore needs only Docker and gives the answer
CI gives, and Linux-only code is checked on any workstation.

| Script                     | Runs                                                                                                                                                                                                   |
|----------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `scripts/check.sh`         | Format, lints, tests, licences, advisories and the image's notices. `--docker`, `--coverage` and `--mutants` add the Docker suite (`docker_service`), line coverage and mutation testing, and combine. |
| `scripts/run.sh`           | One command in the container, with `ANKOR_*` and `FUZZ_*` variables. `--docker` hands it the host's Docker socket, and `--conformance` sets it up for the conformance suite.                           |
| `scripts/protocol-sync.sh` | The check that the generated contract matches `packages/protocol`. `--update` regenerates it, writes the corpus and runs the protocol tests.                                                           |

`scripts/lib` holds the functions the scripts load: [in-docker.sh](scripts/lib/in-docker.sh) runs a
script in the container, and [protocol.sh](scripts/lib/protocol.sh) builds `packages/protocol`.

## Checks

Run every check before a commit:

```sh
scripts/check.sh                                          # every check
scripts/run.sh cargo nextest run -p ankor-agent node_log  # one test
```

CI ([check-agent.yml](../../.github/workflows/check-agent.yml)) runs `scripts/check.sh --docker` and
`scripts/protocol-sync.sh`. Mutation testing and the conformance suite run only by hand.

- [tests/fuzz.rs](crates/agent/tests/fuzz.rs) runs the agent against a careless controller on a
  paused clock. The checks run 8 random sequences. `FUZZ_RUNS` runs more, and the test's header
  says how to replay one.
- Mutation testing uses the `mutants` profile in [.config/nextest.toml](.config/nextest.toml), which
  ends a test that a mutant makes hang. `cargo mutants --iterate` retests only the mutants the last
  run missed.
- Pass `--jobserver-tasks 6` to a `cargo mutants` run of your own, as `scripts/check.sh --mutants`
  does. cargo-mutants otherwise runs one compiler or linker per CPU and ignores `CARGO_BUILD_JOBS`.
- Tests that reach real services are ignored, so a normal run reports them as skipped.
  `--run-ignored only` runs them, as the header of [scripts/run.sh](scripts/run.sh) shows:
  - `--test docker_service` uses the local daemon, through `scripts/run.sh --docker`.
  - `--test overlay_live` extracts the overlay of the cs2 image in `ANKOR_CS2_IMAGE`, and needs the
    daemon too.
  - `--test registry_live` reads tags and digests from ghcr.io and Docker Hub.
  - `steam_logon::live` logs on to Steam with a made-up token.
- `ANKOR_LOG_LEVEL` sets the least severe line the agent prints, in a test or on a node: `debug`,
  `info` (the default), `warn` or `error`. The controller receives every line from `info` up,
  because the contract has no debug level.

## The protocol contract

`@ankorcs/protocol`, in [packages/protocol](../protocol), is the only definition of the wire
contract and of the container contract (`@ankorcs/protocol/container`). `crates/protocol` is
generated from it and committed, so the agent builds with cargo alone. After a protocol change,
review the Rust that `scripts/protocol-sync.sh --update` generates and commit it with the change.

- `codegen/generate.mjs` writes the Rust types, validators and constants from the protocol's zod
  schemas. The refinements zod cannot express as data are written by hand in `refine.rs` and listed
  in the generated `codegen/hooks.md`.
- The generated code carries the contract's JSDoc. zod keeps no comments at runtime, so
  `codegen/docs.mjs` reads them from the protocol's sources.
- `codegen/corpus.mjs` records zod's verdict on random samples and edge cases for every schema, and
  `cargo test` requires the same verdicts of the Rust validators. An edge case moves one value of a
  valid payload to just inside and just past its rule. `scripts/check.sh` writes the corpus, which
  is not committed, when it is missing or its inputs changed.

## Running it on a node

[Development nodes](../../DEVELOPMENT.md#development-nodes) describes how to build the image and
move a node onto it. To test only the connection to the controller, run
`ANKOR_NODE_TOKEN=<a test node's token> scripts/run.sh cargo run -p ankor-agent --example live_hello`.

## License

The agent is [GPL-3.0](LICENSE). Its contract code is generated from `@ankorcs/protocol` and keeps that
package's [Apache-2.0](LICENSE-APACHE) license, which a GPL-3.0 work may include. The image carries both
texts in `/usr/share/licenses/ankor-agent`, with [THIRD-PARTY-LICENSES.txt](THIRD-PARTY-LICENSES.txt), the
notices of the crates the binary bundles.
