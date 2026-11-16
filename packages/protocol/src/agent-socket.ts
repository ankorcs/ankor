/**
 * Outbound agent WebSocket authenticated by bearer token.
 *
 * Requests carry `id`. A reply echoes it in `reply_to` and names the request's `type` in `request`.
 * Unsolicited frames omit correlation fields.
 */
import { z } from 'zod';
import type { EventBatch } from './event-batch.js';
import {
  type GoingLive, type MapEnd, MatchId, type MatchRestored, MAX_SERVER_PLAYERS, PlayerName, PlayingSide, RoundBackup,
  type RoundEnd, type RoundStart, RoundNumber, Side, SteamId64,
} from './game-events.js';
import { addGsltIssues, addSlotLayoutIssues } from './helpers.js';
import { type GameMode, ServerLaunchConfig } from './launch.js';
import type { CommandRefusal } from './plugin-socket.js';
import {
  type AGENT_WELCOME_TIMEOUT_MS, type CS2_BUILD_CHECK_INTERVAL_MS, type CS2_BUILD_POLL_INTERVAL_MS,
  type EVENT_SPOOL_RETENTION_MS, type MAX_MEMORY_MB, MAX_REPORTED_BACKLOGS, MAX_REPORTED_DEMOS, MAX_SLOTS, type MIN_MEMORY_MB,
  type QUIT_TIMEOUT_SECONDS, type REMEMBERED_ALLOCATIONS, type START_TIMEOUT_SECONDS,
} from './limits.js';
import {
  AGENT_MIN_SLOT_PORT, AllocationId, DemoUploadEdit, type EVENT_RELAY_PORT, Gslt, GsltState, ImageReference, Ipv4, MemoryLimit, ReportedDemoUpload,
  ReportedSlot, ServerSlot, SlotId, ControllerAddress,
} from './values.js';

/** The `code` of a failed {@link AgentReply | reply}. `detail` carries the human-readable rest. */
export const AGENT_ERRORS = {
  /** No slot is idle, unreserved and on a token Steam has not refused. */
  noFreeSlot: 'no_free_slot',
  /**
   * A newer public CS2 build is out and the node has not updated to it yet, so its `cs2.build_id`
   * is behind `cs2.latest_build_id`. The node takes launches again once it has updated. A node
   * whose `auto_update` is `false` never refuses with this.
   */
  cs2Outdated: 'cs2_outdated',
  /**
   * CS2 is not installed on the node, so its `cs2.build_id` is `null`. The node takes launches once
   * an {@link ControllerUpdateCs2 | update_cs2} has installed it.
   */
  cs2NotInstalled: 'cs2_not_installed',
  /** The node reports `unavailable` for a reason other than `maintenance`. */
  nodeUnavailable: 'node_unavailable',
  /**
   * An operation that needs every slot idle found one held: {@link ControllerUpdateCs2 | update_cs2}, or a
   * {@link ControllerSetConfig | set_config} that changes any key under `controller`.
   */
  slotsHeld: 'slots_held',
  /**
   * A slot holds a server that is `starting`, `running` or `stopping`, and
   * {@link ControllerSetConfig | set_config} would remove or re-port the slot or give its server's token
   * to another slot, or {@link ControllerReserveSlot | reserve_slot} asked for it. `detail` names the
   * slots.
   */
  slotHeld: 'slot_held',
  /** The node has no slot with that id. */
  unknownSlot: 'unknown_slot',
  /**
   * {@link ControllerSetConfig | set_config} kept the token of a slot the node does not have yet. A new
   * slot needs a new token.
   */
  slotTokenRequired: 'slot_token_required',
  /**
   * {@link ControllerSetConfig | set_config} kept the value of a demo upload header the node does not
   * send. A new header needs its value.
   */
  demoHeaderValueRequired: 'demo_header_value_required',
  /** No server with that allocation id. */
  unknownAllocation: 'unknown_allocation',
  /**
   * A maintenance operation runs on the node. That operation is unaffected, and `unavailable` shows
   * it as `maintenance`.
   */
  maintenanceRunning: 'maintenance_running',
  /**
   * The image is not on the node and the request asked for no pull. Asking again with `pull`
   * fetches it.
   */
  imageMissing: 'image_missing',
  /**
   * The image is not an official one, and the node's `official_images_only` is on. The node's
   * operator decides which images it runs.
   */
  imageNotAllowed: 'image_not_allowed',
  /** The demo is uploading. It can be deleted once the upload ends. */
  demoUploading: 'demo_uploading',
  /** The slot has no demo of that name, such as one an upload or a delete has already removed. */
  unknownDemo: 'unknown_demo',
  /** The demo is still being recorded. It can be deleted once its server ends the recording. */
  demoRecording: 'demo_recording',
  /** The server's plugin is not connected to the agent, or left before it answered. */
  pluginOffline: 'plugin_offline',
  /**
   * The plugin did not answer {@link ControllerGetServerStatus | get_server_status},
   * {@link ControllerConsoleCommand | console_command}, {@link ControllerStartMatch | start_match} or
   * {@link ControllerRestoreRound | restore_round} within {@link SERVER_STATUS_TIMEOUT_MS}.
   */
  pluginStatusTimeout: 'plugin_status_timeout',
  /** The server runs without a match. */
  noMatch: 'no_match',
  /** The match is past its warmup. */
  notInWarmup: 'not_in_warmup',
  /** A team has no player connected. */
  teamEmpty: 'team_empty',
  /** The match is not live, such as in warmup, during the knife round or after it ended. */
  notLive: 'not_live',
  /**
   * The match is in its halftime break, while the sides are about to swap. Ask again once the next
   * round has started.
   */
  atHalftime: 'at_halftime',
  /**
   * The round has ended and the next has not started yet. A backup loaded in this window is never
   * restarted into, so ask again once the next round has started.
   */
  betweenRounds: 'between_rounds',
  /**
   * The node has no backup of the round that {@link ControllerRestoreRound | restore_round} named for that
   * server.
   */
  noBackup: 'no_backup',
  /**
   * {@link ControllerConsoleCommand | console_command} named a command of
   * {@link MATCH_BLOCKED_COMMANDS} while the server runs a match. A new launch restarts a match,
   * and {@link ControllerRestoreRound | restore_round} restores one.
   */
  commandBlocked: 'command_blocked',
  /** The request did not match its schema. */
  invalidRequest: 'invalid_request',
  /**
   * {@link ControllerStartServer | start_server} named an allocation this node is already launching or
   * running. Its server exists or is on its way, as after a {@link ControllerStartServer | start_server}
   * sent again when its reply was slow.
   */
  allocationInUse: 'allocation_in_use',
  /**
   * {@link ControllerStartServer | start_server} named an allocation that this node launched before, among
   * the last {@link REMEMBERED_ALLOCATIONS}. The node files a server's events and demos under its
   * allocation, so every launch needs a new one.
   */
  allocationReused: 'allocation_reused',
  /** The agent does not know the request's `type`, or the plugin the command's. */
  unsupportedRequest: 'unsupported_request',
  /**
   * The agent is stopping and started no work for the request, as after its
   * {@link AgentShutdown | shutdown}. Send the request again once the node reconnects.
   */
  shuttingDown: 'shutting_down',
  /** Any failure that no other code names, such as a Docker failure. */
  failed: 'failed',
} as const;

/** The `code` of a failed reply, one of the values of {@link AGENT_ERRORS}. */
export const AgentErrorCode = z.enum(AGENT_ERRORS);

export type AgentErrorCode = z.infer<typeof AgentErrorCode>;

/** Longest `detail` a refused request carries. The agent cuts the message to it. */
export const AGENT_ERROR_DETAIL_MAX_CHARS = 512;

/** Why a request failed. Branch on `code`. `detail` is for people. */
export const AgentError = z.object({
  /** What went wrong. Branch on it. */
  code: AgentErrorCode,
  /**
   * The rest of the reason, such as Docker's message, up to {@link AGENT_ERROR_DETAIL_MAX_CHARS}
   * characters.
   */
  detail: z.string().max(AGENT_ERROR_DETAIL_MAX_CHARS),
});

export type AgentError = z.infer<typeof AgentError>;

/**
 * The {@link ControllerStartServer | start_server} refusals the agent sends before reserving a slot.
 * Nothing started, so another node may take the launch.
 */
export const StartRefusal = z.enum([
  AGENT_ERRORS.noFreeSlot, AGENT_ERRORS.maintenanceRunning, AGENT_ERRORS.cs2NotInstalled, AGENT_ERRORS.cs2Outdated, AGENT_ERRORS.nodeUnavailable,
  AGENT_ERRORS.shuttingDown,
]);

export type StartRefusal = z.infer<typeof StartRefusal>;

/**
 * What a slot is doing.
 *
 * - `idle`: the slot has no server.
 * - `starting`: the slot holds a server that its plugin has not yet reported ready for players with
 *   a `server_status` with `ready`. A server that a restarted agent took over starts here too.
 * - `running`: the plugin has reported the server ready.
 * - `stopping`: the server is being stopped.
 */
export const SlotState = z.enum(['idle', 'starting', 'running', 'stopping']);

export type SlotState = z.infer<typeof SlotState>;

/**
 * Why the agent stopped a server on its own.
 *
 * - `idle_empty`: the server stayed empty for its launch's `idle_shutdown_seconds`.
 * - `config_rejected`: the plugin refused its match config, as `config_error` says.
 * - `start_timeout`: the server was still `starting` {@link START_TIMEOUT_SECONDS} after its
 *   launch, or after a restarted agent took it over.
 * - `quit_timeout`: the server was still running {@link QUIT_TIMEOUT_SECONDS} after its plugin sent
 *   `quitting`.
 * - `slot_removed`: a restarted agent found the server running on a slot that `agent.yml` no longer
 *   has.
 */
export const AgentStopReason = z.enum(['idle_empty', 'config_rejected', 'start_timeout', 'quit_timeout', 'slot_removed']);

export type AgentStopReason = z.infer<typeof AgentStopReason>;

/**
 * Why a server ended: one of the {@link AgentStopReason} values when the agent stopped it on its
 * own, or one of these.
 *
 * - `requested`: a {@link ControllerStopServer | stop_server} from the controller stopped it.
 * - `clean_exit`: it ended on its own with exit code `0`, as after its match or a console `quit`.
 * - `crashed`: it ended on its own with another exit code.
 * - `launch_failed`: its container never started.
 * - `lost`: it ended and the agent did not see how, such as a container removed by hand.
 */
export const ServerExitReason = z.enum([...AgentStopReason.options, 'requested', 'clean_exit', 'crashed', 'launch_failed', 'lost']);

export type ServerExitReason = z.infer<typeof ServerExitReason>;

/**
 * Up to {@link MAX_SLOTS} slots. Rejected when two slots share an `id`, or when two ports are equal.
 * Every slot's `port` and `tv_port` count, so a slot's two ports differ too. It is also rejected
 * when a port is {@link EVENT_RELAY_PORT}, where the agent listens for plugins.
 */
