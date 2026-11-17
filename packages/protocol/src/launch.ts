/**
 * What a game server is launched with: the configuration that the controller sends the agent in
 * {@link ControllerStartServer | start_server}. The agent's `container.ts` defines what the agent hands
 * the container and the plugin inside it.
 */
import { z } from 'zod';
import type {
  AgentBackup, AgentPhaseChange, AgentScore, AgentSlotStatus, ServerExitReason, ControllerGetServerStatus, ControllerStartMatch, ControllerStartServer, MATCH_BLOCKED_COMMANDS,
} from './agent-socket.js';
import type { EventBatch } from './event-batch.js';
import {
  type GoingLive,
  type MapEnd,
  MatchId,
  MatchRules,
  MAX_SERVER_PLAYERS,
  RestoreFile,
  type RoundBackup,
  Roster,
} from './game-events.js';

/**
 * How starting sides are decided.
 *
 * - `knife`: a knife round decides them.
 * - `team1_ct`: team 1 starts as CT.
 * - `team1_t`: team 1 starts as T.
 */
export const SidesMode = z.enum(['knife', 'team1_ct', 'team1_t']);

export type SidesMode = z.infer<typeof SidesMode>;

/**
 * How warmup ends.
 *
 * - `auto`: counts down once every rostered player is connected. Players have no ready commands.
 * - `manual`: also waits for each rostered player and each rostered spectator to type `!ready`,
 *   which `!unready` withdraws. With `allow_force_ready`, a captain's `!forceready` readies the
 *   captain's team with whichever of its players are connected, and any rostered spectator's
 *   `!forceready` readies the spectators.
 * - `external`: only {@link ControllerStartMatch | start_match} from the controller starts the match, and
 *   players have no commands for it.
 *
 * In every mode, {@link ControllerStartMatch | start_match} from the controller starts the match.
 */
export const ReadyMode = z.enum(['auto', 'manual', 'external']);

export type ReadyMode = z.infer<typeof ReadyMode>;

/**
 * {@link MatchRules} as a launch asks for them, where each rule left out takes the value of a
 * typical competitive match. The reports of {@link GoingLive | going_live} and
 * {@link RoundBackup | round_backup} always carry every rule.
 */
export const LaunchRules = MatchRules.extend({
  /** Rounds in regulation, as `mp_maxrounds`. */
  max_rounds: MatchRules.shape.max_rounds.default(24),
  /** Whether a tie after regulation goes to overtime, as `mp_overtime_enable`. */
  overtime: MatchRules.shape.overtime.default(true),
  /** Rounds in one overtime, both halves together, as `mp_overtime_maxrounds`. */
  overtime_max_rounds: MatchRules.shape.overtime_max_rounds.default(6),
  /** Whether bullets and melee hurt teammates. Grenades always do. */
  friendly_fire: MatchRules.shape.friendly_fire.default(true),
});

export type LaunchRules = z.infer<typeof LaunchRules>;

/**
 * The match a server is launched with, which covers one map. Each server plays one match. A restore
 * launches the same `match_id` again under a new `allocation_id`, from a
 * {@link RoundBackup | round_backup} that the first server sent.
 */
