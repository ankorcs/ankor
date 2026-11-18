---
title: Event uploads
description: How gameplay events reach the controller, and how to handle each batch.
slug: protocol/events
---

To keep match statistics, your controller receives gameplay events. You add one HTTP endpoint to
your controller, and the node posts the events of each match to it in batches. The events
cover kills, damage, grenades, rounds and the final score. Your controller stores each batch and
answers it with a receipt. The node can send the same events again, and the steps in
[Handling a batch](#handling-a-batch) keep your statistics correct when it does.

The plugin sends gameplay events only for a match whose
[`match.send_statistics`](/reference/types/#schema-MatchConfig) is `true`. A match without it sends
no events to this endpoint. Every match reports its phase and score over the socket, as
[Get the result](index.md#7-get-the-result) describes. A match without statistics also sends its
round backups over the socket, as [agent:backup]. A match with statistics sends them only among its
events, as [event:round_backup].

The [protocol reference](/reference/uploads/) defines the batch body, the answers and every event
type.

## Event identity

Each event has an identity that lets your controller recognise a repeat, for example when the
receipt for its batch was lost. The identity has two parts:

- **`allocation_id`**: the server that sent the event, named by the allocation that you gave it in
  [agent:start_server]. The allocation names one server, because a node refuses to launch an
  allocation that it has launched before.
- **`seq`**: the position of the event among the events of that server. It counts from 0 without
  gaps.

The diagram shows the events of one server, and the steps below it follow their uploads:

<figure class="diagram" aria-label="A server numbers its events from 0, and batches carry them to the controller. After a lost receipt, the next batch starts again at the first event that no receipt covered.">
<p class="diagram-head"><span class="diagram-name">Server <code>0199a0b4-…-abc1</code></span><small>one allocation, numbering its events from 0</small></p>
<div class="diagram-groups">
<div class="diagram-group"><span class="diagram-group-label">batch 1 · seq 0–3</span><div class="diagram-chips"><span class="diagram-chip"><b>0</b><span>map_start</span></span><span class="diagram-chip"><b>1</b><span>going_live</span></span><span class="diagram-chip"><b>2</b><span>round_start</span></span><span class="diagram-chip"><b>3</b><span>player_hurt</span></span></div></div>
<div class="diagram-group"><span class="diagram-group-label">batch 2 · seq 4–7</span><div class="diagram-chips"><span class="diagram-chip"><b>4</b><span>player_death</span></span><span class="diagram-chip"><b>5</b><span>round_end</span></span><span class="diagram-chip"><b>6</b><span>round_mvp</span></span><span class="diagram-chip"><b>7</b><span>round_complete</span></span></div></div>
<div class="diagram-group waiting"><span class="diagram-group-label">arrive on the node meanwhile · seq 8–12</span><div class="diagram-chips"><span class="diagram-chip waiting"><b>8</b><span>round_start</span></span><span class="diagram-chip waiting"><b>9</b><span>…</span></span><span class="diagram-chip waiting"><b>12</b><span>player_hurt</span></span></div></div>
</div>
</figure>

1. Batch 1 arrives. The controller stores seq 0–3 and answers with a receipt for 0–3. The agent
   deletes those events.
2. Batch 2 arrives. The controller stores seq 4–7, but the receipt is lost on the way back. The
   agent keeps those events. Meanwhile, seq 8–12 arrive on the node.
3. After its backoff, the agent sends seq 4–12 in one batch. The controller already has seq 4–7 of
   that server, so it stores only 8–12. It answers with a receipt for 4–12.
4. The agent continues with the uploads from seq 13.

The agent builds each attempt from the oldest event that no receipt covered, and fills the batch
with as many of the events of that server as fit. A retry therefore starts at the same `seq` as the attempt that
failed, and it can carry more events. The events that a retry repeats are identical to those you
stored, because a server's events never change once they reach the node.

The node stores the events on disk until a receipt covers them, so they survive an agent restart or
a lost connection. It keeps the events of a server for [limit-time:EVENT_SPOOL_RETENTION_MS] after
the last event or receipt of that server. The node remembers delivered events that long so that it
recognises a replay of them. After that time, the node drops the events, including any that no
controller accepted, and the agent logs how many it dropped. The node keeps the events of a server
whose plugin is still connected.

## `POST <events path>`

The agent posts each batch to [config:controller.events_path], which is `/api/node/events` by
default. Each request carries the agent's [headers](index.md#what-you-need) and a JSON body for one
server. The body holds the `allocation_id` of the server and 1–[limit:EVENT_BATCH_MAX_FRAMES]
frames of its events, with consecutive `seq`.

The frames come to at most [limit-size:EVENT_BATCH_MAX_BYTES] together. Accept request bodies of
that size plus the wrapper, because a batch refused for its size blocks the upload of that
server's events. Your controller can also cap the bytes that one node posts in a time window, and
answer `429` past the cap. [Limits](limits.md) gives the budget that ankor.sh uses.

Each `event` carries the fields of its gameplay event. When a map ends, the last event of its
server is [event:map_end]. A batch and its receipt look like this, with the events cut short:

<div class="message-cards">

<div class="message-card from-agent">
<p class="message-card-head">[upload:event_batch]<span class="message-card-direction">agent → controller</span></p>

```http
POST /api/node/events HTTP/1.1
Authorization: Bearer 3f1c…
Ankor-Protocol: <protocol version>
Content-Type: application/json

{
  "allocation_id": "5b1e2c7a-8d4f-…",
  "frames": [
    { "seq": 397,
      "event": { "type": "player_death", … } },
    …
    { "seq": 412,
      "event": { "type": "map_end",
        "winner_team": 1,
        "team1_score": 13, "team2_score": 9,
        … } }
  ]
}
```

</div>

<div class="message-card from-controller">
<p class="message-card-head">[upload:event_batch_receipt]<span class="message-card-direction">controller → agent</span></p>

```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "rejected": []
}
```

</div>
</div>

## Answers

Your controller answers each batch in the HTTP response to its request. The status decides what
the agent does with the batch:

<figure class="diagram diagram-seq" aria-label="The agent posts a batch; the controller answers a 2xx with a receipt and the agent deletes the batch, or anything else and the agent backs off and retries.">
<div class="diagram-seq-head"><div class="diagram-card"><p class="diagram-name">Agent</p></div><div class="diagram-card"><p class="diagram-name">Controller</p></div></div>
<div class="diagram-seq-body">
<div class="diagram-msg right from-agent"><p class="diagram-msg-label"><code>POST /api/node/events</code><small><code>Authorization: Bearer &lt;node token&gt;</code>, <code>Ankor-Protocol</code><br><code>{ "allocation_id": …, "frames": [ seq 128 … 255 ] }</code></small></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The controller answers in the same HTTP response, with one of:</p>
<div class="diagram-msg left from-controller"><p class="diagram-msg-label"><code>2xx</code> batch stored<small><code>{ "rejected": [] }</code></small></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The agent deletes seq 128–255 and sends the next batch.</p>
<div class="diagram-msg left implied quiet"><p class="diagram-msg-label">Anything else, or no answer within [limit-time:EVENT_UPLOAD_TIMEOUT_MS]<small>body ignored, except a <code>409</code>'s <code>detail</code>, which is logged</small></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The agent keeps the batch and reports <code>event_backlog</code> over the WebSocket. It retries after [limit-time:EVENT_RETRY_MIN_MS] and doubles the wait after each failure, up to [limit-time:EVENT_RETRY_MAX_MS].</p>
</div>
</figure>

| Status                                            | Body                                                | The agent                                                                                                                                                                                   |
|---------------------------------------------------|-----------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `2xx`                                             | `EventBatchReceipt`                                 | Deletes the whole batch. It never resends frames listed in `rejected`, because they would fail the same way. A body that is not a valid receipt counts as a refusal of that server's batch. |
| `401`, `403`, `408`, `429`, `502`, `503` or `504` | ignored                                             | Keeps the batch, pauses every server's uploads, backs off and retries. No answer within [limit-time:EVENT_UPLOAD_TIMEOUT_MS] counts the same.                                               |
| any other status                                  | ignored, except a `409`'s `detail`, which is logged | Keeps the batch and backs off only that server. The other servers keep uploading.                                                                                                           |

While uploads fail, the agent reports the events that wait on the node in an
[agent:event_backlog] over the WebSocket. The agent sends the message when the backlog changes, and
once more with nothing stuck when uploads work again. Each message carries the whole backlog, so
replace your stored backlog with it. Its `reason` gives the cause of the last failed upload.
[agent:hello] carries the same backlog in its `event_backlog` field.

### Receipt

A receipt covers the whole batch. Normally you store every frame, and `rejected` is empty. If you
cannot use a frame, quarantine it and list it in `rejected` with a reason. To quarantine a frame
means to set it aside with that reason instead of counting it. You would quarantine a frame whose
event names a player you did not launch the match with, for example. The agent logs the
quarantined frames.

A receipt for a batch in which your controller quarantined one frame looks like this:

```json
{
  "rejected": [{ "seq": 140, "reason": "unknown weapon" }]
}
```

### Refusing a batch

The agent checks every batch before sending it. A batch that cannot be trusted therefore points to
a bug or to damage in transit. Refuse such a batch with `409` and the body below, so that the cause
becomes visible. Its `detail` appears in the agent's log and in [agent:event_backlog].

```json
{
  "code": "event_integrity_error",
  "detail": "frames 3 and 4 are not consecutive"
}
```

## Handling a batch

You decide what your controller does with the frames. If you keep statistics, the steps below keep
them consistent however often the node sends a batch. You can store, log or drop quarantined
frames.

For each request:

1. **Authenticate.** Answer `401` for an unknown bearer token. Answer `426` when `Ankor-Protocol`
   is missing or names an [incompatible version](versioning.md).
2. **Check the batch.** [Refuse it](#refusing-a-batch) if the body does not parse with
   `EventBatch` from [`@ankorcs/protocol`](https://www.npmjs.com/package/@ankorcs/protocol). The
   body fails to parse, for example, when it names no server or its frames are not consecutive.
3. **Find the server and its match.** The `allocation_id` of the batch must name a server that your
   controller launched on this node. The match of that launch is the match of the batch. Parse each
   frame on its own with [`EventBatchFrame`](/reference/types/#schema-EventBatchFrame). If the
   server or the match is not yours, quarantine every frame and still answer with a receipt, so
   that the node deletes them. A retry cannot fix that batch.
4. **Walk the frames.** Keep a cursor per server, which is the highest `seq` handled so far.
   - If a `seq` is at or below the cursor, the frame is a duplicate. Check that it matches the
     stored event, and store nothing new.
   - If a `seq` is above the cursor, the frame is new. Store the event if it parses and names only
     players that you launched the match with. Otherwise,
     quarantine it with a reason. Either way, the cursor moves past it, so one bad event never
     delays the rest.
   - When an event has a `type` that your protocol version does not have, it comes from a newer
     plugin. Move the cursor past it, and store or quarantine nothing.
     [Versioning](versioning.md#in-a-controller) explains how to detect such an event and why it
     still takes a `seq`.
   - [event:player_connect] with `rejected` set is the one event that names an outsider by design.
     It records that the plugin kicked that player.
   - If a `seq` jumps past cursor + 1, the frames in between are missing. Log the gap and continue
     with the frame. The agent skips numbers only when it lost events, for example when an operator
     deleted its event store.
5. **Commit, then answer.** Store the events, the quarantined frames and the cursor in one
   transaction. Then answer with a `2xx` status and a receipt for the whole batch. If you cannot
   commit, for example because storage is down, answer `503`. The agent keeps the batch, backs off
   and tries again.

### An example

This handler takes the steps above in Node.js.

[example:controller/events]

## Reading the events

These rules apply when you build statistics from the stored events:

- Events can arrive after their round or after their server has shut down. Place each event by its
  `tick`, and by its `round` where it carries one, not by the time it arrived.
- [event:round_complete] follows every event of its round.
- A grenade is spread over several events. [event:grenade_throw] gives the grenade a `grenade_id`
  that is unique within the server. The events that the grenade causes later carry that id, up to
  its [event:grenade_end].
- A `null` measurement means that the plugin could not read it. Store it as missing, not as zero.
- After a [event:match_restored], the server plays its `round` again. The events from the next
  [event:round_start] on replace what the server sent for that round and the rounds after it.
