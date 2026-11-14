using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>Warmup readiness per lobby team. In auto and external mode presence is readiness; manual mode also
/// needs each rostered player's !ready, which lasts until they leave or warmup restarts, and, when the
/// match asks for it, the rostered spectators' !ready too. Under manual, a captain's !forceready readies
/// their own team with whoever of it is connected, and a rostered spectator's readies the spectators.
/// </summary>
public sealed class ReadyUp(RosterEnforcer roster, ReadyMode mode, bool spectatorsReadyUp)
{
    private readonly HashSet<string> _ready = new(StringComparer.Ordinal);
    private readonly HashSet<int> _forced = [];
    private bool _spectatorsForced;

    public bool Manual => mode == ReadyMode.Manual;

    public bool External => mode == ReadyMode.External;

    public bool WaitsForSpectators => Manual && spectatorsReadyUp;

    public void Clear()
    {
        _ready.Clear();
        _forced.Clear();
        _spectatorsForced = false;
    }

    public bool Mark(string steamId) => _ready.Add(steamId);

    public bool Unmark(string steamId) => _ready.Remove(steamId);

    public bool Force(int team) => _forced.Add(team);

    public bool ForceSpectators()
    {
        if (_spectatorsForced) return false;
        _spectatorsForced = true;
        return true;
    }

    /// <summary>Whether a force-ready covers the player, which !unready cannot withdraw.</summary>
    public bool Forced(string steamId) =>
        roster.LobbyTeam(steamId) is { } team && _forced.Contains(team) || _spectatorsForced && roster.IsSpectator(steamId);

    public void Retain(IEnumerable<HostPlayer> connected) => _ready.IntersectWith(Present(connected));

    public bool IsReady(string steamId) =>
        !Manual || _ready.Contains(steamId)
        || roster.LobbyTeam(steamId) is { } team && _forced.Contains(team)
        || _spectatorsForced && roster.IsSpectator(steamId);

    /// <summary>A team of nobody connected is never ready, force-readied or not.</summary>
    public bool TeamReady(int team, IEnumerable<HostPlayer> connected)
    {
        var present = Present(connected);
        var players = roster.TeamPlayers(team).ToList();
        if (!players.Any(present.Contains)) return false;
        return _forced.Contains(team) || players.All(id => present.Contains(id) && IsReady(id));
    }

    /// <summary>Spectators play no part in the match itself, so only a match that asks for it waits for them.</summary>
    public bool SpectatorsReady(IEnumerable<HostPlayer> connected)
    {
        if (!WaitsForSpectators || _spectatorsForced) return true;
        var present = Present(connected);
        return roster.Spectators.All(id => present.Contains(id) && IsReady(id));
    }

    /// <summary>Everyone the ready tally counts: the players, and the spectators when the match waits for them.</summary>
    public int ReadyTotal => roster.PlayerCount + (WaitsForSpectators ? roster.Spectators.Count : 0);

    public int ReadyCount(IEnumerable<HostPlayer> connected) =>
        Present(connected).Count(id => Counted(id) && IsReady(id));

    /// <summary>Connected people still to type !ready.</summary>
    public IReadOnlyList<HostPlayer> Unready(IEnumerable<HostPlayer> connected) => connected
        .Where(p => !p.IsBot && Counted(p.SteamIdString) && !IsReady(p.SteamIdString))
        .ToList();

    /// <summary>Whose !ready counts: a rostered player, or a rostered spectator when the match waits for them.</summary>
    public bool Counted(string steamId) =>
        roster.LobbyTeam(steamId) is not null || WaitsForSpectators && roster.IsSpectator(steamId);

    /// <summary>Each minute from five minutes left, then every 30 seconds.</summary>
    public static bool ShouldWarn(int secondsLeft) =>
        secondsLeft > 0 && ((secondsLeft >= 300 && secondsLeft % 60 == 0) || (secondsLeft < 300 && secondsLeft % 30 == 0));

    public static string FormatTime(int seconds) => $"{Math.Max(0, seconds) / 60}:{Math.Max(0, seconds) % 60:D2}";

    private static HashSet<string> Present(IEnumerable<HostPlayer> connected) =>
        connected.Where(p => !p.IsBot).Select(p => p.SteamIdString).ToHashSet(StringComparer.Ordinal);
}