const SlotLayout = z.array(ReportedSlot).max(MAX_SLOTS).superRefine((slots, ctx) => addSlotLayoutIssues(ctx, slots, AGENT_MIN_SLOT_PORT));

/**
 * A slot as {@link ControllerSetConfig | set_config} asks for it.
 *
 * `gslt` is a new token, which may replace the token of a slot that runs a server. That server
 * keeps the token it started with, and no other slot may use that token until the server ends
 * (`slot_held`). Without `gslt` the slot keeps its current token, so a slot the node does not have
 * yet needs one (`slot_token_required`), and a slot from a {@link AgentHello | hello} can be sent
 * back unchanged.
 */
export const SlotEdit = ServerSlot.extend({
  /** The slot's new token. Without one, the slot keeps the token it has. */
  gslt: Gslt.optional(),
});

export type SlotEdit = z.infer<typeof SlotEdit>;

/** The layout rules of `SlotLayout`, and no token twice among the slots that carry one. */
const SlotEditLayout = z.array(SlotEdit).max(MAX_SLOTS).superRefine((slots, ctx) => {
  addSlotLayoutIssues(ctx, slots, AGENT_MIN_SLOT_PORT);
  addGsltIssues(ctx, slots);
});

/** The id Docker gives an image: `sha256:` and 64 hex digits. */
export const ImageId = z.string().regex(/^sha256:[0-9a-f]{64}$/, 'must be a sha256 image id');

/** A Steam build id, in decimal. */
export const BuildId = z.string().regex(/^\d{1,20}$/);

const slotStatusFields = {
  /** The slot this describes. */
  slot_id: SlotId,
  /** What the slot is doing. */
  state: SlotState,
  /** The server on the slot. Null when the slot is `idle`. */
  allocation_id: AllocationId.nullable(),
  /** The server's match id. Null for a server without a match or a match launched without one. */
  match_id: MatchId.nullable(),
  /**
   * Whether the server's plugin is connected to the agent.
   * {@link ControllerGetServerStatus | get_server_status}, {@link ControllerConsoleCommand | console_command},
   * {@link ControllerRestoreRound | restore_round} and {@link ControllerStartMatch | start_match} reach it only
   * while it is.
   */
  plugin_online: z.boolean(),
  /**
   * Whether Steam accepts the slot's token. {@link ControllerStartServer | start_server} never picks a
   * slot whose token is `invalid`.
   */
  gslt_state: GsltState,
  /**
   * Whether the idle slot is kept from launches, while the controller replaces its token with
   * {@link ControllerReserveSlot | reserve_slot} or while the agent logs its token on to Steam.
   */
  reserved: z.boolean(),
  /**
   * The `server_steam_id` that the server's plugin reported. The agent sends a
   * {@link AgentSlotStatus | slot_status} as soon as it learns the ID. Null when the slot is `idle`
   * or until the server is `ready`.
   */
  server_steam_id: SteamId64.nullable(),
};

function addSlotStatusIssues(slot: z.infer<z.ZodObject<typeof slotStatusFields>>, ctx: z.RefinementCtx): void {
  if (slot.state === 'idle' ? slot.allocation_id !== null || slot.match_id !== null : slot.allocation_id === null) {
    ctx.addIssue({ code: 'custom', message: 'a held slot names its allocation, an idle one none' });
  }

  if (slot.state !== 'idle' && slot.reserved) {
    ctx.addIssue({ code: 'custom', message: 'only an idle slot is reserved' });
  }

  if (slot.state === 'idle' && slot.server_steam_id !== null) {
    ctx.addIssue({ code: 'custom', message: 'an idle slot names no server_steam_id' });
  }
}

/**
 * One slot as the agent sees it.
 *
 * Rejected when an `idle` slot names an `allocation_id`, a `match_id` or a `server_steam_id`, or
 * when a slot in any other state has no `allocation_id`. It is also rejected when a slot that is
 * not `idle` is `reserved`.
 */
export const SlotStatus = z.object(slotStatusFields).superRefine(addSlotStatusIssues);

export type SlotStatus = z.infer<typeof SlotStatus>;

/**
 * The part of `agent.yml` that the controller may edit, as {@link AgentHello | hello} reports it
 * applied.
 */
export const NodeSettings = z.object({
  /** The slots servers run in. */
  slots: SlotLayout,
  /**
   * The address players connect to. Null for the node's `public_ip`. Servers already running keep
   * the address their launch announced.
   */
  pinned_ip: Ipv4.nullable(),
  /** Where the agent dials the controller. It redials when any part of it changes. */
  controller: ControllerAddress,
  /**
   * Whether the node keeps itself up to date: CS2 on `cs2.latest_build_id`, and its agent and cs2
   * images on the newest release of their major. When false, the node stays on `cs2.build_id` until
   * an {@link ControllerUpdateCs2 | update_cs2}, and accepts launches while it is behind
   * `cs2.latest_build_id`, so players on a newer client cannot join its servers. Its images then
   * change only with {@link ControllerUpdateAgent | update_agent} and
   * {@link ControllerUpdateCs2Image | update_cs2_image}.
   */
  auto_update: z.boolean(),
  /**
   * Where the agent uploads demos. Null for no uploads, which keeps demos on the node. The header's
   * value is reported as its SHA-256.
   */
  demo_upload: ReportedDemoUpload.nullable(),
  /**
   * MiB of memory each game container may use. Null for no cap. A change applies to servers launched
   * afterwards, and CS2 installs and updates are exempt.
   *
   * Rejected when it is not a whole number from {@link MIN_MEMORY_MB} to {@link MAX_MEMORY_MB}.
   */
  memory_mb: MemoryLimit.nullable(),
});

export type NodeSettings = z.infer<typeof NodeSettings>;

/**
 * {@link NodeSettings} as {@link ControllerSetConfig | set_config} asks for them. A missing field keeps
 * the node's value. Null is a value, as {@link NodeSettings} describes. `slots`, when present, is
 * the whole layout.
 */
export const NodeSettingsEdit = NodeSettings.extend({
  /**
   * The slots servers run in. {@link SlotEdit} describes their tokens.
   *
   * Rejected when two slots share an `id` or a `gslt`, or when two ports are equal. Every slot's
   * `port` and `tv_port` count, so a slot's two ports differ too. It is also rejected when a port
   * is {@link EVENT_RELAY_PORT}, where the agent listens for plugins. The agent fills in the kept
   * tokens and refuses a layout in which two slots then share one.
   */
  slots: SlotEditLayout,
  /**
   * Where the agent uploads demos. Null for no uploads, which keeps demos on the node.
   * {@link DemoUploadEdit} describes its header.
   */
  demo_upload: DemoUploadEdit.nullable(),
}).partial();

export type NodeSettingsEdit = z.infer<typeof NodeSettingsEdit>;

/** Longer os-release names are cut to this many characters. */
export const OS_NAME_MAX_CHARS = 256;

/** Longest demo file name a node reports, `.dem` included. */
export const DEMO_FILE_NAME_MAX_CHARS = 160;

/**
 * A demo's file name, as it sits in its launch's directory, `demos/<allocation_id>/` in its slot.
 */
export const DemoFileName = z.string().regex(new RegExp(`^[A-Za-z0-9._-]{1,${DEMO_FILE_NAME_MAX_CHARS - 4}}\\.dem$`),
  `must be a .dem file name of up to ${DEMO_FILE_NAME_MAX_CHARS} letters, digits, dots, dashes and underscores`);

/** Longest `upload_error` a demo carries. The agent cuts the failure's message to it. */
export const DEMO_UPLOAD_ERROR_MAX_CHARS = 512;

/** A demo kept on the node. */
export const DemoFile = z.object({
  /** The slot whose directory holds it. */
  slot_id: SlotId,
  /** The file's name. */
  file_name: DemoFileName,
  /** Bytes in the file. */
  size_bytes: z.number().int().nonnegative(),
  /** Unix milliseconds when the file was last written. */
  modified_at: z.number().int(),
  /**
   * Whether its server is still recording it. While it is, the demo grows with each report and is
   * neither uploaded nor deleted.
   */
  recording: z.boolean(),
  /**
   * Whether it is uploading now. {@link AgentDemoProgress | demo_progress} reports how far, and a
   * node uploads one demo at a time.
   */
  uploading: z.boolean(),
  /**
   * Why its upload failed or why it waits, up to {@link DEMO_UPLOAD_ERROR_MAX_CHARS} characters: a
   * failure being retried, a refusal as invalid, or every demo held while the target refuses the
   * node or has failed for a day. Null once an upload works again or the target changes.
   */
  upload_error: z.string().min(1).max(DEMO_UPLOAD_ERROR_MAX_CHARS).nullable(),
});

export type DemoFile = z.infer<typeof DemoFile>;

/** Longer reasons a node cannot run servers are cut to this many characters. */
export const NODE_UNAVAILABLE_DETAIL_MAX_CHARS = 512;

/** Free space a node needs on its data disk to take launches, in bytes. */
export const DISK_SPACE_MIN_BYTES = 1 << 30;

/** Why a node cannot run servers. */
export const NODE_UNAVAILABLE = {
  /** The {@link MaintenanceOperation} in `operation` runs. It clears when the operation ends. */
  maintenance: 'maintenance',
  /**
   * The agent cannot use the local store that keeps gameplay events until the controller takes
   * them. `detail` says why.
   * - If a call to the store waits past its limit, as on a disk that hangs, the code clears once
   *   the store answers. Deleting the store or restarting the agent loses what waits to be written.
   * - If the agent cannot open the store, such as one that a newer agent wrote, a corrupt file or
   *   one it cannot write, the code lasts until a restart. Update the agent, or delete the store on
   *   the host and restart the agent. Deleting it loses the events and server exits it keeps.
   */
  eventStorage: 'event_storage',
  /**
   * The disk under the agent's data directory has less than {@link DISK_SPACE_MIN_BYTES} free, too
   * little for a match's events and demo. The agent asks the disk every 30 seconds and before each
   * launch, so the code clears within 30 seconds of space being freed, and a launch that arrives
   * sooner is taken if the disk has room by then.
   */
  diskSpace: 'disk_space',
} as const;

/** Why a node cannot run servers, one of the values of {@link NODE_UNAVAILABLE}. */
export const NodeUnavailableCode = z.enum(NODE_UNAVAILABLE);

export type NodeUnavailableCode = z.infer<typeof NodeUnavailableCode>;

/**
 * The maintenance operation that runs on the node.
 *
 * - `cs2_update`: a CS2 install or update, also one an earlier agent started.
 * - `agent_update`: an {@link ControllerUpdateAgent | update_agent}, until its replacement has proved it
 *   stays up.
 * - `cs2_image_update`: an {@link ControllerUpdateCs2Image | update_cs2_image}.
 * - `image_cleanup`: the agent's removal of replaced images when it starts.
 */
export const MaintenanceOperation = z.enum(['cs2_update', 'agent_update', 'cs2_image_update', 'image_cleanup']);

export type MaintenanceOperation = z.infer<typeof MaintenanceOperation>;

