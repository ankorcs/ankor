/**
 * The messages of the agent's local plugin socket: sequenced events and status from the plugin, and
 * commands and acknowledgements from the agent.
 */
import { z } from 'zod';
import {
  AGENT_ERRORS,
  type AgentErrorCode,
  AgentBackup,
  AgentPhaseChange,
  AgentScore,
  ServerState,
  type ControllerConsoleCommand,
  type ControllerRestoreRound,
  type ControllerStartMatch,
  type MATCH_BLOCKED_COMMANDS,
} from './index.js';
import { EventBatchFrame, EventSeq, RoundNumber } from './index.js';
import type { EVENT_BATCH_MAX_BYTES, EventBatch, QUIT_TIMEOUT_SECONDS } from './index.js';
import type { MAX_FRAME_BYTES, RELAY_QUEUE_MAX_BYTES, RELAY_QUEUE_MAX_FRAMES } from './container.js';

/**
 * One sequenced event, which the plugin sends and the agent spools without its `type`, then uploads
 * as an {@link EventBatchFrame} of an {@link EventBatch | event_batch}. `seq` runs from 0 without gaps.
 * The frame names no server, because the connection's key already proves which one it is.
 *
 * A frame carries at most {@link MAX_FRAME_BYTES}, which is no more than
 * {@link EVENT_BATCH_MAX_BYTES}, so every event the agent accepts fits an upload batch once stored.
 * The plugin drops a larger event before sending it, and the agent ends a socket that sends one.
 *
 * After a gap past the events it stored, or with more than {@link RELAY_QUEUE_MAX_FRAMES} events or
 * {@link RELAY_QUEUE_MAX_BYTES} of them waiting to be stored, the agent accepts no more history and
 * ends the socket with code `1000` once the rest is stored. The plugin reconnects and resends from its last
 * {@link EventAck | ack}.
 */
export const PluginEventFrame = EventBatchFrame.extend({
  type: z.literal('event'),
});

export type PluginEventFrame = z.infer<typeof PluginEventFrame>;

/**
 * Acknowledges that every frame with `seq <= to_seq` is stored on the node, so the plugin may
 * discard them.
 *
 * The agent sends it as frames are stored, and on every connect once the server has events on the
 * node, so a reconnecting plugin resends only what follows. An `ack` past the last frame the plugin
 * sent breaks the stream, and the plugin drops the connection.
 */
export const EventAck = z.object({
  type: z.literal('ack'),
  to_seq: EventSeq,
});

/**
 * A game server's state, which the plugin sends in answer to every
 * {@link PluginGetServerStatus | get_server_status}. The plugin also sends it unasked once when it
 * connects and once when it turns `ready`.
 */
export const ServerStatus = ServerState.extend({
  type: z.literal('server_status'),
  /**
   * The `request_id` of the {@link PluginGetServerStatus | get_server_status} this answers. Null
   * when the request had none or the status was sent unasked.
   */
  request_id: z.string().max(64).nullable(),
});

export type ServerStatus = z.infer<typeof ServerStatus>;

/**
 * Tells the agent that the plugin is about to run `quit` after its match, with every event
 * acknowledged and the demo finished. The agent stops a server that is still running
 * {@link QUIT_TIMEOUT_SECONDS} later. Like {@link ServerStatus | server_status}, it is not resent,
 * so one sent without a connection is lost.
 */
export const Quitting = z.object({
  type: z.literal('quitting'),
});

export type Quitting = z.infer<typeof Quitting>;

/**
 * The match moved to another phase, which the plugin sends whether or not the match sends
 * statistics, and the agent forwards as {@link AgentPhaseChange | phase_change}. Like
 * {@link ServerStatus | server_status}, it is sent once and lost without a connection.
 */
export const PluginPhaseChange = AgentPhaseChange.omit({ allocation_id: true });

export type PluginPhaseChange = z.infer<typeof PluginPhaseChange>;

/**
 * The match's score changed, which the plugin sends like {@link PluginPhaseChange | phase_change},
 * and the agent forwards as {@link AgentScore | score}.
 */
export const PluginScore = AgentScore.omit({ allocation_id: true });

export type PluginScore = z.infer<typeof PluginScore>;

/**
 * The `round_backup` of a match that sends no statistics, which the plugin
 * sends like {@link PluginPhaseChange | phase_change}, and the agent forwards as
 * {@link AgentBackup | backup}. `backup` holds the complete event, unchanged, as the
 * event stream would carry it.
 */
export const PluginBackup = AgentBackup.omit({ allocation_id: true });

export type PluginBackup = z.infer<typeof PluginBackup>;

