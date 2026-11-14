/**
 * Plugin events use decimal-string SteamIDs, numeric roster teams and `t`, `ct` and `spec` sides.
 *
 * `round` is one-based, `timestamp` is Unix milliseconds, and `round_time` is seconds derived from
 * the engine, rounded to milliseconds. `tick` is the server tick when the event happened, as the
 * engine's map tick plus its sub-tick fraction. `seq` is the order in which the events were sent.
 */
import { z } from 'zod';
import type { ControllerGetServerStatus, ControllerRestoreRound } from './agent-socket.js';

/** A SteamID64 as 17 decimal digits. */
export const SteamId64 = z.string().regex(/^\d{17}$/, 'steamid64');

/**
 * The side a player is on.
 *
 * - `t`: terrorists, a playing side.
 * - `ct`: counter-terrorists, a playing side.
 * - `spec`: spectators and players on no team.
 */
export const Side = z.enum(['t', 'ct', 'spec']);

export type Side = z.infer<typeof Side>;

/**
 * A playing side, for a field that never holds `spec`.
 *
 * - `t`: terrorists.
 * - `ct`: counter-terrorists.
 */
export const PlayingSide = Side.exclude(['spec']);

export type PlayingSide = z.infer<typeof PlayingSide>;

/** A team of the {@link Roster}, team 1 or team 2, whichever side it plays. */
export const TeamNumber = z.union([z.literal(1), z.literal(2)]);

export type TeamNumber = z.infer<typeof TeamNumber>;

/**
 * The id of one map's match, which the controller chooses. A series of maps is a grouping that the
 * controller keeps. The id is a lowercase UUID, because demo file names carry it and case matters
 * there.
 */
export const MatchId = z.uuid().regex(/^[^A-F]*$/, 'must be lowercase');

/**
 * Why a round ended, as the engine names it. When the engine gives a reason that this list does not
 * name, the plugin sends `unknown`.
 */
export const RoundEndReason = z.enum({
  unknown: 'unknown',
  /** The bomb exploded. */
  target_bombed: 'target_bombed',
  terrorists_escaped: 'terrorists_escaped',
  cts_prevent_escape: 'cts_prevent_escape',
  escaping_terrorists_neutralized: 'escaping_terrorists_neutralized',
  /** The bomb was defused. */
  bomb_defused: 'bomb_defused',
  /** Every T was killed. */
  cts_win: 'cts_win',
  /** Every CT was killed. */
  terrorists_win: 'terrorists_win',
  round_draw: 'round_draw',
  all_hostages_rescued: 'all_hostages_rescued',
  /** Time ran out without the bomb planted. */
  target_saved: 'target_saved',
  hostages_not_rescued: 'hostages_not_rescued',
  terrorists_not_escaped: 'terrorists_not_escaped',
  game_commencing: 'game_commencing',
  /** The Ts voted to surrender. The map ends after this round. */
  terrorists_surrender: 'terrorists_surrender',
  /** The CTs voted to surrender. The map ends after this round. */
  cts_surrender: 'cts_surrender',
  terrorists_planted: 'terrorists_planted',
  cts_reached_hostage: 'cts_reached_hostage',
  survival_win: 'survival_win',
  survival_draw: 'survival_draw',
});

export type RoundEndReason = z.infer<typeof RoundEndReason>;

/** A UTF-16 surrogate without its other half, which no text encoding can store. */
const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/;

/** Text of up to `max` characters that any store can hold, so no NUL and no lone surrogate. */
function wireText(max: number) {
  return z.string().max(max).refine((text) => !text.includes('\u0000') && !LONE_SURROGATE.test(text), 'must be text without NUL or lone surrogates');
}

/** Spectator slots on a CS2 server, for casters and anyone else who watches from the server. */
export const MAX_SPECTATORS = 2;

/** Players that a CS2 server holds. The players and spectators of a roster fit in it together. */
export const MAX_SERVER_PLAYERS = 64;

/** Characters in the longest team name. */
export const MAX_TEAM_NAME_LENGTH = 30;

/**
 * The most rounds that `mp_maxrounds` can set for a match, ten times the stock 24. A launch that
 * asks for more fails validation.
 */
export const MAX_ROUNDS = 240;

/*
 * Bounds on gameplay values. Ankor runs competitive Counter-Strike: where a game setting governs a
 * value, the range that setting allows is its bound; where none does, about ten times the stock
 * value.
 */

/** Unix milliseconds from 2020-01-01 to 2100-01-01. */
const Timestamp = z.number().int().min(1_577_836_800_000).max(4_102_444_800_000);

/** A one-based round number. A map's history holds far fewer than the 1000 it allows. */
export const RoundNumber = z.number().int().positive().max(1000);

/**
 * Seconds within one round. The bound covers the 60 minutes that `mp_roundtime` allows, the 90
 * seconds of `mp_c4timer` after a late plant and a few more of `mp_round_restart_delay`.
 */
const RoundSeconds = z.number().nonnegative().max(4000);

/** Rounds won by a team, bounded like a {@link RoundNumber}. */
const Score = z.number().int().nonnegative().max(1000);

/** The most dollars that `mp_maxmoney` allows. */
const MAX_CASH = 65_535;

/** Dollars in hand, up to what `mp_maxmoney` allows. */
const Cash = z.number().int().nonnegative().max(MAX_CASH);

/**
 * The highest equipment value, in dollars. The engine keeps a pawn's equipment value as a 16-bit
 * unsigned integer, which holds over ten times a stock loadout.
 */
const MAX_EQUIPMENT_VALUE = 65_535;

/**
 * Dollars of equipment that one player carries, the engine's figure. A stock loadout comes to about
 * 10,000.
 */
const EquipmentValue = z.number().int().nonnegative().max(MAX_EQUIPMENT_VALUE);

/** The most health or armor that a player can have, ten times the stock 100. */
export const MAX_HEALTH = 1000;

/** Points of health or armor, up to {@link MAX_HEALTH}. */
const Health = z.number().int().nonnegative().max(MAX_HEALTH);

