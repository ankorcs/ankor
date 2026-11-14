/**
 * The agent's upload of stored events to the controller, at {@link DEFAULT_EVENTS_PATH} unless
 * `agent.yml` sets `controller.events_path`.
 */
import { z } from 'zod';
import { PluginToControllerMessage } from './game-events.js';
import type { DEFAULT_EVENTS_PATH } from './paths.js';
import { AllocationId } from './values.js';

/** An event's position among its server's events, from 0. */
export const EventSeq = z.number().int().nonnegative();

/**
 * One frame of an event batch: an event and its position among the events of the server that the
 * batch names. A node runs each allocation once and refuses to launch one it has run before, so the
 * batch's `allocation_id` and the frame's `seq` identify the event. Parse each frame of a batch on
 * its own with this schema.
 */
export const EventBatchFrame = z.object({
  /** The event's position among the events of its server. */
  seq: EventSeq,
  // Retain original payload fields, including additional nested measurements.
  /** The gameplay event, one of {@link PluginToControllerMessage}. */
  event: PluginToControllerMessage.and(z.record(z.string(), z.unknown())),
});

export type EventBatchFrame = z.infer<typeof EventBatchFrame>;

/**
 * The most frames one event upload carries. The agent sends a server's events in batches of up to
 * this many.
 */
export const EVENT_BATCH_MAX_FRAMES = 128;

/**
 * The most bytes the frames of one event upload come to together. A receiver accepts request bodies
 * of this size plus the `frames` wrapper, because refusing a batch for its size stalls the upload
 * of that server's events.
 */
export const EVENT_BATCH_MAX_BYTES = 2 * 1024 * 1024;

/**
 * What the agent reads of a batch frame: its position. The controller validates the event itself.
 */
export const EventFrame = z.looseObject({ seq: EventSeq });

export type EventFrame = z.infer<typeof EventFrame>;

/**
 * The body of an event upload: 1 to {@link EVENT_BATCH_MAX_FRAMES} frames of the server that
 * `allocation_id` names, with consecutive `seq`. Store frames idempotently by `allocation_id` and
 * `seq`, because a lost receipt means the same frames arrive again.
 *
 * This schema checks only each frame's position, so one bad event cannot fail the batch. Parse each
 * frame on its own with {@link EventBatchFrame}, and quarantine a frame that fails by listing it in
 * `rejected` of the {@link EventBatchReceipt}.
 *
 * A server plays the one match of its launch, so the match of a batch is the match that the controller
 * launched `allocation_id` with. The frames come to at most {@link EVENT_BATCH_MAX_BYTES}
 * together.
 *
 * Rejected unless each frame's `seq` is one above the `seq` of the frame before it.
 */
export const EventBatch = z.object({
  /** The server that sent every event of the batch. */
  allocation_id: AllocationId,
  /** The frames, in `seq` order. */
  frames: z.array(EventFrame).min(1).max(EVENT_BATCH_MAX_FRAMES),
}).superRefine(({ frames }, ctx) => {
  const first = frames[0]!;

  for (const [index, frame] of frames.entries()) {
    if (frame.seq !== first.seq + index) {
      ctx.addIssue({ code: 'custom', path: ['frames', index], message: 'A batch must contain one contiguous sequence' });
    }
  }
});

export type EventBatch = z.infer<typeof EventBatch>;

/**
 * The controller's answer to a batch it holds. Every frame of the batch is either canonical
 * history or quarantined under one of the `rejected` entries, and the agent may delete them all.
 * Resending a quarantined frame would fail the same way.
 */
export const EventBatchReceipt = z.object({
  /** Frames quarantined instead of stored. */
  rejected: z.array(z.object({
    /** The quarantined frame. */
    seq: EventSeq,
    /** Why it was quarantined. */
    reason: z.string().min(1),
  })),
});

export type EventBatchReceipt = z.infer<typeof EventBatchReceipt>;

/**
 * The body of a `409` answer to an event upload. The controller could not read the identities of
 * the batch, so it refuses the whole batch. The agent retries that server with a backoff of its
 * own, and the node's other servers upload meanwhile.
 */
export const EventBatchRejection = z.object({
  /** Always `event_integrity_error`. */
  code: z.literal('event_integrity_error'),
  /** What made the batch untrustworthy. */
  detail: z.string(),
});

export type EventBatchRejection = z.infer<typeof EventBatchRejection>;