export const MatchConfig = z.object({
  /**
   * The match's id, which names its demos and which {@link AgentSlotStatus | slot_status} and
   * {@link ControllerGetServerStatus | get_server_status} report. Null for none.
   */
  match_id: MatchId.nullable().default(null),
  /**
   * Whether the plugin sends the match's gameplay events, its {@link RoundBackup | round_backup}
   * among them, which the agent uploads in an {@link EventBatch | event_batch}.
   *
   * Every match reports its {@link AgentPhaseChange | phase_change} and {@link AgentScore | score}.
   * A match without statistics sends each {@link RoundBackup | round_backup} in
   * {@link AgentBackup | backup} instead, and a match with statistics only in its events.
   */
  send_statistics: z.boolean().default(false),
  /** Who may join, and on which team. */
  roster: Roster,
  /** How starting sides are decided. */
  sides: SidesMode,
  /**
   * Seconds the knife winners' captain has to type `!stay`, `!swap`, `!ct` or `!t`, from 5 to 300.
   * The server stays in warmup meanwhile, with its clock counting these seconds down. An answer
   * starts the match immediately. Without one, the team stays on its side when the clock runs out.
   */
  knife_decision_seconds: z.number().int().min(5).max(300).default(60),
  /** How warmup ends. */
  ready_mode: ReadyMode.default('auto'),
  /**
   * Whether, under `manual`, the match also waits for every rostered spectator to connect and type
   * `!ready`, and any of them can force-ready the spectators.
   */
  spectators_ready_up: z.boolean().default(false),
  /** Whether, under `manual`, captains and rostered spectators may type `!forceready`. */
  allow_force_ready: z.boolean().default(false),
  /**
   * Seconds from warmup until both teams must be ready, up to 1800. Null for no time limit.
   *
   * When the time passes, the server kicks every player and the map ends with
   * {@link MapEnd | map_end}, which says who was connected and ready. The controller decides what
   * that means for the match. Must be null with `external`, where only the controller starts the
   * match.
   */
  ready_timeout_seconds: z.number().int().positive().max(1800).nullable().default(null),
  /**
   * Whether GOTV records the live match as a demo.
   */
  record_demo: z.boolean(),
  /**
   * Whether the plugin records a development capture of its callbacks. Only a match that sends
   * statistics records one.
   *
   * @internal
   */
  debug_capture: z.boolean().default(false),
  /** The rules the match is played under, applied over those of the game mode. */
  rules: LaunchRules.prefault({}),
  /**
   * A live round to play on from, taken from the {@link RoundBackup | round_backup} that reported
   * it. Null for a match that plays the map from its start.
   *
   * With a restore, the server skips the knife round and the side choice whatever `sides` says, and
   * after warmup it loads the backup instead of going live. Team 1 takes the restore's
   * `team1_side`, the round and the scores come from the file, and the restore's `rules` replace
   * the match's `rules`. The agent writes `file` to the slot for `mp_backup_restore_load_file`.
   */
  restore: RestoreFile.nullable().default(null),
});

export type MatchConfig = z.infer<typeof MatchConfig>;

/** A match as a controller builds it, which may leave out the fields that have a default. */
export type MatchConfigInput = z.input<typeof MatchConfig>;

/** Control characters, which a launch value must not carry into generated cfg commands. */
export const CFG_CONTROL_CHARS = /[\r\n\u0000-\u001f\u007f]/;

const CFG_TEXT_MESSAGE =
  'must not contain control characters: a newline would end the console line and the rest would ' +
  'be parsed as another command';

const cfgText = z.string().refine((value) => !CFG_CONTROL_CHARS.test(value), CFG_TEXT_MESSAGE);

/**
 * The password players type. It is printable ASCII without spaces, `"` or `;`, because the server
 * would read those as separators.
 */