/**
 * Why the node cannot run servers now, with `maintenance` first while it lasts. Meanwhile
 * {@link ControllerStartServer | start_server} is refused with `maintenance_running` for `maintenance` and
 * `node_unavailable` otherwise. Everything else works.
 *
 * Rejected when `operation` is null while `code` is `maintenance`, or set while `code` is another
 * value.
 */
export const NodeUnavailable = z.object({
  code: NodeUnavailableCode,
  /**
   * The maintenance operation that runs when `code` is `maintenance`. Null for any other `code`.
   */
  operation: MaintenanceOperation.nullable(),
  /** The agent's message, up to {@link NODE_UNAVAILABLE_DETAIL_MAX_CHARS} characters. */
  detail: z.string().min(1).max(NODE_UNAVAILABLE_DETAIL_MAX_CHARS),
}).superRefine((unavailable, ctx) => {
  if ((unavailable.code === NODE_UNAVAILABLE.maintenance) !== (unavailable.operation !== null)) {
    ctx.addIssue({ code: 'custom', path: ['operation'], message: 'names the operation exactly when the code is maintenance' });
  }
});

export type NodeUnavailable = z.infer<typeof NodeUnavailable>;

/** The running agent. */
export const AgentInfo = z.object({
  /** The agent package version. */
  version: z.string().min(1).max(64),
  /** The image this agent's container was created from. */
  image: ImageReference,
  /** The id Docker holds for that image. Two builds of one tag differ here. */
  image_id: ImageId,
});

export type AgentInfo = z.infer<typeof AgentInfo>;

/** Longest `detail` an agent update carries. The agent cuts the swap's message to it. */
export const AGENT_UPDATE_DETAIL_MAX_CHARS = 512;

/**
 * Where an {@link ControllerUpdateAgent | update_agent} or {@link ControllerUpdateCs2Image | update_cs2_image}
 * is.
 *
 * - `pulling`: the image is being pulled.
 * - `switching`: the image is on the node and is put into use. A switch of the agent replaces its
 *   container, so the node goes offline and comes back. A switch of the cs2 image extracts its
 *   plugin files for future launches.
 */
export const ImageUpdateStep = z.enum(['pulling', 'switching']);

export type ImageUpdateStep = z.infer<typeof ImageUpdateStep>;

/**
 * How an {@link ControllerUpdateAgent | update_agent} ended.
 *
 * - `completed`: the replacement agent stayed up and the old container is removed.
 * - `unchanged`: the node already ran that image, so nothing was swapped.
 * - `pull_failed`: the image could not be pulled, so the node never tried it.
 * - `failed`: the swap did not hold and the old agent was put back, or its helper did not start.
 */
export const AgentUpdateOutcome = z.enum(['completed', 'unchanged', 'pull_failed', 'failed']);

export type AgentUpdateOutcome = z.infer<typeof AgentUpdateOutcome>;

/** An agent update: the one in progress, one this agent saw end, or the last one to fail. */
export const AgentUpdate = z.object({
  /** The agent image the update was to. */
  image: ImageReference,
  /**
   * Where the update is while it runs, with the replacement still on trial in `switching`. Null
   * once it ended.
   */
  step: ImageUpdateStep.nullable(),
  /** How the update ended. Null while it runs. */
  outcome: AgentUpdateOutcome.nullable(),
  /**
   * Why the update failed, up to {@link AGENT_UPDATE_DETAIL_MAX_CHARS} characters. Null unless it
   * failed.
   */
  detail: z.string().min(1).max(AGENT_UPDATE_DETAIL_MAX_CHARS).nullable(),
});

export type AgentUpdate = z.infer<typeof AgentUpdate>;

/** The machine the agent runs on. */
export const HostInfo = z.object({
  /** Logical CPUs the agent can use. */
  cpu_count: z.number().int().positive(),
  /**
   * The host's os-release `PRETTY_NAME`, up to {@link OS_NAME_MAX_CHARS} characters. Null when
   * unreadable.
   */
  os_name: z.string().min(1).max(OS_NAME_MAX_CHARS).nullable(),
  /** The kernel release, which the agent's container shares with the host. */
  kernel: z.string().min(1).max(256),
});

export type HostInfo = z.infer<typeof HostInfo>;

/**
 * How thoroughly a CS2 install or update runs.
 *
 * - `update`: downloads what changed.
 * - `verify`: also re-hashes every file.
 * - `reinstall`: wipes the install volume first and downloads all of it.
 */
export const Cs2UpdateMode = z.enum(['update', 'verify', 'reinstall']);

export type Cs2UpdateMode = z.infer<typeof Cs2UpdateMode>;

/**
 * How a CS2 install or update ended.
 *
 * - `completed`: the game files are installed.
 * - `pull_failed`: the cs2 image could not be pulled.
 * - `overlay_failed`: its addon overlay could not be extracted.
 * - `failed`: any other failure, such as the update container exiting with an error.
 * - `unobserved`: an update container that an earlier agent process started exited, and the agent
 *   did not see the exit.
 *
 * An update container that an earlier agent process started reports progress again until it exits.
 * It ends `completed` or `failed` by its exit code, or `unobserved`.
 */
export const Cs2UpdateOutcome = z.enum(['completed', 'pull_failed', 'overlay_failed', 'failed', 'unobserved']);

export type Cs2UpdateOutcome = z.infer<typeof Cs2UpdateOutcome>;

/** Longest `detail` a CS2 update carries. The agent cuts the failure's message to it. */
export const CS2_UPDATE_DETAIL_MAX_CHARS = 512;

/**
 * Chosen by the agent for each CS2 install or update it runs or finds running. A lowercase UUID.
 */
export const UpdateId = z.uuid().regex(/^[^A-F]*$/, 'must be lowercase');

/**
 * A CS2 install or update on the node: one running, or the last one to end since the agent started.
 */
export const Cs2Update = z.object({
  /**
   * Identifies the update and its output, the `cs2_update` {@link LogSource} with this `update_id`.
   */
  id: UpdateId,
  /** How the update runs. */
  mode: Cs2UpdateMode,
  /**
   * Whether the agent started the update on its own, to reach the newest public build. When false,
   * an {@link ControllerUpdateCs2 | update_cs2} started it.
   */
  automatic: z.boolean(),
  /** How the update ended. Null while it runs. */
  outcome: Cs2UpdateOutcome.nullable(),
  /**
   * Why the update failed, up to {@link CS2_UPDATE_DETAIL_MAX_CHARS} characters. Null unless it
   * failed.
   */
  detail: z.string().min(1).max(CS2_UPDATE_DETAIL_MAX_CHARS).nullable(),
});

export type Cs2Update = z.infer<typeof Cs2Update>;

/**
 * How an {@link ControllerUpdateCs2Image | update_cs2_image} ended.
 *
 * - `completed`: future launches use the image.
 * - `pull_failed`: the image could not be pulled.
 * - `overlay_failed`: its addon overlay could not be extracted.
 * - `failed`: anything else, such as the agent not saving the image setting.
 */
export const Cs2ImageUpdateOutcome = z.enum(['completed', 'pull_failed', 'overlay_failed', 'failed']);

export type Cs2ImageUpdateOutcome = z.infer<typeof Cs2ImageUpdateOutcome>;

/**
 * A switch of the cs2 image: the one running now, or the last one to end since the agent started.
 */
export const Cs2ImageUpdate = z.object({
  /** The image the switch is to. */
  image: ImageReference,
  /** Where the switch is while it runs. Null once it ended. */
  step: ImageUpdateStep.nullable(),
  /** How the switch ended. Null while it runs. */
  outcome: Cs2ImageUpdateOutcome.nullable(),
  /**
   * Why the switch failed, up to {@link CS2_UPDATE_DETAIL_MAX_CHARS} characters. Null unless it
   * failed.
   */
  detail: z.string().min(1).max(CS2_UPDATE_DETAIL_MAX_CHARS).nullable(),
});

export type Cs2ImageUpdate = z.infer<typeof Cs2ImageUpdate>;

/** The node's game install and image. */
export const Cs2State = z.object({
  /** The installed game build. Null before the first install. */
  build_id: BuildId.nullable(),
  /**
   * The newest public CS2 build the agent read from Steam. Null until its first successful read.
   * The agent updates an installed node to it on its own once every slot is idle, unless the
   * node's `auto_update` is `false`.
   */
  latest_build_id: BuildId.nullable(),
  /** The cs2 image set in `agent.yml`, which future launches use. */
  image: ImageReference,
  /** The local id that `image` resolves to on the node. Null until the image is there. */
  image_id: ImageId.nullable(),
  /**
   * The install or update that runs now, or else the last one to end since the agent started. Null
   * before any. An agent that starts while an update that it started earlier still runs reports
   * that update.
   */
  update: Cs2Update.nullable(),
  /**
   * The switch of `image` that runs now, or else the last one to end since the agent started. Null
   * before any.
   */
  image_update: Cs2ImageUpdate.nullable(),
});

export type Cs2State = z.infer<typeof Cs2State>;

/** An ISO 3166-1 alpha-2 country code: `DK`, `FR`, or `GB` for the United Kingdom. */
export const CountryCode = z.string().regex(/^[A-Z]{2}$/);

/** Longest `detail` a server's exit carries. The agent cuts the message to it. */
export const SERVER_EXIT_DETAIL_MAX_CHARS = 512;

/** A server whose container is gone, and why. */
export const ServerExit = z.object({
  /** The slot it ran in. */
  slot_id: SlotId,
  /** The server that ended. */
  allocation_id: AllocationId,
  /** Why it ended. */
  reason: ServerExitReason,
  /** The container's exit code. Null when the agent did not see one. */
  exit_code: z.number().int().nullable(),
  /**
   * What the reason does not say, up to {@link SERVER_EXIT_DETAIL_MAX_CHARS} characters: the
   * plugin's `config_error` for `config_rejected`, and Docker's message for `launch_failed`. Null
   * for every other reason.
   */
  detail: z.string().min(1).max(SERVER_EXIT_DETAIL_MAX_CHARS).nullable(),
});

export type ServerExit = z.infer<typeof ServerExit>;

/** Longest `reason` an event backlog carries. The agent cuts the failure's message to it. */
export const EVENT_BACKLOG_REASON_MAX_CHARS = 512;

/** A server whose events wait on the node. */
export const StuckServer = z.object({
  /** The server whose plugin sent them. */
  allocation_id: AllocationId,
  /** Events of the server still queued on the node. */
  pending: z.number().int().positive(),
});

export type StuckServer = z.infer<typeof StuckServer>;

const eventBacklogFields = {
  /**
   * The servers with the oldest stuck events, oldest first, up to {@link MAX_REPORTED_BACKLOGS}.
   */
  servers: z.array(StuckServer).max(MAX_REPORTED_BACKLOGS),
  /** Servers with stuck events on the node, those past `servers` too. */
  server_count: z.number().int().nonnegative(),
  /** Events that every stuck server holds together. */
  event_count: z.number().int().nonnegative(),
  /**
   * Why the last upload failed, up to {@link EVENT_BACKLOG_REASON_MAX_CHARS} characters. Null when
   * nothing is stuck, and only then.
   */
  reason: z.string().min(1).max(EVENT_BACKLOG_REASON_MAX_CHARS).nullable(),
};

