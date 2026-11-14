/**
 * Functions that the agent and a controller in TypeScript share to implement either end of the
 * wire. The wire itself is the schemas and constants in `@ankorcs/protocol`.
 */
import type { z } from 'zod';
import {
  AGENT_RECONNECT_JITTER,
  AGENT_LOG_LINES,
  RESOURCE_TICKS_HELD,
  SERVER_LOG_LINES,
  type AgentDemo,
  type AgentError,
  type AgentLogBatch,
  type AgentReply,
  type AgentResourceBatch,
  type DemoFile,
  type LogLine,
  type LogSource,
  type ResourceTick,
  type SlotStatus,
  type ControllerSetConfig,
} from './agent-socket.js';
import { type AGENT_BUDGETS, type Budget, type BudgetName, EVENT_BATCH_BUDGET, type Rate } from './limits.js';
import { type PROTOCOL_HEADER, PROTOCOL_VERSION } from './paths.js';
import { hostnameOf, SLOT_ID_MESSAGE } from './checks.js';
import {
  EVENT_RELAY_PORT,
  type GsltHint,
  MAX_PORT,
  NODE_TOKEN_PATTERN,
  SLOT_ID_PATTERN,
  type SlotPorts,
} from './values.js';

export { isIpv4, isControllerHost } from './checks.js';

/**
 * A slot whose id is absent where the caller has not assigned ids yet and position decides them.
 */
type SlotLayout = SlotPorts & { id?: string | undefined };

/** One problem in a slot layout, with the slot and the field it belongs to. */
export interface SlotLayoutIssue {
  index: number;
  field: 'id' | 'port' | 'tv_port';
  kind: 'id' | 'range' | 'reserved' | 'duplicate';
  message: string;
}

interface SlotLayoutOptions {
  /** The lowest port the caller allows. */
  minPort: number;
}

/**
 * The layout checks that `agent.yml`, edited slot input and {@link ControllerSetConfig | set_config}
 * share.
 */
export function validateSlotLayout(
  slots: readonly SlotLayout[],
  { minPort }: SlotLayoutOptions,
): SlotLayoutIssue[] {
  const issues: SlotLayoutIssue[] = [];
  const ids = new Set<string>();
  const ports = new Set<number>();

  slots.forEach((slot, index) => {
    if (slot.id !== undefined) {
      if (!SLOT_ID_PATTERN.test(slot.id)) {
        issues.push({ index, field: 'id', kind: 'id', message: SLOT_ID_MESSAGE });
      } else if (ids.has(slot.id)) {
        issues.push({ index, field: 'id', kind: 'duplicate', message: `duplicate slot id "${slot.id}"` });
      }

      ids.add(slot.id);
    }

    for (const field of ['port', 'tv_port'] as const) {
      const value = slot[field];

      if (!Number.isInteger(value) || value < minPort || value > MAX_PORT) {
        issues.push({
          index,
          field,
          kind: 'range',
          message: `port ${value} must be an integer between ${minPort} and ${MAX_PORT}`,
        });

        continue;
      }

      if (value === EVENT_RELAY_PORT) {
        issues.push({ index, field, kind: 'reserved', message: `port ${value} is reserved for the event relay` });
      }

      if (ports.has(value)) {
        issues.push({ index, field, kind: 'duplicate', message: `port ${value} is used by more than one slot` });
      }

      ports.add(value);
    }
  });

  return issues;
}

/** {@link validateSlotLayout} as zod issues, under `prefix`. */
export function addSlotLayoutIssues(
  ctx: z.RefinementCtx,
  slots: readonly SlotLayout[],
  minPort: number,
  prefix: readonly (string | number)[] = [],
): void {
  for (const issue of validateSlotLayout(slots, { minPort })) {
    const path = [...prefix, issue.index, issue.field];

    ctx.addIssue({ code: 'custom', path, message: issue.message });
  }
}


const GSLT_HINT_LENGTH = 10;

/**
 * Game servers a node with `cpuCount` CPUs can run at once: one per CPU, keeping one for the host
 * and the agent, and at least one.
 */
export function gameServerBudget(cpuCount: number): number {
  return Math.max(1, cpuCount - 1);
}

/** The part of a token that leaves the agent, as {@link GsltHint} describes. */
export function gsltHint(token: string): string {
  return token.slice(-GSLT_HINT_LENGTH);
}


/** Indexes of tokens that an earlier slot in the list already has. Missing tokens are skipped. */
export function duplicateGslts(tokens: readonly (string | undefined)[]): number[] {
  const seen = new Set<string>();
  const duplicates: number[] = [];

  tokens.forEach((token, index) => {
    if (token === undefined) {
      return;
    }

    if (seen.has(token)) {
      duplicates.push(index);
    }

    seen.add(token);
  });

  return duplicates;
}