/** Seconds that a flashbang blinds for. A point-blank flashbang blinds for about 5. */
const BlindSeconds = z.number().nonnegative().max(10);

/**
 * The most damage that one hit reports, equal to {@link MAX_HEALTH}, so only overkill past that is
 * cut.
 */
export const MAX_DAMAGE = MAX_HEALTH;

/** Damage that one hit deals, overkill included, up to {@link MAX_DAMAGE}. */
const Damage = z.number().int().nonnegative().max(MAX_DAMAGE);

/** Engine units along one axis. Maps lie within ±16,384. */
const WorldUnits = z.number().min(-32_768).max(32_768);

/** Characters in the longest player name. Steam allows 32, which UTF-16 writes in at most 64. */
export const MAX_PLAYER_NAME_CHARS = 64;

/**
 * A player's name, cut to up to {@link MAX_PLAYER_NAME_CHARS} characters, without control
 * characters. Rejected when it holds a lone UTF-16 surrogate.
 */
export const PlayerName = wireText(MAX_PLAYER_NAME_CHARS).regex(/^[^\p{Cc}]*$/u, 'must not contain control characters');

/** Characters in the longest version string that {@link MapStart | map_start} carries. */
export const MAX_VERSION_CHARS = 128;

/** Characters in the longest `reason` of a {@link PlayerDisconnect | player_disconnect}. */
export const MAX_DISCONNECT_REASON_CHARS = 64;

/** Characters in the longest `message` of a {@link PluginLog | log}. */
export const MAX_LOG_MESSAGE_CHARS = 8192;

/**
 * The most shots that one player fires in a round, over all their weapons. It is about ten times
 * the most ammunition that a stock weapon carries, the Negev's 450. A
 * {@link RoundShots | round_shots} with more fails validation, so the plugin counts no more.
 */
export const MAX_ROUND_SHOTS = 4096;

/** Whether no two entries name the same player. */
function uniquePlayers<T extends { player: { steam_id: string } }>(players: readonly T[]): boolean {
  return new Set(players.map(({ player }) => player.steam_id)).size === players.length;
}

/** One team of a {@link Roster}. */
export const RosterTeam = z.object({
  /**
   * The team name that players see, up to {@link MAX_TEAM_NAME_LENGTH} characters. Rejected when it
   * is blank, or when it holds a NUL or a lone UTF-16 surrogate.
   */
  name: wireText(MAX_TEAM_NAME_LENGTH).regex(/\S/, 'must not be blank'),
  /**
   * The players of the team, captain first. The captain may use `!forceready` and picks the side
   * after the knife round. A team has at least one player, so it always has a captain.
   */
  players: z.array(SteamId64).min(1),
});

export type RosterTeam = z.infer<typeof RosterTeam>;

/**
 * The teams and spectators of a match. Each player holds one place on the server, whichever team or
 * list the player is in.
 *
 * Rejected when a SteamID appears twice across both teams and `spectators`, or when they hold more
 * than {@link MAX_SERVER_PLAYERS} together.
 */
export const Roster = z.object({
  /** Team 1, which events number `1`. */
  team1: RosterTeam,
  /** Team 2, which events number `2`. */
  team2: RosterTeam,
  /**
   * Players allowed in as spectators, who watch with the game's free camera, up to
   * {@link MAX_SPECTATORS}. When `ready_mode` is `manual`, they ready up too.
   */
  spectators: z.array(SteamId64).max(MAX_SPECTATORS),
}).superRefine((roster, ctx) => {
  const ids = [...roster.team1.players, ...roster.team2.players, ...roster.spectators];

  if (new Set(ids).size !== ids.length) {
    ctx.addIssue({ code: 'custom', message: 'a SteamID appears more than once in the roster' });
  }

  if (ids.length > MAX_SERVER_PLAYERS) {
    ctx.addIssue({ code: 'custom', message: `a roster holds at most ${MAX_SERVER_PLAYERS} players and spectators` });
  }
});

export type Roster = z.infer<typeof Roster>;

const base = {
  /** Unix milliseconds on the server's wall clock. */
  timestamp: Timestamp,
  /**
   * The server tick when the event happened, as the engine's map tick plus its sub-tick fraction.
   *
   * The plugin sends events as they happen, so `seq` follows the same order.
   * {@link RoundShots | round_shots} is the exception, because the plugin sends it when the round
   * closes, and the `ticks` of each weapon place each shot.
   */
  tick: z.number().finite().nonnegative().max(Number.MAX_SAFE_INTEGER),
};

const roundScoped = {
  ...base,
  /** The round that the event happened in, even when the event arrives later. */
  round: RoundNumber,
  /** Seconds since the round started. */
  round_time: RoundSeconds,
};

/** A player as the event saw them. */
const PlayerRef = z.object({
  /** The player's SteamID64. */
  steam_id: SteamId64,
  /** The player's name when the event happened. */
  name: PlayerName,
  /** The side that the player was on. */
  side: Side,
  /**
   * The roster team of the player, which the plugin looks up in its roster. Null for spectators and
   * unknown players.
   */
  team: TeamNumber.nullable(),
});

export type PlayerRef = z.infer<typeof PlayerRef>;

/** A point in the world, in engine units. */
export const WorldPos = z.object({
  /** Engine units along the map's x axis. */
  x: WorldUnits,
  /** Engine units along the map's y axis. */
  y: WorldUnits,
  /** Engine units along the map's z axis, the height. */
  z: WorldUnits,
});

export type WorldPos = z.infer<typeof WorldPos>;

/** Where a player stood, as a {@link WorldPos}, and where the player looked. */
export const PlayerPosition = WorldPos.extend({
  /** Degrees of eye pitch. A positive pitch looks down. */
  pitch: z.number().min(-90).max(90),
  /** Degrees of eye yaw, counter-clockwise from the +x axis. */
  yaw: z.number().min(-360).max(360),
});

export type PlayerPosition = z.infer<typeof PlayerPosition>;

/** A {@link PlayerRef} with the position of the player when the event happened. */
const PlayerAt = PlayerRef.extend({
  /** Where the player stood and looked. Null when the player had no body in the world. */
  position: PlayerPosition.nullable(),
});