export const GamePassword = z.string().max(128).regex(/^[!#-:<-~]*$/, 'must be printable ASCII without spaces, quotes or semicolons');

/** A Steam Web API key, which the server needs to download workshop maps. */
export const SteamWebApiKey = z.string().regex(/^[A-Za-z0-9]{0,64}$/, 'a Steam Web API key is letters and digits');

/** The name of a stock map, such as `de_mirage`. A name of all digits would be a workshop id. */
export const StockMapName = z.string().regex(/^(?!\d+$)[A-Za-z0-9_-]{1,64}$/, 'must be a stock map name');

/** A Steam Workshop map's id. */
export const WorkshopId = z.string().regex(/^\d{1,20}$/, 'must be a numeric workshop id');


// The plugin's ChatTagTests mirrors this list by hand; change both together.
/** The colour names SwiftlyS2 accepts in chat markup. */
export const CHAT_COLORS = [
  'white', 'darkred', 'lightpurple', 'green', 'olive', 'lime', 'red', 'gray', 'lightyellow',
  'yellow', 'silver', 'bluegrey', 'lightblue', 'blue', 'darkblue', 'purple', 'magenta', 'lightred',
  'gold', 'orange',
] as const;

/**
 * A chat colour name from
 * [SwiftlyS2's chat styling](https://swiftlys2.net/docs/guides/chat-and-html-styling).
 */
export const ChatColor = z.enum(CHAT_COLORS);

export type ChatColor = z.infer<typeof ChatColor>;

/** The bracketed tag in front of every plugin chat line. */
export const ChatTag = z.object({
  /**
   * The tag's text, without brackets or control characters. Brackets would end the tag early in
   * SwiftlyS2's colour markup.
   */
  text: z.string().min(1).max(24).regex(/^[^[\]\p{Cc}]+$/u, 'must not contain brackets or control characters').default('Ankor'),
  /** The tag's colour. */
  color: ChatColor.default('blue'),
});

export type ChatTag = z.infer<typeof ChatTag>;

/**
 * A cfg the server runs, relative to `game/csgo/cfg/` and without `.cfg`, such as `node/<name>`
 * from the node's cfgs.
 */
export const CfgName = z.string().max(128).regex(/^[A-Za-z0-9_-]+(\/[A-Za-z0-9_-]+)*$/, 'must be cfg path segments of letters, digits, `_` and `-`, without `.cfg`');

/**
 * The game mode a server runs, which sets the game's rules. The engine runs the mode's cfg, such
 * as `gamemode_competitive` or `gamemode_competitive2v2` for `wingman`, on every map load, and the
 * plugin runs it again when a match goes live.
 *
 * `custom` is Valve's mode for community content. Its cfg sets little, so the launch's `exec` and
 * `commands` set the game.
 *
 * A match plays only in `casual`, `competitive` or `wingman`. A server without a match runs in any
 * mode.
 */
export const GameMode = z.enum(['casual', 'competitive', 'wingman', 'rush', 'arms_race', 'demolition', 'deathmatch', 'custom']);

export type GameMode = z.infer<typeof GameMode>;

/** The player capacity of a server. */
const MaxPlayers = z.number().int().min(1).max(MAX_SERVER_PLAYERS);

/** The game modes that a match plays in: Counter-Strike's own, whose rounds end. */
const MATCH_GAME_MODES: readonly GameMode[] = ['casual', 'competitive', 'wingman'];

/**
 * What a game server is launched with: any CS2 server, and the match Ankor's plugin plays on it, if
 * any.
 *
 * Rejected when both `workshop_id` and `map_name` are null. With a `match`, it is also rejected:
 *
 * - when `workshop_id` is set, because a match plays a stock map.
 * - when the roster's players and spectators together outnumber `max_players`.
 * - when `game_mode` is not `casual`, `competitive` or `wingman`.
 * - when `ready_mode` is `external` and `ready_timeout_seconds` is set.
 */
export const ServerLaunchConfig = z.object({
  /**
   * The server's name, which players see in game, such as on the scoreboard. It is up to 128
   * characters. Null or empty leaves the game's own name. Rejected when it holds an ASCII control
   * character, such as a line break.
   */
  hostname: cfgText.pipe(z.string().max(128)).nullable().default(null),
  /**
   * The Steam Web API key that workshop downloads need. Null or empty skips them. **Whoever runs
   * the node can read the key.**
   */
  steam_web_api_key: SteamWebApiKey.nullable().default(null),
  /** The password players type to join. Null or empty for none. */
  game_password: GamePassword.nullable().default(null),
  /** The tag in front of the plugin's chat lines. */
  chat_tag: ChatTag.prefault({}),
  /** The game mode, which sets the game's rules. */
  game_mode: GameMode.default('competitive'),
  /**
   * The player capacity of the server, as `-maxplayers`. A match's roster, players and spectators
   * together, must fit.
   */
  max_players: MaxPlayers,
  /** A workshop map to play. Null for a stock map. A match plays a stock map. */
  workshop_id: WorkshopId.nullable().default(null),
  /**
   * The stock map to play, required without `workshop_id`. With a `workshop_id`, the server boots
   * this map before it switches to the workshop map. Null for `de_dust2` in that case.
   */
  map_name: StockMapName.nullable(),
  /**
   * GOTV that spectators can watch. Null for none. A match that records a demo still runs GOTV
   * then, because demos are recorded through it. That GOTV runs with `tv_delay 0` and a random
   * `tv_password` that the agent sends nowhere, so no spectator can join.
   *
   * Whether GOTV runs is set on the command line, because GOTV starts with the map. The other
   * settings apply on every map load and when a match goes live, never while a demo records.
   */
  gotv: z.object({
    /**
     * The GOTV password, as `tv_password`, apart from `game_password`. Null or empty for none.
     */
    password: GamePassword.nullable().default(null),
    /** Seconds GOTV runs behind the game. */
    delay_seconds: z.number().int().nonnegative(),
  }).nullable().default(null),
  /**
   * Seconds the server may stay empty once its slot is `running` before the agent stops it,
   * reporting `idle_empty` as its {@link ServerExitReason}. Null for a server that runs until
   * stopped.
   * Hibernation must stay off. See [When the cfgs run](/protocol/#when-the-cfgs-run).
   */
  idle_shutdown_seconds: z.number().int().positive().nullable(),
  /**
   * Cfgs the server runs in order, after the game mode's cfg and the match's `rules`. A server
   * without a match runs them 1 second after every map load, and a match server runs them at warmup
   * and when the match goes live. Warmup and the knife round run Ankor's cfgs over them.
   */
  exec: z.array(CfgName).max(16),
  /**
   * Console lines that the server runs after `exec`, in order and at the same moments, so they
   * override `exec` and `rules`. A match server leaves out a line that names a command of
   * {@link MATCH_BLOCKED_COMMANDS}. Rejected when a line holds an ASCII control character, such as
   * a line break, which would end the console line. See
   * [When the cfgs run](/protocol/#when-the-cfgs-run).
   */
  commands: z.array(cfgText.pipe(z.string().min(1).max(256))).max(128),
  /**
   * The match Ankor's plugin plays. Null runs a plain CS2 server. The plugin then runs no match,
   * and the map, `exec`, `commands` and the node's plugins set the game.
   */
  match: MatchConfig.nullable().default(null),
}).superRefine((launch, ctx) => {
  if (launch.workshop_id === null && launch.map_name === null) {
    ctx.addIssue({ code: 'custom', message: 'a launch without a workshop id names its stock map', path: ['map_name'] });
  }

  if (launch.match === null) {
    return;
  }

  if (launch.workshop_id !== null) {
    ctx.addIssue({ code: 'custom', message: 'a match plays a stock map', path: ['workshop_id'] });
  }

  const { team1, team2, spectators } = launch.match.roster;

  if (team1.players.length + team2.players.length + spectators.length > launch.max_players) {
    ctx.addIssue({ code: 'custom', message: 'the roster, players and spectators, must fit in `max_players`', path: ['max_players'] });
  }

  if (!MATCH_GAME_MODES.includes(launch.game_mode)) {
    ctx.addIssue({ code: 'custom', message: 'a match needs `casual`, `competitive` or `wingman`', path: ['game_mode'] });
  }

  if (launch.match.ready_mode === 'external' && launch.match.ready_timeout_seconds !== null) {
    ctx.addIssue({ code: 'custom', message: '`ready_timeout_seconds` must be null with an `external` ready-up, because the controller starts the match', path: ['match', 'ready_timeout_seconds'] });
  }
});

export type ServerLaunchConfig = z.infer<typeof ServerLaunchConfig>;

/** A launch as a controller builds it, which may leave out the fields that have a default. */
export type ServerLaunchConfigInput = z.input<typeof ServerLaunchConfig>;
