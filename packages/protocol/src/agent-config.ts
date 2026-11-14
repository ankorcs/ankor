/**
 * The `agent.yml` schema that the agent's reader and whoever renders the file for a node share.
 * Parsing fills in the defaults for the keys the file may leave out.
 */
import { z } from 'zod';
import type { ControllerSetConfig, ControllerUpdateAgent, ControllerUpdateCs2, ControllerUpdateCs2Image } from './agent-socket.js';
import { MAX_SLOTS, type IMAGE_UPDATE_POLL_INTERVAL_MS, type MAX_MEMORY_MB, type MIN_MEMORY_MB } from './limits.js';
import { addGsltIssues, addSlotLayoutIssues } from './helpers.js';
import {
  AGENT_MIN_SLOT_PORT,
  ConfiguredSlot,
  DemoUploadHeader,
  DemoUploadLimit,
  DemoUploadUrl,
  type EVENT_RELAY_PORT,
  ImageReference,
  Ipv4,
  MemoryLimit,
  type OFFICIAL_IMAGES,
  NodeToken,
  ControllerHost,
  ControllerPath,
} from './values.js';
import { DEFAULT_EVENTS_PATH, DEFAULT_SOCKET_PATH } from './paths.js';

const DemoUploadFields = z.object({
  /**
   * The `http` or `https` URL finished demos are posted to. Rejected when it carries a user name or a
   * password, which belong in `header`.
   */
  url: DemoUploadUrl,
  /** One header sent with every upload. */
  header: DemoUploadHeader.optional(),
  /**
   * Megabits a second an upload may send while a game server runs on the node. Without it, uploads
   * run at full speed.
   */
  limit_mbit: DemoUploadLimit.optional(),
});

const ControllerFields = z.object({
  /**
   * The controller's host with an optional port, without scheme or path, such as
   * `cs.example.com`, `192.168.0.10:3000` or `[fd00::1]`. The agent may dial over IPv6, but players
   * need IPv4.
   *
   * Rejected when it contains whitespace, a control character, `/`, `\`, `?` or `#`. It is also
   * rejected unless `http://` followed by the value parses as a URL that has a host, an optional
   * port and nothing else, such as a user name.
   */
  host: ControllerHost,
  /** Whether the agent dials `wss://` and `https://` rather than `ws://` and `http://`. */
  tls: z.boolean(),
  /** The agent socket's path on `host`, a `/` followed by URL path characters. */
  socket_path: ControllerPath.default(DEFAULT_SOCKET_PATH),
  /** The path on `host` that event batches are posted to, a `/` followed by URL path characters. */
  events_path: ControllerPath.default(DEFAULT_EVENTS_PATH),
});

