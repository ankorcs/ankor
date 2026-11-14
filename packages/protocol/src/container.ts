/**
 * The interface between the agent and the game containers it runs, outside the wire to the
 * controller: the environment the CS2 image and the plugin read, the plugin's config file, where
 * recordings land and how they are named, the cfg files both sides exec, the local plugin socket
 * and the handoff to a replacement agent. `packages/plugin/core/Wire/Launch.cs` mirrors the
 * plugin's half.
 */
import { z } from 'zod';
import { ChatColor, type EVENT_BATCH_MAX_BYTES, type GameMode, type MatchConfig } from './index.js';

export * from './plugin-socket.js';

export * from './agent-image.js';

/** The path of the agent's local plugin socket. */
export const PLUGIN_WS_PATH = '/ws/plugin';

/**
 * The environment variables the image entrypoint reads. The plugin also reads `startMap` and
 * `workshopMap` to know when its map is loaded.
 */
export const CS2_ENV = {
  /** The server's Game Server Login Token. */
  srcdsToken: 'SRCDS_TOKEN',
  port: 'CS2_PORT',
  tvPort: 'CS2_TV_PORT',
  /** `+tv_enable`, `1` or `0`. GOTV starts with the map, so it is set on the command line. */
  tvEnable: 'CS2_TV_ENABLE',
  gamePassword: 'CS2_PASSWORD',
  maxPlayers: 'CS2_MAXPLAYERS',
  /** `+game_type`, from {@link GAME_MODE_ARGS}. */
  gameType: 'CS2_GAME_TYPE',
  /** `+game_mode`, from {@link GAME_MODE_ARGS}. */
  gameMode: 'CS2_GAME_MODE',
  startMap: 'CS2_STARTMAP',
  /**
   * Passed separately so the entrypoint places `host_workshop_map` after the stock bootstrap map.
   */
  workshopMap: 'CS2_WORKSHOP_MAP',
  /** The Steam Web API key that workshop downloads need, as `-authkey`. */
  authKey: 'CS2_AUTHKEY',
  extraArgs: 'CS2_EXTRA_ARGS',
  updateOnly: 'CS2_UPDATE_ONLY',
  validate: 'CS2_VALIDATE',
  /** Empties the install volume before steamcmd runs. */
  reinstall: 'CS2_REINSTALL',
  /** Whole gigabytes the entrypoint requires free before it lets steamcmd stage anything. */
  minFreeGb: 'CS2_MIN_FREE_GB',
} as const;

/**
 * The `+game_type` and `+game_mode` for each {@link GameMode}, as `gamemodes.txt` numbers them. The
 * plugin's `CfgNames.Mode` maps the same pairs back to the modes' cfgs.
 */
export const GAME_MODE_ARGS = {
  casual: { gameType: 0, gameMode: 0 },
  competitive: { gameType: 0, gameMode: 1 },
  wingman: { gameType: 0, gameMode: 2 },
  rush: { gameType: 0, gameMode: 6 },
  arms_race: { gameType: 1, gameMode: 0 },
  demolition: { gameType: 1, gameMode: 1 },
  deathmatch: { gameType: 1, gameMode: 2 },
  custom: { gameType: 3, gameMode: 0 },
} as const satisfies Record<GameMode, { gameType: number; gameMode: number }>;

/** The environment variables that only the plugin reads. */
export const ANKOR_ENV = {
  /** The local relay the plugin dials for its slot. */
  wsUrl: 'ANKOR_WS_URL',
  /**
   * The server's `allocation_id`, which identifies its events. A plugin without one sends nothing.
   */
  allocationId: 'ANKOR_ALLOCATION_ID',
  debugCaptureDir: 'ANKOR_DEBUG_CAPTURE_DIR',
} as const;

/** Where the cs2 image keeps its addon overlay. The agent copies it out once per image id. */
export const IMAGE_OVERLAY_DIR = '/opt/ankor/overlay';

/** The game installation inside the container. Per-slot writable paths shadow parts of it. */
export const GAME_INSTALL_DIR = '/home/steam/cs2-dedicated';

/**
 * The slot's writable directory, relative to {@link GAME_INSTALL_DIR}. It holds everything per slot
 * that the agent, the plugin and the engine share. The agent lays out its contents, so a new file
 * or directory in it needs no mountpoint in the install.
 */
export const ANKOR_DIR = 'game/csgo/ankor';

/** GOTV recordings, relative to {@link GAME_INSTALL_DIR}, kept until uploaded. */
export const DEMO_DIR = `${ANKOR_DIR}/demos`;

/** The engine's round backups, relative to {@link GAME_INSTALL_DIR}, emptied at every launch. */
export const BACKUP_DIR = `${ANKOR_DIR}/backups`;

/**
 * The file name of the engine's backup of a round, from the rounds played before it. The engine
 * numbers a backup by those rounds in two digits or more, so `ankor_round13.txt` is round 14's.
 */
export function backupFileName(roundsPlayed: number): string {
  return `ankor_round${String(roundsPlayed).padStart(2, '0')}.txt`;
}

/**
 * The launch's `restore.file` as the agent wrote it, relative to {@link GAME_INSTALL_DIR}. It is
 * absent on a launch without a restore.
 */
export const RESTORE_FILE = `${ANKOR_DIR}/restore.txt`;

/**
 * The match the server plays, relative to {@link GAME_INSTALL_DIR}. Every match server has it as
 * {@link MatchConfig} in JSON, and a server without a match has none.
 */
export const MATCH_CONFIG_FILE = `${ANKOR_DIR}/match.json`;

/**
 * The suffix the plugin adds to a recording's path to create a file once the recording is
 * finished. The agent uploads a `.dem` only once that file exists.
 */
export const DEMO_READY_SUFFIX = '.ready';

/**
 * The plugin's SwiftlyS2 config file, rendered per launch and bind-mounted over the image's copy.
 * Keys are PascalCase because the framework's configuration provider reads them that way.
 */
export const PluginConfigFile = z.object({
  Chat: z.object({ Tag: z.string(), TagColor: ChatColor }),
});

export type PluginConfigFile = z.infer<typeof PluginConfigFile>;

/**
 * Bytes in the largest frame the local event socket carries, on both the plugin's and the relay's
 * side. It is at most {@link EVENT_BATCH_MAX_BYTES}, so an event that fits a frame fits an upload
 * batch.
 */
export const MAX_FRAME_BYTES = 2 << 20;

/**
 * Events a plugin may have waiting to be stored on the node. Past this or
 * {@link RELAY_QUEUE_MAX_BYTES}, its history pauses.
 */
export const RELAY_QUEUE_MAX_FRAMES = 512;

/** Bytes of events a plugin may have waiting to be stored on the node. */
export const RELAY_QUEUE_MAX_BYTES = 4 << 20;

/**
 * The directory under `cfg/` that the image ships its cfgs in. The plugin execs them, and a launch
 * may exec any of them.
 */
export const CFG_DIR = 'ankor';

export const CFG_NAMES = {
  /**
   * Ankor's settings, which the plugin runs over the game mode's cfg on every map load and when a
   * match goes live.
   */
  base: `${CFG_DIR}/base`,
  warmup: `${CFG_DIR}/warmup`,
  knife: `${CFG_DIR}/knife`,
  /**
   * The cfg the agent renders per launch at `cfg/ankor_launch.cfg`. The plugin runs it after
   * `base`.
   */
  launch: 'ankor_launch',
} as const;
