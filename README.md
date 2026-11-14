<p align="center">
  <a href="https://ankor.sh">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="brand/ankor-wordmark.svg">
      <img src="brand/ankor-wordmark-light.svg" alt="Ankor" width="280">
    </picture>
  </a>
</p>

<h3 align="center">Game nodes for Counter-Strike 2</h3>

<!-- Community and support -->
<p align="center">
  <a href="https://docs.ankor.sh"><img alt="docs" src="https://img.shields.io/badge/docs-docs.ankor.sh-blue"></a>
  <a href="https://discord.gg/H3sGar9BMY"><img alt="Discord" src="https://img.shields.io/discord/1562876815557525575?label=discord&logo=discord&logoColor=white"></a>
</p>

<!-- Code quality -->
<p align="center">
  <a href="https://github.com/ankorcs/ankor/actions/workflows/release.yml?query=branch%3Amaster+event%3Apush"><img alt="build" src="https://img.shields.io/github/actions/workflow/status/ankorcs/ankor/release.yml?branch=master&event=push&label=build"></a>
</p>

<!-- Releases -->
<p align="center">
  <a href="https://github.com/orgs/ankorcs/packages/container/package/ankor-agent"><img alt="ankor-agent image" src="https://img.shields.io/github/v/tag/ankorcs/ankor?filter=agent-v%2A&label=ankor-agent"></a>
  <a href="https://github.com/orgs/ankorcs/packages/container/package/ankor-cs2"><img alt="ankor-cs2 image" src="https://img.shields.io/github/v/tag/ankorcs/ankor?filter=cs2-v%2A&label=ankor-cs2"></a>
  <a href="https://www.npmjs.com/package/@ankorcs/protocol"><img alt="@ankorcs/protocol version" src="https://img.shields.io/npm/v/%40ankorcs%2Fprotocol"></a>
</p>

Ankor runs Counter-Strike 2 servers on your own Linux hosts. You install the Ankor
agent on a machine and connect it to a controller, and the machine becomes a **game node**. The
agent installs CS2, keeps it up to date and starts a dedicated server whenever the controller asks
for one.

Each game server runs in a fresh container from the official
[Ankor CS2 image](packages/plugin/docker). The image carries the [match plugin](packages/plugin) and
the server configuration, and each server shares one CS2 installation.

```mermaid
%%{init: {"theme": "base", "themeVariables": {
  "lineColor": "#5b6478", "textColor": "#1b1e24", "clusterBkg": "transparent", "clusterBorder": "#4a65e0"
}, "themeCSS": ".node.outside .nodeLabel, .node.outside .nodeLabel * { color: #0c0d0f !important; } .node.ankor .nodeLabel, .node.ankor .nodeLabel * { color: #eef1ff !important; } .node.message .nodeLabel, .node.message .nodeLabel * { color: #3550c8 !important; } .cluster-label, .cluster-label * { color: #4a65e0 !important; }"}}%%
flowchart LR
    controller(["Controller"])
    players(["Players"])
    subgraph node["&nbsp;Your game node&nbsp;"]
        direction LR
        agent["Ankor agent"] --> runs(["runs"]) --> server["CS2 server"]
    end
    controller --> starts(["starts a match"]) --> agent
    agent --> reports(["reports results"]) --> controller
    players --> connect(["connect"]) --> server
    classDef ankor fill:#4a65e0,stroke:#3550c8,color:#eef1ff,font-weight:bold
    classDef outside fill:#dfe3ea,stroke:#5b6478,stroke-width:1.5px,color:#0c0d0f
    classDef message fill:#ffffff,stroke:#4a65e0,stroke-width:1.5px,color:#3550c8,font-weight:600
    class agent,server ankor
    class controller,players outside
    class runs,starts,reports,connect message
    style node stroke-dasharray:5 4,color:#4a65e0
    linkStyle default stroke:#5b6478,stroke-width:2px
```

The controller decides who plays, on which map and on which node, and keeps the results. Use
[ankor.sh](https://ankor.sh), or build a service of your own on the open protocol.

## Documentation

The node documentation is at [docs.ankor.sh](https://docs.ankor.sh):

- [Quick start](https://docs.ankor.sh/quickstart/): from a Linux host to a lobby on [ankor.sh](https://ankor.sh).
- [Install a node](https://docs.ankor.sh/install/) and [operate it](https://docs.ankor.sh/operations/).
- [The protocol](https://docs.ankor.sh/protocol/), for building a controller of your own.

## Versions

The protocol, the agent image and the CS2 image each have their own version. The protocol's
version decides which of them work together with a controller:

- Between compatible protocol versions, a newer agent or plugin works with an older controller, and
  the other way round.
- A feature works once both sides are on the protocol minor that added it.

[Versioning](https://docs.ankor.sh/protocol/versioning/) lists the releases that support each protocol
version, and what a controller must accept from a newer agent.

## This repository

It holds the open parts of Ankor: the agent, the match plugin and CS2 image, and the protocol
between a controller and its nodes, published as `@ankorcs/protocol` with a session to run it
from a Node.js controller. [ankor.sh](https://ankor.sh) is not part of it.

| Part                                                     | License                                                    |
|----------------------------------------------------------|------------------------------------------------------------|
| `packages/protocol`, and everything without its own file | [Apache-2.0](LICENSE)                                      |
| `packages/agent`                                         | [GPL-3.0](packages/agent/LICENSE)                          |
| `packages/plugin`                                        | [GPL-3.0](packages/plugin/LICENSE)                         |
| `docs`                                                   | [All rights reserved](docs/LICENSE), open to contributions |
| `brand`                                                  | [All rights reserved](brand/LICENSE), shipped with Ankor   |

[Development](DEVELOPMENT.md) describes how to work on it. [Roadmap](ROADMAP.md) lists the planned
work, and [Decisions](ADR.md) the designs that were turned down.
