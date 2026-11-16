# Working on Ankor game nodes

This repository holds the public parts of Ankor: the agent, the plugin, the protocol and the node
docs. [README.md](README.md) says what Ankor is; [Development](DEVELOPMENT.md) has the repository
layout, setup, checks and the workstation scripts.

## Before making changes

1. Read the guide for the part you are changing.
2. Check [Development](DEVELOPMENT.md#conventions) for repository conventions. For a change to the
   Rust agent, also check its [Conventions](packages/agent/CONVENTIONS.md), and for a change to the
   protocol, the protocol's [Conventions](packages/protocol/CONVENTIONS.md).
3. Make the change and update any documentation it makes inaccurate.
4. Run the [checks](DEVELOPMENT.md#checks).
5. Commit only when asked. The working tree belongs to the user.

## Which guide to read

| Working on                                          | Read                                                                            | Source                                             |
|-----------------------------------------------------|---------------------------------------------------------------------------------|----------------------------------------------------|
| Proposing a refactor or a design change             | [Decisions](ADR.md)                                                             |                                                    |
| Planned agent, plugin or contract work              | [Roadmap](ROADMAP.md)                                                           |                                                    |
| Containers, slots, game files or host recovery      | [Agent](packages/agent/README.md), [Conventions](packages/agent/CONVENTIONS.md) | `packages/agent/crates`                            |
| The wire contract or the controller's agent session | [Protocol](packages/protocol/README.md)                                         | `packages/protocol/src`                            |
| The AsyncAPI document and the agent.yml schema      | [Protocol](packages/protocol/README.md)                                         | `packages/protocol/reference`                      |
| Match logic or native engine calls                  | [Plugin](packages/plugin/README.md)                                             | `packages/plugin/core`, `packages/plugin/GameHost` |
| The CS2 image, server cfgs and SwiftlyS2            | [CS2 image](packages/plugin/docker/README.md)                                   | `packages/plugin/docker`, `packages/plugin/cfg`    |
| Installing or operating a game host                 | [Node docs](docs/index.md)                                                      | `docs/*.md`, `packages/protocol/node-setup.sh`     |
| Writing or rewriting a docs page                    | [Writing documentation](docs/src/README.md#writing-documentation)               | `docs/*.md`, `docs/protocol/*.md`                  |
| Checks, the node images and their releases          | [Development](DEVELOPMENT.md)                                                   | `scripts`                                          |
| The docs site and its protocol reference pages      | [Docs site](docs/src/README.md)                                                 | `docs/src`, `docs/tools`                           |

## Repository rules

- Each package builds, tests and publishes on its own. Package code and package tests read nothing
  outside their package; a package uses another only through its published entry points. The one
  exception is the agent's codegen and its check scripts, which read `packages/protocol` from the
  checkout to generate the committed Rust contract and its corpus.
- Repository-level scripts and checks may span packages.
- Controllers, [ankor.sh](https://ankor.sh) among them, consume the published packages. Nothing here imports or reads
  a controller's code.
- Committed files carry no details of a contributor's environment (hosts, addresses, local paths).
- Coding agents should not co-sign the commits they make.
- Start a commit message with its type in parentheses: `(feat)`, `(fix)`, `(nit)`, `(docs)`,
  `(refactor)`, `(test)` or `(chore)`. Follow it with one short line saying what changed, and add
  at most a few lines more when the reason is not obvious. Do not write a paragraph.
  `(fix) Game servers start without -nomaster, which blocks server queries`.
- Coding agents write, update or delete memories only once the user has approved each one in chat.