export type PlayerAt = z.infer<typeof PlayerAt>;

/**
 * A {@link PlayerRef} with the position of the player, for a player who acted in the world when the
 * event happened, so the position is always known.
 */
const PlacedPlayer = PlayerRef.extend({
  /** Where the player stood and looked. */
  position: PlayerPosition,
});

export type PlacedPlayer = z.infer<typeof PlacedPlayer>;

/**
 * Sent when a map has loaded on the server, as the first event of the map's history.
 *
 * The plugin sends one on every load. A workshop map loads twice, and an admin's `changelevel`
 * loads another map.
 */
export const MapStart = z.object({
  type: z.literal('map_start'),
  ...base,
  /**
   * The version of the Ankor plugin, up to {@link MAX_VERSION_CHARS} characters. Rejected when it holds a NUL or a lone UTF-16 surrogate.
   */
  plugin_version: wireText(MAX_VERSION_CHARS),
  /**
   * The version of SwiftlyS2, up to {@link MAX_VERSION_CHARS} characters. Rejected when it holds a NUL or a lone UTF-16 surrogate.
   */
  framework_version: wireText(MAX_VERSION_CHARS),
  /**
   * The version of the CS2 server, up to {@link MAX_VERSION_CHARS} characters. Rejected when it holds a NUL or a lone UTF-16 surrogate.
   */
  server_version: wireText(MAX_VERSION_CHARS),
  /** The map that loaded, as the engine names it. */
  map_name: z.string().regex(/^[A-Za-z0-9_/-]{1,128}$/, 'must be a map name'),
});

/**
 * Sent when a player connects after the match leaves warmup.
 * {@link ControllerGetServerStatus | get_server_status} lists who is on the server during warmup.
 */
export const PlayerConnect = z.object({
  type: z.literal('player_connect'),
  ...base,
  /** The player who connected. */
  player: PlayerRef,
  /** Whether the plugin kicked the player because the SteamID is not on any roster. */
  rejected: z.boolean(),
});

/** Sent when a player leaves after the match leaves warmup. */
export const PlayerDisconnect = z.object({
  type: z.literal('player_disconnect'),
  ...base,
  /** The player who left. */
  player: PlayerRef,
  /**
   * The engine's disconnect reason, up to {@link MAX_DISCONNECT_REASON_CHARS} characters. Rejected when it holds a NUL or a lone UTF-16 surrogate.
   */
  reason: wireText(MAX_DISCONNECT_REASON_CHARS),
});

/** Sent when the knife round begins. */
export const KnifeStart = z.object({ type: z.literal('knife_start'), ...base });

/**
 * Sent when the knife round ends. When both teams survive, the counts in this event decide the
 * winner.
 *
 * Rejected when a team's health is above {@link MAX_HEALTH} times its players alive.
 */
export const KnifeEnd = z.object({
  type: z.literal('knife_end'),
  ...base,
  /** The team that won the knife round. */
  winner_team: TeamNumber,
  /** The side that the winning team won on. */
  winner_side: PlayingSide,
  /** Players of team 1 alive at the end. */
  team1_alive: z.number().int().nonnegative().max(MAX_SERVER_PLAYERS),
  /** Players of team 2 alive at the end. */
  team2_alive: z.number().int().nonnegative().max(MAX_SERVER_PLAYERS),
  /**
   * Health points left to team 1, summed over its players alive, each at most
   * {@link MAX_HEALTH}.
   */
  team1_health: z.number().int().nonnegative().max(MAX_HEALTH * MAX_SERVER_PLAYERS),
  /** Health points left to team 2, summed like `team1_health`. */
  team2_health: z.number().int().nonnegative().max(MAX_HEALTH * MAX_SERVER_PLAYERS),
}).refine((knife) => knife.team1_health <= MAX_HEALTH * knife.team1_alive && knife.team2_health <= MAX_HEALTH * knife.team2_alive,
  'a team holds at most MAX_HEALTH per player alive');

/** Sent when the knife round winners choose a side, or run out of time and stay. */
export const SideChoice = z.object({
  type: z.literal('side_choice'),
  ...base,
  /** The team that chose. */
  team: TeamNumber,
  /**
   * What the team chose.
   *
   * - `stay`: keep the sides of the knife round.
   * - `swap`: exchange the sides.
   */
  choice: z.enum(['stay', 'swap']),
  /** The side of team 1 for the first half, after the choice. */
  team1_side: PlayingSide,
});

/**
 * The rules of a match, as a launch asks for them and as {@link GoingLive | going_live} and
 * {@link RoundBackup | round_backup} report the server playing them.
 *
 * The plugin reads the round settings off the server's cvars. `friendly_fire` comes from the
 * launch, and Ankor applies it through the damage reductions instead of `mp_friendlyfire`.
 */
export const MatchRules = z.object({
  /**
   * Rounds in regulation, as `mp_maxrounds`, up to {@link MAX_ROUNDS}. MR12 is 24 and MR15 is
   * 30.
   */
  max_rounds: z.number().int().positive().max(MAX_ROUNDS),
  /** Whether a tie after regulation goes to overtime, as `mp_overtime_enable`. */
  overtime: z.boolean(),
  /**
   * Rounds in one overtime, both halves together, as `mp_overtime_maxrounds`. The bound is 60, ten
   * times the stock 6.
   */
  overtime_max_rounds: z.number().int().positive().max(60),
  /** Whether bullets and melee hurt teammates. Grenades always do. */
  friendly_fire: z.boolean(),
});

export type MatchRules = z.infer<typeof MatchRules>;

/** Sent when the match goes live, with the round settings that it plays under. */
export const GoingLive = z.object({
  type: z.literal('going_live'),
  ...base,
  /** Team 1's side for the first half. */
  team1_side: PlayingSide,
  ...MatchRules.shape,
});

/**
 * Characters in the longest backup file that a {@link RoundBackup | round_backup} carries. A 5v5
 * that runs to 40 rounds writes a file of up to a few hundred KB, and tabs and quotes make it about
 * 1.4 times that as JSON.
 */
export const MAX_BACKUP_FILE_CHARS = 512 * 1024;

