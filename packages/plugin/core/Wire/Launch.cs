namespace Ankor.Core.Wire;

/// <summary>
/// The C# side of <c>packages/protocol/src/container.ts</c>: what the agent hands a game
/// container and what this plugin expects to find there. Every value here has the same name and
/// the same content on both sides, which <c>scripts/plugin.sh contract</c> checks.
/// </summary>
public static class AnkorEnv
{
    /// <summary>The local relay this plugin dials for its slot.</summary>
    public const string WsUrl = "ANKOR_WS_URL";
    /// <summary>The server's allocation, which identifies its events; without one the plugin does nothing.</summary>
    public const string AllocationId = "ANKOR_ALLOCATION_ID";

    /// <summary>A lowercase UUID, as the wire's <c>AllocationId</c>; null for anything else.</summary>
    public static string? ParseAllocationId(string? raw) =>
        raw is not null && System.Text.RegularExpressions.Regex.IsMatch(raw, "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\\z") ? raw : null;

    public static Uri? ParseWsUrl(string raw) =>
        Uri.TryCreate(raw.Trim(), UriKind.Absolute, out var url) ? url : null;

    public const string DebugCaptureDir = "ANKOR_DEBUG_CAPTURE_DIR";
}

/// <summary>
/// The directories every other path builds on. The engine's file commands resolve against the game
/// directory, and the agent reads files relative to the install directory.
/// </summary>
public static class ServerPaths
{
    public const string InstallDir = "/home/steam/cs2-dedicated";
    /// <summary>The game directory, relative to the install directory.</summary>
    public const string GameSubdir = "game/csgo";
    public const string GameDir = InstallDir + "/" + GameSubdir;
    /// <summary><c>ANKOR_DIR</c>, the slot's directory that the agent lays out, relative to the game directory.</summary>
    public const string AnkorSubdir = "ankor";
    public const string AnkorDir = GameDir + "/" + AnkorSubdir;
}

/// <summary>A serialized <see cref="MatchConfig"/> on every match server, also one without a match id; empty on a server without a match.</summary>
public static class MatchConfigFile
{
    public const string Path = ServerPaths.AnkorDir + "/match.json";

    // A missing or blank file means a server without a match, which is not an error.
    public static MatchConfig? Decode(string? json, string? readError, out string? error)
    {
        error = readError;
        if (!WireJson.TryDecodeMatchConfig(json, out var config, out var configError) && !string.IsNullOrWhiteSpace(json))
            error = configError;
        return config;
    }
}

/// <summary>The image entrypoint's environment: the map it boots and the workshop map, which concern the plugin.</summary>
public static class Cs2Env
{
    public const string StartMap = "CS2_STARTMAP";
    public const string WorkshopMap = "CS2_WORKSHOP_MAP";
}

/// <summary>Where recordings land and how the agent reads their names back.</summary>
public static class Demos
{
    /// <summary><c>tv_record</c> resolves its argument against the game directory.</summary>
    public const string RecordDir = ServerPaths.AnkorSubdir + "/demos";
    /// <summary>The same directory relative to the install directory, which is how the agent reads it.</summary>
    public const string Dir = ServerPaths.GameSubdir + "/" + RecordDir;
    public const string Path = ServerPaths.GameDir + "/" + RecordDir;
    /// <summary>Appended to a finished recording; the agent uploads a <c>.dem</c> only once it exists.</summary>
    public const string ReadySuffix = ".ready";
    /// <summary>The stamp that opens a demo name, matching the agent's <c>DEMO_NAME</c> regex.</summary>
    public const string TimestampFormat = "yyyyMMdd-HHmmss";
}

/// <summary>Where the engine writes its round backups; the agent empties it at every launch.</summary>
public static class Backups
{
    /// <summary>Relative to the game directory, which the engine's backup commands resolve against.</summary>
    public const string Dir = ServerPaths.AnkorSubdir + "/backups";
    public const string Path = ServerPaths.GameDir + "/" + Dir;
    public const string FilePrefix = "ankor";
    /// <summary><c>mp_backup_round_file</c>.</summary>
    public const string RoundFile = Dir + "/" + FilePrefix;
    /// <summary>The engine's command that loads a backup, hooked so a load from any console is followed.</summary>
    public const string LoadCommand = "mp_backup_restore_load_file";

