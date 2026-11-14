/**
 * The limits a controller enforces on a node before it closes the socket or refuses the request,
 * then the agent's timeouts and retry pacing as a controller sees them. An agent within the limits
 * is never limited, and Ankor's agent stays well within them.
 */
import { z } from 'zod';
import type {
  AGENT_RECONNECT_JITTER,
  AgentCs2UpdateProgress,
  AgentDemo,
  AgentDemoProgress,
  AgentDemoRemoved,
  AgentEventBacklog,
  AgentHello,
  AgentLogBatch,
  AgentLogIndex,
  AgentBackup,
  AgentPhaseChange,
  AgentNodeStatus,
  AgentReply,
  AgentResourceBatch,
  AgentScore,
  AgentServerExited,
  AgentShutdown,
  AgentSlotStatus,
  EventBacklog,
  ServerExitReason,
  ControllerCheckCs2Build,
  ControllerHelloRefused,
  ControllerSetConfig,
  ControllerWelcome,
} from './agent-socket.js';
import type { MAX_BACKUP_FILE_CHARS } from './game-events.js';

/**
 * Most slots a node may have, in `agent.yml`, {@link AgentHello | hello} and
 * {@link ControllerSetConfig | set_config}.
 */
export const MAX_SLOTS = 64;

/**
 * Smallest `memory_mb`, in MiB. A game server uses about 700 MiB for a 1v1 and more for a full
 * match, so a lower cap would stop servers mid-match.
 */
export const MIN_MEMORY_MB = 1024;

/** Largest `memory_mb`, in MiB, far above what a game server uses. */
export const MAX_MEMORY_MB = 65536;

/**
 * Most demos a {@link AgentHello | hello} lists. A node that keeps more lists its newest by
 * `modified_at`. An older demo is listed once newer ones are no longer on the node.
 */
export const MAX_REPORTED_DEMOS = 50;

/**
 * Most stuck servers an {@link EventBacklog} lists, oldest first. Its counts include the rest. A
 * newer server is listed once older ones are uploaded.
 */
export const MAX_REPORTED_BACKLOGS = 100;

/**
 * Allocations a node remembers, the most recently launched, and refuses to launch again with
 * `allocation_reused`. A controller that picks a fresh UUID for every launch never meets the
 * limit.
 */
export const REMEMBERED_ALLOCATIONS = 100;

/**
 * Bytes in the largest socket message the controller reads. A larger message closes the socket
 * with code `1009`. A 64-slot {@link AgentHello | hello} listing every demo and every backlog it
 * may carry uses under half.
 */
export const AGENT_FRAME_MAX_BYTES = 960 * 1024;

/**
 * Bytes in the largest socket message the agent reads from the controller. A larger message closes
 * the socket with code `1009`. The limit leaves room for a launch with a backup of
 * {@link MAX_BACKUP_FILE_CHARS} characters, each of which JSON may write in two bytes.
 */
export const CONTROLLER_FRAME_MAX_BYTES = 2 * 1024 * 1024;

/**
 * Why the controller closed a node's socket, as the close reason. {@link SOCKET_CLOSES} lists the
 * code of each and the agent's response. `frame_too_large` is the controller's record of a `1009`
 * close, which carries no reason.
 */
export const CloseReason = z.enum(['rate_limited', 'frame_too_large', 'hello_timeout', 'agent_deleted', 'resync', 'node_connected', 'hello_refused', 'duplicate_hello']);

export type CloseReason = z.infer<typeof CloseReason>;

/**
 * Milliseconds the agent waits for the controller to close after
 * {@link ControllerHelloRefused | hello_refused}. Past them, the agent closes the socket itself with code
 * `1000`.
 */
export const AGENT_HELLO_REFUSED_CLOSE_MS = 5_000;

/**
 * Milliseconds a socket has after it opens to send a valid {@link AgentHello | hello}. Past them,
 * the controller closes it with code `1008` and `hello_timeout`.
 */
export const AGENT_HELLO_TIMEOUT_MS = 10_000;

/**
 * Milliseconds the agent waits for a {@link ControllerWelcome | welcome} after it sends its
 * {@link AgentHello | hello}. Past them, the agent closes the socket with code `1000` and
 * {@link AGENT_WELCOME_TIMEOUT_REASON}, and redials.
 */
export const AGENT_WELCOME_TIMEOUT_MS = 15_000;

/**
 * The close reason the agent sends, with code `1000`, when no {@link ControllerWelcome | welcome} answered
 * its {@link AgentHello | hello} within {@link AGENT_WELCOME_TIMEOUT_MS}.
 */
export const AGENT_WELCOME_TIMEOUT_REASON = 'welcome_timeout';