const DUPLICATE_GSLT_MESSAGE = 'each slot needs its own token';

/** Adds an issue on each slot whose token another slot of the node already has. */
export function addGsltIssues(ctx: z.RefinementCtx, slots: readonly { gslt?: string | undefined }[], prefix: readonly (string | number)[] = []): void {
  for (const index of duplicateGslts(slots.map((slot) => slot.gslt))) {
    ctx.addIssue({ code: 'custom', path: [...prefix, index, 'gslt'], message: DUPLICATE_GSLT_MESSAGE });
  }
}


/** Loopback, private, link-local and carrier-grade NAT, as `[network, prefix length]`. */
const LOCAL_IPV4 = [[0x7f000000, 8], [0x0a000000, 8], [0xac100000, 12], [0xc0a80000, 16], [0xa9fe0000, 16], [0x64400000, 10]] as const;

function localIpv4(address: number): boolean {
  return LOCAL_IPV4.some(([network, bits]) => address >>> (32 - bits) === network >>> (32 - bits));
}

/**
 * Whether a hostname, as URL parsing normalizes it, is a local IPv4 or IPv6 literal. IPv6 arrives
 * compressed and lowercase, so loopback, `fc00::/7`, `fe80::/10` and IPv4-mapped forms are
 * prefixes.
 */
