/**
 * What a running agent hands a newer agent image when {@link ControllerUpdateAgent | update_agent}
 * replaces it. None of this may change, because the old agent is the caller and a node on any
 * release must be able to swap to any later image.
 */
import type { ControllerUpdateAgent } from './index.js';

export const SELF_UPDATE_COMMAND = ['ankor-self-update'] as const;

export const SELF_UPDATE_ENV = {
  /** The Docker create payload for the replacement container as JSON, including its name. */
  spec: 'ANKOR_SELF_UPDATE_SPEC',
  /** The id of the container being replaced. */
  previousId: 'ANKOR_SELF_UPDATE_PREVIOUS',
  /**
   * Where the helper records a swap that failed, for the agent that comes up after it. A swap that
   * succeeds removes the record.
   */
  failure: 'ANKOR_SELF_UPDATE_FAILURE',
} as const;
