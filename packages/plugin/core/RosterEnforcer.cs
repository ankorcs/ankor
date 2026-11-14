using Ankor.Core.Wire;

namespace Ankor.Core;

public enum RosterVerdict
{
    Place,

    Spectate,

    Kick,
}

/// <summary>Side is the lobby team's current playing side, or Spec for anyone not placed on a team.</summary>
public sealed record RosterPlacement(RosterVerdict Verdict, Side Side);

/// <summary>Resolve rosters against the supplied team-1 side so halftime swaps need no stored side state.</summary>
public sealed class RosterEnforcer
{
    private readonly Dictionary<string, int> _players = new(StringComparer.Ordinal);
    private readonly HashSet<string> _spectators = new(StringComparer.Ordinal);
    private readonly string? _team1Captain;
    private readonly string? _team2Captain;

    public RosterEnforcer(Roster roster)
    {
        Team1Name = roster.team1.name;
        Team2Name = roster.team2.name;
        _team1Captain = roster.team1.players.Count > 0 ? roster.team1.players[0] : null;
        _team2Captain = roster.team2.players.Count > 0 ? roster.team2.players[0] : null;

        foreach (var id in roster.team1.players) _players[id] = 1;
        foreach (var id in roster.team2.players) _players[id] = 2;
        foreach (var id in roster.spectators) _spectators.Add(id);
    }

    public string Team1Name { get; }
    public string Team2Name { get; }

    /// <summary>The name of lobby team 1 or 2. Spectators have no team name.</summary>
    public string TeamName(int team) => team switch
    {
        1 => Team1Name,
        2 => Team2Name,
        _ => throw new ArgumentOutOfRangeException(nameof(team), team, "only lobby teams 1 and 2 have names"),
    };

    /// <summary>The lobby sends each team in captain-first order, so index 0 is that team's captain.</summary>
    public string? Captain(int team) => team switch
    {
        1 => _team1Captain,
        2 => _team2Captain,
        _ => null,
    };

    public int? CaptainTeam(string steamId)
    {
        if (steamId == _team1Captain) return 1;
        if (steamId == _team2Captain) return 2;
        return null;
    }

    public bool IsCaptain(string steamId, int team) => Captain(team) == steamId;

    public int PlayerCount => _players.Count;

    public IReadOnlyCollection<string> Spectators => _spectators;

    public bool IsSpectator(string steamId) => _spectators.Contains(steamId);

    public IEnumerable<string> TeamPlayers(int team) =>
        _players.Where(entry => entry.Value == team).Select(entry => entry.Key);

    public int? LobbyTeam(string steamId) =>
        _players.TryGetValue(steamId, out var team) ? team : null;

    public static Side SideForTeam(int team, Side team1Side) =>
        team == 1 ? team1Side : team1Side.Opposite();

    public static int? TeamForSide(Side side, Side team1Side) => side switch
    {
        Side.Spec => null,
        _ => side == team1Side ? 1 : 2,
    };

    /// <summary>Resolve roster placement; bot exemption is handled by the caller.</summary>
    public RosterPlacement Resolve(string steamId, Side team1Side)
    {
        var team = LobbyTeam(steamId);
        if (team is not null)
        {
            return new RosterPlacement(RosterVerdict.Place, SideForTeam(team.Value, team1Side));
        }

        if (_spectators.Contains(steamId)) return new RosterPlacement(RosterVerdict.Spectate, Side.Spec);

        return new RosterPlacement(RosterVerdict.Kick, Side.Spec);
    }

    /// <summary>Require someone from each rostered team for the controller's start_match.</summary>
    public bool BothTeamsPresent(IEnumerable<HostPlayer> connected)
    {
        var teams = new HashSet<int>();
        foreach (var p in connected)
        {
            if (p.IsBot) continue;
            if (LobbyTeam(p.SteamIdString) is { } team) teams.Add(team);
        }

        return teams.Contains(1) && teams.Contains(2);
    }

    public IReadOnlyList<string> MissingPlayers(IEnumerable<HostPlayer> connected)
    {
        var present = new HashSet<string>(StringComparer.Ordinal);
        foreach (var p in connected)
        {
            if (!p.IsBot) present.Add(p.SteamIdString);
        }

        return _players.Keys.Where(id => !present.Contains(id)).ToList();
    }
}