/**
 * What a server needs to play on from the backup of a live round. That is the engine's backup file,
 * which holds the round, the scores and the money and equipment of every player, and the two things
 * that the file does not hold.
 *
 * A launch carries one as `match.restore`.
 */
export const RestoreFile = z.object({
  /**
   * Team 1's side in that round. On a launch, the plugin seats players by it through warmup, before
   * the load.
   */
  team1_side: PlayingSide,
  /**
   * The rules that the map was played under. A restore plays on under these rules instead of the
   * rules of its launch.
   */
  rules: MatchRules,
  /**
   * The backup file as the engine wrote it, up to {@link MAX_BACKUP_FILE_CHARS} characters. The
   * file holds the money and equipment of every player, keyed by account, and only
   * `mp_backup_restore_load_file` reads it.
   *
   * The file is text with tabs and line breaks, so a file at the length cap stays within the frame
   * limits once JSON escapes it. Rejected when it holds a lone UTF-16 surrogate.
   */
  file: wireText(MAX_BACKUP_FILE_CHARS).min(1).regex(/^[\t\n\r\P{Cc}]*$/u, 'must be text without control characters other than tabs and line breaks'),
});

export type RestoreFile = z.infer<typeof RestoreFile>;

/**
 * A live round as it started. It holds the {@link RestoreFile} of the round and where the round
 * stands in the match, both read off the engine.
 */
export const RestorePoint = RestoreFile.extend({
  /** The one-based round that the backup starts. The file numbers rounds from zero, one lower. */
  round: RoundNumber,
  /** Team 1's score before that round. */
  team1_score: Score,
  /** Team 2's score before that round. */
  team2_score: Score,
});

export type RestorePoint = z.infer<typeof RestorePoint>;

/**
 * Sent when the server has loaded a round backup, from the `restore` of its launch, from a
 * {@link ControllerRestoreRound | restore_round}, or from an `mp_backup_restore_load_file` that someone
 * ran by hand.
 *
 * The {@link RoundStart | round_start} that follows plays `round` again. From that
 * {@link RoundStart | round_start} on, the events of the server replace everything that it sent
 * for that round and the rounds after it.
 *
 * The plugin reads the scores and the side from the engine once the engine has loaded the file. The
 * plugin refuses a load during the knife round, the side choice, the halftime break and after the
 * match has ended.
 */
export const MatchRestored = z.object({
  type: z.literal('match_restored'),
  ...base,
  /** The one-based round that the match plays on from. */
  round: RoundNumber,
  /** Team 1's side in that round. */
  team1_side: PlayingSide,
  /** Team 1's score before that round. */
  team1_score: Score,
  /** Team 2's score before that round. */
  team2_score: Score,
});

export type MatchRestored = z.infer<typeof MatchRestored>;

/**
 * Sent as each live round starts. The newest one of a map, with the launch of that map, holds
 * everything that a restore needs on another node.
 */
export const RoundBackup = z.object({
  type: z.literal('round_backup'),
  ...base,
  ...RestorePoint.shape,
  /** The teams and spectators that the map is played with. */
  roster: Roster,
});

/**
 * A weapon, by the engine's item definition index or, for an item without one, the engine's class
 * name. Rejected when `id` is a number and `fallback` is not null.
 */
export const Weapon = z.object({
  /** The item definition index, a 16-bit unsigned integer. Null when the item has none. */
  id: z.number().int().positive().max(65535).nullable(),
  /**
   * The engine class name of an item without an index. Null when `id` is set, or when the engine
   * named no weapon either.
   */
  fallback: z.string().regex(/^[A-Za-z0-9_]{1,64}$/, 'must be an engine class name').nullable(),
}).refine((weapon) => weapon.id === null || weapon.fallback === null, 'A numeric weapon must not also have a fallback name');

export type Weapon = z.infer<typeof Weapon>;

/**
 * One kind of item that a player holds: the engine's item definition index as `weapon`, a quantity,
 * and the unit price in this game build.
 */
export const InventoryItem = z.object({
  /** The engine's item definition index. */
  weapon: z.number().int().positive().max(65535),
  /** Items of this kind that the player holds, up to ten times the stock limit of 2 flashbangs. */
  quantity: z.number().int().positive().max(20),
  /** Dollars that one item costs, up to ten times the dearest stock item, the M249 at 5,200. */
  unit_value: z.number().int().nonnegative().max(52_000),
});

/** What a player carries. A null inventory means that the plugin could not read the pawn. */
export const PlayerInventory = z.object({
  /** Weapons, grenades and other items held, up to 64. */
  items: z.array(InventoryItem).max(64),
  /** Armor points, up to {@link MAX_HEALTH}, ten times the stock 100. */
  armor: Health,
  /** Whether the armor includes a helmet. */
  helmet: z.boolean(),
  /** Whether the player carries a defuse kit. */
  defuse_kit: z.boolean(),
});

/** The state and money of a player when the plugin sampled the round. Money is in dollars. */
const RoundPlayer = z.object({
  /** The player sampled. */
  player: PlayerRef,
  /** Whether the player was alive when sampled. */
  alive: z.boolean(),
  /** Dollars of equipment that the player carried, the engine's figure. */
  equipment_value: EquipmentValue,
  /** Dollars in hand when sampled. */
  cash: Cash,
  /** Dollars in hand at the start of the round. */
  start_cash: Cash,
  /** Dollars spent this round by the time of the sample. */
  cash_spent: z.number().int().nonnegative().max(2 * MAX_CASH),
});

/**
 * Sent when a round begins.
 *
 * The same round can begin again with no {@link RoundEnd | round_end} between. The engine restarts
 * a round that a team leaves before it is decided, as when every player on it disconnects or loses
 * their connection. The later `round_start` replaces the earlier one, and the round is played
 * from it.
 */