function addBacklogIssues(backlog: z.infer<z.ZodObject<typeof eventBacklogFields>>, ctx: z.RefinementCtx): void {
  if (backlog.servers.length !== Math.min(backlog.server_count, MAX_REPORTED_BACKLOGS)) {
    ctx.addIssue({ code: 'custom', path: ['servers'], message: `lists the oldest ${MAX_REPORTED_BACKLOGS} of server_count` });
  }

  if (backlog.event_count < backlog.servers.reduce((sum, server) => sum + server.pending, 0)) {
    ctx.addIssue({ code: 'custom', path: ['event_count'], message: 'counts at least the listed servers\' events' });
  }

  if ((backlog.server_count === 0) !== (backlog.reason === null)) {
    ctx.addIssue({ code: 'custom', path: ['reason'], message: 'null exactly when nothing is stuck' });
  }
}

/**
 * The events that wait on the node because uploading them fails, as the agent read them. While
 * uploads work, nothing is stuck, so `servers` is empty, the counts are zero and `reason` is
 * `null`.
 *
 * Rejected unless `servers` holds as many entries as `server_count`, or
 * {@link MAX_REPORTED_BACKLOGS} when `server_count` is larger. It is also rejected when
 * `event_count` is below the sum of `pending` over `servers`. A `reason` is rejected when
 * `server_count` is zero, and a null `reason` when it is not.
 */
export const EventBacklog = z.object(eventBacklogFields).superRefine(addBacklogIssues);

export type EventBacklog = z.infer<typeof EventBacklog>;

/**
 * A server has ended and its container is gone. Sent ahead of the `idle`
 * {@link AgentSlotStatus | slot_status} of its slot, also for a launch that failed before or while
 * its container started.
 *
 * A server that ended while no connection was welcomed goes in `server_exits` of the next
 * {@link AgentHello | hello} instead. If it ended after that {@link AgentHello | hello} went out,
 * it is sent after the {@link ControllerWelcome | welcome}, ahead of the slot statuses.
 *
 * The agent keeps a {@link AgentServerExited | server_exited} until the socket takes its write,
 * across its restarts, for up to {@link EVENT_SPOOL_RETENTION_MS}, so one may also come again after
 * a restart. One the socket took is not sent again, even if the connection drops before the
 * controller reads it. The agent does not keep them while it cannot open that store, which it reports as
 * `unavailable` with `event_storage`. While the store does not answer, the agent keeps one only
 * once the store answers again. A server that ends while the agent is not running is missing
 * from its next {@link AgentHello | hello}.
 */
export const AgentServerExited = ServerExit.extend({
  type: z.literal('server_exited'),
});

export type AgentServerExited = z.infer<typeof AgentServerExited>;

/**
 * The agent's first message on every connection, and sent only then. An applied
 * {@link ControllerSetConfig | set_config} answers with the settings instead.
 *
 * Answer an accepted {@link AgentHello | hello} with {@link ControllerWelcome | welcome}. Until then the
 * agent sends only replies, or a {@link AgentShutdown | shutdown} when it stops. Runtime changes
 * use status messages. Reconnecting never changes the node's configuration.
 *
 * Rejected unless `slot_status` names each slot of `config.slots` once and names no other slot.
 */
export const AgentHello = z.object({
  type: z.literal('hello'),
  /** The agent sending this. */
  agent: AgentInfo,
  /** The host the agent runs on. */
  host: HostInfo,
  /**
   * The node's public IPv4 address as the agent looked it up at start. Null when the lookup failed,
   * and players then reach the node only at `config.pinned_ip`. Players connect to
   * `config.pinned_ip` whenever it is set. The agent looks again only when it restarts.
   */
  public_ip: Ipv4.nullable(),
  /**
   * Where the node's address is, as a geolocation lookup reports it when the agent starts. Null
   * when that lookup failed. The agent tries again only when it restarts.
   */
  country: CountryCode.nullable(),
  /** The configuration the agent runs with. */
  config: NodeSettings,
  /** One per configured slot. */
  slot_status: z.array(SlotStatus).max(MAX_SLOTS),
  /** The game install and image. */
  cs2: Cs2State,
  /**
   * Why the node cannot run servers. Null when it can. Changes arrive as
   * {@link AgentNodeStatus | node_status}.
   */
  unavailable: NodeUnavailable.nullable(),
  /**
   * The demos kept on the node, up to {@link MAX_REPORTED_DEMOS}. {@link AgentDemo | demo} reports
   * each change afterwards.
   */
  demos: z.array(DemoFile).max(MAX_REPORTED_DEMOS),
  /**
   * Servers that ended while no connection was welcomed, oldest first, each with its reason. Their
   * slots already show idle here, so a {@link AgentServerExited | server_exited} after the
   * {@link ControllerWelcome | welcome} would come too late. It carries the oldest {@link MAX_SLOTS} at
   * most, and the rest follow as {@link AgentServerExited | server_exited} after the
   * {@link ControllerWelcome | welcome}.
   */
  server_exits: z.array(ServerExit).max(MAX_SLOTS),
  /**
   * The node's event backlog when the {@link AgentHello | hello} was built.
   * {@link AgentEventBacklog | event_backlog} replaces it.
   */
  event_backlog: EventBacklog,
  /**
   * The swap in progress, or else the one this agent saw end, or else the last that failed. A
   * failure is kept, across agent restarts, until a swap holds or `node-setup.sh` starts the agent.
   * Null when there is none of these. A swap that held shows in `agent.image`.
   */
  agent_update: AgentUpdate.nullable(),
}).superRefine((hello, ctx) => {
  const slots = new Set(hello.config.slots.map((slot) => slot.id));
  const seen = new Set<string>();

  for (const [index, status] of hello.slot_status.entries()) {
    if (!slots.has(status.slot_id) || seen.has(status.slot_id)) {
      ctx.addIssue({ code: 'custom', path: ['slot_status', index, 'slot_id'], message: 'slot_status names each configured slot once' });
    }

    seen.add(status.slot_id);
  }

  if (seen.size !== slots.size) {
    ctx.addIssue({ code: 'custom', path: ['slot_status'], message: 'slot_status names every configured slot' });
  }
});

export type AgentHello = z.infer<typeof AgentHello>;

/**
 * Replaces `cs2`, `unavailable` and `agent_update` of the last {@link AgentHello | hello}.
 *
 * Sent when they change:
 *
 * - When a maintenance operation starts or ends, so also when a CS2 install or update finishes,
 *   whoever started it, or {@link ControllerUpdateCs2Image | update_cs2_image} has switched the image.
 * - When `agent_update` or `cs2.image_update` moves from `pulling` to `switching`.
 * - When the newest public build changes.
 * - When an agent swap that is still proving its replacement ends.
 *
 * Each frame is the node's state when it was read, not a step in a history, so a change that is
 * over before its report reads the state may go unreported.
 */
export const AgentNodeStatus = z.object({
  type: z.literal('node_status'),
  /** The game install and image. */
  cs2: Cs2State,
  /** Why the node cannot run servers. Null when it can. */
  unavailable: NodeUnavailable.nullable(),
  /** The last agent swap, as {@link AgentHello | hello} has it. */
  agent_update: AgentUpdate.nullable(),
});

export type AgentNodeStatus = z.infer<typeof AgentNodeStatus>;

/**
 * A slot's status changed since the last {@link AgentHello | hello} or
 * {@link AgentSlotStatus | slot_status} for it. Rejected when it breaks a rule of
 * {@link SlotStatus}.
 */
export const AgentSlotStatus = z.object({
  type: z.literal('slot_status'),
  ...slotStatusFields,
}).superRefine(addSlotStatusIssues);

export type AgentSlotStatus = z.infer<typeof AgentSlotStatus>;

/** How serious an agent log line is. */
export const AgentLogLevel = z.enum(['info', 'warn', 'error']);

export type AgentLogLevel = z.infer<typeof AgentLogLevel>;

/** Longer log lines, from the agent, a server or a CS2 update, are cut to this many characters. */
export const LOG_LINE_MAX_CHARS = 2_000;

/** The agent keeps this many lines of its log. */
export const AGENT_LOG_LINES = 500;

/**
 * The agent keeps this many lines of each server's output and of each CS2 install or update's
 * output.
 */
export const SERVER_LOG_LINES = 2_000;

/** Servers that ended and whose output the agent still keeps, oldest dropped first. */
export const ENDED_SERVER_LOGS = 10;

/**
 * CS2 installs and updates that ended and whose output the agent still keeps, oldest dropped first.
 */
export const ENDED_UPDATE_LOGS = 1;

/**
 * A {@link AgentLogBatch | log_batch} stays under this many bytes of JSON, so a replay spans as
 * many frames as it needs.
 */
export const LOG_BATCH_MAX_BYTES = 256 * 1024;

/**
 * One log of a node: the agent's log, one CS2 install or update's output, or one server's output.
 */
export const LogSource = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('agent') }),
  z.object({
    kind: z.literal('cs2_update'),
    /** The install or update that printed it, the `id` of its `cs2.update`. */
    update_id: UpdateId,
  }),
  z.object({
    kind: z.literal('server'),
    /** The server that printed it. */
    allocation_id: AllocationId,
  }),
]);

export type LogSource = z.infer<typeof LogSource>;

/** One log line. */
export const LogLine = z.object({
  /** Unix milliseconds when the agent logged or read the line. */
  at: z.number().int().nonnegative(),
  /** How serious the agent rated the line. Null for server and CS2 update output. */
  level: AgentLogLevel.nullable(),
  /**
   * The line without its newline. The agent's lines carry the prefix of the component that logged
   * them. A line keeps its ANSI colour escapes: SGR, `ESC[` with digits and `;`, then `m`. The agent
   * removes every other escape, every control character but tab, and the Unicode characters that
   * reorder text or break a line. The length limit may cut the last escape short.
   */
  line: z.string().max(LOG_LINE_MAX_CHARS),
});

export type LogLine = z.infer<typeof LogLine>;

/** Identifies one watcher's replay, so the frames answering it reach only that watcher. */
export const ReplayId = z.string().min(1).max(64);

/**
 * One source's lines, sent only while a {@link ControllerWatchLogs | watch_logs} holds the lease of that
 * source. A replay of the source starts with `reset` and may span several frames. Between replays,
 * new lines follow in batches. A replay larger than the remaining log budget of the agent leaves
 * out the oldest lines.
 */
export const AgentLogBatch = z.object({
  type: z.literal('log_batch'),
  /** The log these lines belong to. */
  source: LogSource,
  /**
   * The {@link ControllerWatchLogs | watch_logs} replay these frames answer, for that watcher alone. Null
   * for new lines and for a replay sent to every watcher.
   */
  replay_id: ReplayId.nullable(),
  /** Whether a replay starts, so the controller drops every line it holds of this source first. */
  reset: z.boolean(),
  /** The lines, oldest first. */
  lines: z.array(LogLine),
});

