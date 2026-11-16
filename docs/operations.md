---
title: Operating a node
description: Test servers, updates, restarts and demos on a running node.
slug: operations
---

Most actions on an installed node are requests from the controller to the agent.
[ankor.sh](ankor-web.md#node-actions) shows where each action is in [ankor.sh](https://ankor.sh).

## Launch a test server

Before a node runs real matches, have the controller launch a server without a match on it. This
is a [agent:start_server] with `match` set to `null`. Join the server to check that CS2 is
installed, the ports are open and players can get in. The node needs an idle slot with a valid GSLT.

## Automatic updates

While [config:auto_update] is on, as it is by default, the node updates CS2 and both images by
itself. It updates them together, because a CS2 update can break the plugin or SwiftlyS2 until a
fixed `ankor-cs2` release is out.

| What               | Source of new versions           | When it updates                                          |
|--------------------|----------------------------------|----------------------------------------------------------|
| The CS2 game build | Steam's public build             | Once every slot is idle. [Update CS2](#update-cs2)       |
| The CS2 image      | The image's tags in its registry | At the next registry read. Running servers carry on      |
| The agent image    | The image's tags in its registry | At the next registry read. Running servers carry on      |

### Images

Nodes run the `ghcr.io/ankorcs/ankor-agent` and `ghcr.io/ankorcs/ankor-cs2` images, so the node
must be able to reach `ghcr.io`. Your controller names their tags in `agent.yml`.

- Each release is tagged `X.Y.Z`, `X.Y` and `X`. The newest release of all is also tagged
  `latest`, which crosses majors.
- A release line is a major, such as `:1`. For a `0.x` image, it is a minor, such as `:0.10`,
  because [SemVer](https://semver.org/#spec-item-4) lets a `0.x` minor break compatibility.
- Both images and the protocol share a line, so `ankor-agent:0.10` and `ankor-cs2:0.10` work
  together.
- Only a new line can break compatibility. The agent can break the protocol, and
  [Versioning](protocol/versioning.md) lists which releases support each version. The CS2 image can
  break the software it carries. For example, a new SwiftlyS2 major requires node plugins to be
  rebuilt.
- Only the newest release is maintained, because only it receives fixes for CS2 updates. It is
  rebuilt each week on updated base images under the same tags. Run the newest line.
- Your controller chooses the line, and moves a node with [agent:update_agent] and
  [agent:update_cs2_image]. An update to a line's tag gives the newest release of that line.

### Follow image releases

The agent reads the image tags from the registry, and waits
[limit-time:IMAGE_UPDATE_POLL_INTERVAL_MS] between reads. It also reads them when it starts, when [config:auto_update] is turned on, and after a manual
update to other images. It then updates each image, the CS2 image first:

- to a newer `X.Y.Z` release on its line, or to any newer release on `latest`
- otherwise, to the [weekly rebuild](#images) of its release, when the registry's digest for the
  running version differs from the running image.

The node reports each update in [agent:node_status], as it reports a manual one.

The agent follows an image only when `agent.yml` names it by `X`, `X.Y`, `X.Y.Z` or `latest`. An
image named by digest or by another tag stays on its build. The new reference keeps the precision
of the tag, so `:1.2` becomes `:1.3` and `:1` stays `:1`. ankor.sh issues `latest`, and a
controller that pins a line issues its tag, such as `:0.10`.

- While [config:auto_update] is on, the next registry read undoes a manual update to an older
  release. To stay on that release, name the image by digest or
  [disable updates](#disable-updates).
- If a pull fails, the agent tries again at the next read. If other maintenance refuses the
  update, the agent tries again when that maintenance ends.
- If an agent update fails, the node keeps the old agent and reports the failure. The agent then
  skips that build and waits for a newer release or rebuild. If the update helper crashed and
  rolled back instead, the agent tries the same build again.
  [Update the agent](#update-the-agent) describes the swap.
- The agent reads the version from the image's `org.opencontainers.image.version` label, and
  leaves an image without it alone. It reads any registry that allows anonymous reads over HTTPS,
  Docker Hub included.

### Disable updates

To keep a node on its CS2 build and images, for example at a LAN event, turn [config:auto_update]
off. Your controller sends [agent:set_config], or you set it to `false` in `agent.yml` and
[restart the agent](#restart-the-agent).

The node then updates nothing by itself, and it accepts launches even when a newer public build is
out. Players whose clients have updated cannot join its servers. Manual updates still run. When
the setting is turned back on, the images update immediately, and CS2 updates once every slot is
idle.

## Update CS2

Once CS2 is installed, the node keeps it up to date by itself, unless you
[disable updates](#disable-updates). When Valve releases a new build, the node
stops accepting launches, since players on the new client cannot join the old build. Running
servers play on, and the node updates as soon as every slot is idle. It makes one attempt per
build, and one more after an agent restart. After a failed attempt, the node accepts no launches
until the next build or a manual update succeeds.

A manual update, [agent:update_cs2], also installs CS2 the first time. It too is refused while any
slot is busy. It takes one of the [update modes](/reference/types/#schema-Cs2UpdateMode). `verify`
repairs a damaged installation. `reinstall` starts over when updates keep failing.

An update usually takes a few minutes and sometimes an hour. The time depends on the host's CPU,
its connection and the size of the update. Each mode refuses to start without enough free space on
the install volume:

| Mode               | Free space needed on the install volume             |
|--------------------|-----------------------------------------------------|
| `update`, `verify` | [limit:CS2_UPDATE_MIN_FREE_GB] GB                   |
| `reinstall`        | [limit:CS2_REINSTALL_MIN_FREE_GB] GB after the wipe |

## Update the CS2 image

The plugin, SwiftlyS2 and the server configuration ship together in the `ankor-cs2` image.
[agent:update_cs2_image] switches the node to another version. Only servers launched afterwards
use it. [Images](#images) explains the version tags. The node also
[follows image releases](#follow-image-releases) by itself.

## Update the agent

[agent:update_agent] replaces the agent's container with another version of the agent image.
Running servers [carry on](how-it-works.md#updates-and-restarts). The node also
[follows image releases](#follow-image-releases) by itself.

The agent cannot replace the container it runs in. Instead, it starts a helper container,
`ankor-self-update-<id>`, from the new image, and the helper makes the swap from outside. When the
old agent creates the helper's container, it describes the swap in three environment variables. If
one is missing, the helper exits with an error before it touches a container:

| Variable                     | Holds                                                                                                                                     |
|------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------|
| `ANKOR_SELF_UPDATE_PREVIOUS` | the id of the agent container that the helper replaces.                                                                                   |
| `ANKOR_SELF_UPDATE_SPEC`     | the Docker create request for the new agent's container as JSON, with the old container's environment, labels, mounts and restart policy. |
| `ANKOR_SELF_UPDATE_FAILURE`  | the path of `agent-update-failed.json`, in the data directory that the helper and both agents mount.                                      |

The new agent receives the old container's environment through `ANKOR_SELF_UPDATE_SPEC`. The
variables that the old image set are left out, because the new image sets its own. The helper
passes its result back through `agent-update-failed.json` to whichever agent runs after the swap.

<figure class="diagram diagram-seq three" aria-label="The old agent creates the helper with the three variables. The helper stops the old agent and renames it, then creates and starts the new agent under its name, which connects while the helper waits. If the new agent stays up, the helper removes the old one and the new agent reports the update as done. If not, the helper removes the new agent, renames the old one back and starts it, and records the failure, which the old agent reports.">
<div class="diagram-seq-head"><div class="diagram-card"><p class="diagram-name">Old agent</p><p class="diagram-sub"><code>ankor-agent</code></p></div><div class="diagram-card"><p class="diagram-name">Helper</p><p class="diagram-sub"><code>ankor-self-update-&lt;id&gt;</code></p></div><div class="diagram-card"><p class="diagram-name">New agent</p><p class="diagram-sub"><code>ankor-agent</code></p></div></div>
<div class="diagram-seq-body">
<div class="diagram-msg right between-1-2 from-agent"><p class="diagram-msg-label">Starts the helper<small>from the new image</small></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The helper reads <code>ANKOR_SELF_UPDATE_PREVIOUS</code>, <code>ANKOR_SELF_UPDATE_SPEC</code> and <code>ANKOR_SELF_UPDATE_FAILURE</code>.</p>
<div class="diagram-msg left between-1-2 quiet"><p class="diagram-msg-label">Renames it <code>ankor-agent.previous</code><small>after stopping it</small></p><span class="diagram-arrow"></span></div>
<div class="diagram-msg right between-2-3 quiet"><p class="diagram-msg-label">Starts it as <code>ankor-agent</code><small>from <code>ANKOR_SELF_UPDATE_SPEC</code></small></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The new agent connects and reports the update as under way in [agent:node_status], while the helper waits 10 seconds for it to stay up without a restart.</p>
<p class="diagram-note">If the new agent held:</p>
<div class="diagram-msg left between-1-2 quiet"><p class="diagram-msg-label">Removes <code>ankor-agent.previous</code></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The helper deletes <code>agent-update-failed.json</code> and exits 0. The new agent reports the update as done.</p>
<p class="diagram-note">If a step failed, or the new agent exited or restarted:</p>
<div class="diagram-msg right between-2-3 implied refused"><p class="diagram-msg-label">Removes the new agent</p><span class="diagram-arrow"></span></div>
<div class="diagram-msg left between-1-2 implied refused"><p class="diagram-msg-label">Renames it back and starts it</p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The helper writes <code>agent-update-failed.json</code> and exits 0. The old agent connects again and reports the failed update.</p>
</div>
</figure>

1. The helper stops `ankor-agent` and renames it `ankor-agent.previous`, which frees the name.
2. The helper creates the new agent's container as `ankor-agent`, with the old container's settings
   and the new image, and starts it. The new agent connects to the controller while the helper
   waits, and reports the update as under way.
3. If the new agent stays up for 10 seconds without a restart, the helper removes
   `ankor-agent.previous`. It also deletes `agent-update-failed.json`, so an earlier failed update
   is no longer reported. The new agent reports the update as done.
4. If a step fails, or the new agent exits or restarts within those 10 seconds, the helper undoes
   the steps that ran. It removes the new container, renames the old one back to `ankor-agent` and
   starts it again. If the helper could not stop the old agent, nothing needs undoing. Either way,
   the helper writes the reason to `agent-update-failed.json` in the
   [data directory](configuration.md#only-on-the-node). The old agent reports the failed update.
5. If the helper crashes or runs out of memory, Docker restarts it, up to 3 times. The rerun checks
   the old agent:
   - If the old agent is gone and the new agent runs, the swap held. The helper reports the update
     as done.
   - If the old agent is gone and no new agent runs, the helper records the failure.
   - If the old agent is stopped or renamed, the helper rolls the swap back as in step 4.
   - If Docker refuses to show the old agent, the helper exits with an error, and Docker runs it
     again.

   A helper stopped with `docker kill` stays stopped, because Docker counts it as a manual stop.

Every agent reports updates in [agent:node_status]. After a rollback, the controller therefore
sees the new version send [agent:hello] and disconnect, and then the old version connect again.

The helper's container stays on the host after it exits, as the record of the swap. The next agent
update removes it. Its log shows how the swap ended and what the helper rolled back:

```sh
docker ps -a --filter name=ankor-self-update-
docker logs ankor-self-update-<id>
```

The agent removes the images that an update replaced once no container uses them. It checks after
each update and when it starts.

### Update the agent from the host

If the agent cannot connect, or the controller rejects its protocol version, it cannot receive
[agent:update_agent]. Update it on the host instead, with `--update` of the
[setup script](setup-script.md):

```sh
curl -fsSL https://ankor.sh/node-setup.sh | bash -s -- --update
```

## Restart the agent

Restarting the agent is [safe while servers run](how-it-works.md#updates-and-restarts).

```sh
docker restart ankor-agent
```

The agent's log lists the servers it took over and its reconnection to the controller.
[Logs](#logs) shows how to read it.

A server whose slot has been removed from `agent.yml` is stopped at startup. Its demo still uploads.

## One host per node

- [ankor.sh](https://ankor.sh) connects one agent per node token at a time. It closes the socket
  of a second agent with `node_connected`. A second host with the same `agent.yml` therefore keeps
  retrying, and connects only once the first disconnects. A controller of your own should do the
  same, as [Closing the socket](/reference/socket/#closing) describes. To move a node,
  [remove it](install.md#remove-a-node) and set the new host up as a new node.
- A host runs one agent. If an agent finds another agent container running on its host, it stops
  before it changes anything. Its log names both containers. Remove the extra one with
  `docker rm -f <name>`. Re-running setup does not help, because setup replaces only the container
  named `ankor-agent`.

## Logs

The agent keeps its log and the recent output of each server. The controller reads them with
[agent:watch_logs]. On the host:

```sh
docker logs --tail 200 ankor-agent
docker ps -a --filter label=ankor.slot --format 'table {{.ID}}\t{{.Names}}\t{{.Status}}'
docker logs --tail 200 CONTAINER_ID
```

A game server's log is deleted with its container when the server exits.

## Server console

[agent:console_command] runs a console command on a running server. On [ankor.sh](https://ankor.sh) it is under Node
options → **Logs & console**. Use it to inspect a server (`status`), not to steer a match.

**Do not change the game during a match.** The events record every round as it is played.
`mp_restartgame`, a map change, or cvars that change round rules, teams or money therefore leave
statistics that do not match the rounds played. The damage is permanent, because the events
themselves record the repeated rounds and changed rules. To play a match on from an earlier round,
restore it as [Backups and restores](backups.md#restore-a-running-match) describes.

## Replace a slot's token

When Steam refuses a slot's [Game Server Login Token](requirements.md#game-server-login-tokens),
the slot receives no launches until it has a new login token. Steam refuses a token, for example,
after Valve invalidated it. The controller replaces the token on an idle slot, because revoking the
token of a running server drops that server.

[ankor.sh](ankor-web.md#login-tokens) replaces it automatically and on request. For a controller of
your own, [Replace a slot's token](protocol/minimal-controller.md#replace-a-slots-token) has the
steps. The agent logs the new token on immediately and reports whether Steam accepted it.

## Demos

When a match records a demo, the agent uploads the finished file to the node's [config:demo_upload]
target. It deletes the file once uploaded. Without a target, demos stay on the node and fill its
disk over time. [Demo uploads](protocol/demos.md) covers the request, the answers and the retries.

An upload can fill the node's uplink and make running servers lag. [config:demo_upload.limit_mbit]
caps the rate while any slot runs a server. Uploads return to full speed once the node is idle. To
size the cap, reserve about 10 Mbit/s of the uplink for each full server that the node runs at the
same time. Set the cap to the remainder.

## Add SwiftlyS2 plugins

Drop the plugin's folder into `/srv/ankor/swiftlys2/plugins/`. Servers launched afterwards load it
beside Ankor.

Paths in the game container are relative to `/home/steam/cs2-dedicated/game/`.
[File layout](configuration.md#file-layout) shows the whole tree.

<div class="tree-table">

| On the node                                         | Written by                     | In the game container                                         | Access                                   |
|-----------------------------------------------------|--------------------------------|---------------------------------------------------------------|------------------------------------------|
| <span class="branch"></span>`/srv/ankor/swiftlys2/` | Setup                          |                                                               |                                          |
| <span class="branch">├─ </span>`plugins/`           | Setup                          |                                                               |                                          |
| <span class="branch">│  └─ </span>`MyPlugin/`       | <span class="yours">You</span> | `csgo/addons/swiftlys2/plugins/MyPlugin/`                     | <span class="read-only">Read-only</span> |
| <span class="branch">│     └─ </span>`MyPlugin.dll` | <span class="yours">You</span> | `csgo/addons/swiftlys2/plugins/MyPlugin/MyPlugin.dll`         | <span class="read-only">Read-only</span> |
| <span class="branch">└─ </span>`configs/plugins/`   | Setup                          |                                                               |                                          |
| <span class="branch">   └─ </span>`MyPlugin/`       | The agent                      | `csgo/addons/swiftlys2/configs/plugins/MyPlugin/`             | <span class="read-write">Writable</span> |
| <span class="branch">      └─ </span>`config.jsonc` | The plugin, then you           | `csgo/addons/swiftlys2/configs/plugins/MyPlugin/config.jsonc` | <span class="read-write">Writable</span> |

</div>

- The folder is the plugin as it ships, and its DLL has the folder's name.
- The plugin writes its config at its first launch. Edit it there.
- A server loads its plugins and their configs when it starts, so changes to either apply from the
  next launch.
- A plugin must support the SwiftlyS2 major version of the node's CS2 image.
- A folder named `Ankor` is ignored.
- Plugins run in matches too, and their changes to the game appear in the match's events.
- Other plugins are unsupported. They must not interfere with the parts of the game that Ankor's
  plugin controls, such as team assignment, the match flow and the match cvars. Report a conflict
  with Ankor's plugin as an [issue on GitHub](https://github.com/ankorcs/ankor/issues).

### Permissions

The agent and the game servers run as uid 1000. [Setup](setup-script.md) creates the folders above
owned by that user. After copying a plugin in, make it readable for the game servers:

```sh
chmod -R a+rX /srv/ankor/swiftlys2/plugins/MyPlugin
```

## Add cfgs for servers

Your cfgs go in `/srv/ankor/cfg/`, which is read-only in every game container. A server sees them in
`csgo/cfg/node/`, and a launch runs one by listing `node/<name>` in
[`exec`](/reference/types/#schema-ServerLaunchConfig), without `.cfg`.
[When the cfgs run](protocol/index.md#when-the-cfgs-run) shows when a server runs each cfg, and in
which order.

Ankor's own cfgs sit apart from yours, in `csgo/cfg/ankor/`. They come from the CS2 image, and a
file that you add on the node never lands there. A node cfg that a launch runs overrides
`ankor/base`, but not warmup or the knife round.

<div class="tree-table">

| On the node                                      | Written by                     | In the game container       | A launch uses it with    |
|--------------------------------------------------|--------------------------------|-----------------------------|--------------------------|
| <span class="branch"></span>`/srv/ankor/cfg/`    | Setup                          | `csgo/cfg/node/`            |                          |
| <span class="branch">└─ </span>`retakes.cfg`     | <span class="yours">You</span> | `csgo/cfg/node/retakes.cfg` | `exec: ["node/retakes"]` |

</div>

- A server reads a cfg when it runs it, so an edit applies from the next run or launch.
- Make the cfgs readable to the game servers, as Permissions above shows.
