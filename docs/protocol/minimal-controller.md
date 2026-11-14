---
title: A minimal controller
description: The shortest path from a new node token to a running CS2 server on a node of your own controller.
slug: protocol/minimal-controller
---

These steps take one node from a new token to a running CS2 server on it. The
[protocol overview](index.md) explains each exchange, and this page puts them in order. The
examples use `cs.example.com` for your controller.

## Talking to the agent

The [protocol overview](index.md) describes how to accept a node's connection and exchange messages
with its agent. In Node.js, the `@ankorcs/protocol` package holds the message schemas. Its session,
`@ankorcs/protocol/session`, runs the exchanges for you.

Agents may run a newer protocol version than your controller. [Versioning](versioning.md) lists
what your controller must accept from them, and what it skips.

## Setup

### 1. Create the node and its token

Generate a [node token](index.md#node-tokens). Show the token to the operator once and store only
its SHA-256.

### 2. Give the operator the setup command

The [setup script](../setup-script.md) is served from ankor.sh for every controller. `--controller`
points it at yours:

```sh
curl -fsSL https://ankor.sh/node-setup.sh | bash -s -- --controller https://cs.example.com
```

The operator also opens the game ports of the slots you add in step 5.
[Install a node](../install.md) has the step.

### 3. Answer the setup request

The script posts the token to your [setup endpoint](node-setup.md). Look the token up by its hash.
Answer with the node's `agent.yml`, and put in it the token that the request carried, since you
store only the hash. `slots` may be empty:

```yaml
token: <the token from the request>
controller:
  host: cs.example.com
  tls: true
cs2_image: ghcr.io/ankorcs/ankor-cs2:0.9
agent_image: ghcr.io/ankorcs/ankor-agent:0.9
slots: []
```

<div class="hint">

In TypeScript, `renderAgentYml` from `@ankorcs/protocol/agent-yml` writes this file.

</div>

Without the endpoint, the operator writes this file before running the script.
[Write `agent.yml` yourself](../setup-script.md#write-agentyml-yourself) has the steps.

### 4. Accept the socket and welcome the node

The agent dials [config:controller.socket_path] with its token and its
[protocol version](versioning.md). [Accept the upgrade](index.md#1-accept-the-connection) only when
both check out, then answer its [agent:hello] with [agent:welcome]. With `@ankorcs/protocol` and the
`ws` package:

```ts
import { createHash } from 'node:crypto';
import { WebSocketServer } from 'ws';
import { PROTOCOL_HEADER } from '@ankorcs/protocol';
import { bearerNodeToken, isCompatibleProtocol, protocolVersionOf, upgradeRefusal } from '@ankorcs/protocol/helpers';
import { createAgentSession } from '@ankorcs/protocol/session';

const sockets = new WebSocketServer({ noServer: true });

server.on('upgrade', (request, socket, head) => {
  const token = bearerNodeToken(request.headers.authorization);
  const node = token === null ? null : findNodeByTokenHash(createHash('sha256').update(token).digest('hex'));

  if (node === null) {
    socket.end(upgradeRefusal(401));

    return;
  }

  const protocol = request.headers[PROTOCOL_HEADER.toLowerCase()];

  if (!isCompatibleProtocol(protocol)) {
    socket.end(upgradeRefusal(426));

    return;
  }

  // The node's version, to offer it only the features it has.
  node.protocol = protocolVersionOf(protocol);
  sockets.handleUpgrade(request, socket, head, (ws) => run(node, ws));
});

function run(node, ws) {
  const session = createAgentSession({ send: (text) => ws.send(text) });

  ws.on('message', (raw) => {
    const frame = session.receive(raw);

    if (frame.kind === 'report' && frame.message.type === 'hello') {
      saveHello(node, frame.message);
      session.notify({ type: 'welcome' });
    }

    // Skip `unsupported`, a message from a newer agent.
  });
  ws.on('close', () => session.closed());
}
```

### 5. Add slots

With no slots in `agent.yml`, send [agent:set_config] with each slot's ports and its own
[Game Server Login Token](../requirements.md#game-server-login-tokens). `slots` is the whole layout:

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

The reply carries the node's settings with the new slots. An [agent:slot_status] follows for each
new slot. With the session:

```ts
const result = await node.request({ type: 'set_config', config: { slots } });

if (result.status === 'ok') {
  saveSettings(result.data);
}
```

A controller can create the tokens with a user's Steam Web API key.
[`IGameServersService`](https://partner.steamgames.com/doc/webapi/IGameServersService)
`CreateAccount`, with `appid` `730` and a `memo`, returns the `login_token`. `GetAccountList`,
`ResetLoginToken` and `DeleteAccount` manage tokens later. [ankor.sh](../ankor-web.md#login-tokens)
creates its tokens this way.

### 6. Install CS2

Send [agent:update_cs2] with `mode` `update`, then wait until `cs2.update` in [agent:node_status]
has an `outcome`. The install can take tens of minutes. [Install CS2](index.md#5-install-cs2)
explains the reports.

### 7. Start a server

Store a new allocation id, then send it in [agent:start_server]. [Keep track of every
server](#keep-track-of-every-server) explains why. This launch leaves out every field that has a
default. It therefore has no `match`, and runs a plain CS2 server on `de_dust2` in competitive
mode. Ankor's plugin runs no match. `map_name`, `exec`, `commands` and the node's own plugins set
the game. [`ServerLaunchConfig`](/reference/types/#schema-ServerLaunchConfig) lists the defaults.
The agent replies with the allocation and the slot it picked.

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

### 8. Hand out the address

Wait for the slot's [agent:slot_status] with `state` `running`. Players connect to the node's
address and the slot's `port`, both from the latest [agent:hello]. The address is
`config.pinned_ip`, or `public_ip` when none is pinned. A `public_ip` of null means the node found
no public address, so set `pinned_ip` with [agent:set_config] first. A player then connects with:

```text
connect 203.0.113.7:27015
```

[agent:stop_server] ends the server. To run matches with statistics, launch with a
[`MatchConfig`](/reference/types/#schema-MatchConfig) whose `send_statistics` is `true`, and accept
[event uploads](events.md).

## Keep track of every server

Launches and stops race with the node's reports:

- A reply may never come.
- A node may reconnect mid-launch.
- A stop may arrive before the launch it ends.

Store each server's state and update it only from the node's reports. A stop is done once the node
confirms it.

### Launches

- Pick a new `allocation_id` for every launch and store it before you send [agent:start_server].
- Rank nodes by their latest reports with `slotTakesLaunch` and `cs2Behind` from
  `@ankorcs/protocol/helpers`. The node still decides, and may refuse.
- The server is running once the reply or a [agent:slot_status] names its slot, whichever comes
  first.
- A [`StartRefusal`](/reference/types/#schema-StartRefusal) means no server started. Try the next
  node.
- `allocation_in_use` answers a [agent:start_server] sent again with the same `allocation_id`. It
  means the first launch is starting or running on that node. Wait for its [agent:slot_status]
  instead of retrying on a different node.
- Any other error, or no reply at all, may leave a server running, because the node's reports can
  arrive after the reply would have. Stop it on that node before you launch it anywhere else. With
  the session, no reply is a `timeout` or `closed` result.

### Stops

- A stop may be sent while the server's launch is still waiting for its reply. The node then ends
  the launch after its current step, without starting the server. The launch's reply is an error.
  If the stop arrives while Docker starts the container, the launch succeeds and the server is
  then stopped.
- The reply to a stop comes within [limit:STOP_TIMEOUT_SECONDS] seconds for the stop and
  [limit:STOP_REPLY_MARGIN_SECONDS] seconds more. With the session, the
  stop waits that long whatever wait the session or the call sets.
- `unknown_allocation` means no server runs under the allocation. `failed` leaves the server
  running. Send the stop again.

### Disconnects and restarts

- A disconnected node may still run its servers. Keep their records until the node reconnects.
- Compare each [agent:hello] with your records. A server that the `hello` no longer lists has
  ended. A launch whose reply never came is running if the `hello` lists it. Stop a server that you
  have no record of.
- `server_exits` in the `hello` gives the reason for each server that ended while the node was
  away. A server with no entry there, and no [agent:server_exited] after the [agent:welcome], ended
  without the agent knowing why.
- An exit after the agent sent the [agent:hello] arrives as [agent:server_exited] after the
  [agent:welcome]. It can repeat what the `hello` already told you.
- Keep [one socket per node](index.md#1-accept-the-connection).

### Logs

- Send [agent:watch_logs] for a source while someone is looking at it. Send it again before
  [limit-time:WATCH_LEASE_MS] pass to keep the source streaming. Each source needs a separate
  request.
- Ask for a replay with a `replay_id` when a viewer opens a source. Apply each [agent:log_batch]
  with `withLogBatch` from `@ankorcs/protocol/helpers`. A batch with `reset` replaces the lines you
  hold.
- A line can carry ANSI colour escapes (SGR, such as `ESC[32m`), as the server printed them. Render
  them as colours, or strip each `ESC[` … `m` to show plain text. The agent has already removed every
  other escape and control character, so a line cannot move a cursor, open a link or write to a
  clipboard. A line cut at the length limit can end in part of an escape; drop that part.
  [`LogLine`](/reference/types/#schema-LogLine) has the details.
- [agent:log_index] lists every source the node keeps. A source missing from it no longer exists on
  the node.
- A [agent:console_command] is answered once the server's plugin has handed the command to the
  engine. That answer says nothing about whether the command worked. The command's output arrives
  as log lines of that server, so watch the server's log to show it.

### Browsers

- Push every change to an allocation or a node to every connected browser over a WebSocket. Push
  it from your stored state. The reply to a launch or stop request says only that the request was
  accepted. The outcome comes later, from the node, a deadline, or another tab or player.
- Send the whole state when a browser connects, since it missed every change in between.

### Events

- A batch arrives again when its receipt is lost, and can arrive after its server has ended.
  [Handling a batch](events.md#handling-a-batch) keeps statistics consistent through both.

## Replace a slot's token

When Steam refuses a slot's login token, the slot receives no launches until you replace the token.
Replace it once the slot is idle, because revoking the token of a running server drops that server.

1. Send [agent:reserve_slot]. From the reply on, the agent starts no server in the slot, so none
   starts on a token Steam is about to drop. A slot running a server cannot be reserved.
2. Revoke the old token at Steam and obtain a new one.
3. Send the new token with [agent:set_config], which ends the reservation. If no new token follows,
   the reservation runs out.

The agent logs the new token on immediately and reports whether Steam accepted it.

## One instance or several

Run your controller as one instance. Add instances only when one process can no longer serve your
nodes and viewers, because several instances turn simple in-memory state into distributed state.

Each node keeps one socket open to the controller. Much of what the protocol asks of you is state
about that socket:

- the requests waiting for a reply
- the slots and servers that the node last reported
- which socket owns the node.

In one process, that state is plain memory. Apply each node's updates one at a time, in the order
its messages arrive, so two updates to a node never interleave. A single reader per socket or a
lock per node does that in any language. A restart costs little, because every agent redials and
its [agent:hello] rebuilds what the instance knew.

Several instances behind a load balancer each hold some of the sockets. A request for a node has
to reach the instance that holds the node's socket. An example is a launch from a user whose
browser reached another instance. That routing needs a message bus to coordinate the instances,
such as NATS with JetStream, Redis or Valkey streams, Kafka, RabbitMQ or Postgres
`LISTEN`/`NOTIFY`. Each piece of per-node state also needs one owner:

- **Socket ownership.** A node that reconnects can land on another instance while its old socket
  is still open on the first. Record which instance holds each node, with an expiry. Close a
  second socket with `node_connected`, as the [protocol](index.md#1-accept-the-connection) asks.
- **Requests and replies.** A reply arrives on the instance that holds the socket, matched by
  `reply_to`. A caller on another instance waits for it over the bus. When the holding instance
  dies, the caller receives no reply. Treat that as a launch or stop
  [that may have run](#launches).
- **Decisions about servers.** Choosing a node for a match and recording the launch has to happen
  once. Two instances that act on the same match can launch it twice, so route each match's
  decisions through one queue or lock.
- **Event uploads.** Batches are plain HTTP and can reach any instance. Lock the per-server cursor
  from [Handling a batch](events.md#handling-a-batch) inside its transaction, for example with
  `SELECT … FOR UPDATE`. Two batches of one server then cannot both advance it.
- **Live views.** Slot statuses, resources and logs arrive on the instance that holds the socket.
  Fan them out to viewers on the other instances over the bus.

<p class="choice-label pros">Pros</p>

- **Rolling updates.** A deploy replaces one instance at a time. The controller stays up, and only
  that instance's nodes redial.
- **Partial outages.** A crashed instance drops only its share of the sockets.
- **More capacity.** The controller grows past what one process can hold.

<p class="choice-label cons">Cons</p>

- **A bus to run.** The controller depends on one more system, and on its delivery guarantees.
- **Distributed state.** Every rule above has to hold across instances. A missed lock or a lost
  bus message shows up as a launch that ran twice or a reply that never came. Such failures are
  hard to reproduce.