const RawAgentConfigSchema = z
  .object({
    /**
     * The node token from the controller, 32 to 128 letters and digits. [Node
     * tokens](/protocol/#node-tokens) explains how one is made. **Keep the file secret.**
     */
    token: NodeToken,
    /** Where the agent dials the controller. */
    controller: ControllerFields,
    /**
     * The IPv4 address players connect to, in place of the one the agent looks up. Rejected unless it
     * is four numbers from 0 to 255 joined by dots, each without leading zeros.
     */
    pinned_ip: Ipv4.optional(),
    /** The `ankor-cs2` image future game servers run, as a Docker image reference. */
    cs2_image: ImageReference,
    /**
     * The agent's image, which `node-setup.sh --update` runs and the agent replaces itself from, as
     * a Docker image reference.
     */
    agent_image: ImageReference,
    /**
     * MiB of memory each game container may use. Without it, game containers are not capped. CS2
     * installs and updates are exempt.
     *
     * Rejected when it is not a whole number from {@link MIN_MEMORY_MB} to {@link MAX_MEMORY_MB}.
     */
    memory_mb: MemoryLimit.optional(),
    /**
     * Whether the agent keeps the node up to date on its own. It updates CS2 to the newest public
     * build and refuses launches while it is behind that build. It also moves `cs2_image` and
     * `agent_image` to the newest release or rebuild on their release line. An image tagged `latest`
     * stays `latest` and follows every release, across lines. It reads their tags from the
     * registry every {@link IMAGE_UPDATE_POLL_INTERVAL_MS}, and leaves an image named by digest or
     * by a custom tag alone. When false, only {@link ControllerUpdateCs2 | update_cs2},
     * {@link ControllerUpdateCs2Image | update_cs2_image} and {@link ControllerUpdateAgent | update_agent} change
     * the build and the images.
     */
    auto_update: z.boolean().default(true),
    /**
     * Whether the node runs only the official images: `agent_image` from
     * {@link OFFICIAL_IMAGES.agent} and `cs2_image` from {@link OFFICIAL_IMAGES.cs2}, with any tag or
     * digest. While it is on, the agent refuses an {@link ControllerUpdateAgent | update_agent} or
     * {@link ControllerUpdateCs2Image | update_cs2_image} to any other image with
     * `image_not_allowed`, and `node-setup.sh` refuses to run one.
     *
     * Only the node's operator can turn it off, by setting it to false in the file, to run images of
     * their own. {@link ControllerSetConfig | set_config} cannot change it. If the `agent.yml` that
     * `node-setup.sh` fetches from the controller turns it off, the script stops.
     */
    official_images_only: z.boolean().default(true),
    /**
     * The node's game server slots. Empty for a node that runs no servers yet. Such a node connects
     * and reports, but accepts no launch.
     */
    slots: z.array(ConfiguredSlot).max(MAX_SLOTS),
    /**
     * Where finished demos are uploaded. Without it, demos stay on the node.
     * [Demo uploads](/protocol/demos/) describes the request.
     */
    demo_upload: DemoUploadFields.optional(),
  });

/**
 * The node's `agent.yml` as the agent reads it. It is {@link RawAgentConfigSchema} with the checks
 * that span slots, and it fills in `pinned_ip` and `demo_upload` as null when absent.
 *
 * Rejected when two `slots` share an `id` or a `gslt`, or when two ports are equal. Every slot's
 * `port` and `tv_port` count, so a slot's two ports differ too. It is also rejected when a port is
 * {@link EVENT_RELAY_PORT}, where the agent listens for plugins.
 */
export const AgentConfigSchema = RawAgentConfigSchema.superRefine((config, ctx) => {
  // Add list-wide issues not already covered by individual slot schemas.
  addSlotLayoutIssues(ctx, config.slots, AGENT_MIN_SLOT_PORT, ['slots']);
  addGsltIssues(ctx, config.slots, ['slots']);
}).transform((config) => ({
  ...config,
  pinned_ip: config.pinned_ip ?? null,
  demo_upload: config.demo_upload === undefined
    ? null
    : { url: config.demo_upload.url, header: config.demo_upload.header ?? null, limit_mbit: config.demo_upload.limit_mbit ?? null },
}));

export type AgentConfig = z.infer<typeof AgentConfigSchema>;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Keys the file carries that this agent does not read, as dotted paths. Parsing ignores them. */
export function unsupportedAgentYmlKeys(raw: unknown): string[] {
  const unknown = (value: unknown, known: object, prefix: string): string[] => isRecord(value)
    ? Object.keys(value).filter((key) => !(key in known)).map((key) => `${prefix}${key}`)
    : [];

  if (!isRecord(raw)) {
    return [];
  }

  const keys = unknown(raw, RawAgentConfigSchema.shape, '');

  if (Array.isArray(raw['slots'])) {
    raw['slots'].forEach((slot, index) => keys.push(...unknown(slot, ConfiguredSlot.shape, `slots.${index}.`)));
  }

  keys.push(...unknown(raw['controller'], ControllerFields.shape, 'controller.'));

  const upload = raw['demo_upload'];

  keys.push(...unknown(upload, DemoUploadFields.shape, 'demo_upload.'));

  if (isRecord(upload)) {
    keys.push(...unknown(upload['header'], DemoUploadHeader.shape, 'demo_upload.header.'));
  }

  return keys;
}
