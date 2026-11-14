---
title: Configuration and files
description: The node's agent.yml, which settings the controller can change, and where a node keeps its files.
slug: configuration
---

A node's settings are in one file, `agent.yml`. The controller sets most of them. It writes the
file during setup. Later it can change parts of the file over the socket, such as the images when
it updates the agent or the plugin. You edit the rest, such as the memory cap, on the host.

## `agent.yml`

The file is `/srv/ankor/agent.yml`. Its schema is `AgentConfigSchema` in
[agent-config.ts](../packages/protocol/src/agent-config.ts).

```yaml
token: 3f1c…
controller:
  host: cs.example.com
  tls: true
pinned_ip: 203.0.113.7
cs2_image: ghcr.io/ankorcs/ankor-cs2:0.9
agent_image: ghcr.io/ankorcs/ankor-agent:0.9
memory_mb: 1536
demo_upload:
  url: https://demos.example.com/upload
  header:
    name: Authorization
    value: Bearer 9b2e…
  limit_mbit: 40
slots:
  - id: s1
    port: 27015
    tv_port: 27020
    gslt: 0123456789ABCDEF0123456789ABCDEF
  - id: s2
    port: 27016
    tv_port: 27021
    gslt: FEDCBA9876543210FEDCBA9876543210
```

[table:agent-yml]

- A nested key is required only when its parent is present.
- The agent warns about keys it does not know and ignores them.
- A file that fails validation stops the agent, and its log lists each invalid key.
- A controller on a private address or `localhost`, including `127.0.0.1:<port>` on the node's
  host, usually needs `tls: false` under `controller`. No certificate authority issues certificates for
  those addresses.

## Changing settings

The controller can change `slots`, `pinned_ip`, `auto_update`, `controller`, `demo_upload` and
`memory_mb` while the node is connected, with [agent:set_config].