    // The console matches command names regardless of case.
    public static bool IsLoadCommand(string? name) => string.Equals(name, LoadCommand, StringComparison.OrdinalIgnoreCase);

    /// <summary>The engine numbers a backup by the rounds played before it, in two digits or more.</summary>
    public static string FileName(int roundsPlayed) => $"{FilePrefix}_round{roundsPlayed:D2}.txt";

    /// <summary>The backup that starts <paramref name="round"/>, which the engine wrote after the round before it.</summary>
    public static string FileNameForRound(int round) => FileName(round - 1);

    /// <summary>The launch's restore point as the agent wrote it (<c>RESTORE_FILE</c> in container.ts), for <c>mp_backup_restore_load_file</c>, which resolves against the game directory.</summary>
    public const string RestoreFile = ServerPaths.AnkorSubdir + "/restore.txt";
    public const string RestorePath = ServerPaths.GameDir + "/" + RestoreFile;
}

/// <summary>Keys of the config file the agent renders over the image's copy.</summary>
public static class PluginConfigKeys
{
    public const string ChatSection = "Chat";
    public const string ChatTag = "Tag";
    public const string ChatTagColor = "TagColor";
}

/// <summary>
/// The cfgs the plugin execs: the image's under <c>cfg/ankor/</c>, the launch's that the agent
/// renders, and Valve's for the game mode. <c>CFG_NAMES</c> in container.ts.
/// </summary>
public static class CfgNames
{
    public const string Dir = "ankor";
    /// <summary>Ankor's own settings for a match, over the game mode's cfg.</summary>
    public const string Base = Dir + "/base";
    /// <summary>Puts what warmup and the knife round change back to the engine's defaults, ahead of the mode's cfg.</summary>
    public const string Reset = Dir + "/reset";
    public const string Warmup = Dir + "/warmup";
    public const string Knife = Dir + "/knife";
    /// <summary><c>cfg/ankor_launch.cfg</c>, rendered by the agent per launch: its rules, GOTV settings, cfgs and commands.</summary>
    public const string Launch = "ankor_launch";

    /// <summary>The console command the plugin registers, which <see cref="Applied"/> runs at its end.</summary>
    public const string AppliedCommand = "ankor_cfg_applied";

    /// <summary>The cfg the plugin execs after the cfgs it waits for; it runs <see cref="AppliedCommand"/>.</summary>
    public const string Applied = Dir + "/applied";

    /// <summary>
    /// Valve's cfg for a <c>game_type</c> and <c>game_mode</c>, as gamemodes.txt names it, for each
    /// pair that <c>GAME_MODE_ARGS</c> in container.ts passes. The engine runs it on every map load;
    /// in a match, the plugin runs it again at the side choice and when the match goes live, to put
    /// back what warmup and the knife round changed. Null for a pair this table does not know.
    /// </summary>
    public static string? Mode(int gameType, int gameMode) => (gameType, gameMode) switch
    {
        (0, 0) => "gamemode_casual",
        (0, 1) => "gamemode_competitive",
        (0, 2) => "gamemode_competitive2v2",
        (0, 6) => "gamemode_rush",
        (1, 0) => "gamemode_armsrace",
        (1, 1) => "gamemode_demolition",
        (1, 2) => "gamemode_deathmatch",
        (3, 0) => "gamemode_custom",
        _ => null,
    };

    /// <summary>
    /// One console line that execs <paramref name="cfgs"/> in order. CS2 runs the line in one pass of
    /// its command buffer, on a single tick, so no other command or player sees the cfgs half applied.
    /// </summary>
    public static string Exec(params string[] cfgs) => string.Join("; ", cfgs.Select(cfg => $"exec {cfg}"));
}

/// <summary>The wire's version, the <c>major.minor</c> of <c>@ankorcs/protocol</c>, from <c>packages/protocol/src/paths.ts</c>.</summary>
public static class Protocol
{
    public const string Version = "0.9";
    public const string Header = "Ankor-Protocol";
}

public static class WireLimits
{
    /// <summary>Largest frame the local event socket carries, on this side and the relay's.</summary>
    public const int MaxFrameBytes = 2 << 20;
}