/**
 * Milliseconds the controller answers a node's upgrades with `429` after it closed the node's
 * socket with `rate_limited` or `frame_too_large`.
 */
export const AGENT_LIMIT_COOLDOWN_MS = 60_000;

/**
 * The WebSocket close codes the agent's socket is closed with on purpose, from
 * [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455#section-7.4.1). {@link SOCKET_CLOSES} lists
 * when each is sent.
 */
export const CLOSE_CODES = {
  /** A normal close, by either side. */
  normal: 1000,
  /** The controller ends the connection, with a {@link CloseReason} saying why. */
  policy: 1008,
  /** A message over the size limit. */
  messageTooBig: 1009,
} as const;

export type CloseCode = (typeof CLOSE_CODES)[keyof typeof CLOSE_CODES];

/** One way the agent's socket is closed on purpose. */
export interface SocketClose {
  /** The WebSocket close code, one of {@link CLOSE_CODES}. */
  code: CloseCode;
  /** The close reason, as Markdown. Null when the close carries none. */
  reason: string | null;
  /** The side that closes the socket. */
  by: 'controller' | 'agent';
  /** When the close happens. */
  when: string;
  /** What the agent does next. */
  then: string;
}

/**
 * Every close that the controller or the agent makes on purpose, with a code from
 * {@link CLOSE_CODES}. Any other code, or none, is a broken connection, and the agent redials.
 */
export const SOCKET_CLOSES: readonly SocketClose[] = [
  { code: CLOSE_CODES.normal, reason: '`SIGTERM`, `SIGINT` or `SIGQUIT`', by: 'agent', when: 'The agent is stopping, after its `shutdown`.', then: 'The controller closes its side with the same code and reason.' },
  { code: CLOSE_CODES.policy, reason: '`hello_refused`', by: 'controller', when: 'The controller will not accept the node\'s `hello`, because the `hello` fails its schema or the controller refuses the node. A `hello_refused` frame just before the close says which.', then: 'Logs the frame\'s reason and detail, and stops dialling until it is restarted.' },
  { code: CLOSE_CODES.normal, reason: null, by: 'agent', when: `The controller sent \`hello_refused\` and has not closed within ${AGENT_HELLO_REFUSED_CLOSE_MS / 1000} seconds.`, then: 'Stays stopped until it is restarted.' },
  { code: CLOSE_CODES.normal, reason: null, by: 'agent', when: 'A `set_config` moved the node to another controller address, once its reply has gone.', then: 'Dials the new address immediately.' },
  { code: CLOSE_CODES.policy, reason: '`hello_timeout`', by: 'controller', when: `No valid \`hello\` within ${AGENT_HELLO_TIMEOUT_MS / 1000} seconds of the upgrade.`, then: 'Redials with its backoff.' },
  { code: CLOSE_CODES.normal, reason: '`welcome_timeout`', by: 'agent', when: `No \`welcome\` answered its \`hello\` within ${AGENT_WELCOME_TIMEOUT_MS / 1000} seconds.`, then: 'Redials with its backoff.' },
  { code: CLOSE_CODES.policy, reason: '`duplicate_hello`', by: 'controller', when: 'A second `hello` arrived on the same socket.', then: 'Redials with its backoff.' },
  { code: CLOSE_CODES.policy, reason: '`node_connected`', by: 'controller', when: 'The node is already connected on another socket.', then: 'Redials with its backoff, and connects once the other socket closes or times out.' },
  { code: CLOSE_CODES.policy, reason: '`rate_limited`', by: 'controller', when: 'The node went over a [budget](/protocol/limits/).', then: `Redials with its backoff. The controller answers \`429\` for ${AGENT_LIMIT_COOLDOWN_MS / 1000} seconds.` },
  { code: CLOSE_CODES.policy, reason: '`agent_deleted`', by: 'controller', when: 'The node was deleted in the controller.', then: 'Stops dialling until it is restarted.' },
  { code: CLOSE_CODES.policy, reason: '`resync`', by: 'controller', when: 'The controller lost track of the node\'s state.', then: 'Redials and sends a fresh `hello`.' },
  { code: CLOSE_CODES.messageTooBig, reason: null, by: 'controller', when: `A message over ${AGENT_FRAME_MAX_BYTES / 1024} KiB.`, then: 'Redials with its backoff.' },
  { code: CLOSE_CODES.messageTooBig, reason: null, by: 'agent', when: `The controller sent a message over ${CONTROLLER_FRAME_MAX_BYTES / (1024 * 1024)} MiB.`, then: 'Redials with its backoff.' },
];

/**
 * The close reasons after which the controller answers upgrades with `429` for
 * {@link AGENT_LIMIT_COOLDOWN_MS}.
 */