export const RoundStart = z.object({
  type: z.literal('round_start'),
  ...roundScoped,
  /** Team 1's side this round. */
  team1_side: PlayingSide,
  /**
   * The players present when the round started, each once. An empty array is authoritative, so the
   * round had no players. Rejected when two entries share a `player.steam_id`.
   */
  players: z.array(RoundPlayer).max(MAX_SERVER_PLAYERS).refine(uniquePlayers, 'Round players must be unique'),
  /**
   * The players of the previous round as that round left them, after exit kills and pickups. Null
   * across an equipment reset.
   */
  carryover: z.object({
    /** The round after which the plugin took the snapshot. */
    from_round: RoundNumber,
    /** The server tick when the plugin took the snapshot. */
    sampled_tick: base.tick,
    /**
     * The players as that round left them, with what each carried. Rejected when two entries share a
     * `player.steam_id`.
     */
    players: z.array(RoundPlayer.extend({
      /** What the player carried. Null when the plugin could not read the pawn. */
      inventory: PlayerInventory.nullable(),
    })).max(MAX_SERVER_PLAYERS).refine(uniquePlayers, 'Carried-over players must be unique'),
  }).nullable(),
});

/**
 * Sent when freeze time ends, as the authoritative list of the players who take part in the round
 * after reconnects during freeze time.
 */
export const RoundFreezeEnd = z.object({
  type: z.literal('round_freeze_end'),
  ...roundScoped,
  /** Team 1's side this round. */
  team1_side: PlayingSide,
  /** Everyone who takes part in the round, each once. Rejected when two entries share a `player.steam_id`. */
  players: z.array(RoundPlayer).max(MAX_SERVER_PLAYERS).refine(uniquePlayers, 'Round participants must be unique'),
});

/**
 * Sent when buy time closes, `mp_buytime` after freeze time ends, with the equipment value that
 * each roster team carried, as the sum of the engine's figure for each pawn.
 *
 * The plugin reads a player at the last buy-zone exit inside the window, or live at the close while
 * the player is still in a buy zone. Snapshots at a death or disconnect keep a buy that was made
 * before the window closes. An earlier buy-zone exit counts for a player who never returned.
 *
 * Rejected unless `team1_equipment_value` and `team2_equipment_value` are each the sum of
 * `equipment_value` over the `players` whose `player.team` is that team.
 */
export const RoundBuyEnd = z.object({
  type: z.literal('round_buy_end'),
  ...roundScoped,
  /**
   * The samples behind the team totals, each player once. Rejected when two entries share a
   * `player.steam_id`.
   */
  players: z.array(z.object({
    /** The player read. */
    player: PlayerRef,
    /** Dollars of equipment that the player carried. */
    equipment_value: EquipmentValue,
    /** Dollars in hand. */
    cash: Cash,
    /** Dollars in hand at the start of the round. */
    start_cash: Cash,
    /** Dollars spent this round. */
    cash_spent: z.number().int().nonnegative().max(2 * MAX_CASH),
    /** The server tick when the plugin read the player. */
    sampled_tick: base.tick,
  })).max(MAX_SERVER_PLAYERS).refine(uniquePlayers, 'Buy samples must be unique'),
  /**
   * Dollars of equipment that team 1 carried, the sum of `equipment_value` over its `players`, so
   * the event states the economy of the round on its own.
   */
  team1_equipment_value: z.number().int().nonnegative().max(MAX_EQUIPMENT_VALUE * MAX_SERVER_PLAYERS),
  /** Dollars of equipment that team 2 carried, summed like `team1_equipment_value`. */
  team2_equipment_value: z.number().int().nonnegative().max(MAX_EQUIPMENT_VALUE * MAX_SERVER_PLAYERS),
}).refine((buy) => buy.team1_equipment_value === teamEquipment(buy.players, 1) && buy.team2_equipment_value === teamEquipment(buy.players, 2),
  'Team equipment values must be the sums of their players');

function teamEquipment(players: readonly { player: { team: TeamNumber | null }; equipment_value: number }[], team: TeamNumber): number {
  return players.reduce((sum, sample) => (sample.player.team === team ? sum + sample.equipment_value : sum), 0);
}

/**
 * Sent when a round is decided. The last gameplay reports of the round may still follow, and
 * {@link RoundComplete | round_complete} closes it.
 */
export const RoundEnd = z.object({
  type: z.literal('round_end'),
  ...roundScoped,
  /** The side that won the round. */
  winner_side: PlayingSide,
  /** The roster team that won the round. */
  winner_team: TeamNumber,
  /** Why the round ended. */
  reason: RoundEndReason,
  /** Team 1's score after this round. */
  team1_score: Score,
  /** Team 2's score after this round. */
  team2_score: Score,
  /** Seconds that the round lasted, from the end of freeze time to the end of the round. */
  round_length: RoundSeconds,
});

/**
 * Sent after every gameplay report of a scored round, its {@link RoundShots | round_shots}
 * included, before the next round or the map end.
 */
export const RoundComplete = z.object({
  type: z.literal('round_complete'),
  ...roundScoped,
});

/**
 * What caused damage or a death.
 *
 * - `player`: another player dealt it, with a weapon or a grenade.
 * - `bomb`: the planted bomb exploded.
 * - `fall`: the victim fell.
 * - `self`: the victim's own grenade or fire.
 * - `disconnect`: the victim left the server, which kills a living player.
 * - `other`: anything else, such as a map's damage trigger or the plugin moving a player to the
 *   spectators.
 */
export const DamageCause = z.enum(['player', 'bomb', 'fall', 'self', 'disconnect', 'other']);

export type DamageCause = z.infer<typeof DamageCause>;

/**
 * Sent when a player dies. Rejected when the `assister` has the `steam_id` of the `victim` or the
 * `attacker`.
 */