export type AgentLogBatch = z.infer<typeof AgentLogBatch>;

/** One source the agent keeps. */
export const LogSourceSummary = z.object({
  /** Which log. */
  source: LogSource,
  /** Lines kept of it. */
  line_count: z.number().int().nonnegative(),
  /** Unix milliseconds of the newest line kept of it. Null for a source with no lines kept. */
  last_line_at: z.number().int().nonnegative().nullable(),
  /**
   * Whether the source has ended, as a server whose slot went idle or a CS2 install or update that
   * ended. Always `false` for the agent's log.
   */
  ended: z.boolean(),
});

export type LogSourceSummary = z.infer<typeof LogSourceSummary>;

/**
 * Milliseconds between {@link AgentLogIndex | log_index} frames that only bring counts and
 * `last_line_at` up to date while lines arrive. A source's newest line shows in an index within
 * this long.
 */
export const LOG_INDEX_REFRESH_MS = 30_000;

/**
 * Every source the agent keeps, whole every time: its log, each CS2 install or update running or
 * among the last {@link ENDED_UPDATE_LOGS} ended, and each server with output, running or among the
 * last {@link ENDED_SERVER_LOGS} ended.
 *
 * Sent while any log lease runs, with each lease that starts and each replay asked for. Also sent
 * when an update starts or ends, a server's first line arrives, a server ends, or a source's output
 * is dropped, and every {@link LOG_INDEX_REFRESH_MS} while lines arrive. Counts and `last_line_at`
 * are as of that frame. A source missing from it is no longer kept.
 */
export const AgentLogIndex = z.object({
  type: z.literal('log_index'),
  /**
   * The agent's log first, then CS2 updates in the order they started, then servers in the order
   * their output began.
   */
  sources: z.array(LogSourceSummary).max(2 + ENDED_UPDATE_LOGS + MAX_SLOTS + ENDED_SERVER_LOGS),
});

export type AgentLogIndex = z.infer<typeof AgentLogIndex>;

/** Longest `message` a CS2 update's progress carries. The agent cuts the line to it. */
export const CS2_UPDATE_MESSAGE_MAX_CHARS = 512;

/**
 * Progress of a CS2 install or update while it runs. `cs2.update` in
 * {@link AgentNodeStatus | node_status} says how it ended.
 */
export const AgentCs2UpdateProgress = z.object({
  type: z.literal('cs2_update_progress'),
  /**
   * The update it reports on, as `cs2.update.id`. A frame can arrive before the
   * {@link AgentNodeStatus | node_status} that names the update.
   */
  update_id: UpdateId,
  /** Percent done, as `steamcmd` reports it. Null when `steamcmd` reports none. */
  percent: z.number().min(0).max(100).nullable(),
  /** What the install is doing, up to {@link CS2_UPDATE_MESSAGE_MAX_CHARS} characters. */
  message: z.string().max(CS2_UPDATE_MESSAGE_MAX_CHARS),
});

export type AgentCs2UpdateProgress = z.infer<typeof AgentCs2UpdateProgress>;

/**
 * One demo as it is now, sent when it appears or changes: its file, its upload starting or
 * stopping, or its `upload_error`. It replaces the demo with the same `slot_id` and `file_name`, or
 * adds it. {@link AgentHello | hello} lists the newest {@link MAX_REPORTED_DEMOS}. A demo that
 * appears after the {@link AgentHello | hello} is added, however many the node keeps.
 */
export const AgentDemo = DemoFile.extend({
  type: z.literal('demo'),
});

export type AgentDemo = z.infer<typeof AgentDemo>;

/**
 * Why a demo is no longer on the node.
 *
 * - `uploaded`: the target took it and the agent removed it.
 * - `deleted`: {@link ControllerDeleteDemo | delete_demo} or anything else removed it.
 */
export const DemoRemovedReason = z.enum(['uploaded', 'deleted']);

export type DemoRemovedReason = z.infer<typeof DemoRemovedReason>;

/** A demo that the controller knows of is no longer on the node. */
export const AgentDemoRemoved = z.object({
  type: z.literal('demo_removed'),
  /** The slot whose directory held it. */
  slot_id: SlotId,
  /** The file's name. */
  file_name: DemoFileName,
  /** Why it is no longer on the node. */
  reason: DemoRemovedReason,
});

export type AgentDemoRemoved = z.infer<typeof AgentDemoRemoved>;

/** How far the uploading demo is, at most once a second while it uploads. */
export const AgentDemoProgress = z.object({
  type: z.literal('demo_progress'),
  /** The slot whose directory holds it. */
  slot_id: SlotId,
  /** The file's name. */
  file_name: DemoFileName,
  /** Bytes sent so far. */
  sent_bytes: z.number().int().nonnegative(),
  /**
   * Bytes a second since the previous {@link AgentDemoProgress | demo_progress} of this upload, or
   * since it began.
   */
  bytes_per_second: z.number().int().nonnegative(),
});

export type AgentDemoProgress = z.infer<typeof AgentDemoProgress>;

/**
 * Milliseconds between the agent's pings to the controller. The agent drops the connection when
 * the last ping went unanswered.
 */
export const AGENT_PING_INTERVAL_MS = 30_000;

/**
 * Milliseconds an agent may send nothing before the controller drops it, which allows two pings
 * and slack.
 */
export const AGENT_PING_TIMEOUT_MS = 65_000;

/** Milliseconds each agent dial has to finish the WebSocket upgrade. */
export const AGENT_HANDSHAKE_TIMEOUT_MS = 10_000;

/**
 * Milliseconds of the agent's first redial delay. Each failed attempt doubles it, and a
 * {@link ControllerWelcome | welcome} or a redial to a new address resets it.
 */
export const AGENT_RECONNECT_MIN_MS = 1_000;

/** Milliseconds of the agent's longest redial delay, before jitter. */
export const AGENT_RECONNECT_MAX_MS = 30_000;

/**
 * Each redial delay is randomised by up to this fraction either way, never below
 * {@link AGENT_RECONNECT_MIN_MS}.
 */
export const AGENT_RECONNECT_JITTER = 0.25;

/** Milliseconds between the agent's resource ticks, taken while it runs, watched or not. */
export const RESOURCE_TICK_MS = 3_000;

/** Ticks that the agent holds and the controller shows per node, the newest last. */
export const RESOURCE_TICKS_HELD = 100;

/**
 * Milliseconds a {@link ControllerWatchResources | watch_resources} or {@link ControllerWatchLogs | watch_logs}
 * keeps its stream flowing. The next one resets it.
 */
export const WATCH_LEASE_MS = 120_000;

/** A capacity and how much of it is in use, in bytes. */
export const ResourceUsage = z.object({
  /** Bytes of capacity. */
  cap: z.number().int().nonnegative(),
  /** Bytes in use. */
  used: z.number().int().nonnegative(),
});

export type ResourceUsage = z.infer<typeof ResourceUsage>;

/** One sample of the node's disk, memory and CPU, taken every {@link RESOURCE_TICK_MS}. */
export const ResourceTick = z.object({
  /** Unix milliseconds when the tick was taken. */
  at: z.number().int().nonnegative(),
  /**
   * The filesystem holding the node's data directory, as `df` counts it. Null when it could not be
   * read.
   */
  disk: ResourceUsage.nullable(),
  /**
   * The memory the node may use, and how much it uses. Cache the kernel can free counts as free.
   */
  memory: ResourceUsage,
  /**
   * How busy the node's CPUs are, in percent of all the CPUs it may use. Null in the first tick.
   */
  cpu: z.object({
    /** Percent busy at the busiest moment of the tick. */
    max: z.number().int().min(0).max(100),
    /** Percent busy on average over the tick. */
    average: z.number().int().min(0).max(100),
  }).nullable(),
});

export type ResourceTick = z.infer<typeof ResourceTick>;

/**
 * The agent's resource ticks, oldest first. A {@link ControllerWatchResources | watch_resources} lease
 * that starts receives every tick the agent holds, then each new tick while it runs. The agent
 * sends nothing while nobody watches, and nothing is stored.
 */
export const AgentResourceBatch = z.object({
  type: z.literal('resource_batch'),
  /**
   * The ticks, oldest first: every tick the agent holds, up to the last
   * {@link RESOURCE_TICKS_HELD}, or the ticks since the previous frame.
   */
  ticks: z.array(ResourceTick).min(1).max(RESOURCE_TICKS_HELD),
  /**
   * Whether `ticks` is every tick the agent holds and replaces the ticks the controller holds. It
   * is `true` when a {@link ControllerWatchResources | watch_resources} starts the lease or asks for a
   * replay, and on the first tick after each accepted {@link AgentHello | hello} while the lease
   * runs. When false, `ticks` adds to them.
   */
  reset: z.boolean(),
});

export type AgentResourceBatch = z.infer<typeof AgentResourceBatch>;

/**
 * The node's {@link EventBacklog} now, replacing the backlog of the last {@link AgentHello | hello}
 * or {@link AgentEventBacklog | event_backlog}. Sent when it changes: after each failed upload, as
 * its counts grow, and once, with nothing stuck, when uploads work again. Rejected when it breaks a
 * rule of {@link EventBacklog}.
 */
export const AgentEventBacklog = z.object({
  type: z.literal('event_backlog'),
  ...eventBacklogFields,
}).superRefine(addBacklogIssues);

export type AgentEventBacklog = z.infer<typeof AgentEventBacklog>;

/**
 * Chosen by the controller. The reply repeats it as `reply_to`. While a request is running, the
 * agent ignores another with the same id on that connection, because a reply could not tell the two
 * apart.
 */
const RequestId = z.string().min(1).max(64);

/**
 * Milliseconds a stopping agent waits for the requests it is running before it closes the socket,
 * counted from its {@link AgentShutdown | shutdown}. A request still running then is cut off.
 */
export const SHUTDOWN_REQUEST_WAIT_MS = 5_000;

/**
 * The agent is stopping cleanly and starts no new work. It answers a request that arrives after
 * this frame with `shutting_down` at once. Replies to the requests it is running may still follow
 * for up to {@link SHUTDOWN_REQUEST_WAIT_MS}, and then it closes with code `1000`.
 *
 * Send no new requests after it. A request without a reply when the socket closes may or may not
 * have run; the node's reports after it reconnects say which. Its servers keep running, and the
 * agent takes them over when it returns. It is sent whether or not the controller has welcomed the
 * {@link AgentHello | hello}.
 */
export const AgentShutdown = z.object({
  type: z.literal('shutdown'),
  /** The signal that stopped the agent. */
  signal: z.enum(['SIGINT', 'SIGTERM', 'SIGQUIT']),
});

const request = {
  /**
   * Chosen by the controller and unique among its open requests. The reply repeats it as
   * `reply_to`. The agent ignores a request whose id is still running on the connection.
   */
  id: RequestId,
};