- The agent refuses an edit that would disturb an occupied slot. It also refuses any change to the
  controller address while a slot is occupied. The
  [`AgentErrorCode`](/reference/types/#schema-AgentErrorCode) in its reply names the case. Stop new
  launches on the node in the controller, wait for the slots to go idle, then edit again.
- Demo settings and changes to idle slots apply while other slots are occupied.
- An offline node cannot receive a corrected address. Fix `controller.host` in the file and restart the
  agent.
- A slot's login token can be changed while the slot runs a server. The server keeps the token it
  started with until it ends. Revoking that token at Steam drops the server, so
  [replace a token](operations.md#replace-a-slots-token) once its slot is idle.

Edit the other keys in the file on the host, then [restart the agent](operations.md#restart-the-agent).

## File layout

Everything a node keeps is under `/srv/ankor/` or in the Docker volume `ankor_cs2_data`.

### In the game container

Node paths are relative to `/srv/ankor/`, and container paths to `/home/steam/cs2-dedicated/game/`.
Each column is a tree. A row names its path relative to the row it is indented under. The rest of
the install is the Docker volume `ankor_cs2_data`. Every server shares it read-only, and only a CS2
update writes to it.

<div class="tree-table">

| On the node                                                                                                                                                                                                                                                                                      | From                                                                           | In the game container                                                                                                                                                          | Access                                   |
|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------|
| <span class="branch"></span>`overlay/<image id>/`                                                                                                                                                                                                                                                | CS2 image                                                                      | <span class="branch"></span>`csgo/`                                                                                                                                            |                                          |
| <span class="branch">├─ </span>`addons/`                                                                                                                                                                                                                                                         | CS2 image                                                                      | <span class="branch">├─ </span>`addons/`                                                                                                                                       | <span class="read-only">Read-only</span> |
| <span class="branch">│  └─ </span>`swiftlys2/`                                                                                                                                                                                                                                                   |                                                                                | <span class="branch">│  └─ </span>`swiftlys2/`                                                                                                                                 |                                          |
| <span class="branch">│     ├─ </span>`plugins/Ankor/`                                                                                                                                                                                                                                            | CS2 image                                                                      | <span class="branch">│     ├─ </span>`plugins/Ankor/`                                                                                                                          | <span class="read-only">Read-only</span> |
| <span class="branch">│     └─ </span>`configs/plugins/Ankor/`                                                                                                                                                                                                                                    | CS2 image                                                                      | <span class="branch">│     └─ </span>`configs/plugins/Ankor/`                                                                                                                  | <span class="read-only">Read-only</span> |
| <span class="branch">└─ </span><span class="tip" tabindex="0" data-tip="Ankor&#39;s cfgs. A launch may exec any of them.">`cfg/ankor/`</span>                                                                                                                                                    | CS2 image                                                                      | <span class="branch">└─ </span>`cfg/ankor/`                                                                                                                                    | <span class="read-only">Read-only</span> |
| <span class="branch"></span>`slots/<slot id>/game/csgo/`                                                                                                                                                                                                                                         | Slot                                                                           | <span class="branch"></span>`csgo/`                                                                                                                                            |                                          |
| <span class="branch">├─ </span>`addons/swiftlys2/`                                                                                                                                                                                                                                               |                                                                                | <span class="branch">├─ </span>`addons/swiftlys2/`                                                                                                                             |                                          |
| <span class="branch">│  ├─ </span><span class="tip" tabindex="0" data-tip="The launch&#39;s chat tag for Ankor&#39;s plugin. The agent writes it for each launch, over the image&#39;s copy.">`configs/plugins/Ankor/config.jsonc`</span>                                                        | Launch                                                                         | <span class="branch">│  ├─ </span>`configs/plugins/Ankor/config.jsonc`                                                                                                         | <span class="read-only">Read-only</span> |
| <span class="branch">│  ├─ </span>`data/`                                                                                                                                                                                                                                                        | Slot                                                                           | <span class="branch">│  ├─ </span>`data/`                                                                                                                                      | <span class="read-write">Writable</span> |
| <span class="branch">│  ├─ </span>`logs/`                                                                                                                                                                                                                                                        | Slot                                                                           | <span class="branch">│  ├─ </span>`logs/`                                                                                                                                      | <span class="read-write">Writable</span> |
| <span class="branch">│  └─ </span>`dumps/`                                                                                                                                                                                                                                                       | Slot                                                                           | <span class="branch">│  └─ </span>`dumps/`                                                                                                                                     | <span class="read-write">Writable</span> |
| <span class="branch">├─ </span>`cfg/`                                                                                                                                                                                                                                                            |                                                                                | <span class="branch">├─ </span>`cfg/`                                                                                                                                          |                                          |
| <span class="branch">│  └─ </span><span class="tip" tabindex="0" data-tip="The launch&#39;s hostname, GOTV settings, match rules, cfgs and commands. The agent writes it for each launch.">`ankor_launch.cfg`</span>                                                                             | Launch                                                                         | <span class="branch">│  └─ </span>`ankor_launch.cfg`                                                                                                                           | <span class="read-only">Read-only</span> |
| <span class="branch">└─ </span><span class="tip" tabindex="0" data-tip="The slot&#39;s match file and round backups, replaced at each launch.">`ankor/`</span>                                                                                                                                   | Slot                                                                           | <span class="branch">└─ </span>`ankor/`                                                                                                                                        | <span class="read-write">Writable</span> |
| <span class="branch">   ├─ </span><span class="tip" tabindex="0" data-tip="The match the plugin plays. The agent writes it for each launch with a match.">`match.json`</span>                                                                                                                    | Launch                                                                         | <span class="branch">   ├─ </span>`match.json`                                                                                                                                 | <span class="read-only">Read-only</span> |
| <span class="branch">   ├─ </span><span class="tip" tabindex="0" data-tip="The round backup that a restore loads. The agent writes it for each launch with a restore.">`restore.txt`</span>                                                                                                      | Launch                                                                         | <span class="branch">   ├─ </span>`restore.txt`                                                                                                                                | <span class="read-only">Read-only</span> |
| <span class="branch">   ├─ </span><span class="tip" tabindex="0" data-tip="The round backups the game writes during a match, one ankor_roundNN.txt per round. The plugin sends each one to the controller, a restore_round loads one, and the next launch empties the folder.">`backups/`</span> | Launch                                                                         | <span class="branch">   ├─ </span>`backups/`                                                                                                                                   | <span class="read-write">Writable</span> |
| <span class="branch">   └─ </span><span class="tip" tabindex="0" data-tip="The server&#39;s demos, until they are uploaded or deleted. Each launch gets its own folder, named by its allocation id, so an upload names the server that recorded it.">`demos/<allocation id>/`</span>             | Launch                                                                         | <span class="branch">   └─ </span>`demos/`                                                                                                                                     | <span class="read-write">Writable</span> |
| <span class="branch"></span><span class="tip" tabindex="0" data-tip="Your cfgs. A launch runs one as exec node/<name>.">`cfg/`</span>                                                                                                                                                            | [<span class="yours">Node cfgs</span>](operations.md#add-cfgs-for-servers)     | <span class="branch"></span>`csgo/cfg/node/`                                                                                                                                   | <span class="read-only">Read-only</span> |
| <span class="branch"></span>`swiftlys2/`                                                                                                                                                                                                                                                         | [<span class="yours">Node plugins</span>](operations.md#add-swiftlys2-plugins) | <span class="branch"></span>`csgo/addons/swiftlys2/`                                                                                                                           |                                          |
| <span class="branch">├─ </span>`plugins/<plugin>/`                                                                                                                                                                                                                                               | [<span class="yours">Node plugins</span>](operations.md#add-swiftlys2-plugins) | <span class="branch">├─ </span><span class="tip" tabindex="0" data-tip="Ankor&#39;s plugin and the node plugins, the only plugins SwiftlyS2 loads.">`plugins/<plugin>/`</span> | <span class="read-only">Read-only</span> |
| <span class="branch">└─ </span>`configs/plugins/<plugin>/`                                                                                                                                                                                                                                       | [<span class="yours">Node plugins</span>](operations.md#add-swiftlys2-plugins) | <span class="branch">└─ </span><span class="tip" tabindex="0" data-tip="Each plugin&#39;s config folder.">`configs/plugins/<plugin>/`</span>                                   | <span class="read-write">Writable</span> |
| <span class="branch"></span><span class="tip" tabindex="0" data-tip="Workshop maps. Every slot shares them, so a map downloads once per node.">`shared/game/bin/linuxsteamrt64/steamapps/`</span>                                                                                                | Every slot                                                                     | <span class="branch"></span>`bin/linuxsteamrt64/steamapps/`                                                                                                                    | <span class="read-write">Writable</span> |

</div>

A launch runs your cfgs from `cfg/` by naming them in `exec`. The protocol guide lists
[when they run](protocol/index.md#when-the-cfgs-run), and their order among the game mode's cfg and
Ankor's.

<a href="https://swiftlys2.net/docs/development/configuration" target="_blank" rel="noopener">SwiftlyS2</a> gives a
plugin its paths through these functions. They map to these directories on the node:

<div class="tree-table">

| SwiftlyS2                                                                                                                                                                                            | On the node                                                            |
|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------|
| [`Core.PluginPath`](https://swiftlys2.net/api-docs/stable/shared/iswiftlycore#swiftlys2-shared-iswiftlycore-pluginpath)                                                                              | `/srv/ankor/swiftlys2/plugins/<plugin>/`                               |
| [`Core.Configuration.GetConfigPath()`](https://swiftlys2.net/api-docs/stable/services/ipluginconfigurationservice#swiftlys2-shared-services-ipluginconfigurationservice-getconfigpath-system-string) | `/srv/ankor/swiftlys2/configs/plugins/<plugin>/`                       |
| [`Core.PluginDataDirectory`](https://swiftlys2.net/api-docs/stable/shared/iswiftlycore#swiftlys2-shared-iswiftlycore-plugindatadirectory)                                                            | `/srv/ankor/slots/<slot id>/game/csgo/addons/swiftlys2/data/<plugin>/` |

</div>

Data and logs stay across the slot's launches.

### Only on the node

No game container mounts these. Only the agent reads and writes them.

<div class="tree-table">

| On the node                           | Contents                                                                                                                                                            |
|---------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `/srv/ankor/agent.yml`                | The node's [configuration](#agentyml), including its node token.                                                                                                    |
| `/srv/ankor/agent-update-failed.json` | Why the last agent update failed. The agent reports it to the controller until an agent update succeeds or the setup script starts the agent.                       |
| `/srv/ankor/events/`                  | The agent's SQLite database of gameplay events and ended servers that wait for the controller. Its layout is internal to the agent. An update upgrades it in place. |
| `/srv/ankor/gslt-refused.json`        | The SHA-256 of each login token Steam refused, so the agent never logs one on again. Deleting it and restarting the agent makes it check every token once more.     |

</div>
