using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>Coordinate roster enforcement across player lifecycle callbacks.
/// See NOTICE for hook/move ordering attribution.
/// </summary>
public sealed class TeamLock(
    MatchState match,
    IGameHost host,
    IMessages messages) : IDisposable
{
    private readonly HashSet<int> _kicked = [];

    /// <summary>
    /// Seconds after player_connect_full a player placed by a claimed SteamID has for Steam to verify
    /// it before the kick. SwiftlyS2 raises no event when Steam rejects a claim it never verified, so
    /// this covers a rejection as well as a Steam that never answers.
    /// </summary>
    public const double AuthorizationGraceSeconds = 10;

    /// <summary>The kick timers of clients that fully connected before Steam verified them, by slot.</summary>
    private readonly Dictionary<int, IHostTimer> _pending = [];

    public HostPlayer? ReadPlayer(int slot) =>
        host.ReadPlayerOrKick(slot, messages.Get("kick.player_unreadable"));

    /// <summary>The caller supplies the Steam-authorized identity for roster checks.</summary>
    public void OnClientAuthorized(HostPlayer player)
    {
        if (player.IsBot) return;

        // SwiftlyS2 verifies the SteamID the client claimed, so the placement stands.
        Verified(player.Slot);

        // Placement waits for player_connect_full; only a kick runs now.
        if (match.Phase == MatchPhase.PostLive || match.Canceled ||
            match.Roster.Resolve(player.SteamIdString, match.Team1PlacedSide).Verdict == RosterVerdict.Kick)
        {
            Apply(player);
        }
    }

    /// <summary>Place or kick a player; return true when rejected.</summary>
    public bool Apply(HostPlayer player)
    {
        if (player.IsBot) return false;

        if (match.Canceled)
        {
            host.Kick(player, messages.Get("kick.ready_timeout"));
            return true;
        }

        if (match.Phase == MatchPhase.PostLive)
        {
            host.Kick(player, messages.Get("kick.match_ended"));
            return true;
        }

        var placement = match.Roster.Resolve(player.SteamIdString, match.Team1PlacedSide);

        if (placement.Verdict == RosterVerdict.Kick)
        {
            if (_kicked.Add(player.Slot))
            {
                host.Log(LogLevel.Warn,
                    $"{player.SteamIdString} ({player.Name}) is not on the roster; kicking");
                host.Kick(player, messages.Get("kick.not_on_roster"));
            }

            return true;
        }

        if (match.Phase == MatchPhase.PendingRestore) return false;
        if (player.Side != placement.Side) Force(player, placement.Side);
        return false;
    }

    /// <summary>Steam verified the slot's client: its kick timer, if any, ends.</summary>
    private void Verified(int slot)
    {
        if (_pending.Remove(slot, out var timer)) timer.Cancel();
    }

    /// <summary>
    /// Places a client at player_connect_full, and starts its kick timer when Steam has not verified
    /// the SteamID it claimed. Returns true when rejected.
    /// </summary>
    public bool Connect(HostPlayer player)
    {
        var rejected = Apply(player);
        if (rejected || player.IsBot || player.Authorized || _pending.ContainsKey(player.Slot)) return rejected;

        // A raw timer: SwiftlyS2 skips a cancelled one even when it is due, and verification,
        // Forget and Dispose cancel it. A map change does not: a client that stays connected keeps
        // its deadline, and one that disconnects is forgotten.
        _pending[player.Slot] = host.AddTimer(AuthorizationGraceSeconds, () =>
        {
            host.Log(LogLevel.Warn, $"Steam did not verify slot {player.Slot} ({player.Name}) as {player.SteamIdString}; kicking");
            host.Kick(player, messages.Get("kick.not_authorized"));
        }, stopOnMapChange: false);
        return false;
    }

    private void Force(HostPlayer player, Side side)
    {
        if (side == Side.Spec) host.ChangeTeam(player, side);
        else host.SwitchTeam(player, side);
    }

    /// <summary>Recognize commands that could bypass roster placement, including the engine's own
    /// <c>coach</c>, which would take a player off their team.</summary>
    public static bool IsTeamChangeCommand(string commandLine)
    {
        var name = commandLine.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries).FirstOrDefault()?.Trim('"') ?? "";
        return name.Equals("jointeam", StringComparison.OrdinalIgnoreCase)
            || name.Equals("spectate", StringComparison.OrdinalIgnoreCase)
            || name.Equals("coach", StringComparison.OrdinalIgnoreCase);
    }

    /// <summary>Block manual team changes and restore the rostered side when needed.</summary>
    public bool BlockTeamChange(HostPlayer player)
    {
        if (player.IsBot) return false;

        var placement = match.Roster.Resolve(player.SteamIdString, match.Team1PlacedSide);

        if (placement.Verdict == RosterVerdict.Kick) return true;

        if (match.Phase != MatchPhase.PendingRestore && player.Side != placement.Side) Force(player, placement.Side);

        return true;
    }

    public void OnPlayerTeam(HostPlayer player, Side to)
    {
        if (match.Phase == MatchPhase.PostLive) return;
        if (player.IsBot) return;

        var placement = match.Roster.Resolve(player.SteamIdString, match.Team1PlacedSide);
        if (placement.Verdict == RosterVerdict.Kick) return;

        if (to == placement.Side) return;

        // A backup being loaded unseats everyone and seats them where its round had them; the
        // restored round's start takes the sides from where the engine put them.
        if (match.Phase == MatchPhase.PendingRestore) return;

        // Through the break nothing but the engine's swap moves a player onto their new side: every
        // placement goes to the old one. A correction here would land after the player spawned.
        if (match.SidesSwapPending && to == match.Roster.Resolve(player.SteamIdString, match.Team1Side).Side)
        {
            match.SidesSwapped();
            return;
        }

        host.Log(LogLevel.Info,
            $"{player.SteamIdString} landed on {to} but belongs on {placement.Side}; correcting");
        Force(player with { Side = to }, placement.Side);
    }

    /// <summary>Places every connected player.</summary>
    public void ReapplyAll()
    {
        foreach (var player in host.ReadPlayersOrKick(messages.Get("kick.player_unreadable")))
        {
            Apply(player);
        }
    }

    /// <summary>The slot's client left: its next client starts unverified and with no timer.</summary>
    public void Forget(int slot)
    {
        _kicked.Remove(slot);
        Verified(slot);
    }

    /// <summary>The match unloads: no kick timer outlives it.</summary>
    public void Dispose()
    {
        foreach (var timer in _pending.Values) timer.Cancel();
    }
}
