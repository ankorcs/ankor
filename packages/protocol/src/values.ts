/** Value types that more than one wire and `agent.yml` share. */
import { z } from 'zod';
import { isIpv4, isControllerHost, SLOT_ID_MESSAGE, CONTROLLER_HOST_MESSAGE } from './checks.js';
import type { AgentHello, ControllerSetConfig } from './agent-socket.js';
import { DEMO_HEADERS, PROTOCOL_HEADER, type DEFAULT_EVENTS_PATH, type DEFAULT_SOCKET_PATH } from './paths.js';
import { MAX_MEMORY_MB, MIN_MEMORY_MB } from './limits.js';

/** The loopback port the agent listens on for plugins. No slot may use it. */
export const EVENT_RELAY_PORT = 4500;

/** The highest port a slot may use, for its game port and its GOTV port. */
export const MAX_PORT = 65535;

/**
 * The lowest port `agent.yml` permits for a slot. A configuration front end may require a higher
 * one.
 */
export const AGENT_MIN_SLOT_PORT = 1;

/** A slot id: 1 to 32 letters, digits, `_` or `-`. It becomes part of the game container's name. */
export const SLOT_ID_PATTERN = /^[A-Za-z0-9_-]{1,32}$/;

/** A slot's two ports, before the controller has given it an id. */
export type SlotPorts = Omit<ServerSlot, 'id'>;

/**
 * The shape of a node token. It has letters and digits only, so that it reads the same in a
 * header, a YAML file and a shell.
 */
export const NODE_TOKEN_PATTERN = /^[A-Za-z0-9]{32,128}$/;

/** What a node presents as `Authorization: Bearer <token>` on every request. */
export const NodeToken = z.string().regex(NODE_TOKEN_PATTERN, 'must be 32 to 128 letters and digits');

/**
 * The id the controller picks for each server it launches. It identifies that server and its
 * events from then on.
 *
 * The id is a lowercase UUID, new for every launch. UUIDv7 is recommended, because its time-ordered
 * values keep a database index on it compact as events arrive.
 */
export const AllocationId = z.uuid().regex(/^[^A-F]*$/, 'must be lowercase');

/** A slot's id, which is also part of the names of its containers. */
export const SlotId = z.string().regex(SLOT_ID_PATTERN, SLOT_ID_MESSAGE);

/** A TCP or UDP port. */
export const Port = z.number().int().min(AGENT_MIN_SLOT_PORT).max(MAX_PORT);

/** A slot's id and ports. */
export const ServerSlot = z.object({
  /** The slot's id, 1 to 32 letters, digits, `_` and `-`. It is unique on its node. */
  id: SlotId,
  /**
   * The game port, UDP. Every `port` and `tv_port` on a node is different, free on the host, and
   * not {@link EVENT_RELAY_PORT}, the port the agent listens on for plugins.
   * [Host networking](/how-it-works/#host-networking) explains why.
   */
  port: Port,
  /** The GOTV port, UDP, under the same rules as `port`. */
  tv_port: Port,
});

export type ServerSlot = z.infer<typeof ServerSlot>;

/** The shape of a Game Server Login Token as Steam issues them. */
export const GSLT_PATTERN = /^[A-Za-z0-9]{10,64}$/;

/** A Game Server Login Token, which the servers of a slot log in to Steam with. */
export const Gslt = z.string().regex(GSLT_PATTERN, 'a token is 10 to 64 letters and digits');

/**
 * The last ten characters of a token, which the agent reports instead of the token. They are enough
 * to tell apart every token on one Steam account.
 */
export const GsltHint = z.string().regex(/^[A-Za-z0-9]{10}$/, 'the last ten characters of a token');

/**
 * Whether Steam accepts a slot's token.
 *
 * - `ok`: a logon with the token succeeded, either the agent's own or that of a server that
 *   turned `ready`.
 * - `invalid`: Steam refused the token.
 * - `unchecked`: the agent has seen neither since the token was set.
 */
export const GsltState = z.enum(['ok', 'invalid', 'unchecked']);