/**
 * Launch a server on an idle slot the agent picks. The server logs in with the slot's token. The
 * agent reports the slot `starting` before any container work, so two launches never share one.
 *
 * The reply comes once the container has started, before the server is up, and
 * {@link AgentSlotStatus | slot_status} reports it `running` later. The agent pulls no image here.
 * A launch on a `cs2_image` whose plugin files the agent has not extracted yet waits while it does.
 * Refused with:
 *
 * - A {@link StartRefusal} when the node cannot take the launch.
 * - `allocation_in_use` when the node is already launching the allocation or its server holds a
 *   slot.
 * - `allocation_reused` when the node launched the allocation before.
 * - `failed` when Docker fails the launch, once its container is gone, after a
 *   {@link AgentServerExited | server_exited} with `launch_failed`.
 */
export const ControllerStartServer = z.object({
  type: z.literal('start_server'),
  ...request,
  /**
   * Identifies the server from now on. Every launch needs a new one, a relaunch too, because the
   * node files the server's events and demos under it. A launch that a {@link StartRefusal} turned
   * away used nothing, so the same id may be sent again. Any other failure uses it up.
   */
  allocation_id: AllocationId,
  /** What to launch. */
  launch: ServerLaunchConfig,
});

export type ControllerStartServer = z.infer<typeof ControllerStartServer>;

/** The server that started, and the slot the agent picked for it. */
export const StartServerReply = z.object({
  /** The allocation the request launched. */
  allocation_id: AllocationId,
  /** The slot the server runs in. */
  slot_id: SlotId,
});

export type StartServerReply = z.infer<typeof StartServerReply>;

/** Seconds of grace before `SIGKILL` for an ordinary stop. */
export const STOP_TIMEOUT_SECONDS = 10;


/**
 * Milliseconds the agent waits for an answer from the Docker daemon on its host before it fails the
 * call. A stop waits its grace on top.
 */
export const DOCKER_ANSWER_TIMEOUT_MS = 30_000;

/**
 * Seconds a stop's reply may take beyond its grace: Docker's answer to the stop, which the agent
 * waits for up to {@link DOCKER_ANSWER_TIMEOUT_MS}, and the reply.
 */
export const STOP_REPLY_MARGIN_SECONDS = 35;

/**
 * Stop an allocation's server. Once asked, the agent stops it: a stop that Docker fails is answered
 * with `failed`, and the agent tries it again until the server is gone. The slot stays held until
 * then.
 *
 * The reply comes once its container has stopped: up to {@link STOP_TIMEOUT_SECONDS} before the kill, then up
 * to {@link STOP_REPLY_MARGIN_SECONDS} seconds for the kill and the reply. A stop during a launch
 * of the allocation first waits for the launch's current step to end. That step can take minutes on
 * the first launch after an image switch, which extracts the overlay, so the reply can come after
 * the session's wait. If the launch is stopped between its steps, it starts no server and its
 * `start_server` fails. If it is stopped while Docker starts its container, its `start_server`
 * succeeds and the server is then stopped. Refused with `unknown_allocation` for an allocation the
 * node does not run.
 */
export const ControllerStopServer = z.object({
  type: z.literal('stop_server'),
  /** The server to stop. */
  allocation_id: AllocationId,
  ...request,
});

/** The server that stopped. */
export const StopServerReply = z.object({
  /** The allocation the request stopped. */
  allocation_id: AllocationId,
});

export type StopServerReply = z.infer<typeof StopServerReply>;

/**
 * Install or update CS2 on the node's shared volume. The first install happens only through this
 * request.
 *
 * The reply comes immediately with the update's id, as {@link UpdateCs2Reply}, and the update runs
 * after it, which can take tens of minutes. Refused with `slots_held` while any slot is held and
 * `maintenance_running` while a maintenance operation runs.
 *
 * `cs2.update` in {@link AgentNodeStatus | node_status} reports the update from `outcome: null` to
 * how it ended, a failed pull of the cs2 image included. Meanwhile
 * {@link AgentCs2UpdateProgress | cs2_update_progress} and {@link AgentLogBatch | log_batch} report
 * on it, each naming it by that id.
 *
 * An installed node also updates itself to `cs2.latest_build_id` once every slot is idle, unless
 * its `auto_update` is `false`.
 */
export const ControllerUpdateCs2 = z.object({
  type: z.literal('update_cs2'),
  ...request,
  /** How thoroughly to update. */
  mode: Cs2UpdateMode,
});

/** The update an accepted {@link ControllerUpdateCs2 | update_cs2} started. */
export const UpdateCs2Reply = z.object({
  /**
   * The update's id, as `cs2.update.id`, {@link AgentCs2UpdateProgress | cs2_update_progress} and
   * its log source carry it.
   */
  update_id: UpdateId,
});

export type UpdateCs2Reply = z.infer<typeof UpdateCs2Reply>;

/**
 * Read the newest public CS2 build from Steam now instead of at the next scheduled read, which
 * comes every {@link CS2_BUILD_POLL_INTERVAL_MS}.
 *
 * It receives no reply. The agent handles at most one per {@link CS2_BUILD_CHECK_INTERVAL_MS} and
 * drops any sooner one. One sent during a scheduled read shares it. Neither busy slots nor
 * maintenance stop it. A changed build arrives as {@link AgentNodeStatus | node_status}, and an
 * installed node updates to it once every slot is idle, as it always does.
 */
export const ControllerCheckCs2Build = z.object({
  type: z.literal('check_cs2_build'),
});

/**
 * Delete one demo by slot and file name.
 *
 * The reply comes once the file is removed. Refused with:
 *
 * - `unknown_slot` when the node has no directory for the slot.
 * - `unknown_demo` when the slot has no demo of that name.
 * - `demo_recording` while its server records the demo.
 * - `demo_uploading` while the demo uploads.
 * - `invalid_request` when the name is outside the slot's `demos/` directory.
 * - `failed` when the removal fails.
 */
export const ControllerDeleteDemo = z.object({
  type: z.literal('delete_demo'),
  ...request,
  /** The slot whose directory holds it. */
  slot_id: SlotId,
  /** The file to delete. */
  file_name: DemoFileName,
});

/** The demo that was removed. */
export const DeleteDemoReply = z.object({
  /** The slot whose directory held it. */
  slot_id: SlotId,
  /** The file removed. */
  file_name: DemoFileName,
});

export type DeleteDemoReply = z.infer<typeof DeleteDemoReply>;

/**
 * Seconds a {@link ControllerReserveSlot | reserve_slot} lasts, unless a {@link ControllerSetConfig | set_config}
 * with the slot's new token ends it first.
 */
export const SLOT_RESERVATION_SECONDS = 60;

/**
 * Keep an idle slot from launches while the controller replaces its token, so that no server
 * starts on a token that the controller is about to revoke at Steam.
 *
 * The agent marks the slot `reserved` before it replies, the same way a launch marks its slot
 * `starting`, so no {@link ControllerStartServer | start_server} can take the slot once the reply is sent.
 * The reply comes immediately. Refused with `slot_held` for a slot holding a server, and with
 * `unknown_slot` for a slot the node does not have.
 *
 * The reservation ends with the {@link ControllerSetConfig | set_config} that gives the slot a new token,
 * with the slot's removal, after {@link SLOT_RESERVATION_SECONDS}, or with the agent's restart.
 * Asking again restarts the wait.
 */
export const ControllerReserveSlot = z.object({
  type: z.literal('reserve_slot'),
  ...request,
  /** The slot to reserve. */
  slot_id: SlotId,
});

export type ControllerReserveSlot = z.infer<typeof ControllerReserveSlot>;

/** The slot that is reserved. */
export const ReserveSlotReply = z.object({
  /** The slot reserved. */
  slot_id: SlotId,
});

export type ReserveSlotReply = z.infer<typeof ReserveSlotReply>;

/**
 * Apply an edit to `agent.yml`, or refuse it without saving any part. Nothing is queued.
 *
 * The reply comes once the edit is saved and applied, and carries the node's current settings, the
 * {@link NodeSettings} that `hello.config` reports. An edit that changes nothing is not saved
 * again. A slot missing from the reply's `slots` has been removed. Refused with:
 *
 * - `slot_held` when the edit would remove or re-port a held slot, or give its server's token to
 *   another slot.
 * - `slots_held` when the edit changes any key under `controller` (its address, TLS or paths) while any
 *   slot is held.
 * - `slot_token_required` or `demo_header_value_required` when the edit keeps a value that the
 *   node does not have.
 * - `invalid_request` when two slots have one token or anything else fails validation.
 *
 * After the reply, the agent sends a {@link AgentSlotStatus | slot_status} for each slot that the
 * edit added or gave a new token. The slot reads `unchecked` until the agent has logged the new
 * token on to Steam, which the agent starts then unless a server on the node is using the token. A
 * change to `controller` closes the socket with `1000` once the reply has gone, and the agent sends its
 * {@link AgentHello | hello} to the new address.
 *
 * Required of a controller that hands nodes an `agent.yml` without slots, because it is how they
 * receive any.
 */
export const ControllerSetConfig = z.object({
  type: z.literal('set_config'),
  ...request,
  /** The settings to change. */
  config: NodeSettingsEdit,
});

export type ControllerSetConfig = z.infer<typeof ControllerSetConfig>;

/**
 * Replace the agent container with another image. Running servers keep running.
 *
 * The reply comes immediately with what happens next, as {@link UpdateAgentReply}, and
 * `agent_update` reports the rest: its `step` while it runs, then how it ended. Refused with
 * `maintenance_running` while a maintenance operation runs, with `image_missing` for an image the
 * node lacks without `pull`, and with `image_not_allowed` for an image outside the official agent
 * repository while the node's `official_images_only` is on.
 *
 * Switching stops this agent, and the agent that comes up, the new one or the old one put back,
 * reports how it went. The update counts as a maintenance operation until it ends, and a switch
 * until its replacement has proved it stays up.
 */
export const ControllerUpdateAgent = z.object({
  type: z.literal('update_agent'),
  ...request,
  /** The agent image to run. */
  image: ImageReference,
  /** Whether to pull the image. When false, the agent uses an image already on the node. */
  pull: z.boolean(),
});

/**
 * What follows an accepted {@link ControllerUpdateAgent | update_agent}: the update's first
 * {@link ImageUpdateStep}, or `unchanged` when the node runs that image already, as the registry or
 * the local image says, and nothing more happens.
 */
export const UpdateAgentReply = z.object({
  /** The agent image the update is to. */
  image: ImageReference,
  /** The next step. */
  step: z.enum([...ImageUpdateStep.options, 'unchanged']),
});

export type UpdateAgentReply = z.infer<typeof UpdateAgentReply>;

/**
 * Switch future launches to another cs2 image. Running servers keep theirs, and so does a launch
 * that took its slot before the switch.
 *
 * The reply comes immediately with what happens next, as {@link UpdateCs2ImageReply}. Refused with
 * `maintenance_running` while a maintenance operation runs, with `image_missing` for an image the
 * node lacks without `pull`, and with `image_not_allowed` for an image outside the official cs2
 * repository while the node's `official_images_only` is on.
 *
 * The agent then pulls the image when asked to, extracts its plugin files and removes the images
 * it supersedes. `cs2.image_update` in {@link AgentNodeStatus | node_status} reports its `step`
 * while it runs, then how it ended, in the same {@link AgentNodeStatus | node_status} as the new
 * `cs2.image`.
 */
