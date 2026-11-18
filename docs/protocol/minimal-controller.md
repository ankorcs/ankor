---
title: A minimal controller
description: Build a simple controller that sets up an Ankor node and starts a CS2 server on it.
slug: protocol/minimal-controller
---

These steps explain what's required to build a simple controller that can accept and communicate
with an official Ankor node. The examples use `cs.example.com` for your controller, but you can
replace that with your LAN address for testing. Normally, an "operator" is someone who connects
a node to a controller. The "operator" in this context is likely yourself.

## Talking to the agent

The [protocol overview](index.md) describes how to accept a node's connection and exchange messages
with its agent. In Node.js, the `@ankorcs/protocol` package runs that exchange with a session,
which matches each reply to its request, because a WebSocket has no request-response cycle like
HTTP. [A Node.js example](index.md#a-nodejs-example) shows the package and the code that accepts
a node.

Agents may run a newer protocol version than your controller. [Versioning](versioning.md) lists
what your controller must accept from them, and what it skips.

## Setup

### 1. Create the node and its token

Generate a [node token](index.md#node-tokens). Show the token to the operator once and store only
its SHA-256.

### 2. Give the operator the setup command

The operator sets up the host by following [Install a node](../install.md). That page covers
everything that happens on the host, including the packages each distribution needs, opening
the game ports and testing on your LAN. Your controller's part is the setup command. Show the
operator the [setup script](../setup-script.md) command with `--controller` set to your
controller's address or domain:

```sh
curl -fsSL https://ankor.sh/node-setup.sh | bash -s -- --controller https://cs.example.com
```

### 3. Answer the setup request

The script posts the token to your controller's [setup endpoint](node-setup.md). Look the node
up by the token's SHA-256, then reply with the node's `agent.yml`. Put the token from the
request in that file, since your controller stores only its hash. You can send an empty
`slots` array and populate it later.

```yaml
token: <the token from the request>
controller:
  host: cs.example.com
  tls: true
cs2_image: ghcr.io/ankorcs/ankor-cs2:0.10
agent_image: ghcr.io/ankorcs/ankor-agent:0.10
slots: []
```

<div class="hint">

In TypeScript, `renderAgentYml` from `@ankorcs/protocol/agent-yml` writes this file.

</div>

If you don't have a setup endpoint on your controller (yet), you'll have to write
[the `agent.yml` file](../setup-script.md#write-agentyml-yourself) on the node
manually before running the script.

### 4. Accept the socket and welcome the node

The agent connects to the [config:controller.socket_path] with its token and its
[protocol version](versioning.md). [Accept the upgrade](index.md#1-accept-the-connection) only when
both check out, then answer its [agent:hello] with [agent:welcome]. A welcome carries no data. It
acknowledges the node, which then starts reporting to your controller.
[A Node.js example](index.md#a-nodejs-example) has the code for this step, with `@ankorcs/protocol`
and the `ws` package.

### 5. Add slots

If the node's `agent.yml` has no slots, send [agent:set_config] to add them. Give each slot its
ports and a [Game Server Login Token](../requirements.md#game-server-login-tokens). The `slots`
array replaces the node's whole slot layout, so always send every slot the node should have:

```json
{
  "type": "set_config",
  "id": "r1",
  "config": {
    "slots": [
      { "id": "s1", "port": 27015, "tv_port": 27020, "gslt": "<GSLT>" }
    ]
  }
}
```

The reply carries the node's settings with the new slots. A [agent:slot_status] follows for each
new slot. [A Node.js example](index.md#a-nodejs-example) sends this request with the session and
stores the reply.

A controller can create the GSLTs with a user's Steam Web API key.
[`IGameServersService`](https://partner.steamgames.com/doc/webapi/IGameServersService)
`CreateAccount`, with `appid` `730` and a `memo`, returns the `login_token`. `GetAccountList`,
`ResetLoginToken` and `DeleteAccount` manage tokens later. [ankor.sh](../ankor-web.md#login-tokens)
creates its tokens this way. You can also create GSLTs by hand and assign them to slots. The
agent accepts any valid token.

### 6. Install CS2

Send [agent:update_cs2] with `mode` `update`, then wait until `cs2.update` in [agent:node_status]
has an `outcome`. The installation usually takes about 30–40 minutes, depending on the node's
connection. [Install CS2](index.md#5-install-cs2) explains the reports sent to your controller
during an update.

### 7. Start a server

Create and store a new allocation id, then send it in [agent:start_server]. [Keep track of every
server](#keep-track-of-every-server) explains why. This launch leaves out every field that has a
default, so it has no `match`. The node then runs a plain CS2 server on `de_dust2` in competitive
mode. Ankor's plugin does nothing to the game. `map_name`, `exec`, `commands` and any plugins
installed on the node determine the game instead.
[`ServerLaunchConfig`](/reference/types/#schema-ServerLaunchConfig)
lists the defaults. The agent replies with the allocation and the slot it picked.

<div class="message-cards">

<div class="message-card from-controller">
<p class="message-card-head">[agent:start_server]<span class="message-card-direction">controller → agent</span></p>

```json
{
  "type": "start_server",
  "id": "r3",
  "allocation_id": "e7a3d9c1-6b2f-4e8a-9c5d-1f0b3a7e2d64",
  "launch": {
    "hostname": "Ankor",
    "map_name": "de_dust2",
    "max_players": 10,
    "idle_shutdown_seconds": null,
    "exec": [],
    "commands": []
  }
}
```

</div>

<div class="message-card from-agent">
<p class="message-card-head">[agent:reply]<span class="message-card-direction">agent → controller</span></p>

```json
{
  "type": "reply",
  "reply_to": "r3",
  "request": "start_server",
  "ok": true,
  "data": {
    "allocation_id": "e7a3d9c1-6b2f-4e8a-9c5d-1f0b3a7e2d64",
    "slot_id": "s1"
  }
}
```

</div>
</div>

### 8. Hand out the address

Wait for the slot's [agent:slot_status] with `state` `running`. Players connect to the node's
address and the slot's `port`, both from the latest [agent:hello] or [agent:set_config] reply.
The address is `config.pinned_ip`, or `public_ip` when none is pinned. A `public_ip` of null
means the node found no public address, so set `pinned_ip` with [agent:set_config] first.
A player then connects from the CS2 console as usual:

```text
-- if WAN/public IP
connect 203.0.113.7:27015
-- if LAN/local IP was set
connect 192.168.1.20:27015
```

[agent:stop_server] shuts down the server. To run matches that collect statistics and events,
launch with a [`MatchConfig`](/reference/types/#schema-MatchConfig) whose `send_statistics` is
`true`, and accept [event uploads](events.md) on your controller. To store demos, set
[config:demo_upload] and accept [demo uploads](demos.md).

## Keep track of every server

Your controller and the node work at the same time, so the messages for one server do not always
arrive in the order you expect:

- The reply to a request may never come, for example when the connection drops.
- The node may disconnect and reconnect while a launch is in progress.
- You can stop a server while its launch is still in progress, before the `start_server` reply
  arrives. [Stops](#stops) explains what the node does then.

Store each server's state on your controller, and change it only when the node reports a change.
Treat a server as stopped only once the node confirms the stop.

### Launches

- Pick a new `allocation_id` (prefer UUIDv7) for every launch and store it before you send
  [agent:start_server].
- Rank nodes by their latest reports with `slotTakesLaunch` and `cs2Behind` from
  `@ankorcs/protocol/helpers`. The node still decides based on available slots, and may refuse.
- The launch has succeeded once the reply to the start request or a [agent:slot_status] names its
  slot, whichever comes first. Players can join once that slot's `state` is `running`.
- A [`StartRefusal`](/reference/types/#schema-StartRefusal) means no server started. If you have
  several nodes, retry the launch on the next one.
- `allocation_in_use` answers a [agent:start_server] sent again with the same `allocation_id`. It
  means the first launch is starting or running on that node. Wait for its [agent:slot_status]
  instead of retrying on a different node, or you may inadvertently launch the allocation twice.
- Any other error, or no reply at all, *may* leave a server running, because the node's reports can
  arrive after the reply would have. Send [agent:stop_server] to the failing node before you launch
  the allocation anywhere else. When using the session helper, "no reply" means a `timeout` or
  `closed` result.

### Stops

- You can send a stop while the server's launch is still waiting for its reply. The node finishes
  the launch's current step, which can take minutes after an image switch. It then ends the launch
  without starting the server, and the reply to the start is an error. If the stop arrives while
  Docker starts the container, the launch succeeds and the node stops the server right after.
- The node replies to a stop once the server's container has stopped. That takes up to
  [limit:STOP_TIMEOUT_SECONDS] seconds for the server to shut down, and up to
  [limit:STOP_REPLY_MARGIN_SECONDS] seconds more if Docker has to kill it. The session waits
  this long for a stop's reply, even if you set a shorter timeout on the session or the call.
  During a launch, the reply can arrive after the session gives up.
- `unknown_allocation` means no server runs under the allocation. `failed` leaves the server
  running, so send the stop again.

### Disconnects and restarts

- A disconnected node may still run its servers. Keep their records until the node reconnects.
- Compare each [agent:hello] with your records. If the `hello` no longer lists a server that you
  have a record of, that server has ended. A launch whose reply never came is running if the
  `hello` lists it. Stop any server that you have no record of.
- `server_exits` in the `hello` gives the reason for each server that ended while the node was
  away. A server with no entry there, and no [agent:server_exited] after the [agent:welcome], ended
  without the agent knowing why.
- A server can end after the agent sent its [agent:hello] but before your [agent:welcome]
  arrived. The agent then reports it as a [agent:server_exited] right after the `welcome`.
- You can receive the same exit twice, in the `hello` and again as a `server_exited`. Record
  each exit by its `allocation_id` and ignore one you already have.
- Keep [one socket per node](index.md#1-accept-the-connection). The node will never open more than
  one connection at a time.

### Logs

A node keeps several logs, called sources. A source is the agent's log, the output of one CS2
install or update, or the output of one server. The agent streams a source only while your
controller asks for it, so a node with no one watching sends no log traffic.

- Send [agent:watch_logs] for a source when someone opens it in your UI. The request starts a
  lease of [limit-time:WATCH_LEASE_MS]. Send it again before the lease runs out to keep the lines
  coming. If you want several sources, send a separate request for each.
- Pass a `replay_id` when a viewer opens a source, and the agent first sends the lines it already
  has for that source. When the viewer is already watching and you only renew the lease, set
  `replay_id` to null.
- Lines arrive in [agent:log_batch] frames. Apply each one to the lines you hold with
  `withLogBatch` from `@ankorcs/protocol/helpers`. A batch with `reset` replaces everything you
  hold for that source, which is how a replay starts.
- A line can carry ANSI colour codes (SGR, such as `ESC[32m`), exactly as the server printed
  them. Render them as colours, or strip each `ESC[` … `m` to show plain text. The agent has
  already removed every other escape and control character, so a line cannot move the cursor,
  open a link or write to the clipboard in your viewer. A line cut at the length limit can end in
  part of a colour code. Drop that part. [`LogLine`](/reference/types/#schema-LogLine) has the
  details.
- While any source is watched, the agent also sends [agent:log_index], which lists every source
  the node keeps. Use it to show which logs a viewer can open. If a source is missing from it,
  the node has deleted that log.
- The agent answers a [agent:console_command] once the server's plugin has handed the command to
  the engine. The answer does not tell you whether the command worked. The command's output
  arrives as lines in that server's log, so watch the server's source to show it.

### Browsers

If your controller has a web UI, keep it in sync with your stored state rather than with the
replies to its requests.

- Push every change to an allocation or a node to every connected browser over a WebSocket, and
  build each update from your stored state.
- The reply to a launch or stop only tells you that the node accepted the request. The outcome
  arrives later, from the node's reports, from a deadline running out, or from another tab or
  player acting on the same server.
- Send the whole state when a browser connects, because it missed every change while it was away.

### Events

The node can send the same event batch twice, when its receipt is lost. A batch can also arrive
after its server has ended. [Handling a batch](events.md#handling-a-batch) explains how to keep
statistics consistent in both cases.

## Replace a slot's token

When Steam refuses a slot's login token, the agent starts no server in that slot until you replace
the token. Wait until the slot is idle, because revoking the token of a running server drops that
server from Steam.

1. Send [agent:reserve_slot]. Once the reply arrives, the agent starts no server in the slot. That
   keeps a new server from starting on a token you are about to revoke. A slot that runs a server
   cannot be reserved.
2. Revoke the old token at Steam and create a new one.
3. Send the new token with [agent:set_config], which ends the reservation. If you send no new token,
   the reservation runs out after [limit:SLOT_RESERVATION_SECONDS] seconds.

The agent logs the new token on to Steam right away. The slot's `gslt_state` in
[agent:slot_status] tells you whether Steam accepted it.

## One instance or several

Run your controller as a single instance. Add more instances only when one process can no longer
serve your nodes and viewers, because several instances turn simple in-memory state into
distributed state.

Each node keeps one socket open to your controller. Much of what the protocol asks you to track is
state about that socket:

- the requests that are waiting for a reply
- the slots and servers that the node last reported
- which socket currently belongs to the node.

In a single process, that state is plain memory. Apply each node's updates one at a time, in the
order its messages arrive, so that two updates to one node never interleave. A single reader per
socket or a lock per node does that in any language. A restart is cheap, because every agent
reconnects and its [agent:hello] rebuilds what the controller knew.

With several instances behind a load balancer, each instance holds some of the sockets. A request
for a node has to reach the instance that holds that node's socket. For example, a user's browser
can reach one instance while the node they want to launch on is connected to another. Routing
those requests needs a message bus between the instances, such as NATS with JetStream, Redis or
Valkey streams, Kafka, RabbitMQ or Postgres `LISTEN`/`NOTIFY`. Each piece of per-node state also
needs a single owner:

- **Socket ownership.** A node that reconnects can land on another instance while its old socket
  is still open on the first. Record which instance holds each node, with an expiry. Close a
  second socket with `node_connected`, as described in
  [accepting the connection](index.md#1-accept-the-connection).
- **Requests and replies.** A reply arrives on the instance that holds the socket, matched to its
  request by `reply_to`. A caller on another instance waits for it over the bus. If the holding
  instance dies, the caller receives no reply. Treat that as a launch or stop
  [that may have run](#launches).
- **Decisions about servers.** Choosing a node for a match and recording the launch must happen
  exactly once. Two instances that act on the same match can launch it twice, so route each
  match's decisions through one queue or lock.
- **Event uploads.** Event batches are plain HTTP requests and can reach any instance. Lock the
  per-server cursor from [Handling a batch](events.md#handling-a-batch) inside its transaction,
  for example with `SELECT … FOR UPDATE`. Two batches for the same server then cannot both
  advance it.
- **Live views.** Slot statuses, resource usage and logs arrive on the instance that holds the
  socket. Forward them over the bus to viewers connected to the other instances.

<p class="choice-label pros">Pros</p>

- **Rolling updates.** A deploy replaces one instance at a time. The controller stays up, and only
  the nodes connected to that instance reconnect.
- **Partial outages.** A crashed instance drops only the sockets it held.
- **More capacity.** The controller can serve more nodes and viewers than one process can.

<p class="choice-label cons">Cons</p>

- **A bus to run.** Your controller depends on one more system and on its delivery guarantees.
- **Distributed state.** Every rule above must hold across all instances. A missed lock or a lost
  bus message shows up as a launch that ran twice or a reply that never came. Such failures are
  hard to reproduce.
