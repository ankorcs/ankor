// #region example
import { createHash } from 'node:crypto';
import type { IncomingMessage, ServerResponse } from 'node:http';
import {
  EventBatch,
  EventBatchFrame,
  PluginToControllerMessage,
  PROTOCOL_HEADER,
  type EventBatchReceipt,
  type EventBatchRejection,
} from '@ankorcs/protocol';
import { bearerNodeToken, isCompatibleProtocol, unknownMessageType } from '@ankorcs/protocol/helpers';

function answer(response: ServerResponse, status: number, body?: EventBatchReceipt | EventBatchRejection): void {
  response.writeHead(status, body === undefined ? {} : { 'Content-Type': 'application/json' });
  response.end(body === undefined ? undefined : JSON.stringify(body));
}

// POST <events path>. The numbered steps follow Handling a batch.
async function handleEventBatch(request: IncomingMessage, response: ServerResponse): Promise<void> {
  // 1. Authenticate.
  const token = bearerNodeToken(request.headers.authorization);
  // Your code: find the stored node by its token, as in Accept a node.
  const node = token === null ? null : findNodeByTokenHash(createHash('sha256').update(token).digest('hex'));

  if (node === null) {
    answer(response, 401);

    return;
  }

  if (!isCompatibleProtocol(request.headers[PROTOCOL_HEADER.toLowerCase()])) {
    answer(response, 426);

    return;
  }

  // 2. Check the batch.
  let body: unknown;

  try {
    // Your code: read the body as JSON. Accept bodies up to EVENT_BATCH_MAX_BYTES plus the wrapper.
    body = await readJson(request);
  } catch {
    answer(response, 409, { code: 'event_integrity_error', detail: 'the body is not a JSON batch' });

    return;
  }

  const batch = EventBatch.safeParse(body);

  if (!batch.success) {
    answer(response, 409, {
      code: 'event_integrity_error',
      detail: batch.error.issues.map((issue) => issue.message).join('; '),
    });

    return;
  }

  const { allocation_id: allocationId, frames } = batch.data;

  try {
    // Your code: run this in one storage transaction, committed when it returns.
    const rejected = await inTransaction(async (tx) => {
      // 3. Find the server and its match.
      // Your code: find this node's launch of the allocation and lock its cursor for the
      // transaction, for example with SELECT … FOR UPDATE. Return null when it is not yours.
      const launch = await tx.lockLaunch(node, allocationId);

      if (launch === null) {
        // Quarantine every frame so that the node deletes them. A retry cannot fix this batch.
        return frames.map((frame) => ({ seq: frame.seq, reason: 'not a server launched on this node' }));
      }

      // 4. Walk the frames.
      const quarantined: EventBatchReceipt['rejected'] = [];

      for (const raw of frames) {
        if (raw.seq <= launch.cursor) {
          // A duplicate, sent again because a receipt was lost. Store nothing new.
          // Your code: compare the frame with the event stored under its `seq`.
          if (!(await launch.matchesStored(raw))) {
            log.warn(`event ${raw.seq} of ${allocationId} differs from the one stored`);
          }

          continue;
        }

        if (raw.seq > launch.cursor + 1) {
          log.warn(`events ${launch.cursor + 1}–${raw.seq - 1} of ${allocationId} are missing`);
        }

        launch.cursor = raw.seq;

        // An event type from a newer plugin. The cursor has moved past it; store nothing.
        if (unknownMessageType(PluginToControllerMessage, raw['event']) !== null) {
          continue;
        }

        const frame = EventBatchFrame.safeParse(raw);

        if (!frame.success) {
          // Your code: set the frame aside with its reason, out of your statistics. The cursor has
          // already moved past it.
          await launch.quarantine(raw.seq, 'does not match its event schema');
          quarantined.push({ seq: raw.seq, reason: 'does not match its event schema' });

          continue;
        }

        // Your code: check that the event names only players from the match's roster. A
        // `player_connect` with `rejected` set names an outsider by design.
        if (!namesOnlyRoster(launch, frame.data)) {
          await launch.quarantine(raw.seq, 'names a player outside the roster');
          quarantined.push({ seq: raw.seq, reason: 'names a player outside the roster' });

          continue;
        }

        // Your code: store the event.
        await launch.store(frame.data);
      }

      // Your code: store the cursor in the same transaction.
      await launch.saveCursor(launch.cursor);

      return quarantined;
    });

    // 5. Commit, then answer with a receipt for the whole batch.
    answer(response, 200, { rejected });
  } catch {
    // Storage failed: the agent keeps the batch and retries after a backoff.
    answer(response, 503);
  }
}
// #endregion

export { handleEventBatch };