export const ControllerUpdateCs2Image = z.object({
  type: z.literal('update_cs2_image'),
  ...request,
  /** The cs2 image future launches use. */
  image: ImageReference,
  /** Whether to pull the image. When false, the agent uses an image already on the node. */
  pull: z.boolean(),
});

/**
 * What follows an accepted {@link ControllerUpdateCs2Image | update_cs2_image}: its first
 * {@link ImageUpdateStep}.
 */
export const UpdateCs2ImageReply = z.object({
  /** The cs2 image the switch is to. */
  image: ImageReference,
  /** The next step. */
  step: ImageUpdateStep,
});

export type UpdateCs2ImageReply = z.infer<typeof UpdateCs2ImageReply>;

/**
 * Start or renew the resource lease, which lasts {@link WATCH_LEASE_MS}. It receives no reply.
 *
 * While the lease runs, the agent streams {@link AgentResourceBatch | resource_batch}. A lease that
 * starts first sends every tick the agent holds.
 */
export const ControllerWatchResources = z.object({
  type: z.literal('watch_resources'),
  /** Whether to send every tick the agent holds now, also while the lease runs. */
  replay: z.boolean(),
});

export type ControllerWatchResources = z.infer<typeof ControllerWatchResources>;

/**
 * Start or renew the lease of one log source, which lasts {@link WATCH_LEASE_MS}. It receives no
 * reply.
 *
 * While the lease runs, the agent streams the source's {@link AgentLogBatch | log_batch} frames.
 * While any source's lease runs, the agent also sends {@link AgentLogIndex | log_index}. A lease
 * that starts replays the source, to every watcher unless a `replay_id` names the one that asked.
 * Any number of sources can be watched together, each on a lease of its own.
 */
export const ControllerWatchLogs = z.object({
  type: z.literal('watch_logs'),
  /** The log to stream. A source the agent does not keep streams nothing until it prints. */
  source: LogSource,
  /**
   * The id that the replay frames carry, for the watcher that asked for the replay. Null for no
   * replay, as in a renewal. A lease that starts still replays the source, to every watcher.
   */
  replay_id: ReplayId.nullable(),
});

export type ControllerWatchLogs = z.infer<typeof ControllerWatchLogs>;

/**
 * The controller's answer to a {@link AgentHello | hello} it accepted, before anything else on the
 * socket. Without one the agent redials after {@link AGENT_WELCOME_TIMEOUT_MS}.
 *
 * After each {@link ControllerWelcome | welcome} the agent sends what changed since the
 * {@link AgentHello | hello}, its pending {@link AgentServerExited | server_exited} first and then
 * the changed statuses, demos, backlog and watched streams. It sends each state once, so a slot
 * that went from `starting` to `running` arrives as `running`.
 */
export const ControllerWelcome = z.object({ type: z.literal('welcome') });

/**
 * Why the controller refused a {@link AgentHello | hello}: `invalid_hello`, the
 * {@link AgentHello | hello} fails its schema. A controller refuses nothing else at hello: an agent
 * on a version it does not support connects, so it can still be updated, and receives no launches.
 */
export const HelloRefusedReason = z.enum(['invalid_hello']);

export type HelloRefusedReason = z.infer<typeof HelloRefusedReason>;

/**
 * The controller's answer to a {@link AgentHello | hello} it will not accept, followed by a close
 * with code `1008` and reason `hello_refused`. The agent stops dialling until it is restarted,
 * because the same {@link AgentHello | hello} would be refused again.
 */
export const ControllerHelloRefused = z.object({
  type: z.literal('hello_refused'),
  reason: HelloRefusedReason,
  /** A reason for the operator reading the agent's log, such as the field that failed. */
  detail: z.string().max(512),
});

export type ControllerHelloRefused = z.infer<typeof ControllerHelloRefused>;

/**
 * Longest console command line, in characters. CS2 refuses a longer one with `Command too long...
 * ignoring!` somewhere past 500 (tested in October 2026), and reports it only in its log.
 */
export const CONSOLE_COMMAND_MAX_CHARS = 500;

/**
 * Console commands that a server running a match refuses, because each one changes the match
 * outside the plugin's control. The plugin runs them itself where the match needs them.
 *
 * A {@link ControllerConsoleCommand | console_command} line that names one is refused, and a line
 * of the launch's `commands` that names one is left out. A line names a command when one of its
 * parts, split at every `;`, starts with it, in any case.
 */
export const MATCH_BLOCKED_COMMANDS = [
  'mp_restartgame', 'mp_warmup_start', 'mp_warmup_end', 'mp_warmup_pausetimer', 'mp_warmuptime',
  'mp_swapteams', 'mp_switchteams', 'mp_scrambleteams',
  'mp_backup_restore_load_file', 'mp_backup_round_file', 'mp_backup_round_auto',
  'restart', 'changelevel', 'map', 'host_workshop_map', 'ds_workshop_changelevel',
  'tv_record', 'tv_stoprecord', 'alias',
] as const;

/**
 * Run a console command on a server through its plugin.
 *
 * The reply comes once the plugin has handed the command to the engine, which reports nothing
 * back, so the reply cannot say that it worked. Refused with:
 *
 * - `unknown_allocation` when the node does not run the server.
 * - `plugin_offline` while the slot's `plugin_online` is `false`.
 * - `plugin_status_timeout` when the plugin does not answer.
 * - `command_blocked` when the server runs a match and the line names a command of
 *   {@link MATCH_BLOCKED_COMMANDS}. The plugin then runs none of the line.
 *
 * The output of the command joins the server's log, which {@link AgentLogBatch | log_batch} streams
 * while a {@link ControllerWatchLogs | watch_logs} lease covers it and replays when one starts.
 */
export const ControllerConsoleCommand = z.object({
  type: z.literal('console_command'),
  ...request,
  /** The server to run it on. */
  allocation_id: AllocationId,
  /**
   * The console command line, up to {@link CONSOLE_COMMAND_MAX_CHARS} characters, handed to the
   * engine as it is, so a newline or `;` runs more than one command. It must not be blank, and tabs
   * and newlines are its only control characters.
   */
  command: z.string().max(CONSOLE_COMMAND_MAX_CHARS)
    .regex(/\S/u, 'must not be blank')
    .regex(/^(?:[^\p{Cc}]|[\t\n])*$/u, 'must not contain control characters other than tabs and newlines'),
});

export type ControllerConsoleCommand = z.infer<typeof ControllerConsoleCommand>;

/** The server the command was handed to. */
export const ConsoleCommandReply = z.object({
  /** The allocation the command went to. */
  allocation_id: AllocationId,
});

export type ConsoleCommandReply = z.infer<typeof ConsoleCommandReply>;

/**
 * Play a running match on from one of its earlier rounds, on the same server, without a new
 * launch. The engine writes a backup as each live round starts, and the node keeps the backups for
 * the server's life. This request loads the one for `round`.
 *
 * The reply comes once the plugin has asked the engine to load the backup. The
 * {@link MatchRestored | match_restored} event, which the plugin sends ahead of the restored
 * round's {@link RoundStart | round_start}, confirms that the engine did. Refused with:
 *
 * - `unknown_allocation` when the node does not run the server.
 * - `no_backup` when the node has no backup of that round, as in a game mode without backups
 *   ({@link GameMode}).
 * - `plugin_offline` while the slot's `plugin_online` is `false`.
 * - `plugin_status_timeout` when the plugin does not answer.
 * - A {@link CommandRefusal} code when the plugin refuses the restore.
 *
 * The node keeps the backup of every round it has played, including rounds after one it was
 * restored to. If `round` is past the current round, the server loads play that the match has
 * since left. Whether to allow that is the controller's rule.
 */
export const ControllerRestoreRound = z.object({
  type: z.literal('restore_round'),
  ...request,
  /** The server whose match to restore. */
  allocation_id: AllocationId,
  /** The one-based round to play on from, whose backup the engine wrote as the round started. */
  round: RoundNumber,
});

export type ControllerRestoreRound = z.infer<typeof ControllerRestoreRound>;

/** The server and round the restore was handed to. */
export const RestoreRoundReply = z.object({
  /** The allocation the restore went to. */
  allocation_id: AllocationId,
  /** The round it loads. */
  round: RoundNumber,
});

export type RestoreRoundReply = z.infer<typeof RestoreRoundReply>;

/**
 * Start a match in warmup whatever its `ready_mode`, without waiting for anyone to be ready. An
 * admin can always start the match this way, and under `external` it is the only way to.
 *
 * The reply comes once the plugin has started the countdown to the knife round or live play.
 * Refused with:
 *
 * - `unknown_allocation` when the node does not run the server.
 * - `plugin_offline` while the slot's `plugin_online` is `false`.
 * - `plugin_status_timeout` when the plugin does not answer.
 * - `no_match`, `not_in_warmup` or `team_empty` when the plugin refuses the start.
 *
 * A repeat while the countdown runs is accepted and changes nothing.
 */
export const ControllerStartMatch = z.object({
  type: z.literal('start_match'),
  ...request,
  /** The server whose match to start. */
  allocation_id: AllocationId,
});

export type ControllerStartMatch = z.infer<typeof ControllerStartMatch>;

/** The server the start was handed to. */
export const StartMatchReply = z.object({
  /** The allocation the start went to. */
  allocation_id: AllocationId,
});

export type StartMatchReply = z.infer<typeof StartMatchReply>;

/**
 * Milliseconds the agent waits for the plugin to answer
 * {@link ControllerGetServerStatus | get_server_status}, {@link ControllerConsoleCommand | console_command},
 * {@link ControllerRestoreRound | restore_round} or {@link ControllerStartMatch | start_match}. Past it, the
 * request fails with `plugin_status_timeout`.
 */
export const SERVER_STATUS_TIMEOUT_MS = 10_000;

/**
 * Read a server's state as its plugin reports it now.
 *
 * The reply comes within {@link SERVER_STATUS_TIMEOUT_MS}. Refused with:
 *
 * - `unknown_allocation` when the node does not run the server.
 * - `plugin_offline` while the slot's `plugin_online` is `false` or when the plugin leaves before
 *   answering.
 * - `plugin_status_timeout` after {@link SERVER_STATUS_TIMEOUT_MS}.
 */
export const ControllerGetServerStatus = z.object({
  type: z.literal('get_server_status'),
  ...request,
  /** The server to ask. */
  allocation_id: AllocationId,
});

/**
 * Where a match is.
 *
 * - `loading`: its map is loading.
 * - `warmup`: warmup.
 * - `pending_restore`: a launch with a restore went here from warmup while the engine loads the
 *   backup, and goes to `live` once the restored round starts. A load that fails leaves the match
 *   here, where the plugin reports nothing, until a load is tried again or the server is stopped.
 * - `knife`: the knife round.
 * - `side_choice`: the side choice.
 * - `live`: live play.
 * - `post_live`: after the last round.
 */
