---
title: Configuration and files
description: The node's agent.yml, which settings the controller can change, and where a node keeps its files.
slug: configuration
---

A node keeps its settings in one file, `agent.yml`. This page lists the keys of that file, the
settings that your controller can change while the node is connected, and where the node keeps
its files.

Your controller sets most of the settings. During setup, the [setup script](setup-script.md)
fetches `agent.yml` from the controller and writes it on the host. Later, the controller changes
parts of the file over the agent's connection. For example, it changes the images when it updates
the agent or the CS2 image, which carries Ankor's plugin. You edit the other keys, such as
[config:official_images_only], on the host.

## `agent.yml`

The file is `/srv/ankor/agent.yml`. Its schema is `AgentConfigSchema` in
[agent-config.ts](../packages/protocol/src/agent-config.ts). The agent reads the file when it
starts.

This `agent.yml` describes a node with two slots that [uploads its demos](protocol/demos.md):

```yaml
token: 3f1c…
controller:
  host: cs.example.com
  tls: true
pinned_ip: 203.0.113.7
cs2_image: ghcr.io/ankorcs/ankor-cs2:0.10
agent_image: ghcr.io/ankorcs/ankor-agent:0.10
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

The agent reads the file by these rules:

- A nested key is required only when its parent is present. For example, `demo_upload.url` is
  required only when the file has a `demo_upload`.
- If the file has a key that the agent does not know, the agent logs a warning and ignores the key.
- If the file fails validation, the agent stops. The agent's log lists each invalid key.

If your controller runs on a private address or on `localhost`, you usually need to set
[config:controller.tls] to `false`. This includes `127.0.0.1:<port>` on the node's host. No
certificate authority issues certificates for those addresses.

## Changing settings

While the node is connected, your controller can change these keys with [agent:set_config]:
[config:slots], [config:pinned_ip], [config:auto_update], [config:controller], [config:demo_upload]
and [config:memory_mb]. A slot is occupied while it holds a server that is starting, running or
stopping. The agent refuses some edits while slots are occupied, so that an edit does not break a
server that is running:

- The agent refuses an edit that would remove an occupied slot, change its ports or give its
  server's token to another slot.
- The agent refuses any change under `controller` while any slot is occupied.
- When the agent refuses an edit, the [`AgentErrorCode`](/reference/types/#schema-AgentErrorCode) in
  its reply names the case. In the controller, stop new launches on the node. When the slots are
  idle, send the edit again.
- Demo settings and changes to idle slots apply while other slots are occupied.
- You can change the login token of a slot while the slot runs a server. The server keeps the
  token that it started with until it shuts down. Revoking that token at Steam drops the server,
  so [replace a token](operations.md#replace-a-slots-token) once its slot is idle.
- If the node is offline, the controller cannot send it a corrected address. On the host, fix
  [config:controller.host] in the file and restart the agent.

To change any other key, edit the file on the host, then
[restart the agent](operations.md#restart-the-agent).

## File layout

A node keeps all its files under `/srv/ankor/` on the host and in the Docker volume
`ankor_cs2_data`. The volume holds the CS2 install. The agent mounts some of the files under
`/srv/ankor/` into each game container, and keeps the others to itself.

### In the game container

Every server shares the CS2 install in `ankor_cs2_data` read-only. Only a CS2 install or update
writes to the volume. The agent mounts the paths in this table over the install. They give each
slot its writable directories and each launch its files.

The table holds two trees, one in each path column. Paths **On the node** are relative to
`/srv/ankor/`, and paths **In the game container** are relative to
`/home/steam/cs2-dedicated/game/`. Each row names its path relative to the row that it is indented
under. **From** names the source of the files:

- **CS2 image**: the agent extracts the files from the CS2 image that the slot's server runs.
- **Slot**: the slot keeps the files across its launches.
- **Launch**: the files belong to one launch. The agent sets them up when the launch starts.
- **Node cfgs** and **Node plugins**: you add the files on the host.
- **Every slot**: all slots on the node share the files.

**Access** says whether the server can write to the path.

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
| <span class="branch">└─ </span><span class="tip" tabindex="0" data-tip="The slot&#39;s match file and round backups. The agent empties backups/ when the next launch starts.">`ankor/`</span>                                                                                                    | Slot                                                                           | <span class="branch">└─ </span>`ankor/`                                                                                                                                        | <span class="read-write">Writable</span> |
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

A launch runs one of your cfgs from `cfg/` when its
[`exec`](/reference/types/#schema-ServerLaunchConfig) lists `node/<name>`.
[Add cfgs for servers](operations.md#add-cfgs-for-servers) has the steps. The protocol guide
shows [when the cfgs run](protocol/index.md#when-the-cfgs-run), and their order among the game
mode's cfg and Ankor's cfgs.

A plugin gets its directories from
<a href="https://swiftlys2.net/docs/development/configuration" target="_blank" rel="noopener">SwiftlyS2</a>
through these functions. Each function returns a path in the game container, which maps to this
directory on the node:

<div class="tree-table">

| SwiftlyS2                                                                                                                                                                                            | On the node                                                            |
|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------|
| [`Core.PluginPath`](https://swiftlys2.net/api-docs/stable/shared/iswiftlycore#swiftlys2-shared-iswiftlycore-pluginpath)                                                                              | `/srv/ankor/swiftlys2/plugins/<plugin>/`                               |
| [`Core.Configuration.GetConfigPath()`](https://swiftlys2.net/api-docs/stable/services/ipluginconfigurationservice#swiftlys2-shared-services-ipluginconfigurationservice-getconfigpath-system-string) | `/srv/ankor/swiftlys2/configs/plugins/<plugin>/`                       |
| [`Core.PluginDataDirectory`](https://swiftlys2.net/api-docs/stable/shared/iswiftlycore#swiftlys2-shared-iswiftlycore-plugindatadirectory)                                                            | `/srv/ankor/slots/<slot id>/game/csgo/addons/swiftlys2/data/<plugin>/` |

</div>

Each slot has its own `data/` and `logs/` directories. The slot keeps them across its launches,
so a plugin's data stays with the slot and is not shared with other slots.

### Only on the node

No game container mounts these files. The agent reads and writes them.

<div class="tree-table">

| On the node                           | Contents                                                                                                                                                            |
|---------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `/srv/ankor/agent.yml`                | The node's [configuration](#agentyml), including its node token.                                                                                                    |
| `/srv/ankor/agent-update-failed.json` | Why the last agent update failed. The agent reports it to the controller until an agent update succeeds or the setup script starts the agent.                       |
| `/srv/ankor/events/`                  | The agent's SQLite database of gameplay events and ended servers that wait for the controller. Its layout is internal to the agent. An update upgrades it in place. |
| `/srv/ankor/gslt-refused.json`        | The SHA-256 of each login token Steam refused, so the agent never logs one on again. Deleting it and restarting the agent makes it check every token once more.     |

</div>
