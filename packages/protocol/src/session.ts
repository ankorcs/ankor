/**
 * One connected agent, from the controller's side of the agent socket. The controller hands the
 * session each frame that arrives and a function that writes one, so any WebSocket server works.
 */
import type { z } from 'zod';
import {
  AGENT_ERRORS,
  AgentToControllerMessage,
  STOP_REPLY_MARGIN_SECONDS,
  STOP_TIMEOUT_SECONDS,
  ControllerToAgentMessage,
  type AgentError,
  type AgentReply,
  type AgentShutdown,
  type ReplyData,
  type RequestType,
  type ControllerCheckCs2Build,
  type ControllerHelloRefused,
  type ControllerStopServer,
  type ControllerWatchLogs,
  type ControllerWatchResources,
  type ControllerWelcome,
} from './index.js';
import { unknownMessageType } from './helpers.js';

/** Milliseconds a request waits for its reply unless the session or the call sets another wait. */
export const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;

/**
 * How long `request` waits for its reply, when other requests wait `wait`. A
 * {@link ControllerStopServer | stop_server} replies once its container stopped, so it waits
 * {@link STOP_TIMEOUT_SECONDS} and {@link STOP_REPLY_MARGIN_SECONDS}, whatever `wait` is. Null waits until
 * the socket closes. A controller that relays requests through something with its own deadline
 * gives that deadline this wait too.
 */
export function replyWait(request: { type: string }, wait: number): number;

export function replyWait(request: { type: string }, wait: number | null): number | null;

export function replyWait(request: { type: string }, wait: number | null): number | null {
  if (request.type !== 'stop_server') {
    return wait;
  }

  return (STOP_TIMEOUT_SECONDS + STOP_REPLY_MARGIN_SECONDS) * 1000;
}

/**
 * A request as the controller builds it: its `type` and fields, without the `id` that the session
 * picks. Fields with a default may be left out, since the session sends what the schema accepts.
 */
export type SessionRequest<T extends RequestType = RequestType> = Extract<z.input<typeof ControllerToAgentMessage>, { type: T }> extends infer M
  ? M extends unknown ? Omit<M, 'id'> : never
  : never;

/**
 * A frame the controller sends that gets no reply: {@link ControllerWelcome | welcome},
 * {@link ControllerHelloRefused | hello_refused}, the watches {@link ControllerWatchResources | watch_resources}
 * and {@link ControllerWatchLogs | watch_logs}, and {@link ControllerCheckCs2Build | check_cs2_build}.
 */
export type ControllerNotice = Exclude<ControllerToAgentMessage, { id: string }>;

/** A frame the agent sends that is not a reply. */
export type AgentReport = Exclude<AgentToControllerMessage, { type: 'reply' }>;

/** A {@link AgentReply | reply} that the agent sent. */
export type AgentReplyMessage = Extract<AgentToControllerMessage, { type: 'reply' }>;

/** One way a frame failed the contract, as zod reports it. */
export type ContractIssue = NonNullable<ReturnType<typeof AgentToControllerMessage.safeParse>['error']>['issues'][number];

/**
 * How a request came out.
 *
 * - `ok`: the agent replied that the request succeeded.
 * - `error`: the agent replied that it refused or failed the request.
 * - `timeout`: no reply came before the request's wait ran out.
 * - `closed`: the socket closed before a reply came.
 *
 * After `timeout` or `closed`, the agent may still have done the work, and its reports say whether
 * it did.
 */
export type RequestResult<T extends RequestType> =
  | { status: 'ok'; data: ReplyData<T> }
  | { status: 'error'; error: AgentError }
  | { status: 'timeout' }
  | { status: 'closed' };

/**
 * What one received frame was.
 *
 * - `report`: an {@link AgentReport}.
 * - `reply`: a reply, which the frame carries. `answered` is `true` when the reply settled the
 *   request it answers, and `false` when it answered no open request, as one past its timeout does.
 * - `unsupported`: a message whose `type` this version does not have, from a newer agent. Skip it.
 * - `invalid`: any other frame that failed the contract, one of a known type included. `issues`
 *   lists how, and is empty for a frame that is not JSON. `detail` joins the issues for a log line.
 */
export type ReceivedFrame =
  | { kind: 'report'; message: AgentReport }
  | { kind: 'reply'; answered: boolean; message: AgentReplyMessage }
  | { kind: 'unsupported'; type: string }
  | { kind: 'invalid'; detail: string; issues: ContractIssue[] };

export interface AgentSessionOptions {
  /** Writes one text frame to this agent's socket. */
  send: (text: string) => void;
  /**
   * Milliseconds a request waits for its reply. Null waits until the socket closes. A
   * {@link ControllerStopServer | stop_server} waits as long as its stop takes instead: see
   * {@link replyWait}.
   */
  timeoutMs?: number | null;
}

export interface RequestOptions {
  /** Milliseconds this request waits for its reply, in place of the session's wait; not for a stop, which waits by its grace. */
  timeoutMs?: number | null;
}