export const PlayerDeath = z.object({
  type: z.literal('player_death'),
  ...roundScoped,
  /** The player who died. */
  victim: PlacedPlayer,
  /**
   * The player credited with the kill. Null when no player dealt it.
   *
   * For a grenade kill, the position is where the thrower was at the death. The
   * {@link GrenadeThrow | grenade_throw} gives where they threw from.
   */
  attacker: PlayerAt.nullable(),
  /** The player credited with the assist. Null when there is none. */
  assister: PlayerRef.nullable(),
  /** The killing weapon. */
  weapon: Weapon,
  /** What caused the death. */
  cause: DamageCause,
  /** Whether the killing hit was to the head. */
  headshot: z.boolean(),
  /** Surfaces that the bullet went through. The bound is a margin, not a measured engine limit. */
  penetrated: z.number().int().nonnegative().max(10),
  /** Whether the killing shot passed through smoke. */
  thru_smoke: z.boolean(),
  /** Whether the attacker fired a scoped weapon unscoped. */
  no_scope: z.boolean(),
  /** Whether the attacker was flashed. */
  attacker_blind: z.boolean(),
  /** Whether the attacker was airborne. */
  attacker_in_air: z.boolean(),
  /** Whether the assist came from a flashbang. */
  flash_assist: z.boolean(),
  /** Whether the attacker and the victim are on the same roster team, a team kill. */
  friendly_fire: z.boolean(),
  /**
   * The player who last blinded the victim, through a {@link PlayerBlind | player_blind} still
   * running at the death. Null when the victim could see.
   *
   * `victim_blind_remaining` gives the seconds of that blind left. When the attacker is the
   * blinder, the kill is a self-flash kill. The consumer decides how much blind must remain for
   * that to count.
   */
  victim_blind_by: PlayerRef.nullable(),
  /** Seconds left of the blind from `victim_blind_by`. Zero when the victim could see. */
  victim_blind_remaining: BlindSeconds,
  /**
   * Dollars of equipment that the victim carried when the killing blow landed, the engine's
   * figure.
   */
  victim_equipment_value: EquipmentValue,
  /**
   * Grenades still in the victim's inventory when the killing blow landed. The C4 and the knife are
   * not grenades. Two flashbangs show as a `quantity` of 2, and `unit_value` is the price per
   * grenade.
   *
   * An empty array means that the victim held none. Null when the plugin could not read the
   * inventory from before the damage.
   */
  victim_grenades_held: z.array(InventoryItem).max(64).nullable(),
}).refine((death) => death.assister === null
  || (death.assister.steam_id !== death.victim.steam_id && death.assister.steam_id !== death.attacker?.steam_id),
'The assister is neither the victim nor the attacker');

/**
 * The zero-based id of a grenade within its server, counted in throw order. A server throws far
 * fewer than a million.
 */
export const GrenadeId = z.number().int().nonnegative().max(1_000_000);

/**
 * Sent when a player takes damage. Rejected when `health_before` is a number and `health_after` is
 * above it.
 */
export const PlayerHurt = z.object({
  type: z.literal('player_hurt'),
  ...roundScoped,
  /** The player hit. */
  victim: PlacedPlayer,
  /**
   * The player who dealt the damage. Null for world damage.
   *
   * For grenade damage, the position is where the thrower was at the hit. The
   * {@link GrenadeThrow | grenade_throw} gives where they threw from.
   */
  attacker: PlayerAt.nullable(),
  /** What dealt the damage. */
  weapon: Weapon,
  /** What caused the damage. */
  cause: DamageCause,
  /**
   * Health damage as the engine reports it, overkill included. `health_before` and `health_after`
   * give the health that the victim actually lost.
   */
  dmg_health: Damage,
  /**
   * The victim's health before the hit, as the engine reports it. Null when the plugin could not
   * read it.
   */
  health_before: Health.nullable(),
  /** The victim's health after the hit, at most `health_before`. */
  health_after: Health,
  /** Armor damage. */
  dmg_armor: Damage,
  /**
   * The engine's hit group, `HitGroup_t` in the Source 2 SDK. The plugin sends the engine's invalid
   * group as `0`, and `9` is unused.
   *
   * - `0`: generic.
   * - `1`: head.
   * - `2`: chest.
   * - `3`: stomach.
   * - `4`: left arm.
   * - `5`: right arm.
   * - `6`: left leg.
   * - `7`: right leg.
   * - `8`: neck.
   * - `10`: gear, the backpack hitbox.
   */
  hitgroup: z.number().int().nonnegative().max(15),
  /** Whether the attacker and the victim are on the same roster team. */
  friendly_fire: z.boolean(),
  /**
   * Whether the grenade itself struck the player before it detonated. Such a hit counts as damage
   * and is stored as a hit, but not as utility damage, which matches the in-game scoreboard.
   */
  projectile_impact: z.boolean(),
  /**
   * The grenade that dealt the damage, as its {@link GrenadeThrow | grenade_throw} identifies it.
   * Null for anything else.
   */
  grenade_id: GrenadeId.nullable(),
}).refine((hurt) => hurt.health_before === null || hurt.health_after <= hurt.health_before,
  'A hit never raises health');

/** Sent when a flashbang blinds a player. */
export const PlayerBlind = z.object({
  type: z.literal('player_blind'),
  ...roundScoped,
  /** The player blinded. */
  victim: PlacedPlayer,
  /**
   * The player who threw the flashbang, placed where they were when it went off. The
   * {@link GrenadeThrow | grenade_throw} gives where they threw from.
   */
  attacker: PlayerAt,
  /** Seconds that the blind lasts. */
  duration: BlindSeconds,
  /** Whether the thrower and the victim are on the same roster team. */
  friendly_fire: z.boolean(),
  /**
   * The flashbang, as its {@link GrenadeThrow | grenade_throw} identifies it. Null when the plugin
   * did not see it thrown.
   */
  grenade_id: GrenadeId.nullable(),
});

/** The engine class name of a grenade. */
export const GrenadeKind = z.enum(['flashbang', 'hegrenade', 'smokegrenade', 'molotov', 'incgrenade', 'decoy']);

export type GrenadeKind = z.infer<typeof GrenadeKind>;

/**
 * How a fire ended.
 *
 * - `expired`: the fire burned out, or the round closed on it.
 * - `extinguished`: a smoke put the fire out.
 * - `smothered`: the fire grenade landed inside a smoke.
 */
export const FireEnd = z.enum(['expired', 'extinguished', 'smothered']);

export type FireEnd = z.infer<typeof FireEnd>;

