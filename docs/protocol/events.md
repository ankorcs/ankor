---
title: Event uploads
description: How gameplay events reach the controller, and how to handle each batch.
slug: protocol/events
---

Statistics are built from gameplay events: kills, damage, grenades, rounds and the final score.
The plugin reports them to the agent, which uploads them to your controller in batches. You need
this endpoint when you launch matches that send statistics
([`match.send_statistics`](/reference/types/#schema-MatchConfig)).

A match's phase and score go over the socket instead, as
[Get the result](index.md#7-get-the-result) describes. A match without statistics sends its backups
there too, as [agent:backup]. The [protocol reference](/reference/uploads/) has the batch body, the
answers and every event type.

## Event identity

Every event has an identity of two parts:

- **`allocation_id`**: the server that sent it, named by the allocation you gave it in
  [agent:start_server]. The allocation names one server, because a node refuses to launch an
  allocation it has launched before. A batch carries the events of one server and names it once
  for all of them.
- **`seq`**: the event's position among that server's events, counting from 0 without gaps.

<figure class="diagram" aria-label="A server numbers its events from 0, and batches carry them to the controller. After a lost receipt, the next batch starts again at the first event that no receipt covered.">
<p class="diagram-head"><span class="diagram-name">Server <code>0199a0b4-…-abc1</code></span><small>one allocation, numbering its events from 0</small></p>
<div class="diagram-groups">
<div class="diagram-group"><span class="diagram-group-label">batch 1 · seq 0–3</span><div class="diagram-chips"><span class="diagram-chip"><b>0</b><span>map_start</span></span><span class="diagram-chip"><b>1</b><span>going_live</span></span><span class="diagram-chip"><b>2</b><span>round_start</span></span><span class="diagram-chip"><b>3</b><span>player_hurt</span></span></div></div>
<div class="diagram-group"><span class="diagram-group-label">batch 2 · seq 4–7</span><div class="diagram-chips"><span class="diagram-chip"><b>4</b><span>player_death</span></span><span class="diagram-chip"><b>5</b><span>round_end</span></span><span class="diagram-chip"><b>6</b><span>round_mvp</span></span><span class="diagram-chip"><b>7</b><span>round_complete</span></span></div></div>
<div class="diagram-group waiting"><span class="diagram-group-label">arrive on the node meanwhile · seq 8–12</span><div class="diagram-chips"><span class="diagram-chip waiting"><b>8</b><span>round_start</span></span><span class="diagram-chip waiting"><b>9</b><span>…</span></span><span class="diagram-chip waiting"><b>12</b><span>player_hurt</span></span></div></div>
</div>
</figure>

1. Batch 1 arrives. The controller stores seq 0–3 and answers a receipt for 0–3. The agent deletes
   them.
2. Batch 2 arrives. The controller stores seq 4–7, but the receipt is lost on the way back. The
   agent keeps them, and seq 8–12 arrive on the node meanwhile.
3. After its backoff, the agent sends seq 4–12 in one batch. The controller already has seq 4–7 of
   that server, so it stores 8–12 and answers a receipt for 4–12.
4. Uploads continue from seq 13.

Each attempt is rebuilt from the oldest event that no receipt covered. It carries as many of the
server's events as fit, up to [limit:EVENT_BATCH_MAX_FRAMES] events and
[limit-size:EVENT_BATCH_MAX_BYTES]. A retry therefore starts at the same `seq` as the attempt that
failed, and it can carry more events. The events it repeats are the same as those you stored,
because a server's events never change once the node has them.

Events survive an agent restart or a lost connection. They stay on the node until a receipt covers
them. The node keeps a server's events for [limit-time:EVENT_SPOOL_RETENTION_MS] after its last
event or receipt, so that it recognises a replay of delivered events. Events that no controller
took in that time are dropped, and the agent logs how many.

The agent backs off every server when no answer comes, or when your controller answers that it is
unavailable or refuses the node. When you refuse one server's batch, the agent backs off only that
server. The node's other servers keep uploading.

## `POST <events path>`

The agent posts to [config:controller.events_path], `/api/node/events` by default. Each request carries
the agent's [headers](index.md#what-you-need) and a JSON body for one server. The body holds the
server's `allocation_id` and 1–[limit:EVENT_BATCH_MAX_FRAMES] frames of its events, with
consecutive `seq`. The frames come to at most [limit-size:EVENT_BATCH_MAX_BYTES] together, so
accept request bodies of that size plus the wrapper. Each `event` carries the fields of its
gameplay event, cut short here, and the last batch of a map ends with [event:map_end]:

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

Your controller answers each batch in the HTTP response. Uploads stay off the WebSocket, apart from
[agent:event_backlog]. That message carries the node's whole backlog of stuck events, and the agent
sends it when the backlog changes. Replace your stored backlog with it. [agent:hello] carries the
same backlog in its `event_backlog`.

<figure class="diagram diagram-seq" aria-label="The agent posts a batch; the controller answers a 2xx with a receipt and the agent deletes the batch, or anything else and the agent backs off and retries.">
<div class="diagram-seq-head"><div class="diagram-card"><p class="diagram-name">Agent</p></div><div class="diagram-card"><p class="diagram-name">Controller</p></div></div>
<div class="diagram-seq-body">
<div class="diagram-msg right from-agent"><p class="diagram-msg-label"><code>POST /api/node/events</code><small><code>Authorization: Bearer &lt;node token&gt;</code>, <code>Ankor-Protocol</code><br><code>{ "allocation_id": …, "frames": [ seq 128 … 255 ] }</code></small></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The controller answers in the same HTTP response, with one of:</p>
<div class="diagram-msg left from-controller"><p class="diagram-msg-label"><code>2xx</code> batch stored<small><code>{ "rejected": [] }</code></small></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The agent deletes seq 128–255 and sends the next batch.</p>
<div class="diagram-msg left implied quiet"><p class="diagram-msg-label">Anything else, or no answer within [limit-time:EVENT_UPLOAD_TIMEOUT_MS]<small>body ignored, except a <code>409</code>'s <code>detail</code>, which is logged</small></p><span class="diagram-arrow"></span></div>
<p class="diagram-note">The agent keeps the batch, backs off from [limit-time:EVENT_RETRY_MIN_MS] up to [limit-time:EVENT_RETRY_MAX_MS], reports <code>event_backlog</code> over the WebSocket and retries.</p>
</div>
</figure>

| Status                                            | Body                                                | The agent                                                                                                                                                                                   |
|---------------------------------------------------|-----------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `2xx`                                             | `EventBatchReceipt`                                 | Deletes the whole batch. It never resends frames listed in `rejected`, because they would fail the same way. A body that is not a valid receipt counts as a refusal of that server's batch. |
| `401`, `403`, `408`, `429`, `502`, `503` or `504` | ignored                                             | Keeps the batch, pauses every server's uploads, backs off and retries. No answer within [limit-time:EVENT_UPLOAD_TIMEOUT_MS] counts the same.                                               |
| any other status                                  | ignored, except a `409`'s `detail`, which is logged | Keeps the batch and backs off only that server. The other servers keep uploading.                                                                                                           |

### Receipt

A receipt covers the whole batch. Normally you keep every frame, and `rejected` is empty. If you
cannot use a frame, for example because its event names another match, quarantine it and list it in
`rejected`. Quarantining sets a frame aside with a reason instead of counting it. For a batch where
one event was set aside:

```json
{
  "rejected": [{ "seq": 140, "reason": "unknown weapon" }]
}
```

### Refusing a batch

A batch that cannot be trusted points to a bug or to damage in transit, because the agent checks
every batch before sending it. To make the cause visible, refuse the batch with `409` and this
body. Its `detail` shows up in the agent's log and in [agent:event_backlog]:

```json
{
  "code": "event_integrity_error",
  "detail": "frames 3 and 4 are not consecutive"
}
```

## Handling a batch

You decide what your controller does with the frames. If you keep statistics, the steps below
keep them consistent however often a batch is retried. You can store, log or drop quarantined
frames.

For each request:

1. **Authenticate.** Answer `401` for an unknown bearer token. Answer `426` when `Ankor-Protocol`
   is missing or names an [incompatible version](versioning.md).
2. **Check the batch.** [Refuse it](#refusing-a-batch) if the body does not parse with
   `EventBatch` from [`@ankorcs/protocol`](https://www.npmjs.com/package/@ankorcs/protocol). It
   fails to parse when it does not name a server or its frames are not consecutive.
3. **Find the server and its match.** The batch's `allocation_id` must name a server that your
   controller launched on this node. That launch's match is the batch's match. Parse each frame
   with [`EventBatchFrame`](/reference/types/#schema-EventBatchFrame). If the server or the match
   is not yours, quarantine every frame and still answer with a receipt, so that the node deletes
   them. A retry cannot fix that batch.
4. **Walk the frames.** Keep a cursor per server: the highest `seq` handled so far.
   - A `seq` at or below the cursor is a duplicate. Check that it matches the stored event, and
     store nothing new.
   - A `seq` above the cursor is new. Store the event if it parses, belongs to the batch's match
     and names only players you launched the match with. Otherwise quarantine it with a reason.
     Either way the cursor moves past it, so one bad event never delays the rest.
   - An event whose `type` your protocol version does not have comes from a newer plugin. Move the
     cursor past it, and store or quarantine nothing. [Versioning](versioning.md) explains why.
   - [event:player_connect] with `rejected` set is the one event that names an outsider by design.
     It means the plugin kicked that player.
   - A jump past cursor + 1 is a gap. Log it and carry on. The agent skips numbers only when it
     lost them, as when an operator deleted its event store.
5. **Commit, then answer.** Store the events, the quarantined frames and the cursor in one
   transaction. Then answer with a `2xx` status and a receipt for the whole batch. If you cannot,
   for example because storage is down, answer with a `5xx`. The agent backs off and tries again.

## Reading the events

- Events can arrive after their round or after the server has ended. Place them by their `tick`,
  and their `round` where they carry one, not by arrival.
- [event:round_complete] follows every event of its round.
- A grenade is spread over several events. [event:grenade_throw] gives it a `grenade_id` that is
  unique within the server. The events it causes later carry that id, up to its
  [event:grenade_end].
- A `null` measurement means the plugin could not read it. Keep it missing, not zero.
- After a [event:match_restored], the events from the next [event:round_start] on replace what the
  server sent for that round and the rounds after it.
