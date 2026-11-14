using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>World-space origin in engine units, read from the pawn at snapshot time.</summary>
public readonly record struct WorldPosition(float X, float Y, float Z);

/// <summary>View direction in degrees (pitch down-positive, yaw counter-clockwise), read from the pawn at snapshot time.</summary>
public readonly record struct ViewAngles(float Pitch, float Yaw);

/// <summary>Player snapshot without engine handles; remains readable after the originating frame.</summary>
public sealed record HostPlayer(
    int Slot,
    ulong SteamId,
    string Name,
    Side Side,
    int Health,
    bool IsAlive,
    bool IsBot)
{
    /// <summary>
    /// Whether Steam has verified <see cref="SteamId"/>. Until it has, the id is the one the client
    /// claims, which the match trusts until Steam fails to verify it in time. False unless the host
    /// says otherwise, so that a player built without it waits for Steam.
    /// </summary>
    public bool Authorized { get; init; }

    /// <summary>Engine id used by kickid, distinct from Slot; zero when unknown.</summary>
    public int UserId { get; init; }

    /// <summary>Pawn origin at snapshot time; null when the pawn was not available.</summary>
    public WorldPosition? Position { get; init; }

    /// <summary>Pawn eye angles at snapshot time; null when the pawn was not available.</summary>
    public ViewAngles? Angles { get; init; }

    /// <summary>The engine's own figure for what the pawn carries (m_unCurrentEquipmentValue); zero without a pawn.</summary>
    public int EquipmentValue { get; init; }

    /// <summary>Engine money service values from the player controller.</summary>
    public int Cash { get; init; }
    public int StartCash { get; init; }
    public int CashSpent { get; init; }

    /// <summary>Pawn is standing in a buy zone (m_bInBuyZone); false without a pawn.</summary>
    public bool InBuyZone { get; init; }

    public string SteamIdString => SteamId.ToString();

    // A disconnect can arrive after the player object is gone.
    public static HostPlayer Departed(int slot, ulong steamId, string? name) => new(
        Slot: slot,
        SteamId: steamId,
        Name: name ?? "",
        Side: Side.Spec,
        Health: 0,
        IsAlive: false,
        IsBot: steamId == 0);

    /// <summary>Exclude bots and unauthorized humans from wire events. Match logic still receives their snapshots.</summary>
    public bool IsReportable => !IsBot && SteamId != 0;
}

public interface IHostTimer
{
    bool IsCancellationRequested { get; }
    void Cancel();
}

/// <summary>Game-thread interface for match logic; implemented by SwiftlyGameHost.</summary>
public interface IGameHost
{
    /// <summary>Unix milliseconds. Goes on the wire as <c>timestamp</c>.</summary>
    long NowMs { get; }

    /// <summary>Engine tick plus its sub-tick fraction, captured on the game thread.</summary>
    double SimulationTick { get; }
    /// <summary>Engine elapsed round time in seconds, zero during freeze time.</summary>
    double RoundElapsedTime { get; }

    /// <summary>Map clock in seconds for round timing and effect lifetimes; may reset on map change.</summary>
    double CurrentTime { get; }

    string MapName { get; }
    /// <summary>The game server is logged on to Steam; false while the Steam API is not up.</summary>
    bool SteamLoggedOn { get; }
    /// <summary>The SteamID64 that Steam gave the server's GSLT; null until the server logs on with one.</summary>
    string? ServerSteamId { get; }
    string ServerVersion { get; }
    string FrameworkVersion { get; }

    IReadOnlyList<HostPlayer> Players { get; }

    /// <summary>Roster reads fail closed: disconnect an unreadable connection immediately.
    /// Missing connections and GOTV are ignored; no native handle survives this call.</summary>
    HostPlayer? ReadPlayerOrKick(int slot, string reason);
    IReadOnlyList<HostPlayer> ReadPlayersOrKick(string reason);

    /// <summary>Read once at round boundaries; null means unavailable, not empty inventory.</summary>
    PlayerInventory? ReadInventory(HostPlayer player);

    void ExecuteCommand(string command);

    /// <summary>Runs the action on the next game frame. The only safe path in from the WS thread.</summary>
    void NextFrame(Action action);

    /// <summary>
    /// Runs the action once the engine has run every command queued before this call, the lines of
    /// the cfgs they exec included, nested execs too: the host execs <see cref="Wire.CfgNames.Applied"/> behind them.
    /// </summary>
    void AfterCommands(Action action);

    void PrintToChatAll(string message);
    void PrintToChat(int slot, string message);

    bool IsWarmup { get; }

    /// <summary>Engine round counter used to derive one-based wire rounds.</summary>
    int RoundsPlayed { get; }

    /// <summary>The scoreboard's score of a playing side; null when the engine has no team for it.</summary>
    int? ReadTeamScore(Side side);

    /// <summary>True while the engine is applying a round backup it was told to load.</summary>
    bool LoadingRoundBackup { get; }

    /// <summary>
    /// Moves a player between playing sides, keeping them alive and armed (used for team lock).
    /// Spectator is not a valid target here — use <see cref="ChangeTeam"/>.
    /// </summary>
    void SwitchTeam(HostPlayer player, Side side);

    /// <summary>
    /// Moves a player the way the <c>jointeam</c> command does (kills them). The only way into
    /// spectator.
    /// </summary>
    void ChangeTeam(HostPlayer player, Side side);

    void Kick(HostPlayer player, string reason);

    /// <summary>
    /// Ends the match as aborted, with the end screen and no winner, until the server quits. Call it
    /// after warmup, with no team intro running: the client shows no end screen otherwise.
    /// </summary>
    void AbortMatch();

    /// <summary>Scoreboard clan tag; empty clears it.</summary>
    void SetClanTag(HostPlayer player, string tag);

    /// <summary>
    /// Timers normally belong to the current map; opt out for work spanning maps. A delay of 0 runs
    /// the callback on the next tick.
    /// </summary>
    IHostTimer AddTimer(double seconds, Action callback, bool stopOnMapChange = true);

    /// <summary>
    /// Runs <paramref name="callback"/> after <paramref name="delaySeconds"/>, then every
    /// <paramref name="periodSeconds"/>, on the scheduler's fixed cadence until cancelled. Use it for
    /// anything that repeats: a one-shot timer that schedules its successor from its own callback
    /// fires a frame late each time, and the lateness adds up.
    /// </summary>
    IHostTimer AddRepeatingTimer(double delaySeconds, double periodSeconds, Action callback, bool stopOnMapChange = true);

    void SetConVarString(string name, string value);
    /// <summary>The convar's value as text; null when the convar is missing or cannot be read.</summary>
    string? GetConVarString(string name);

    int? GetConVarInt(string name);
    bool? GetConVarBool(string name);
    /** Float-valued convars such as mp_round_restart_delay. */
    double? GetConVarNumber(string name);

    void Log(LogLevel level, string message);
}