/** Why a plugin did not carry out a command, as an {@link AgentErrorCode}. */
export const CommandRefusal = z.enum([
  AGENT_ERRORS.noMatch, AGENT_ERRORS.notInWarmup, AGENT_ERRORS.teamEmpty,
  AGENT_ERRORS.noBackup, AGENT_ERRORS.notLive, AGENT_ERRORS.atHalftime, AGENT_ERRORS.betweenRounds,
  AGENT_ERRORS.commandBlocked, AGENT_ERRORS.unsupportedRequest,
]);

/** The plugin's answer to a command that carries a `request_id`. */
export const CommandResult = z.object({
  type: z.literal('command_result'),
  /** The `request_id` of the command this answers. */
  request_id: z.string().max(64),
  /** Why the plugin refused the command. Null when the plugin carried it out. */
  error: CommandRefusal.nullable(),
});

export type CommandResult = z.infer<typeof CommandResult>;

/**
 * What a plugin sends its agent: sequenced {@link PluginEventFrame | event} frames, its
 * {@link ServerStatus | server_status}, {@link PluginPhaseChange | phase_change},
 * {@link PluginScore | score}, {@link PluginBackup | backup},
 * {@link CommandResult | command_result} and {@link Quitting | quitting}.
 */
export const PluginToAgentMessage = z.discriminatedUnion('type', [PluginEventFrame, ServerStatus, PluginPhaseChange, PluginScore, PluginBackup, CommandResult, Quitting]);

export type PluginToAgentMessage = z.infer<typeof PluginToAgentMessage>;

/**
 * Asks the plugin for its server's state, which the plugin answers with a
 * {@link ServerStatus | server_status}.
 */
export const PluginGetServerStatus = z.object({
  type: z.literal('get_server_status'),
  /**
   * An id that the answering {@link ServerStatus | server_status} carries back, to match it to this
   * request. Null for none.
   */
  request_id: z.string().max(64).nullable(),
});

/**
 * Runs a server console command. The plugin answers with a {@link CommandResult | command_result}
 * once it has handed the command to the engine, or refuses a line that names a command of
 * {@link MATCH_BLOCKED_COMMANDS} while it runs a match.
 */
export const ExecCommand = z.object({
  type: z.literal('exec'),
  /** The console command line. */
  command: z.string(),
  /** An id that the answering {@link CommandResult | command_result} carries back. */
  request_id: z.string().max(64),
});

/**
 * Loads the server's backup of a round, as {@link ControllerRestoreRound | restore_round} from the controller
 * asks. The plugin runs `mp_backup_restore_load_file` on the file the engine wrote for it
 * and answers with a {@link CommandResult | command_result}.
 */
export const PluginRestoreRound = z.object({
  type: z.literal('restore_round'),
  /** The round to play on from, one-based. */
  round: RoundNumber,
  /** An id that the answering {@link CommandResult | command_result} carries back. */
  request_id: z.string().max(64),
});

/**
 * Starts a match in warmup whatever its `ready_mode`, as {@link ControllerStartMatch | start_match} from
 * the controller asks. The plugin answers with a {@link CommandResult | command_result}.
 */
export const PluginStartMatch = z.object({
  type: z.literal('start_match'),
  /** An id that the answering {@link CommandResult | command_result} carries back. */
  request_id: z.string().max(64),
});

/**
 * What the agent asks of a game server.
 *
 * - `exec`: relays {@link ControllerConsoleCommand | console_command} from the controller.
 * - `restore_round`: relays {@link ControllerRestoreRound | restore_round} from the controller.
 * - `start_match`: relays {@link ControllerStartMatch | start_match} from the controller.
 * - `get_server_status`: the agent's own request.
 */
export const PluginCommand = z.discriminatedUnion('type', [ExecCommand, PluginRestoreRound, PluginStartMatch, PluginGetServerStatus]);

export type PluginCommand = z.infer<typeof PluginCommand>;

type WithoutRequestId<T> = T extends unknown ? Omit<T, 'request_id'> : never;

/**
 * A command the plugin answers with a {@link CommandResult | command_result}, before the agent
 * gives it a `request_id`.
 */
export type AnsweredCommand = WithoutRequestId<Extract<PluginCommand, { type: 'exec' | 'start_match' | 'restore_round' }>>;

/**
 * What the agent sends a plugin: commands, and an {@link EventAck | ack} once events are stored on
 * the node.
 *
 * When the agent ends a plugin's socket, it sends a close frame whose reason says why: `1001` when
 * the agent shuts down, and `1000` when the plugin must replay its history, when a newer connection
 * of the same server takes its place, or when the server's launch has ended. The plugin reconnects
 * after each and resends from its last {@link EventAck | ack}.
 */
export const AgentToPluginMessage = z.discriminatedUnion('type', [...PluginCommand.options, EventAck]);

export type AgentToPluginMessage = z.infer<typeof AgentToPluginMessage>;