export const MatchPhase = z.enum(['loading', 'warmup', 'pending_restore', 'knife', 'side_choice', 'live', 'post_live']);

export type MatchPhase = z.infer<typeof MatchPhase>;

/** Longest `map_name` a server's state carries. The plugin cuts the engine's name to it. */
export const MAP_NAME_MAX_CHARS = 64;

/** Longest `config_error` a server's state carries. The plugin cuts its reason to it. */
export const CONFIG_ERROR_MAX_CHARS = 512;

/** A game server's state as its plugin reports it. */
export const ServerState = z.object({
  /** The map the server is on, up to {@link MAP_NAME_MAX_CHARS} characters. */
  map_name: z.string().max(MAP_NAME_MAX_CHARS),
  /**
   * The loaded match and its team state. Null on a server without a match or after a refused
   * config.
   */
  match: z.object({
    /**
     * The `match_id` the server was launched with. Null for a match without one. The launch's
     * config is the controller's to keep, so it is not echoed.
     */
    match_id: MatchId.nullable(),
    /** Where the match is. */
    phase: MatchPhase,
    /** Team 1's current side. */
    team1_side: PlayingSide,
    /**
     * Team 2's current side, always the other side, sent so a reader shows each team without
     * deriving it.
     */
    team2_side: PlayingSide,
    /** Team 1's score. */
    team1_score: z.number().int().nonnegative(),
    /** Team 2's score. */
    team2_score: z.number().int().nonnegative(),
  }).nullable(),
  /** Everyone connected, up to {@link MAX_SERVER_PLAYERS}. */
  players: z.array(z.object({
    /** The player's SteamID64. */
    steam_id: SteamId64,
    /** The player's name. */
    name: PlayerName,
    /** The player's side. */
    side: Side,
  })).max(MAX_SERVER_PLAYERS),
  /**
   * Why the plugin could not read or use the match config it was launched with, up to
   * {@link CONFIG_ERROR_MAX_CHARS} characters. Null when the server has no match config or the
   * plugin accepted it.
   */
  config_error: z.string().max(CONFIG_ERROR_MAX_CHARS).nullable(),
  /**
   * Whether players can join. When `true`, the server is logged on to Steam and has loaded the map
   * that its launch asked for, the workshop map when it has one. It stays `true` once set.
   *
   * CS2 does not fall back to an anonymous logon when Steam refuses its token, so `true` also
   * indicates that Steam accepted the GSLT and that its last use can be updated.
   */
  ready: z.boolean(),
  /**
   * The SteamID64 that Steam gave the server when it logged on with its GSLT. Each token keeps
   * one SteamID across logons, so a controller can look the server up in Steam's server list by
   * it. The plugin also adds the `allocation_id` without its dashes to `sv_tags`, which that list
   * shows. Null until `ready`.
   */
  server_steam_id: SteamId64.nullable(),
});

export type ServerState = z.infer<typeof ServerState>;

/** The server asked about, and its state as its plugin reports it now. */
export const GetServerStatusReply = ServerState.extend({
  /** The allocation the request asked about. */
  allocation_id: AllocationId,
});

export type GetServerStatusReply = z.infer<typeof GetServerStatusReply>;

/**
 * A match moved to another phase, as its plugin reports it, whether or not the match sends
 * statistics. An {@link EventBatch | event_batch} carries the events behind it, such as
 * {@link GoingLive | going_live} and {@link MapEnd | map_end}.
 *
 * The agent sends it once. If the plugin is not connected to the agent, or the controller has not
 * welcomed the agent's {@link AgentHello | hello}, it is lost, and
 * {@link ControllerGetServerStatus | get_server_status} reads the phase again.
 */
export const AgentPhaseChange = z.object({
  type: z.literal('phase_change'),
  /** The server whose match changed phase. */
  allocation_id: AllocationId,
  /** The phase the match is in now. */
  phase: MatchPhase,
});

export type AgentPhaseChange = z.infer<typeof AgentPhaseChange>;

/**
 * A match's score changed, as its plugin reads it off the scoreboard, whether or not the match
 * sends statistics: after a round, when going live resets it, or after a restore. An
 * {@link EventBatch | event_batch} carries the rounds behind it as {@link RoundEnd | round_end}
 * and {@link MatchRestored | match_restored}.
 *
 * Like {@link AgentPhaseChange | phase_change}, it is sent once, and
 * {@link ControllerGetServerStatus | get_server_status} reads the score again.
 */
export const AgentScore = z.object({
  type: z.literal('score'),
  /** The server whose match scored. */
  allocation_id: AllocationId,
  /** Team 1's score. */
  team1_score: z.number().int().nonnegative(),
  /** Team 2's score. */
  team2_score: z.number().int().nonnegative(),
});

export type AgentScore = z.infer<typeof AgentScore>;

/**
 * The {@link RoundBackup | round_backup} of a match that sends no statistics, which a match with
 * statistics carries in its {@link EventBatch | event_batch} instead. Keep the newest of each match
 * to restore it on a new server.
 *
 * The message wraps the complete event, unchanged, so code that reads a `round_backup` from the
 * event stream reads `backup` the same way. The wrapper adds only the server that sent it.
 *
 * Like {@link AgentPhaseChange | phase_change}, it is sent once, so a backup is lost if the plugin
 * or the agent is not connected when the round starts. The next round's backup replaces it.
 */
export const AgentBackup = z.object({
  type: z.literal('backup'),
  /** The server whose match the backup is of. */
  allocation_id: AllocationId,
  /** The {@link RoundBackup | round_backup} event, as the event stream would carry it. */
  backup: RoundBackup,
});

export type AgentBackup = z.infer<typeof AgentBackup>;

/**
 * What the controller sends:
 *
 * - The {@link ControllerWelcome | welcome} that accepts a {@link AgentHello | hello}, or the
 *   {@link ControllerHelloRefused | hello_refused} that refuses it.
 * - Requests, which carry an `id` and receive one {@link AgentReply | reply} each.
 * - The watches that start or renew a stream of logs or resource samples.
 * - {@link ControllerCheckCs2Build | check_cs2_build}, which receives no answer.
 */
export const ControllerToAgentMessage = z.discriminatedUnion('type', [
  ControllerStartServer,
  ControllerStopServer,
  ControllerGetServerStatus,
  ControllerUpdateCs2,
  ControllerCheckCs2Build,
  ControllerDeleteDemo,
  ControllerReserveSlot,
  ControllerSetConfig,
  ControllerUpdateAgent,
  ControllerUpdateCs2Image,
  ControllerWatchResources,
  ControllerWatchLogs,
  ControllerWelcome,
  ControllerHelloRefused,
  ControllerConsoleCommand,
  ControllerRestoreRound,
  ControllerStartMatch,
]);

export type ControllerToAgentMessage = z.infer<typeof ControllerToAgentMessage>;

/**
 * The `type` of each request the controller sends, which its {@link AgentReply | reply} repeats as
 * `request`.
 */
export const RequestType = z.enum([
  'start_server',
  'stop_server',
  'get_server_status',
  'update_cs2',
  'delete_demo',
  'reserve_slot',
  'set_config',
  'update_agent',
  'update_cs2_image',
  'console_command',
  'restore_round',
  'start_match',
]);

export type RequestType = z.infer<typeof RequestType>;

/**
 * What every successful reply carries. Each request's branch of {@link AgentReplySuccess} sets
 * `request` and `data`.
 */
const replySuccess = z.object({
  type: z.literal('reply'),
  /** The `id` of the request this answers. */
  reply_to: RequestId,
  /** The `type` of the request this answers, which says what `data` holds. */
  request: RequestType,
  /** The request succeeded. */
  ok: z.literal(true),
  /** What the request returns, which names the object it acted on. */
  data: z.unknown(),
});

/**
 * A request that succeeded. `request` names the request, so `data` can be read without the request
 * that `reply_to` answers. The settings in a {@link ControllerSetConfig | set_config} reply are the
 * node's, whichever edit asked.
 */
export const AgentReplySuccess = z.discriminatedUnion('request', [
  replySuccess.extend({ request: z.literal('start_server'), data: StartServerReply }),
  replySuccess.extend({ request: z.literal('stop_server'), data: StopServerReply }),
  replySuccess.extend({ request: z.literal('get_server_status'), data: GetServerStatusReply }),
  replySuccess.extend({ request: z.literal('update_cs2'), data: UpdateCs2Reply }),
  replySuccess.extend({ request: z.literal('delete_demo'), data: DeleteDemoReply }),
  replySuccess.extend({ request: z.literal('reserve_slot'), data: ReserveSlotReply }),
  replySuccess.extend({ request: z.literal('set_config'), data: NodeSettings }),
  replySuccess.extend({ request: z.literal('update_agent'), data: UpdateAgentReply }),
  replySuccess.extend({ request: z.literal('update_cs2_image'), data: UpdateCs2ImageReply }),
  replySuccess.extend({ request: z.literal('console_command'), data: ConsoleCommandReply }),
  replySuccess.extend({ request: z.literal('restore_round'), data: RestoreRoundReply }),
  replySuccess.extend({ request: z.literal('start_match'), data: StartMatchReply }),
]);

/** A request that failed. */
export const AgentReplyFailure = z.object({
  type: z.literal('reply'),
  /** The `id` of the request this answers. */
  reply_to: RequestId,
  /** The `type` of the request this answers. Null when the agent did not know it. */
  request: RequestType.nullable(),
  /** The request failed. */
  ok: z.literal(false),
  /** Why it failed. */
  error: AgentError,
});

/**
 * The answer to a request, in any order. A success carries the request's data, and a failure its
 * reason.
 */
export const AgentReply = z.discriminatedUnion('ok', [AgentReplySuccess, AgentReplyFailure]);

/** What a request comes to, without the correlation fields of the frame. */
export type AgentReplyOutcome = { ok: true; data: unknown } | { ok: false; error: AgentError };

/** What a successful reply to that request carries as `data`. */
export type ReplyData<T extends RequestType> = Extract<z.infer<typeof AgentReplySuccess>, { request: T }>['data'];

/**
 * What the agent sends: its {@link AgentHello | hello}, one {@link AgentReply | reply} to each
 * request, and reports of the node as it changes. Until a {@link ControllerWelcome | welcome} answers the
 * {@link AgentHello | hello}, only replies follow it, and a {@link AgentShutdown | shutdown} if the
 * agent stops.
 */
export const AgentToControllerMessage = z.discriminatedUnion('type', [
  AgentHello,
  AgentNodeStatus,
  AgentShutdown,
  AgentSlotStatus,
  AgentServerExited,
  AgentLogBatch,
  AgentLogIndex,
  AgentCs2UpdateProgress,
  AgentDemo,
  AgentDemoRemoved,
  AgentDemoProgress,
  AgentResourceBatch,
  AgentEventBacklog,
  AgentPhaseChange,
  AgentScore,
  AgentBackup,
  AgentReply,
]);

export type AgentToControllerMessage = z.infer<typeof AgentToControllerMessage>;
