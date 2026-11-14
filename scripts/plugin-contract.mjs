// Compares the plugin's copy of the contract's values (paths, env names, cfg names, limits, the
// protocol version, the command refusals and the commands a match blocks) with what packages/protocol exports, and holds every
// frame of the plugin's replay fixture to the contract. The plugin's tests cannot read
// packages/protocol, so this check spans the two packages. Run it through
// `scripts/plugin.sh contract`, which builds the protocol first.

import { execFileSync } from 'node:child_process';
import console from 'node:console';
import { readFileSync } from 'node:fs';
import process from 'node:process';
import {
  CONFIG_ERROR_MAX_CHARS,
  DEMO_NAME,
  MAP_NAME_MAX_CHARS,
  MATCH_BLOCKED_COMMANDS,
  MAX_BACKUP_FILE_CHARS,
  MAX_DAMAGE,
  MAX_DISCONNECT_REASON_CHARS,
  MAX_LOG_MESSAGE_CHARS,
  MAX_PLAYER_NAME_CHARS,
  MAX_ROUND_SHOTS,
  MAX_SERVER_PLAYERS,
  MAX_SPECTATORS,
  MAX_TEAM_NAME_LENGTH,
  MAX_VERSION_CHARS,
  PluginToControllerMessage,
  PROTOCOL_HEADER,
  PROTOCOL_VERSION,
} from '@ankorcs/protocol';
import {
  ANKOR_DIR,
  ANKOR_ENV,
  BACKUP_DIR,
  backupFileName,
  CFG_DIR,
  CFG_NAMES,
  CommandRefusal,
  CS2_ENV,
  DEMO_DIR,
  DEMO_READY_SUFFIX,
  GAME_INSTALL_DIR,
  GAME_MODE_ARGS,
  MATCH_CONFIG_FILE,
  MAX_FRAME_BYTES,
  PluginConfigFile,
  RESTORE_FILE,
} from '@ankorcs/protocol/container';

const plugin = JSON.parse(execFileSync(
  'dotnet',
  ['run', '--project', 'packages/plugin/replay/Ankor.Replay.csproj', '--', 'constants'],
  { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] },
));

const absolute = (relative) => `${GAME_INSTALL_DIR}/${relative}`;
const pairs = (list) => list.map(([type, mode]) => `${type},${mode}`).sort();

const expected = {
  ANKOR_ENV: { wsUrl: ANKOR_ENV.wsUrl, allocationId: ANKOR_ENV.allocationId, debugCaptureDir: ANKOR_ENV.debugCaptureDir },
  CS2_ENV: { startMap: CS2_ENV.startMap, workshopMap: CS2_ENV.workshopMap },
  GAME_INSTALL_DIR,
  ANKOR_DIR: absolute(ANKOR_DIR),
  DEMO_DIR: absolute(DEMO_DIR),
  'DEMO_DIR (relative)': DEMO_DIR,
  BACKUP_DIR: absolute(BACKUP_DIR),
  RESTORE_FILE: absolute(RESTORE_FILE),
  MATCH_CONFIG_FILE: absolute(MATCH_CONFIG_FILE),
  DEMO_READY_SUFFIX,
  MAX_FRAME_BYTES,
  CFG_DIR,
  CFG_NAMES,
  PROTOCOL_VERSION,
  PROTOCOL_HEADER,
  CommandRefusal: CommandRefusal.options,
  MATCH_BLOCKED_COMMANDS,
  PluginConfigFile: Object.fromEntries(Object.entries(PluginConfigFile.shape)
    .map(([section, schema]) => [section, Object.keys(schema.shape)])),
  GAME_MODE_ARGS: pairs(Object.values(GAME_MODE_ARGS).map(({ gameType, gameMode }) => [gameType, gameMode])),
  MAX_BACKUP_FILE_CHARS,
  MAX_DAMAGE,
  MAX_ROUND_SHOTS,
  MAX_PLAYER_NAME_CHARS,
  MAX_LOG_MESSAGE_CHARS,
  MAP_NAME_MAX_CHARS,
  CONFIG_ERROR_MAX_CHARS,
  MAX_VERSION_CHARS,
  MAX_DISCONNECT_REASON_CHARS,
  MAX_SPECTATORS,
  MAX_SERVER_PLAYERS,
  MAX_TEAM_NAME_LENGTH,
  backupFileName: Object.fromEntries(Object.keys(plugin.backupFileName)
    .map((rounds) => [rounds, backupFileName(Number(rounds))])),
};
const actual = { ...plugin, GAME_MODE_ARGS: pairs(plugin.GAME_MODE_ARGS) };

const mismatches = Object.entries(expected)
  .filter(([key, value]) => JSON.stringify(actual[key]) !== JSON.stringify(value))
  .map(([key, value]) => `${key}: the contract has ${JSON.stringify(value)}, the plugin ${JSON.stringify(actual[key])}`);

if (!DEMO_NAME.test(plugin.DEMO_NAME)) {
  mismatches.push(`DEMO_NAME: the plugin names a demo ${plugin.DEMO_NAME}, which ${DEMO_NAME} rejects`);
}

const unchecked = Object.keys(plugin).filter((key) => !(key in expected) && key !== 'DEMO_NAME');

if (unchecked.length > 0) {
  mismatches.push(`the plugin prints values this script does not check: ${unchecked.join(', ')}`);
}

// The fixture is what controllers ingest as the plugin's output, so each frame must be one the
// contract accepts. The plugin's tests keep it up to date with the scenario.
const fixture = readFileSync('packages/plugin/tests/fixtures/plugin-replay.jsonl', 'utf8').split('\n').filter(Boolean);

fixture.forEach((line, index) => {
  const parsed = PluginToControllerMessage.safeParse(JSON.parse(line));

  if (!parsed.success) {
    mismatches.push(`plugin-replay.jsonl line ${index + 1} fails the contract: ${parsed.error.issues.map((issue) => `${issue.path.join('.')}: ${issue.message}`).join('; ')}`);
  }
});

if (mismatches.length > 0) {
  console.error(`The plugin disagrees with packages/protocol:\n- ${mismatches.join('\n- ')}`);
  process.exit(1);
}

console.log(`The plugin matches packages/protocol on ${Object.keys(expected).length + 1} values, and its replay fixture's ${fixture.length} frames pass the contract.`);
