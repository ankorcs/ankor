---
title: Protocol overview
description: Everything a controller exchanges with Ankor agents, for anyone building their own.
slug: protocol
---

The Ankor protocol is how a controller runs game servers on Ankor nodes. Each node's agent
connects to the controller over a WebSocket and keeps that connection open.

Over the socket, the controller tells the node what to do, such as installing CS2 or starting a
game server. The node reports what it is doing: which slots are busy, which CS2 build it runs and
what its game servers print. Gameplay events and recorded demos are uploaded separately over HTTP.

A node can sit behind NAT, because the agent opens every connection. The node needs no inbound
ports apart from the game ports that players join on. Your controller accepts the connections and
answers them.

This page walks through your controller's tasks step by step.
[A minimal controller](minimal-controller.md) takes one node from token to running server. The
[protocol reference](/reference/) has every message, field and example.

In Node.js, [`@ankorcs/protocol`](https://www.npmjs.com/package/@ankorcs/protocol) does the work:

```sh
npm install @ankorcs/protocol
```

- `@ankorcs/protocol/session` runs the request side for you. An agent session sends each request
  under an id and parses every report. It resolves each request with the typed `data` or `error`
  of its reply, or with a timeout or the socket's close. It works with any WebSocket server if you
  hand it each frame and a function that writes one.
- `@ankorcs/protocol` has the zod schema of every message and the constants. A controller can
  also parse and build every message with these schemas directly.
- `@ankorcs/protocol/helpers` holds the functions that both ends share. Among them are
  `bearerNodeToken` for the upgrade, `slotTakesLaunch` for ranking nodes and `withLogBatch` for
  applying a [agent:log_batch]. Each function's JSDoc documents it.

Check the [protocol version](versioning.md) on the upgrade, then run one session per accepted
socket:

```ts
import { PROTOCOL_HEADER } from '@ankorcs/protocol';
import { isCompatibleProtocol, protocolVersionOf, upgradeRefusal } from '@ankorcs/protocol/helpers';
import { createAgentSession } from '@ankorcs/protocol/session';

server.on('upgrade', (request, socket, head) => {
  const protocol = request.headers[PROTOCOL_HEADER.toLowerCase()];

  // Check the node token as well, as step 1 describes.
  if (!isCompatibleProtocol(protocol)) {
    socket.end(upgradeRefusal(426));

    return;
  }

  // The node's version, to offer it only the features it has.
  const version = protocolVersionOf(protocol);
  // … accept the socket.
});

const node = createAgentSession({ send: (text) => ws.send(text) });

ws.on('message', (raw) => {
  const frame = node.receive(raw);

  if (frame.kind === 'report') {
    handleReport(frame.message);
  }
});
ws.on('close', () => node.closed());

const result = await node.request({ type: 'reserve_slot', slot_id: 's1' });
```

- `request` adds the `id` and resolves with the reply's `data` or `error`, or with `timeout` or
  `closed` when no reply came. Without a reply the agent may still have done the work, and its
  reports say whether it did.
- A request waits 30 seconds by default. A `stop_server` waits [limit:STOP_TIMEOUT_SECONDS]
  seconds for the stop and [limit:STOP_REPLY_MARGIN_SECONDS] seconds more, because its reply comes
  once the container has stopped. Set `timeoutMs` on the session or on one call,
  or null for no deadline. A stop behind a launch of the same allocation waits for that launch's
  reply first, so give such a stop a longer wait.
- `receive` reads a frame as it arrived, and `receiveParsed` one your server already parsed from
  JSON. Each returns a `report`, a `reply` with whether it answered an open request, `unsupported`
  for a message from a newer agent, which you skip, or `invalid` with zod's `issues`.
- `notify` sends the frames that get no reply, such as [agent:welcome] and the watches. The session
  checks what it sends against the contract, so a request the contract refuses never reaches the
  socket.

## What you need

| Endpoint                  | Caller                    | Auth                  | Support                               | Reference                             |
|---------------------------|---------------------------|-----------------------|---------------------------------------|---------------------------------------|
| `WS <socket path>`        | agent                     | bearer token          | Required                              | [Steps 1–4](#1-accept-the-connection) |
| `POST <events path>`      | agent                     | bearer token          | Required for matches with statistics  | [Event uploads](events.md)            |
| `POST <demo target>`      | agent                     | the configured header | Optional                              | [Demo uploads](demos.md)              |
| `POST /api/node/setup`    | Ankor's `node-setup.sh`   | bearer token          | Optional                              | [Setup endpoint](node-setup.md)       |

The socket runs game servers. Ankor's setup script fetches a node's `agent.yml` from the setup
endpoint. Without that endpoint, operators write the file themselves. The reference marks each
message `required` or `optional`, and optional ones can be ignored.

The agent builds its URLs from [config:controller.host], [config:controller.tls], [config:controller.socket_path]
(default `/ws/agent`) and [config:controller.events_path] (default `/api/node/events`) in its
[`agent.yml`](../configuration.md#agentyml). It sends two headers with the socket upgrade and every
event upload:

- `Authorization: Bearer <node token>`, the node's [token](#node-tokens).
- `Ankor-Protocol`, the [protocol version](#versions) the agent speaks, such as `0.9`.

### Node tokens

Your controller issues one node token per node. A token is 32 to 128 letters and digits. Generate
it from a cryptographic random source, for example 32 random bytes as hex:

```sh
openssl rand -hex 32
```

```js
import { randomBytes } from 'node:crypto';

const token = randomBytes(32).toString('hex');
```

- An agent whose [config:token] has another shape refuses to start.
- Store a hash of the token rather than the token. A SHA-256 is enough for a random one.

## Messages and replies

- Every socket frame is one JSON object with a `type`.
- Every field of what the agent sends is present. `| null` means the value may be null.
- Edits you send, such as [agent:set_config], may leave out the fields the reference marks
  optional.
- Unknown keys are ignored on both sides.
- Requests carry an `id` that you choose. The agent answers each request with one [agent:reply], in
  any order. The reply sets `reply_to` to that `id` and `request` to the request's `type`. In a
  failure, `request` is `null` when the agent did not know the `type`.
- Keep `id`s unique while their requests run. The agent ignores a request whose `id` is still
  running on the connection, because a reply could not tell the two apart. It logs a warning.
- A reply has `ok: true` and `data`, or `ok: false` and an `error`. Branch on `error.code`.
- `request` says what `data` holds, so a success can be handled without its request. A failure is
  about the request's arguments, so look the request up by `reply_to`.
- When a request has an unknown `type` or does not match its schema, the agent answers with an
  [`AgentErrorCode`](/reference/types/#schema-AgentErrorCode) saying which.
- A request stays open until its work is done, so time requests out by kind. The reference entry
  of each request says what its reply waits for.

A request that only changes something replies with the id of the object it changed:

<div class="message-cards">

<div class="message-card from-controller">
<p class="message-card-head">[agent:reserve_slot]<span class="message-card-direction">controller → agent</span></p>

```json
{
  "type": "reserve_slot",
  "id": "r2",
  "slot_id": "s1"
}
```

</div>

<div class="message-card from-agent">
<p class="message-card-head">[agent:reply]<span class="message-card-direction">agent → controller</span></p>

```json
{
  "type": "reply",
  "reply_to": "r2",
  "request": "reserve_slot",
  "ok": true,
  "data": {
    "slot_id": "s1"
  }
}
```

</div>
</div>

A failed request:

<div class="message-cards">

<div class="message-card from-controller">
<p class="message-card-head">[agent:stop_server]<span class="message-card-direction">controller → agent</span></p>

```json
{
  "type": "stop_server",
  "id": "r9",
  "allocation_id": "9c04e1d2-3a7b-…"
}
```

</div>

<div class="message-card from-agent refused">
<p class="message-card-head">[agent:reply]<span class="message-card-direction">agent → controller</span></p>

```json
{
  "type": "reply",
  "reply_to": "r9",
  "request": "stop_server",
  "ok": false,
  "error": {
    "code": "unknown_allocation",
    "detail": "unknown_allocation"
  }
}
```

</div>
</div>

## Versions

- The agent sends the protocol's `major.minor` in the `Ankor-Protocol` header. A peer on an
  incompatible version is refused with `426`. [Versioning](versioning.md) explains which versions
  are compatible.
- Pin each node to its [release line's](../requirements.md#images) tag, such as
  `ghcr.io/ankorcs/ankor-agent:0.9`, in its `agent.yml` and in updates. Never pin `latest` unless
  your node is connected to ankor.sh. Releases on a line only add messages and fields, which a
  controller can ignore until it uses them.
- Read the agent's version from `agent.version` in [agent:hello]. Still welcome a node on a line
  your controller does not support, but launch no servers on it. You can then still move it with
  [agent:update_agent].

## 1. Accept the connection

The agent connects to [config:controller.socket_path] with an ordinary WebSocket upgrade. The example uses
the default. Check `Authorization` and [`Ankor-Protocol`](#versions) before you accept.

<div class="message-cards">

<div class="message-card from-agent">
<p class="message-card-head"><code>GET /ws/agent</code><span class="message-card-direction">agent → controller</span></p>

```http
GET /ws/agent HTTP/1.1
Host: cs.example.com
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==
Sec-WebSocket-Version: 13
Authorization: Bearer 3f1c…
Ankor-Protocol: <protocol version>
```

</div>

<div class="message-card from-controller">
<p class="message-card-head"><code>101 Switching Protocols</code><span class="message-card-direction">controller → agent</span></p>

```http
HTTP/1.1 101 Switching Protocols
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=
```

</div>
</div>

Your WebSocket library writes the `101`. You only decide whether to accept. To refuse, answer with
a plain status. Only the status matters, so the body can be empty.

<div class="message-cards">

<div class="message-card from-controller refused">
<p class="message-card-head"><code>401 Unauthorized</code><span class="message-card-direction">controller → agent</span></p>

```http
HTTP/1.1 401 Unauthorized
Content-Length: 0
```

</div>

<div class="message-card from-controller refused">
<p class="message-card-head"><code>426 Upgrade Required</code><span class="message-card-direction">controller → agent</span></p>

```http
HTTP/1.1 426 Upgrade Required
Content-Length: 0
```

</div>
</div>

- `401` for an unknown token. The agent stops trying until it is restarted.
- `426` for a missing or incompatible [protocol version](versioning.md). The agent stops trying too.
- `429` to make it wait, as for a node over its [limits](limits.md). Never answer `401` for that,
  because the agent would stop dialling until it is restarted.
- A `5xx` if something on your side is temporarily broken.

The agent retries a `429` or `5xx` as [How the agent reconnects](#how-the-agent-reconnects)
describes.

Keep one socket per node. [ankor.sh](https://ankor.sh), for example,
[closes](/reference/socket/#closing) a second socket with `node_connected` until the first closes
or [times out](#4-keep-the-connection-alive). The agent keeps retrying. A host whose connection
dropped therefore reconnects once you close its old socket.

When you close a socket, fail the requests still open on it. A session from
`@ankorcs/protocol/session` does that on `closed()`.

## 2. Welcome the node

The agent's first message is [agent:hello]. It carries the node's configuration, its slots and
their servers, its CS2 build and its demos. Save what it reports and answer [agent:welcome], also
for a node on a [release line you do not support](#versions). Send no requests before that. A node with
`unavailable` set stays connected but accepts no launches. The value of `unavailable` gives the
reason.

<div class="message-cards">

<div class="message-card from-agent">
<p class="message-card-head">[agent:hello]<span class="message-card-direction">agent → controller</span></p>

```json
{
  "type": "hello",
  "agent": { "version": "0.1.0", … },
  "public_ip": "203.0.113.7",
  "country": "FR",
  "config": { "slots": [ … ], … },
  "slot_status": [
    { "slot_id": "s1", "state": "idle", … }
  ],
  "cs2": { "build_id": "20114612", … },
  …
}
```

</div>

<div class="message-card from-controller">
<p class="message-card-head">[agent:welcome]<span class="message-card-direction">controller → agent</span></p>

```json
{ "type": "welcome" }
```

</div>

</div>

If a hello fails its schema, answer [agent:hello_refused] with
[`invalid_hello`](/reference/types/#schema-HelloRefusedReason). Then
[close the socket](/reference/socket/#closing) with `hello_refused`. The agent logs the `detail`
and stops dialling until it is restarted.

Accept every other hello, whatever its `agent.version`. An agent on a version that your controller
does not support stays connected but receives no launches. You can still send it
[agent:update_agent]. If you refused it at hello, it would stop dialling, and its host would have
to update it.

## 3. Node sends information

After the [agent:welcome], the agent reports every change by itself, so there is nothing to poll.
[agent:slot_status] reports each slot change. [agent:node_status] reports each change to the node's
state.

<div class="message-cards">

<div class="message-card from-agent">
<p class="message-card-head">[agent:slot_status]<span class="message-card-direction">agent → controller</span></p>

```json
{
  "type": "slot_status",
  "status": {
    "slot_id": "s1",
    "state": "running",
    "allocation_id": "5b1e2c7a-8d4f-…",
    …
  }
}
```

</div>

<div class="message-card from-agent">
<p class="message-card-head">[agent:node_status]<span class="message-card-direction">agent → controller</span></p>

```json
{
  "type": "node_status",
  "cs2": { "build_id": "20114612", "latest_build_id": "20114670", … },
  …
}
```

</div>

</div>

Some reports arrive only while you watch them. A [agent:watch_logs] or [agent:watch_resources]
request stays in force for [limit-time:WATCH_LEASE_MS]. Send it again before then to keep the
reports coming.

- [agent:resource_batch], while a [agent:watch_resources] request is in force.
- [agent:log_batch] with the lines of one source, while a [agent:watch_logs] request for it is in
  force.
- [agent:log_index], listing every source with its line count, while any source is watched.

<figure class="diagram diagram-seq" aria-label="The agent upgrades to a WebSocket with its token, sends hello, gets welcome, catches up with its log, then reports slot and node changes.">
<div class="diagram-seq-head"><div class="diagram-card"><p class="diagram-name">Agent</p></div><div class="diagram-card"><p class="diagram-name">Controller</p></div></div>
<div class="diagram-seq-body">
<div class="diagram-msg right from-agent"><p class="diagram-msg-label"><code>GET /ws/agent</code><small><code>Upgrade: websocket</code>, <code>Authorization: Bearer &lt;node token&gt;</code>, <code>Ankor-Protocol</code></small></p><span class="diagram-arrow"></span></div>
<div class="diagram-msg left from-controller"><p class="diagram-msg-label"><code>101 Switching Protocols</code></p><span class="diagram-arrow"></span></div>
<div class="diagram-msg right from-agent"><p class="diagram-msg-label"><code>hello</code></p><span class="diagram-arrow"></span></div>
<div class="diagram-msg left from-controller"><p class="diagram-msg-label"><code>welcome</code></p><span class="diagram-arrow"></span></div>
<div class="diagram-msg right from-agent"><p class="diagram-msg-label"><code>log_batch</code><small>while a watch_logs request is in force</small></p><span class="diagram-arrow"></span></div>
<div class="diagram-msg right from-agent"><p class="diagram-msg-label"><code>slot_status</code><small>whenever a slot changes</small></p><span class="diagram-arrow"></span></div>
<div class="diagram-msg right from-agent"><p class="diagram-msg-label"><code>node_status</code><small>the node's state changed</small></p><span class="diagram-arrow"></span></div>
</div>
</figure>

## 4. Keep the connection alive

The agent pings every [limit-time:AGENT_PING_INTERVAL_MS]. WebSocket libraries answer pings for
you. Close the socket of an agent that has sent nothing for [limit-time:AGENT_PING_TIMEOUT_MS]. A
ping counts as something sent, so an idle node stays connected.

### How the agent reconnects

- Each dial has [limit-time:AGENT_HANDSHAKE_TIMEOUT_MS] to finish the upgrade.
- After a lost or failed connection, the agent dials again after
  [limit-time:AGENT_RECONNECT_MIN_MS]. The delay doubles with each attempt, up to
  [limit-time:AGENT_RECONNECT_MAX_MS]. Each delay is randomised by
  ±[limit-percent:AGENT_RECONNECT_JITTER].
- The delay starts over once a [agent:welcome] arrives.
- The agent also reconnects when a ping is still unanswered as the next one is due.
- When the agent stops dialling, it also stops [uploading events](events.md) until it is restarted.
  Events keep collecting on the node.
- The agent replies only on the socket that a request arrived on. When that socket closes, the
  agent drops any reply still due.
  [Disconnects and restarts](minimal-controller.md#disconnects-and-restarts) shows how to find a
  launch whose reply was lost.

[Closing the socket](/reference/socket/#closing) lists each close and the agent's response to it.
After every [agent:welcome], the agent sends what the [agent:hello] did not cover, in the order
that [agent:welcome] describes.

## 5. Install CS2

A new node has no CS2. Send [agent:update_cs2] once to install it.

- The reply comes immediately and carries the update's `update_id`.
- `cs2.update` in [agent:node_status] reports the same `id`. It gets its `outcome` when the
  install has ended.
- Every [agent:cs2_update_progress] names its update by `update_id`.

After the install, the agent [updates CS2 by itself](../operations.md#update-cs2). To make it check
for a new build immediately, send [agent:check_cs2_build].

## 6. Launch a game

Send [agent:start_server] with a request `id`, a new
[allocation id](/reference/types/#schema-AllocationId) and the launch configuration, such as the
map and the roster. The allocation id names the server and its events, so every launch needs a new
one.

The agent picks a free, unreserved slot whose login token Steam has not refused. It starts the
server with that slot's token. Once the container has started, the agent replies with the
allocation and the slot's id.

A `steam_web_api_key` in a launch is readable by whoever runs the node.
[Trust](../how-it-works.md#trust) covers the risk.

<div class="message-cards">

<div class="message-card from-controller">
<p class="message-card-head">[agent:start_server]<span class="message-card-direction">controller → agent</span></p>

```json
{
  "type": "start_server",
  "id": "r1",
  "allocation_id": "5b1e2c7a-8d4f-…",
  "launch": {
    "map_name": "de_mirage",
    "game_mode": "competitive",
    "max_players": 10,
    "exec": [],
    "commands": [],
    "match": { "roster": { … }, "rules": { "max_rounds": 24, … }, … },
    …
  }
}
```

</div>

<div class="message-card from-agent">
<p class="message-card-head">[agent:start_server_reply]<span class="message-card-direction">agent → controller</span></p>

```json
{
  "type": "reply",
  "reply_to": "r1",
  "request": "start_server",
  "ok": true,
  "data": {
    "allocation_id": "5b1e2c7a-8d4f-…",
    "slot_id": "s1"
  }
}
```

</div>
</div>

A launch with `match` set to `null` leaves Ankor's plugin idle. The server plays the launch's game
mode, with the cfgs in `exec`, the console lines in `commands` and the node's own plugins. Use such
launches to run your own game modes on Ankor nodes.

A [`StartRefusal`](/reference/types/#schema-StartRefusal) comes before the node reserves a slot, so
you can try the launch on another node. Each refusal's entry in
[`AgentErrorCode`](/reference/types/#schema-AgentErrorCode) says when the node takes launches again.

When the reply arrives, CS2 is still starting. Wait for that slot's [agent:slot_status] to report
`running`, then [hand out the address](minimal-controller.md#8-hand-out-the-address). When the
server has ended, the agent sends [agent:server_exited] with its `allocation_id`. Then it sends a
[agent:slot_status] with the slot back to `idle` and free for another launch.

`reason` in [agent:server_exited] gives the reason the server ended, as one of the
[exit reasons](/reference/types/#schema-ServerExitReason). `exit_code` carries the container's exit
code.

To stop a server early, send [agent:stop_server]. The agent reports the slot `stopping`, then
`idle`, and replies once the container has stopped.

A match whose server died can play on from a round it reached. Launch the same
[`match_id`](/reference/types/#schema-MatchConfig) again, on any node and under a new
`allocation_id`. Take its [`match.restore`](/reference/types/#schema-MatchConfig) from the newest
[event:round_backup]. To play a running match on from an earlier round, send [agent:restore_round].
The plugin confirms either with [event:match_restored]. [Backups and restores](../backups.md)
describes both.

Warmup ends as the match's [`ready_mode`](/reference/types/#schema-MatchConfig) says.
[agent:start_match] starts the match in every mode. With `external`, it is the only way to start
the match, because the players cannot. The reply comes when the countdown has started. The
[ready system](../ready-system.md) describes each mode and the ready deadline.

### Cfgs and commands

[agent:start_server] carries the launch's settings in its `launch` field, a
[`ServerLaunchConfig`](/reference/types/#schema-ServerLaunchConfig). The agent writes them into one
cfg, `cfg/ankor_launch.cfg`, so that they all run together. The plugin execs it on one console
line with Ankor's cfgs, and CS2 runs that line on a single tick. No other command runs between
the settings.

For a match, the file looks like this:

```c
// generated by @ankorcs/agent - overwritten on every launch, do not edit

// 1. Only when the launch sets a hostname.
hostname "Ankor | cs.example.com"

// 2. Only with the launch's gotv: its delay and any password. Without gotv, a match that
//    records a demo gets tv_delay 0 and a random tv_password that the agent sends nowhere.
tv_delay 90
tv_password "spectate"

// 3. Only for a match: its rules, or the restore's rules when the launch has one. The two
//    friendly-fire lines appear only when friendly_fire is off.
mp_maxrounds 24
mp_overtime_enable 1
mp_overtime_maxrounds 6
ff_damage_reduction_bullets 0
ff_damage_reduction_other 0

// 4. Only when the launch's exec lists cfgs: one exec for each, in order. For example:
exec node/league

// 5. Only when the launch has commands: each line, in order. For example:
mp_team_timeout_max 2
```

- The match's `rules` are a field of its [`MatchConfig`](/reference/types/#schema-MatchConfig).
- A name in `exec` is a cfg path under `game/csgo/cfg/` without `.cfg`, such as `node/league` for
  one of the [node's cfgs](../operations.md#add-cfgs-for-servers).

A later line wins over an earlier one. `commands` therefore override the cfgs in `exec`, and both
override `rules`. Keep `commands` consistent with `rules`, because [event:going_live] reports
`rules` as the format of the match.

<div class="callout warning">
<p class="callout-label">Leave the match flow to the plugin</p>

In a match, keep `exec` and `commands` to settings. Leave out commands that move the match on,
such as `mp_warmup_end`, `mp_restartgame`, `mp_swapteams`, `mp_pause_match` or a map change. They
run at warmup, at the side choice and again when the match goes live. There they disrupt the
plugin, which runs warmup, the knife round and going live itself.

</div>

### Ankor's cfgs and your own

The CS2 image ships Ankor's cfgs under `cfg/ankor/`. Their
[source](https://github.com/ankorcs/ankor/tree/master/packages/plugin/cfg) lists each setting.

- `cfg/ankor/base.cfg` holds Ankor's settings for every match, whatever the game mode. It keeps an
  empty server running and hands team assignment and the end of warmup to the plugin.
- `cfg/ankor/reset.cfg` puts what warmup and the knife round change back to CS2's defaults. It runs
  ahead of the game mode's cfg, so the mode's values then apply over it.
- `cfg/ankor/warmup.cfg` sets a practice warmup: respawns, full money, buying anywhere and no
  round or time limit.
- `cfg/ankor/knife.cfg` sets a knife-only round with no money, buying or respawns.

Every cfg is read-only in the game container. CS2 ships the game mode's cfgs, the CS2 image
ships Ankor's, and the agent writes `ankor_launch` for each launch. To change a setting, use one of
these:

- **A node cfg**, which the node's operator provides. The operator puts `<name>.cfg` in
  `/srv/ankor/cfg/` on the node, and the server reads it as `cfg/node/<name>.cfg`. A launch runs it
  with `exec: ["node/<name>"]`.
  [Add cfgs for servers](../operations.md#add-cfgs-for-servers) has the
  operator's steps.
- **A line in `commands`**, which the controller sends with the launch. It needs no file on the
  node.

Both run inside `ankor_launch`, so they override the game mode's cfg and `ankor/base`.

### When the cfgs run

The engine runs the game mode's cfg, such as `gamemode_competitive`, on every map load. Ankor's
plugin runs `ankor_launch` over it, together with Ankor's cfgs.
Each step lists its cfgs in the order that they run, as paths under `game/csgo/`:

<figure class="diagram" aria-label="After a map loads, the engine runs the game mode's cfg. One second later, a server without a match runs ankor_launch. A match runs ankor/base, ankor_launch and ankor/warmup for its warmup, ankor/knife for an optional knife round, ankor/reset, the game mode's cfg, ankor/base, ankor_launch and ankor/warmup for the side choice after it, and ankor/reset, the game mode's cfg, ankor/base and ankor_launch when it goes live.">
<div class="diagram-card"><p class="diagram-name">A map loads</p><p class="diagram-text">The launch's <code>game_mode</code> sets the engine's <code>game_type</code> and <code>game_mode</code>. The engine then runs that mode's <code>cfg/gamemode_&lt;mode&gt;.cfg</code>, such as <code>gamemode_competitive.cfg</code>. CS2 ships this cfg with the game mode's default values.</p></div>
<div class="diagram-link quiet down"><span>1 second later</span></div>
<div class="diagram-row">
<div class="diagram-place"><span class="diagram-place-label">Server without a match<small>on every map load</small></span>
<div class="diagram-card"><p class="diagram-name">Launch settings</p><ol class="diagram-list"><li><code>cfg/ankor_launch.cfg</code></li></ol></div>
</div>
<div class="diagram-place"><span class="diagram-place-label">Match<small>once per map</small></span>
<div class="diagram-card"><p class="diagram-name">Warmup</p><ol class="diagram-list"><li><code>cfg/ankor/base.cfg</code></li><li><code>cfg/ankor_launch.cfg</code></li><li><code>cfg/ankor/warmup.cfg</code></li></ol></div>
<div class="diagram-link quiet down"><span>The match starts</span></div>
<div class="diagram-place"><span class="diagram-place-label">Optional<small>only with <code>sides: knife</code></small></span>
<div class="diagram-card"><p class="diagram-name">Knife round</p><ol class="diagram-list"><li><code>cfg/ankor/knife.cfg</code></li></ol></div>
<div class="diagram-link quiet down"><span>The knife round ends</span></div>
<div class="diagram-card"><p class="diagram-name">Side choice</p><ol class="diagram-list"><li><code>cfg/ankor/reset.cfg</code></li><li><code>cfg/gamemode_&lt;mode&gt;.cfg</code></li><li><code>cfg/ankor/base.cfg</code></li><li><code>cfg/ankor_launch.cfg</code></li><li><code>cfg/ankor/warmup.cfg</code></li></ol></div>
</div>
<div class="diagram-link quiet down"><span>The side is chosen<small>or the match starts, without a knife round</small></span></div>
<div class="diagram-card"><p class="diagram-name">Going live</p><p class="diagram-sub">or a launch with a <code>restore</code> loading its backup</p><ol class="diagram-list"><li><code>cfg/ankor/reset.cfg</code></li><li><code>cfg/gamemode_&lt;mode&gt;.cfg</code></li><li><code>cfg/ankor/base.cfg</code></li><li><code>cfg/ankor_launch.cfg</code></li></ol></div>
</div>
</div>
</figure>

`ankor/warmup` and `ankor/knife` run over the launch's settings, so warmup and the knife round play
the same on every launch. Going live runs `ankor/reset` and then the game mode's cfg, which undo
their changes. The match therefore plays the game mode's format with the launch's settings.

- A line in `commands` runs at every point where `ankor_launch` runs. In a match, that is warmup,
  the side choice and going live. A command that acts instead of setting a value, such as
  `mp_restartgame`, acts at each point.
- On a server without a match, the game mode's settings apply for the first second after a map
  loads, until `ankor_launch` runs.
- [agent:restore_round] plays a round of the running match again and runs no cfgs. The match plays
  on under the settings it has. A launch's `restore` starts a new server instead, which goes live
  on the cfgs above.
- Leave hibernation off in `exec`, `commands` and the node's cfgs and plugins. The CS2 image starts
  every server with `sv_hibernate_when_empty 0`, and `ankor/base` sets it again. A hibernating
  server runs no frames, so Ankor's plugin runs no timers and reports no players. On a server
  without a match, `ankor_launch` then does not run after a map loads. The agent also never sees
  the server empty, so [`idle_shutdown_seconds`](/reference/types/#schema-ServerLaunchConfig) never
  stops it.

## 7. Get the result

Every match reports its phase in [agent:phase_change] and its score in [agent:score] on the socket.
The match is over when its phase is `post_live`. The last [agent:score] is the final score. The
agent sends each once. If one is lost while the node is disconnected, [agent:get_server_status]
reads the phase and score again.

A match that sends statistics, as [`match.send_statistics`](/reference/types/#schema-MatchConfig)
asks, also uploads its gameplay events to [the events path](events.md). You build statistics from
them there. The last event of a map is [event:map_end]. Its
[`reason`](/reference/types/#schema-MapEndReason) says why the map ended, and only a map that was
played out names a winner.
