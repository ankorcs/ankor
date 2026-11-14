---
title: Ankor game nodes
description: Managed CS2 game servers on your own Linux hosts, driven by a controller they connect to.
slug: index
---

Ankor hosts Counter-Strike 2 servers on your own hardware. You install the Ankor agent on a Linux machine and connect it
to a controller. From then on the machine is a **game node**. The agent installs CS2 and keeps it up to date. It starts
a dedicated server whenever a game server is requested. Players join that server directly and play their map. The
server shuts itself down when the match is over.

Every node connects to a controller. The controller decides who plays, on which map and on which
node, and keeps the results. You choose which controller your nodes connect to:

<div class="choices">
<div class="choice">
<p class="choice-name">Official Ankor</p>
<p class="choice-sub">ankor.sh</p>

Sign in with Steam, add your nodes, start lobbies.

<p class="choice-label pros">Pros</p>

- Nothing to build or host besides your nodes.
- Lobbies, map picks, statistics and demo storage.
- Share nodes and update them from the browser.

<p class="choice-label cons">Cons</p>

- [ankor.sh](https://ankor.sh)'s rules and a [Steam Web API key](ankor-web.md#your-steam-web-api-key).
- Nodes run what [ankor.sh](https://ankor.sh) sends. [Trust](how-it-works.md#trust) lists what that allows.

</div>
<div class="choice">
<p class="choice-name">Self-hosted</p>
<p class="choice-sub">your own controller</p>

Build your own controller on the open [protocol](protocol/index.md).

<p class="choice-label pros">Pros</p>

- Your own match flow, rules and game modes.
- Everything runs on your infrastructure.
- A minimal implementation needs only a few messages.

<p class="choice-label cons">Cons</p>

- You build, host and maintain the controller.
- You track the agent's release lines, as [Versioning](protocol/versioning.md) explains.

</div>
</div>

A controller that someone else built on the protocol has the same
[control over your nodes](how-it-works.md#trust) as [ankor.sh](https://ankor.sh).

## A node, not a server

With <a href="https://github.com/shobhit-pathak/MatchZy" target="_blank" rel="noopener">MatchZy</a>, <a href="https://github.com/Lan2Play/PugSharp" target="_blank" rel="noopener">PugSharp</a> or <a href="https://github.com/splewis/get5" target="_blank" rel="noopener">get5</a> you own a server. You install it, add a plugin and your
configs, and keep them working through every game update. An Ankor node has no such server to look
after. Each match runs in a fresh container from the official `ankor-cs2` image. The image carries
the plugin, SwiftlyS2 and the server configuration. It runs on top of
[one CS2 installation](how-it-works.md#one-installation-many-servers) that the node shares between
its servers.

A match on ankor.sh plays CS2's competitive defaults, like one on Premier or FACEIT. Its launch
sets only the match rules, such as friendly fire, GOTV and overtime. To play custom games, you must
build your own implementation on the public agent protocol.

Ankor's plugin runs in every game server. It reports its state and gameplay events to the agent,
and it runs the match. [Kinds of launch](how-it-works.md#kinds-of-launch) lists what it does in
each launch. The plugin cannot be left out or replaced, because nodes depend on it.

## Get started

- To play on [ankor.sh](https://ankor.sh) with your own node, follow the [quick start](quickstart.md).
- To run nodes in depth, read the [requirements](requirements.md), then [install a node](install.md).
- To build a controller, start with the [protocol overview](protocol/index.md), or follow
  [a minimal controller](protocol/minimal-controller.md) step by step.
- To run your own game modes, such as [ankor.sh](https://ankor.sh)'s aim warmups, add
  [plugins and cfgs](operations.md#add-swiftlys2-plugins) to your nodes and
  [launch servers without a match](how-it-works.md#kinds-of-launch).

<div class="callout">
<p class="callout-label">Working with an AI assistant?</p>

Give it `https://docs.ankor.sh/llms.txt` instead of these pages. It lists every guide as plain
Markdown and links the full protocol as one AsyncAPI document. [llms-full.txt](/llms-full.txt)
holds all guides in a single file.

</div>