export const LimitCloseReason = CloseReason.extract(['rate_limited', 'frame_too_large']);

export type LimitCloseReason = z.infer<typeof LimitCloseReason>;

/** The close code that goes with the reason `rate_limited`. */
export const RATE_LIMITED_CLOSE_CODE = CLOSE_CODES.policy;

/** A token bucket, which allows up to `burst` immediately and refills at `perSecond`. */
export interface Rate {
  burst: number;
  perSecond: number;
}

/**
 * A node's rate, which is `base` plus `perSlot` for every slot the node has. The slots are those of
 * its last {@link AgentHello | hello} or applied {@link ControllerSetConfig | set_config} reply, and one
 * before its first {@link AgentHello | hello}.
 */
export interface Budget {
  base: Rate;
  perSlot: Rate;
}

const NONE: Rate = { burst: 0, perSecond: 0 };

const KIB = 1024;

const MIB = 1024 * KIB;

/** The budgets of one socket connection, each starting full. A new connection starts over. */
export const AGENT_BUDGETS = {
  /**
   * Bytes of every message, counted before the controller reads it. Over it, the controller
   * closes the socket with `rate_limited`. The budget holds every other budget's worth of messages
   * with room to spare.
   */
  bytes: { base: { burst: 32 * MIB, perSecond: 512 * KIB }, perSlot: { burst: MIB, perSecond: 16 * KIB } },
  /**
   * {@link AgentHello | hello}, {@link AgentSlotStatus | slot_status},
   * {@link AgentNodeStatus | node_status}, {@link AgentServerExited | server_exited},
   * {@link AgentDemo | demo}, {@link AgentDemoRemoved | demo_removed},
   * {@link AgentEventBacklog | event_backlog} and {@link AgentShutdown | shutdown} messages, and
   * any message the controller cannot read. Over it, the controller closes the socket with
   * `rate_limited`.
   */
  state: { base: { burst: 64, perSecond: 2 }, perSlot: { burst: 32, perSecond: 1 } },
  /**
   * {@link AgentLogBatch | log_batch} and {@link AgentLogIndex | log_index} messages. Over it, or
   * over {@link AGENT_BUDGETS.logBytes}, the controller drops the message.
   *
   * An agent should hold its log output to these budgets, because nothing else bounds the replays a
   * controller asks for. Ankor's agent trims a replay to the remaining budget and drops new lines
   * beyond it.
   */
  logFrames: { base: { burst: 128, perSecond: 32 }, perSlot: NONE },
  /**
   * Bytes of {@link AgentLogBatch | log_batch} and {@link AgentLogIndex | log_index} messages,
   * about four replays of a full source together.
   */
  logBytes: { base: { burst: 16 * MIB, perSecond: 256 * KIB }, perSlot: NONE },
  /** {@link AgentResourceBatch | resource_batch} messages. Over it, the controller drops them. */
  resources: { base: { burst: 10, perSecond: 1 }, perSlot: NONE },
  /**
   * {@link AgentCs2UpdateProgress | cs2_update_progress} messages. Over it, the controller drops
   * them.
   */
  cs2UpdateProgress: { base: { burst: 50, perSecond: 10 }, perSlot: NONE },
  /** {@link AgentDemoProgress | demo_progress} messages. Over it, the controller drops them. */
  demoProgress: { base: { burst: 10, perSecond: 2 }, perSlot: NONE },
  /**
   * {@link AgentPhaseChange | phase_change}, {@link AgentScore | score} and
   * {@link AgentBackup | backup} messages. Over it, the controller drops them. A match
   * sends a few of them a round.
   */
  matchReports: { base: NONE, perSlot: { burst: 16, perSecond: 1 } },
  /**
   * {@link AgentReply | reply} messages that answer no open request, such as one past its request's
   * timeout. Replies to open requests are free. Over it, the controller closes the socket with
   * `rate_limited`.
   */
  strayReplies: { base: { burst: 32, perSecond: 1 }, perSlot: NONE },
  /**
   * WebSocket pings, each of which the controller answers with a pong. Over it, the controller
   * closes the socket with `rate_limited`.
   */
  pings: { base: { burst: 5, perSecond: 1 }, perSlot: NONE },
} as const satisfies Record<string, Budget>;

/** The name of one budget in {@link AGENT_BUDGETS}. */
export type BudgetName = keyof typeof AGENT_BUDGETS;

/**
 * Socket upgrades a node may make per window. Past it, the controller answers `429` until the
 * window that began with the first of them ends. Ankor's agent redials only after a lost connection
 * or a moved address.
 */
export const AGENT_CONNECTS = { max: 10, windowMs: 60_000 } as const;