export interface AgentSession {
  /**
   * Sends a request under an id that the session picks, and resolves with its reply, a timeout or
   * the socket's close. After the agent's {@link AgentShutdown | shutdown}, it sends nothing and
   * resolves with the `shutting_down` error the agent would answer.
   */
  request<R extends SessionRequest>(request: R, options?: RequestOptions): Promise<RequestResult<R['type']>>;
  /** Sends a frame that gets no reply. */
  notify(message: ControllerNotice): void;
  /** Reads one frame the agent sent, as it arrived on the socket. */
  receive(raw: string | ArrayBuffer | Uint8Array): ReceivedFrame;
  /** Reads one frame the agent sent that the controller has already parsed from JSON. */
  receiveParsed(json: unknown): ReceivedFrame;
  /**
   * Tells the session that the socket closed. Every open request ends `closed`, and later ones end
   * `closed` immediately.
   */
  closed(): void;
}

interface Pending {
  type: RequestType;
  settle: (result: RequestResult<RequestType>) => void;
  timer: ReturnType<typeof setTimeout> | undefined;
}

const decoder = new TextDecoder();

function invalid(issues: ContractIssue[]): ReceivedFrame {
  return { kind: 'invalid', detail: issues.map((issue) => `${issue.path.join('.')}: ${issue.message}`).join('; '), issues };
}

export function createAgentSession(options: AgentSessionOptions): AgentSession {
  const pending = new Map<string, Pending>();
  const sessionTimeout = options.timeoutMs === undefined ? DEFAULT_REQUEST_TIMEOUT_MS : options.timeoutMs;
  let isClosed = false;
  // After the agent's shutdown, it refuses every new request, so the session answers for it.
  let isStopping = false;

  function take(id: string): Pending | undefined {
    const open = pending.get(id);

    if (open !== undefined) {
      pending.delete(id);
      clearTimeout(open.timer);
    }

    return open;
  }

  /** The call's wait, else the session's; a stop's by its grace period as parsed with its default. */
  function waitFor(message: ControllerToAgentMessage, requestOptions: RequestOptions): number | null {
    return replyWait(message, requestOptions.timeoutMs !== undefined ? requestOptions.timeoutMs : sessionTimeout);
  }

  function receiveParsed(json: unknown): ReceivedFrame {
    const unknownType = unknownMessageType(AgentToControllerMessage, json);

    if (unknownType !== null) {
      return { kind: 'unsupported', type: unknownType };
    }

    const parsed = AgentToControllerMessage.safeParse(json);

    if (!parsed.success) {
      return invalid(parsed.error.issues);
    }

    const message = parsed.data;

    if (message.type !== 'reply') {
      if (message.type === 'shutdown') {
        isStopping = true;
      }

      return { kind: 'report', message };
    }

    const open = pending.get(message.reply_to);

    if (open === undefined) {
      return { kind: 'reply', answered: false, message };
    }

    if (message.request !== null && message.request !== open.type) {
      return invalid([{
        code: 'custom',
        path: ['request'],
        message: `the reply to ${open.type} ${message.reply_to} names ${message.request}`,
        input: message.request,
      }]);
    }

    take(message.reply_to);
    open.settle(message.ok ? { status: 'ok', data: message.data } : { status: 'error', error: message.error });

    return { kind: 'reply', answered: true, message };
  }

  return {
    request<R extends SessionRequest>(request: R, requestOptions: RequestOptions = {}): Promise<RequestResult<R['type']>> {
      if (isClosed) {
        return Promise.resolve({ status: 'closed' });
      }

      if (isStopping) {
        return Promise.resolve({ status: 'error', error: { code: AGENT_ERRORS.shuttingDown, detail: 'the agent is shutting down' } });
      }

      const id = crypto.randomUUID();

      // A throw in here, from the contract or the socket, rejects the promise.
      return new Promise<RequestResult<R['type']>>((resolve) => {
        const message = ControllerToAgentMessage.parse({ ...request, id });
        const timeoutMs = waitFor(message, requestOptions);
        const timer = timeoutMs === null ? undefined : setTimeout(() => {
          take(id);
          resolve({ status: 'timeout' });
        }, timeoutMs);

        // The reply's `request` was checked against this type before it settles the promise.
        pending.set(id, { type: request.type, settle: resolve as (result: RequestResult<RequestType>) => void, timer });

        try {
          options.send(JSON.stringify(message));
        } catch (error) {
          take(id);

          throw error;
        }
      });
    },

    notify(message: ControllerNotice): void {
      options.send(JSON.stringify(ControllerToAgentMessage.parse(message)));
    },

    receive(raw: string | ArrayBuffer | Uint8Array): ReceivedFrame {
      let json: unknown;

      try {
        json = JSON.parse(typeof raw === 'string' ? raw : decoder.decode(raw));
      } catch {
        return { kind: 'invalid', detail: 'not JSON', issues: [] };
      }

      return receiveParsed(json);
    },

    receiveParsed,

    closed(): void {
      isClosed = true;

      for (const id of [...pending.keys()]) {
        take(id)?.settle({ status: 'closed' });
      }
    },
  };
}
