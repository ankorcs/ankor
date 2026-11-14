/**
 * Limits that depend on each other, so a change to one cannot leave the other unsafe.
 */
import { describe, expect, it } from 'vitest';
import { MAX_FRAME_BYTES } from '../src/container.js';
import { EVENT_BATCH_MAX_BYTES } from '../src/index.js';

describe('event limits', () => {
  // The agent stores an event as its frame without the frame's type, so an event that fits a frame
  // fits an upload batch; one larger than a batch could never be uploaded.
  it('keeps every frame the relay accepts inside one upload batch', () => {
    expect(MAX_FRAME_BYTES).toBeLessThanOrEqual(EVENT_BATCH_MAX_BYTES);
  });
});
