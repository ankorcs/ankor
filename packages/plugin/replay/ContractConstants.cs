using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Ankor.Tests;

/// <summary>
/// The plugin's copy of the values that packages/protocol defines, as JSON for
/// scripts/plugin-contract.mjs, which compares them with the contract. Each key names the
/// contract's export.
/// </summary>
internal static class ContractConstants
{
    public static string Json()
    {
        // The plugin names a demo through DemoRecorder, so a real recording start gives the name the
        // agent must accept.
        var host = new FakeGameHost { MapName = "workshop/123456/de_example v2" };
        using var recorder = new DemoRecorder(host, Fixture.Config(), (_, _) => { });
        recorder.Start();

        return JsonSerializer.Serialize(new Dictionary<string, object>
        {
            ["ANKOR_ENV"] = new Dictionary<string, string>
            {
                ["wsUrl"] = AnkorEnv.WsUrl,
                ["allocationId"] = AnkorEnv.AllocationId,
                ["debugCaptureDir"] = AnkorEnv.DebugCaptureDir,
            },
            ["CS2_ENV"] = new Dictionary<string, string>
            {
                ["startMap"] = Cs2Env.StartMap,
                ["workshopMap"] = Cs2Env.WorkshopMap,
            },
            ["GAME_INSTALL_DIR"] = ServerPaths.InstallDir,
            // The contract's directories are relative to GAME_INSTALL_DIR; the plugin's absolute ones
            // are compared after the script prefixes them.
            ["ANKOR_DIR"] = ServerPaths.AnkorDir,
            ["DEMO_DIR"] = Demos.Path,
            ["DEMO_DIR (relative)"] = Demos.Dir,
            ["BACKUP_DIR"] = Backups.Path,
            ["RESTORE_FILE"] = Backups.RestorePath,
            ["MATCH_CONFIG_FILE"] = MatchConfigFile.Path,
            ["DEMO_READY_SUFFIX"] = Demos.ReadySuffix,
            ["MAX_FRAME_BYTES"] = WireLimits.MaxFrameBytes,
            ["CFG_DIR"] = CfgNames.Dir,
            ["CFG_NAMES"] = new Dictionary<string, string>
            {
                ["base"] = CfgNames.Base,
                ["warmup"] = CfgNames.Warmup,
                ["knife"] = CfgNames.Knife,
                ["launch"] = CfgNames.Launch,
            },
            ["PROTOCOL_VERSION"] = Protocol.Version,
            ["PROTOCOL_HEADER"] = Protocol.Header,
            ["CommandRefusal"] = new[]
            {
                CommandResult.NoMatch, CommandResult.NotInWarmup, CommandResult.TeamEmpty,
                CommandResult.NoBackup, CommandResult.NotLive, CommandResult.AtHalftime, CommandResult.BetweenRounds, CommandResult.CommandBlocked,
                CommandResult.UnsupportedRequest,
            },
            ["MATCH_BLOCKED_COMMANDS"] = AgentControl.MatchBlockedCommands,
            ["PluginConfigFile"] = new Dictionary<string, object>
            {
                [PluginConfigKeys.ChatSection] = new[] { PluginConfigKeys.ChatTag, PluginConfigKeys.ChatTagColor },
            },
            // The game modes the plugin has a cfg for, as (game_type, game_mode) pairs. The range covers
            // every pair gamemodes.txt numbers, rush's game_mode 6 included.
            ["GAME_MODE_ARGS"] = Enumerable.Range(0, 8)
                .SelectMany(type => Enumerable.Range(0, 8).Select(mode => (type, mode)))
                .Where(pair => CfgNames.Mode(pair.type, pair.mode) is not null)
                .Select(pair => new[] { pair.type, pair.mode })
                .ToArray(),
            // The wire's size limits, which the plugin applies before it sends.
            ["MAX_BACKUP_FILE_CHARS"] = RoundBackup.MaxFileChars,
            ["MAX_DAMAGE"] = PlayerHurt.MaxDamage,
            ["MAX_ROUND_SHOTS"] = RoundShots.MaxShots,
            ["MAX_PLAYER_NAME_CHARS"] = WireText.MaxPlayerName,
            ["MAX_LOG_MESSAGE_CHARS"] = WireText.MaxLogMessage,
            ["MAP_NAME_MAX_CHARS"] = WireText.MaxMapName,
            ["CONFIG_ERROR_MAX_CHARS"] = WireText.MaxConfigError,
            ["MAX_VERSION_CHARS"] = WireText.MaxVersion,
            ["MAX_DISCONNECT_REASON_CHARS"] = WireText.MaxDisconnectReason,
            ["MAX_SPECTATORS"] = Roster.MaxSpectators,
            ["MAX_SERVER_PLAYERS"] = Roster.MaxPlayers,
            ["MAX_TEAM_NAME_LENGTH"] = Roster.MaxTeamNameLength,
            ["backupFileName"] = new[] { 0, 9, 13, 99, 100 }.ToDictionary(n => n.ToString(), Backups.FileName),
            ["DEMO_NAME"] = recorder.Name + ".dem",
        }, new JsonSerializerOptions { WriteIndented = true });
    }
}