/**
 * Sent when a grenade leaves a player's hand. Its `grenade_id` identifies the grenade in the events
 * that follow: its {@link GrenadeDetonate | grenade_detonate} and {@link GrenadeEnd | grenade_end},
 * and the {@link PlayerHurt | player_hurt} and {@link PlayerBlind | player_blind} that it causes.
 */
export const GrenadeThrow = z.object({
  type: z.literal('grenade_throw'),
  ...roundScoped,
  /** The grenade's id, unique within the server. */
  grenade_id: GrenadeId,
  /** The player who threw the grenade, placed where they stood when the projectile spawned. */
  player: PlacedPlayer,
  /** The kind of grenade thrown. */
  grenade: GrenadeKind,
});

/**
 * Sent when a grenade detonates or a fire grenade lights. A grenade that is removed before then
 * sends none.
 */
export const GrenadeDetonate = z.object({
  type: z.literal('grenade_detonate'),
  ...roundScoped,
  /** The grenade, as its {@link GrenadeThrow | grenade_throw} identifies it. */
  grenade_id: GrenadeId,
  /** The player who threw the grenade, as its {@link GrenadeThrow | grenade_throw} saw them. */
  player: PlayerRef,
  /** Where the engine reported the detonation. Null for a fire that lit without reporting one. */
  detonate_pos: WorldPos.nullable(),
});

/**
 * Sent when the effect of a grenade is over, because its last entity was removed or the round
 * closed or reset on it. Every grenade that sent a {@link GrenadeThrow | grenade_throw} sends one
 * before the {@link RoundComplete | round_complete} of its round. A grenade whose round a restore
 * cut off sends none; the {@link MatchRestored | match_restored} replaces that round.
 */
export const GrenadeEnd = z.object({
  type: z.literal('grenade_end'),
  ...base,
  /** The round that the grenade was thrown in. */
  round: RoundNumber,
  /** The grenade, as its {@link GrenadeThrow | grenade_throw} identifies it. */
  grenade_id: GrenadeId,
  /** The player who threw the grenade, as its {@link GrenadeThrow | grenade_throw} saw them. */
  player: PlayerRef,
  /**
   * Seconds since the round started, at the moment the effect ended. A fire ends when it goes out.
   * When the round closed on the grenade, the value is the round length plus the restart delay,
   * which is when the engine clears the entities.
   */
  end_round_time: RoundSeconds,
  /**
   * How the fire ended. Null for a grenade that was not a fire, and for a fire projectile removed
   * before it lit.
   */
  fire_end: FireEnd.nullable(),
  /**
   * The smoke whose cloud put this fire out, credited to the nearest live smoke. Null when no smoke
   * put the fire out.
   */
  extinguished_by: GrenadeId.nullable(),
});

/**
 * The shots that a player fired with one weapon over a round. Rejected unless `shots` equals the
 * number of `ticks`.
 */
export const WeaponShots = z.object({
  /** The weapon fired. */
  weapon: Weapon,
  /** Shots fired, which equals the number of `ticks`. */
  shots: z.number().int().positive().max(MAX_ROUND_SHOTS),
  /** The server tick of each shot. */
  ticks: z.array(base.tick).min(1).max(MAX_ROUND_SHOTS),
}).refine((shots) => shots.shots === shots.ticks.length, 'shots must count ticks');

export type WeaponShots = z.infer<typeof WeaponShots>;

/** The shots that one player fired over a round. */
export const PlayerShots = z.object({
  /** The player. */
  player: PlayerRef,
  /** Each weapon that the player fired, once each. */
  weapons: z.array(WeaponShots).min(1).max(MAX_ROUND_SHOTS),
});

export type PlayerShots = z.infer<typeof PlayerShots>;

/**
 * Sent once for each round when it closes, at halftime, the next {@link RoundStart | round_start}
 * or the {@link MapEnd | map_end}, with the shots that each player fired over the round.
 *
 * A round that starts again with no {@link RoundEnd | round_end}, as described on
 * {@link RoundStart | round_start}, sends none for the attempt it abandoned. Neither does a round
 * that a restore cut off, which the {@link MatchRestored | match_restored} replaces.
 *
 * `players` lists each player who fired, and is empty when nobody did. The plugin counts shots
 * from the engine's `weapon_fire`, up to {@link MAX_ROUND_SHOTS} for each player, and counts no
 * more. Knives and grenades are left out, because they fire no rounds. A throw arrives as
 * {@link GrenadeThrow | grenade_throw} instead. Hits are the {@link PlayerHurt | player_hurt}
 * events of the same round, attacker and weapon.
 *
 * Rejected when a player's shots add up to more than {@link MAX_ROUND_SHOTS}, or when a player or a
 * weapon of a player is listed twice.
 */
export const RoundShots = z.object({
  type: z.literal('round_shots'),
  ...base,
  /** The round that the shots were fired in. */
  round: RoundNumber,
  /** Each player who fired, once each. */
  players: z.array(PlayerShots).max(MAX_SERVER_PLAYERS),
}).superRefine((round, ctx) => {
  if (!uniquePlayers(round.players)) {
    ctx.addIssue({ code: 'custom', path: ['players'], message: 'each player is listed once' });
  }

  for (const [index, { weapons }] of round.players.entries()) {
    if (weapons.reduce((total, shots) => total + shots.shots, 0) > MAX_ROUND_SHOTS) {
      ctx.addIssue({ code: 'custom', path: ['players', index, 'weapons'], message: 'a player fires at most MAX_ROUND_SHOTS shots' });
    }

    if (new Set(weapons.map(({ weapon }) => JSON.stringify([weapon.id, weapon.fallback]))).size !== weapons.length) {
      ctx.addIssue({ code: 'custom', path: ['players', index, 'weapons'], message: 'each weapon is listed once' });
    }
  }
});

/** Sent when a player begins to plant the bomb. */
export const BombBeginPlant = z.object({
  type: z.literal('bomb_begin_plant'),
  ...roundScoped,
  /** The player planting. */
  player: PlacedPlayer,
  /** The bombsite. Null when the engine's target index did not resolve to a `func_bomb_target`. */
  site: z.enum(['a', 'b']).nullable(),
});