/**
 * Upgrades with a missing or unknown token per remote address and window. Past it, the controller
 * answers every upgrade from that address with `429`, a known token too, until the window ends.
 */
export const AGENT_AUTH_FAILURES = { max: 20, windowMs: 60_000 } as const;

/**
 * Event-upload bytes a node may post per window: `base` plus `perSlot` for each configured slot,
 * every request counting `requestBytes` more than its body. Past it, the controller answers `429`.
 */
export const EVENT_BATCH_BUDGET = { windowMs: 60_000, base: 32 * MIB, perSlot: 4 * MIB, requestBytes: KIB } as const;

/**
 * Milliseconds that must pass between two {@link ControllerCheckCs2Build | check_cs2_build} messages the
 * agent handles. The agent drops one that arrives sooner.
 */
export const CS2_BUILD_CHECK_INTERVAL_MS = 60_000;

/**
 * Milliseconds between an installed node's reads of the newest public CS2 build from Steam. The
 * node also reads it on {@link ControllerCheckCs2Build | check_cs2_build}.
 */
export const CS2_BUILD_POLL_INTERVAL_MS = 30 * 60 * 1000;

/**
 * Milliseconds between a node's reads of its images' tags from their registries, while its
 * `auto_update` is on. The agent also reads them when it starts, when `auto_update` is turned on,
 * when an update changes an image in `agent.yml`, and when maintenance that an image update waited
 * for ends.
 */
export const IMAGE_UPDATE_POLL_INTERVAL_MS = 60 * 60 * 1000;

/**
 * Seconds a server may stay `starting` after its launch, or after a restarted agent took it over.
 * Past them, the agent stops it with `start_timeout` as its {@link ServerExitReason}.
 */
export const START_TIMEOUT_SECONDS = 60;

/**
 * Seconds a server may keep running after its plugin sent `quitting`. Past them, the agent stops it
 * with `quit_timeout` as its {@link ServerExitReason}.
 */
export const QUIT_TIMEOUT_SECONDS = 10;

/** Milliseconds an event upload may go unanswered before it counts as failed. */
export const EVENT_UPLOAD_TIMEOUT_MS = 15_000;

/**
 * Milliseconds before the first retry after a failed event upload. Each further failure doubles the
 * wait.
 */
export const EVENT_RETRY_MIN_MS = 1_000;

/**
 * Milliseconds of the longest wait between event upload retries, before jitter. Each wait is
 * randomised like an agent redial, by up to {@link AGENT_RECONNECT_JITTER} either way.
 */
export const EVENT_RETRY_MAX_MS = 30_000;

/**
 * Milliseconds a server's events are kept on the node after its last event or receipt. Delivered
 * events are remembered that long so a replay of them is recognised. Events that no controller
 * took in that time are dropped with them, and the agent logs how many, so a server whose batches
 * are refused cannot fill the node's disk. A server whose plugin is still connected keeps its
 * events. An undelivered {@link AgentServerExited | server_exited} is kept as long after its
 * server ended.
 */
export const EVENT_SPOOL_RETENTION_MS = 7 * 24 * 60 * 60 * 1000;

/**
 * Fewest milliseconds between two {@link AgentDemoProgress | demo_progress} messages of one upload.
 */
export const DEMO_PROGRESS_INTERVAL_MS = 1_000;

/** Milliseconds a demo upload may send nothing before it is aborted and retried. */
export const DEMO_STALL_TIMEOUT_MS = 2 * 60 * 1000;

/**
 * Milliseconds before the first retry after a failed demo upload. Each further failure doubles the
 * wait.
 */
export const DEMO_RETRY_MIN_MS = 30 * 1000;

/** Milliseconds of the longest wait between demo upload retries. */
export const DEMO_RETRY_MAX_MS = 15 * 60 * 1000;

/**
 * Milliseconds demo uploads may keep failing before they stop. They start again when a new demo is
 * recorded, the target changes or the agent restarts.
 */
export const DEMO_OUTAGE_LIMIT_MS = 24 * 60 * 60 * 1000;

/**
 * Gigabytes of free disk a CS2 update needs. steamcmd stages a whole new copy of every changed file
 * before it swaps them in, and a content drop changes many gigabyte-sized VPKs together, so 6 GB
 * free was not enough for one.
 */
export const CS2_UPDATE_MIN_FREE_GB = 20;

/**
 * Gigabytes of free disk a CS2 reinstall needs, because a reinstall downloads the entire game. The
 * space is checked after the wipe.
 */
export const CS2_REINSTALL_MIN_FREE_GB = 70;

/**
 * Gigabytes of disk a node needs beyond its operating system: about 70 for the CS2 installation,
 * and the rest for an update to stage changed files in. A controller may warn about less.
 */
export const NODE_MIN_DISK_GB = 90;