function isLocalAddress(hostname: string): boolean {
  const v4 = /^(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(hostname);
  const mapped = /^::ffff:([\da-f]{1,4}):([\da-f]{1,4})$/.exec(hostname);

  if (v4 !== null) {
    return localIpv4(v4.slice(1).reduce((address, octet) => address * 256 + Number(octet), 0) >>> 0);
  }

  if (mapped !== null) {
    return localIpv4(Number.parseInt(mapped[1]!, 16) * 0x10000 + Number.parseInt(mapped[2]!, 16));
  }

  return hostname === '::1' || /^f[cd][\da-f]{2}:/.test(hostname) || /^fe[89ab][\da-f]:/.test(hostname);
}

/**
 * Whether a node that reaches the controller at this address should encrypt by default. It should
 * for every address except the literals of a local network, because no certificate authority issues
 * certificates for them. The operator decides, and this function picks only the side the toggle
 * starts on.
 */
export function controllerTlsByDefault(host: string): boolean {
  const hostname = hostnameOf(host);

  if (hostname === null) {
    return true;
  }

  if (hostname.toLowerCase() === 'localhost') {
    return false;
  }

  return !isLocalAddress(hostname);
}

/** The controller's origin for `host` and `tls`, as `http(s)://` or `ws(s)://`. */
export function controllerOrigin(host: string, tls: boolean, kind: 'http' | 'ws'): string {
  if (kind === 'http') {
    return `${tls ? 'https' : 'http'}://${host}`;
  }

  return `${tls ? 'wss' : 'ws'}://${host}`;
}

/** The agent socket's URL: the `ws(s)://` origin and the socket path. */
export function agentWsUrl(host: string, tls: boolean, path: string): string {
  return `${controllerOrigin(host, tls, 'ws')}${path}`;
}

/** A refusal as a log line reads it: the code, and what the node said beyond it. */
export function agentErrorText(error: AgentError): string {
  return error.detail === '' || error.detail === error.code ? error.code : `${error.code}: ${error.detail}`;
}

/** Orders build ids numerically. Null, for an unknown build, sorts lowest. */
export function compareBuildIds(a: string | null, b: string | null): number {
  if (a === b) {
    return 0;
  }

  if (a === null) {
    return -1;
  }

  if (b === null) {
    return 1;
  }

  // BigInt: a 20-digit id is past Number's exact range.
  const diff = BigInt(a) - BigInt(b);

  return diff === 0n ? 0 : diff < 0n ? -1 : 1;
}

/**
 * Whether a node is behind the newest public CS2 build. Such a node refuses launches with
 * `cs2_outdated`, because players on the new client cannot join it. A node whose `auto_update`
 * is `false` does not refuse. The answer is `false` while either build is unknown.
 */
export function cs2Behind(installed: string | null, latest: string | null): boolean {
  return installed !== null && latest !== null && compareBuildIds(installed, latest) < 0;
}

/**
 * Whether a launch may use a slot. The slot must be idle and unreserved, and Steam must not have
 * refused its token. The node decides, and the controller's ranking follows it.
 */
export function slotTakesLaunch(status: Pick<SlotStatus, 'state' | 'gslt_state' | 'reserved'>): boolean {
  return status.state === 'idle' && !status.reserved && status.gslt_state !== 'invalid';
}

/**
 * The key that log sources are matched on: `agent`, `cs2_update:<updateId>` or
 * `server:<allocationId>`.
 */
export function logSourceKey(source: LogSource): string {
  switch (source.kind) {
    case 'agent':
      return 'agent';
    case 'cs2_update':
      return `cs2_update:${source.update_id}`;
    case 'server':
      return `server:${source.allocation_id}`;
  }
}

/** Lines the agent keeps of a log source, and the controller after it. */
export function logSourceLines(source: LogSource): number {
  return source.kind === 'agent' ? AGENT_LOG_LINES : SERVER_LOG_LINES;
}

/**
 * A source's lines with one {@link AgentLogBatch | log_batch} of it applied, kept to what the agent
 * keeps.
 */
export function withLogBatch(lines: readonly LogLine[], batch: Pick<AgentLogBatch, 'source' | 'reset' | 'lines'>): LogLine[] {
  return [...batch.reset ? [] : lines, ...batch.lines].slice(-logSourceLines(batch.source));
}

/** The key that {@link AgentDemo | demo} frames are matched on. */
export function demoKey(demo: Pick<DemoFile, 'slot_id' | 'file_name'>): string {
  return `${demo.slot_id}/${demo.file_name}`;
}

/**
 * A demo list with one {@link AgentDemo | demo} frame applied: the demo replaced in place, added at
 * the end, or dropped.
 */
export function withDemo(demos: readonly DemoFile[], demo: DemoFile, removed: boolean): DemoFile[] {
  const key = demoKey(demo);

  if (removed) {
    return demos.filter((candidate) => demoKey(candidate) !== key);
  }

  return demos.some((candidate) => demoKey(candidate) === key)
    ? demos.map((candidate) => (demoKey(candidate) === key ? demo : candidate))
    : [...demos, demo];
}

/**
 * A ring with one {@link AgentResourceBatch | resource_batch} applied, kept to the last
 * {@link RESOURCE_TICKS_HELD}.
 */
export function withTicks(ring: readonly ResourceTick[], frame: Pick<AgentResourceBatch, 'ticks' | 'reset'>): ResourceTick[] {
  return frame.reset ? frame.ticks : [...ring, ...frame.ticks].slice(-RESOURCE_TICKS_HELD);
}

/**
 * The budget in {@link AGENT_BUDGETS} that a socket message spends from, by its `type`. Log
 * messages spend `logBytes` as well, and a {@link AgentReply | reply} spends `strayReplies` only
 * when it answers nothing open.
 */
export function frameBudget(type: unknown): Exclude<BudgetName, 'bytes' | 'logBytes' | 'pings'> {
  switch (type) {
    case 'log_batch':
    case 'log_index':
      return 'logFrames';
    case 'resource_batch':
      return 'resources';
    case 'cs2_update_progress':
      return 'cs2UpdateProgress';
    case 'demo_progress':
      return 'demoProgress';
    case 'phase_change':
    case 'score':
    case 'backup':
      return 'matchReports';
    case 'reply':
      return 'strayReplies';
    default:
      return 'state';
  }
}

/** A budget's rate for a node with `slots` slots. A count below one counts as one. */
export function budgetRate(budget: Budget, slots: number): Rate {
  const scale = Math.max(1, slots);

  return {
    burst: budget.base.burst + budget.perSlot.burst * scale,
    perSecond: budget.base.perSecond + budget.perSlot.perSecond * scale,
  };
}

/** One budget of a connection: a burst that refills at a steady rate. */
export class TokenBucket {
  private tokens: number;

  constructor(private rate: Rate, private at: number) {
    this.tokens = rate.burst;
  }

  /** Spends `amount` and returns `true`, or spends nothing and returns `false`. */
  take(amount: number, now: number): boolean {
    this.refill(now);

    if (this.tokens < amount) {
      return false;
    }

    this.tokens -= amount;

    return true;
  }

  /** What {@link TokenBucket.take} could spend now. */
  available(now: number): number {
    this.refill(now);

    return this.tokens;
  }

  /**
   * Changes the rate. A node that grows gets the extra burst immediately, so its first report of
   * the new slots fits.
   */
  resize(rate: Rate, now: number): void {
    this.refill(now);
    this.tokens = Math.max(0, Math.min(rate.burst, this.tokens + rate.burst - this.rate.burst));
    this.rate = rate;
  }

  private refill(now: number): void {
    this.tokens = Math.min(this.rate.burst, this.tokens + (Math.max(0, now - this.at) / 1000) * this.rate.perSecond);
    this.at = now;
  }
}

/** {@link EVENT_BATCH_BUDGET}'s bytes per window for a node with `slots` slots. */
export function eventBatchAllowance(slots: number): number {
  return EVENT_BATCH_BUDGET.base + EVENT_BATCH_BUDGET.perSlot * Math.max(1, slots);
}

const REFUSAL_REASONS = {
  401: 'Unauthorized',
  404: 'Not Found',
  426: 'Upgrade Required',
  429: 'Too Many Requests',
  503: 'Service Unavailable',
} as const;

/** An HTTP status a controller refuses an agent with, as {@link REFUSAL_REASONS} names them. */
export type RefusalStatus = keyof typeof REFUSAL_REASONS;

/**
 * The whole answer that refuses a WebSocket upgrade: the status line, then the connection closes.
 */
export function upgradeRefusal(status: RefusalStatus): string {
  return `HTTP/1.1 ${status} ${REFUSAL_REASONS[status]}\r\nConnection: close\r\n\r\n`;
}

/**
 * A zod union of messages told apart by `type`, such as {@link AgentToControllerMessage}. zod keeps
 * every `type` it accepts, nested unions included, in `propValues`.
 */
interface MessageUnion {
  _zod: { propValues: Partial<Record<string, ReadonlySet<unknown>>> };
}

/**
 * The `type` of a frame that names no message in `union`. A receiver skips such a frame, because a
 * newer peer may send a message that this version does not have yet. Null when the frame has a
 * known `type` or no string `type` at all, which is then for the schema to judge.
 */
export function unknownMessageType(union: MessageUnion, frame: unknown): string | null {
  const type = typeof frame === 'object' && frame !== null ? (frame as { type?: unknown }).type : undefined;

  if (typeof type !== 'string') {
    return null;
  }

  return union._zod.propValues['type']?.has(type) === false ? type : null;
}

/** A protocol version as its major and minor. */
export interface ProtocolVersion {
  major: number;
  minor: number;
}

/** A {@link PROTOCOL_HEADER} value, such as `1.3`, as its major and minor. Null for any other value. */
export function protocolVersionOf(header: string | string[] | undefined): ProtocolVersion | null {
  const match = typeof header === 'string' ? /^(0|[1-9]\d{0,5})\.(0|[1-9]\d{0,5})$/.exec(header) : null;

  return match === null ? null : { major: Number(match[1]), minor: Number(match[2]) };
}

/**
 * Whether a request's {@link PROTOCOL_HEADER} names a version this build can talk to: the same
 * major as {@link PROTOCOL_VERSION}, at any minor.
 */
export function isCompatibleProtocol(header: string | string[] | undefined): boolean {
  const peer = protocolVersionOf(header);
  const own = protocolVersionOf(PROTOCOL_VERSION);

  return peer !== null && own !== null && peer.major === own.major;
}

/** The node token of an `Authorization` header. Null when the header holds none. */
export function bearerNodeToken(header: string | undefined): string | null {
  const token = /^Bearer (\S+)$/.exec(header ?? '')?.[1];

  return token !== undefined && NODE_TOKEN_PATTERN.test(token) ? token : null;
}

/** A JSON text parsed and held to `schema`. It throws on either failure. */
export function parseJson<Schema extends z.ZodTypeAny>(schema: Schema, text: string): z.output<Schema> {
  const value: unknown = JSON.parse(text);

  return schema.parse(value);
}

/** `value` held to `schema`, then written as JSON. It throws when the value does not match. */
export function serializeJson<Schema extends z.ZodTypeAny>(schema: Schema, value: z.input<Schema>): string {
  return JSON.stringify(schema.parse(value));
}

/** A failed parse as one indented line per issue, each under its dotted path. */
export function formatConfigError(error: z.ZodError): string {
  return error.issues
    .map((issue) => `  ${issue.path.length > 0 ? issue.path.join('.') : '(root)'}: ${issue.message}`)
    .join('\n');
}

/**
 * The agent's redial and retry delay: exponential backoff, capped, then jittered by
 * {@link AGENT_RECONNECT_JITTER}.
 */
export function backoffDelay(attempt: number, minMs: number, maxMs: number, random: () => number): number {
  const base = Math.min(maxMs, minMs * 2 ** Math.max(0, attempt - 1));
  const jitter = base * AGENT_RECONNECT_JITTER * (random() * 2 - 1);

  return Math.max(minMs, Math.round(base + jitter));
}