export type GsltState = z.infer<typeof GsltState>;

/** A slot as `agent.yml` holds it, with its full token. */
export const ConfiguredSlot = ServerSlot.extend({
  /**
   * The slot's Game Server Login Token, which every server in the slot logs in with. It is 10 to 64
   * letters and digits. [Game Server Login Tokens](/requirements/#game-server-login-tokens)
   * explains them.
   */
  gslt: Gslt,
});

export type ConfiguredSlot = z.infer<typeof ConfiguredSlot>;

/**
 * A slot as the agent reports it, with the last ten characters of its token in place of the token.
 */
export const ReportedSlot = ServerSlot.extend({
  /** The last ten characters of the slot's token. */
  gslt_hint: GsltHint,
});

export type ReportedSlot = z.infer<typeof ReportedSlot>;

/** A Docker image reference as `docker run` accepts it: `registry/repo:tag` or `repo@sha256:…`. */
const IMAGE_REFERENCE_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._\-/:@]{0,254}$/;

/** A Docker image reference, as `cs2_image` and `agent_image` in `agent.yml` hold it. */
export const ImageReference = z.string().regex(IMAGE_REFERENCE_PATTERN, 'must be a Docker image reference');

/**
 * MiB of memory each game container may use. CS2 installs and updates are exempt.
 *
 * Rejected when it is not a whole number from {@link MIN_MEMORY_MB} to {@link MAX_MEMORY_MB}.
 */
export const MemoryLimit = z.number().int().min(MIN_MEMORY_MB).max(MAX_MEMORY_MB);

/**
 * The repository of each official image. With `official_images_only` on, a node runs an image only
 * from its kind's repository, with any tag or digest, such as `ghcr.io/ankorcs/ankor-agent:0.8`.
 */
export const OFFICIAL_IMAGES = {
  agent: 'ghcr.io/ankorcs/ankor-agent',
  cs2: 'ghcr.io/ankorcs/ankor-cs2',
} as const;

/**
 * A dotted IPv4 address. Rejected unless it is four numbers from 0 to 255 joined by dots, each
 * without leading zeros.
 */
export const Ipv4 = z.string().refine((value) => isIpv4(value), 'must be an IPv4 address');

/**
 * A host with an optional port and no scheme or path, such as `cs.example.com`,
 * `192.168.0.10:3000` or `[fd00::1]`.
 *
 * Rejected when it contains whitespace, a control character, `/`, `\`, `?` or `#`. It is also
 * rejected unless `http://` followed by the value parses as a URL that has a host, an optional port
 * and nothing else, such as a user name.
 */
export const ControllerHost = z.string().refine((value) => isControllerHost(value), CONTROLLER_HOST_MESSAGE);

/**
 * A path on the controller's host. It starts with `/`, followed by URL path characters and percent
 * escapes, without a query or fragment.
 */
