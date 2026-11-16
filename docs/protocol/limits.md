---
title: Limits
description: The limits ankor.sh holds nodes to, and how a controller limits a node.
slug: protocol/limits
---

[ankor.sh](https://ankor.sh) bounds what each node sends, because a node may be run by someone
else. Your controller can apply the same limits or its own. It enforces either with the answers in
[Limiting a node](#limiting-a-node).

## ankor.sh's limits

The numbers are constants in the contract, most of them in
[limits.ts](../../packages/protocol/src/limits.ts). An agent within them is never limited. Ankor's
agent stays within them even with [limit:MAX_SLOTS] busy slots.

- A node has at most [limit:MAX_SLOTS] slots. Its [agent:hello] lists at most the
  [limit:MAX_REPORTED_DEMOS] newest demos. Each demo's file name has at most
  [limit:DEMO_FILE_NAME_MAX_CHARS] characters.
- A socket message is at most [limit-size:AGENT_FRAME_MAX_BYTES]. A socket carries one
  [agent:hello], which is due within [limit-time:AGENT_HELLO_TIMEOUT_MS] of the upgrade.
- Each kind of message draws on a budget per connection. The budget allows a burst and refills at
  a steady rate. Where the table says "a slot", add that amount for every slot the node has. The
  slot count comes from the node's last [agent:hello] or applied [agent:set_config] reply. Before
  the first of these, count one slot.

| Messages                                                                                                                                                                | Burst                                               | Refill                                                | Over budget |
|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------|-------------------------------------------------------|-------------|
| Every message, in bytes                                                                                                                                                 | [limit-burst:bytes]                                 | [limit-refill:bytes]                                  | close       |
| [agent:hello], [agent:slot_status], [agent:node_status], [agent:server_exited], [agent:demo], [agent:demo_removed], [agent:event_backlog], [agent:shutdown], unreadable | [limit-burst:state]                                 | [limit-refill:state]                                  | close       |
| [agent:log_batch], [agent:log_index]                                                                                                                                    | [limit-burst:logFrames], and [limit-burst:logBytes] | [limit-refill:logFrames], and [limit-refill:logBytes] | dropped     |
| [agent:resource_batch]                                                                                                                                                  | [limit-burst:resources]                             | [limit-refill:resources]                              | dropped     |
| [agent:cs2_update_progress]                                                                                                                                             | [limit-burst:cs2UpdateProgress]                     | [limit-refill:cs2UpdateProgress]                      | dropped     |
| [agent:demo_progress]                                                                                                                                                   | [limit-burst:demoProgress]                          | [limit-refill:demoProgress]                           | dropped     |
| [agent:phase_change], [agent:score], [agent:backup]                                                                                                                     | [limit-burst:matchReports]                          | [limit-refill:matchReports]                           | dropped     |
| [agent:reply] to no open request                                                                                                                                        | [limit-burst:strayReplies]                          | [limit-refill:strayReplies]                           | close       |
| WebSocket pings                                                                                                                                                         | [limit-burst:pings]                                 | [limit-refill:pings]                                  | close       |

- A log replay that would exceed the byte budget arrives without its oldest lines, so frequent
  replays do not close the socket.
- [ankor.sh](https://ankor.sh) closes a socket that breaks these rules with the code and reason that
  [Closing the socket](/reference/socket/#closing) lists. After a close for a larger message or a
  budget, ankor.sh answers the node's upgrades with `429` for
  [limit-time:AGENT_LIMIT_COOLDOWN_MS].
- More than [limit:AGENT_CONNECTS.max] upgrades of one node within
  [limit-time:AGENT_CONNECTS.windowMs] are answered `429` until that window ends.
- More than [limit:AGENT_AUTH_FAILURES.max] missing or unknown tokens from one address within
  [limit-time:AGENT_AUTH_FAILURES.windowMs] are answered `429` until that window ends. An IPv6
  address counts as its whole /64.
- [Event uploads](events.md) may carry [limit-size:EVENT_BATCH_BUDGET.base] +
  [limit-size:EVENT_BATCH_BUDGET.perSlot] a slot in [limit-time:EVENT_BATCH_BUDGET.windowMs]. Each
  request counts [limit-size:EVENT_BATCH_BUDGET.requestBytes] more than its body. Past that
  budget, [ankor.sh](https://ankor.sh) answers `429`.
- The agent handles at most one [agent:check_cs2_build] in
  [limit-time:CS2_BUILD_CHECK_INTERVAL_MS] and drops any that comes sooner. The agent's scheduled
  reads of the public CS2 build, [limit-time:CS2_BUILD_POLL_INTERVAL_MS] apart, are not limited.
- [ankor.sh](https://ankor.sh)'s [demo receiver](demos.md) accepts one upload per node at a time.
  It answers `429` to a second upload. It answers `403` to an upload that would push the node's
  stored demos past the service's quota.

## Limiting a node

Enforce limits with the answers below. The agent needs nothing else from you. It backs off,
retries and sends a fresh [agent:hello] by itself. It keeps every event it stored.

- **Socket over a budget:** [close it](/reference/socket/#closing) with code `1008` and
  `rate_limited`. Refuse the node's upgrades with `429` for as long as you want to keep the node
  out. The agent redials with its usual [backoff](index.md#how-the-agent-reconnects). Its next
  [agent:hello] carries its state.
- **A message you would rather drop:** ignore it and keep the socket open. Only the messages that
  the table marks `dropped` are safe to ignore. The others carry state that the agent sends again
  only after it reconnects.
- **Event upload:** answer `429`. The agent keeps the batch and retries with backoff.
- **Demo upload:** answer `429`. The agent keeps the file and retries.
  [Answers](demos.md#answers) has the details.
- The agent ignores `Retry-After` and the response body of a `429`.