/** Sent when a player abandons a plant. */
export const BombAbortPlant = z.object({
  type: z.literal('bomb_abort_plant'),
  ...roundScoped,
  /** The player who stopped planting. */
  player: PlacedPlayer,
  /** The bombsite. Null when the engine's target index did not resolve to a `func_bomb_target`. */
  site: z.enum(['a', 'b']).nullable(),
});

/** Sent when the bomb is planted. */
export const BombPlanted = z.object({
  type: z.literal('bomb_planted'),
  ...roundScoped,
  /** The player who planted. */
  player: PlacedPlayer,
  /** The bombsite. Null when the engine's target index did not resolve to a `func_bomb_target`. */
  site: z.enum(['a', 'b']).nullable(),
});

/** Sent when a player begins to defuse the bomb. */
export const BombBeginDefuse = z.object({
  type: z.literal('bomb_begin_defuse'),
  ...roundScoped,
  /** The player defusing. */
  player: PlacedPlayer,
  /** Whether the defuser has a kit. */
  has_kit: z.boolean(),
});

/** Sent when a player abandons a defuse. */
export const BombAbortDefuse = z.object({
  type: z.literal('bomb_abort_defuse'),
  ...roundScoped,
  /** The player who stopped defusing. */
  player: PlacedPlayer,
});

/** Sent when the bomb is defused. */
export const BombDefused = z.object({
  type: z.literal('bomb_defused'),
  ...roundScoped,
  /** The player who defused. */
  player: PlacedPlayer,
  /** Seconds left on the bomb, up to 90, the most that `mp_c4timer` allows. */
  time_remaining: z.number().nonnegative().max(90),
  /** Whether the defuser has a kit. */
  has_kit: z.boolean(),
});

/** Sent when the bomb explodes. */
export const BombExploded = z.object({ type: z.literal('bomb_exploded'), ...roundScoped });

/** Sent when the engine names the MVP of a round. */
export const RoundMvp = z.object({
  type: z.literal('round_mvp'),
  ...roundScoped,
  /** The MVP of the round. */
  player: PlayerRef,
  /** The engine's MVP reason code, a small enum. */
  reason: z.number().int().nonnegative().max(31),
});

/**
 * Why a map ended:
 *
 * - `played`: the map was played out.
 * - `ready_timeout`: `ready_timeout_seconds` passed in warmup, before the match started.
 * - `abandoned`: every rostered player left after the match began, in the knife round, the side
 *   choice or live play.
 *
 * Only a `played` map has a winner. The controller decides what any other end means for the match.
 */
export const MapEndReason = z.enum(['played', 'ready_timeout', 'abandoned']);

export type MapEndReason = z.infer<typeof MapEndReason>;

/** A team's players when the ready deadline passed. */
export const ReadyTeam = z.object({
  /** Rostered players of the team on the server. */
  connected: z.number().int().nonnegative().max(MAX_SERVER_PLAYERS),
  /** Whether the team counted as ready, its captain's force-ready included. */
  ready: z.boolean(),
});

export type ReadyTeam = z.infer<typeof ReadyTeam>;

/**
 * Sent when the map is over. After a `played` map, the scoreboard stays up for 60 seconds, then the
 * server kicks the players, sends its remaining events and quits. After a `ready_timeout`, the
 * server kicks the players at once.
 *
 * Rejected unless `winner_team` is the team with the higher score of a `played` map, or null when
 * the scores are equal or the map was not played out. Rejected unless `teams` is set for a
 * `ready_timeout`, with both scores 0, and null otherwise.
 */
export const MapEnd = z.object({
  type: z.literal('map_end'),
  ...base,
  /** Why the map ended. */
  reason: MapEndReason,
  /** The team with the higher score of a `played` map. Null for a draw, and for any other end. */
  winner_team: TeamNumber.nullable(),
  /** Team 1's score when the map ended. */
  team1_score: Score,
  /** Team 2's score when the map ended. */
  team2_score: Score,
  /**
   * Who was on the server and ready when `ready_timeout_seconds` passed. Null for any other end.
   */
  teams: z.object({
    team1: ReadyTeam,
    team2: ReadyTeam,
    /** The rostered spectators, as one group. Null unless the match has them ready up. */
    spectators: ReadyTeam.nullable(),
  }).nullable(),
}).refine((end) => end.winner_team === (end.reason !== 'played' || end.team1_score === end.team2_score
  ? null
  : end.team1_score > end.team2_score ? 1 : 2), 'winner_team must be the score leader of a played map')
  .refine((end) => (end.reason === 'ready_timeout'
    ? end.teams !== null && end.team1_score === 0 && end.team2_score === 0
    : end.teams === null), 'teams must be set, at 0-0, exactly for a ready_timeout');

/** Sent when the plugin logs a line worth keeping with the match. */
export const PluginLog = z.object({
  type: z.literal('log'),
  ...base,
  /** The severity of the line. */
  level: z.enum(['debug', 'info', 'warn', 'error']),
  /**
   * The log line, up to {@link MAX_LOG_MESSAGE_CHARS} characters. Rejected when it holds a NUL or a lone UTF-16 surrogate.
   */
  message: wireText(MAX_LOG_MESSAGE_CHARS),
});

/** Any gameplay event. */
export const PluginToControllerMessage = z.discriminatedUnion('type', [
  MapStart,
  PlayerConnect,
  PlayerDisconnect,
  KnifeStart,
  KnifeEnd,
  SideChoice,
  GoingLive,
  RoundBackup,
  MatchRestored,
  RoundStart,
  RoundFreezeEnd,
  RoundBuyEnd,
  RoundEnd,
  RoundComplete,
  PlayerDeath,
  PlayerHurt,
  PlayerBlind,
  GrenadeThrow,
  GrenadeDetonate,
  GrenadeEnd,
  RoundShots,
  BombBeginPlant,
  BombAbortPlant,
  BombPlanted,
  BombBeginDefuse,
  BombAbortDefuse,
  BombDefused,
  BombExploded,
  RoundMvp,
  MapEnd,
  PluginLog,
]);

export type PluginToControllerMessage = z.infer<typeof PluginToControllerMessage>;