export const ControllerPath = z.string().max(255)
  .regex(/^\/(?:[A-Za-z0-9\-._~!$&'()*+,;=:@/]|%[0-9A-Fa-f]{2})*$/, 'must be a path starting with /, without a query, fragment or spaces');

/** Whether a URL carries no user name or password. */
function withoutCredentials(url: string): boolean {
  const parsed = URL.parse(url);

  return parsed === null || (parsed.username === '' && parsed.password === '');
}

/**
 * The URL finished demos are posted to. A target that needs credentials takes them in the configured
 * `header`, such as `Authorization: Basic …`, which the node reports only as a hash.
 *
 * Rejected when the URL carries a user name or a password.
 */
export const DemoUploadUrl = z.url({ protocol: /^https?$/, error: 'must be an http or https URL' }).max(255)
  .refine((url) => withoutCredentials(url), 'credentials go in the header, not the URL');

/**
 * Header names, in lower case, that a demo upload's configured header cannot take: the ones the
 * agent sets itself on every upload, and the ones that change how the request is framed.
 */
export const RESERVED_DEMO_HEADERS = [
  'host',
  'content-type',
  'content-length',
  'transfer-encoding',
  'connection',
  'keep-alive',
  'upgrade',
  'expect',
  'te',
  'trailer',
  PROTOCOL_HEADER.toLowerCase(),
  ...Object.values(DEMO_HEADERS).map((name) => name.toLowerCase()),
] as const;

/** Whether a header name, in any case, is one of the {@link RESERVED_DEMO_HEADERS}. */
function reservedDemoHeader(name: string): boolean {
  return (RESERVED_DEMO_HEADERS as readonly string[]).includes(name.toLowerCase());
}

/**
 * One header sent with every demo upload, verbatim: any name the target expects, such as an API key
 * header or `Authorization: Basic …`, except the {@link RESERVED_DEMO_HEADERS}.
 */
export const DemoUploadHeader = z
  .object({
    /**
     * The header's name, up to 64 letters, digits and `-`. Rejected when it is, in any case, one of
     * {@link RESERVED_DEMO_HEADERS}, which the agent sets itself or which change how the request is
     * framed.
     */
    name: z.string().regex(/^[A-Za-z0-9-]{1,64}$/, 'header names are up to 64 letters, digits and dashes')
      .refine((name) => !reservedDemoHeader(name), 'the agent sets this header itself'),
    /** The header's value, sent verbatim, without line breaks. */
    value: z.string().max(1024).regex(/^[^\r\n\u0000]*$/, 'header values cannot contain line breaks'),
  });

/** Whole megabits a second, from 1 to 10 Gbit/s. */
export const DemoUploadLimit = z.number().int().min(1).max(10_000);

/** Where finished demos go. The header, when set, is sent verbatim and is the only credential. */
export const DemoUpload = z.object({
  /** The upload target. */
  url: DemoUploadUrl,
  /** A header to send with each upload. Null for none. */
  header: DemoUploadHeader.nullable(),
  /**
   * Megabits a second an upload may send while a game server runs on the node. Null for no limit.
   */
  limit_mbit: DemoUploadLimit.nullable(),
});

export type DemoUpload = z.infer<typeof DemoUpload>;

/**
 * A demo upload as the node reports it, with the header's value replaced by its hash, because the
 * value is a credential that the controller set.
 */
export const ReportedDemoUpload = DemoUpload.extend({
  /** The header sent with each upload. Null for none. */
  header: DemoUploadHeader.pick({ name: true }).extend({
    /**
     * The lowercase hex SHA-256 of the value's UTF-8 bytes, to compare with the value the controller
     * set.
     */
    value_sha256: z.string().regex(/^[0-9a-f]{64}$/, 'a lowercase hex SHA-256'),
  }).nullable(),
});

export type ReportedDemoUpload = z.infer<typeof ReportedDemoUpload>;

/**
 * A demo upload as {@link ControllerSetConfig | set_config} sets it. A header without its value keeps the
 * value the node has.
 */
export const DemoUploadEdit = DemoUpload.extend({
  /**
   * A header to send with each upload. Null for none. Without `value`, the header keeps the value
   * of the node's current header of that name, so the `demo_upload` of a {@link AgentHello | hello}
   * can be sent back unchanged.
   */
  header: DemoUploadHeader.partial({ value: true }).nullable(),
});

export type DemoUploadEdit = z.infer<typeof DemoUploadEdit>;

/**
 * Where a node dials the controller: a host with an optional port, whether it encrypts, and the
 * paths it reaches there.
 */
export const ControllerAddress = z.object({
  /** The controller's host with an optional port. */
  host: ControllerHost,
  /** Whether the agent dials over TLS. */
  tls: z.boolean(),
  /**
   * The path of the agent socket. It is {@link DEFAULT_SOCKET_PATH} unless `agent.yml` sets
   * `controller.socket_path`.
   */
  socket_path: ControllerPath,
  /**
   * The path event batches are posted to. It is {@link DEFAULT_EVENTS_PATH} unless `agent.yml` sets
   * `controller.events_path`.
   */
  events_path: ControllerPath,
});

export type ControllerAddress = z.infer<typeof ControllerAddress>;
