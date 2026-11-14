using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class TeamLockTests
{
    private readonly FakeGameHost _host = new();
    private readonly List<PluginMessage> _sent = [];
    private readonly MatchState _match;
    private readonly TeamLock _teams;

    public TeamLockTests()
    {
        _match = new MatchState(_host, Fixture.Config(SidesMode.Team1Ct), new FakeAgentChannel(_sent), Fixture.Messages, () => { });
        _teams = new TeamLock(_match, _host, Fixture.Messages);
    }

    [Fact]
    public void UnreadableRosterConnectionIsDisconnectedWithRetryReason()
    {
        var player = Fixture.Player(Fixture.Team1Player1, Side.T);
        _host.PlayerList.Add(player);
        _host.UnreadableSlots.Add(player.Slot);
        Assert.Null(_teams.ReadPlayer(player.Slot));
        var kick = Assert.Single(_host.Kicked);
        Assert.Equal(player, kick.Player);
        Assert.Equal(Fixture.Messages.Get("kick.player_unreadable"), kick.Reason);
        Assert.Empty(_host.SwitchedTeams);
        Assert.Null(_teams.ReadPlayer(99));
        Assert.Single(_host.Kicked);
    }

    [Fact]
    public void RosterReapplicationDisconnectsUnreadablePlayersAndPlacesReadablePlayers()
    {
        var unreadable = Fixture.Player(Fixture.Team1Player1, Side.T);
        var readable = Fixture.Player(Fixture.Team2Player1, Side.Ct, 1);
        _host.PlayerList.AddRange([unreadable, readable]);
        _host.UnreadableSlots.Add(unreadable.Slot);
        _teams.ReapplyAll();
        Assert.Equal(unreadable, Assert.Single(_host.Kicked).Player);
        Assert.Equal(readable, Assert.Single(_host.SwitchedTeams).Player);
        Assert.Equal(Side.T, _host.SwitchedTeams[0].Side);
        _host.UnreadableSlots.Clear();
        Assert.Equal(unreadable, _teams.ReadPlayer(unreadable.Slot));
    }

    /// <summary>While a backup loads, the engine seats everyone where the round had them; a correction would fight it.</summary>
    [Fact]
    public void ABackupBeingLoadedLeavesPlacementToTheEngine()
    {
        _match.EnterWarmup();
        _host.LoadingRoundBackup = true;
        _match.NoticeBackupLoading();
        Assert.Equal(MatchPhase.PendingRestore, _match.Phase);
        var player = Fixture.Player(Fixture.Team1Player1, Side.T);
        _host.PlayerList.Add(player);

        Assert.False(_teams.Apply(player));
        _teams.OnPlayerTeam(player, Side.T);
        Assert.True(_teams.BlockTeamChange(player));
        Assert.Empty(_host.SwitchedTeams);
        Assert.Empty(_host.ChangedTeams);

        _teams.OnClientAuthorized(Fixture.Player(Fixture.Stranger, Side.Spec, 5));
        Assert.Equal(Fixture.Stranger, Assert.Single(_host.Kicked).Player.SteamIdString);
    }

    [Fact]
    public void AuthorizationKicksStrangersButDefersRosteredPlacementUntilConnect()
    {
        var player = Fixture.Player(Fixture.Team1Player1, Side.T);
        _teams.OnClientAuthorized(player);
        Assert.Empty(_host.SwitchedTeams);
        Assert.False(_teams.Apply(player));
        Assert.Equal(Side.Ct, Assert.Single(_host.SwitchedTeams).Side);
        _teams.OnClientAuthorized(Fixture.Player(Fixture.Stranger, Side.Spec, 5));
        Assert.Equal(Fixture.Stranger, Assert.Single(_host.Kicked).Player.SteamIdString);
    }

    [Fact]
    public void DuplicateRejectionsKickOnceAndDisconnectReleasesTheSlot()
    {
        var stranger = Fixture.Player(Fixture.Stranger, Side.Spec);
        _teams.OnClientAuthorized(stranger);
        Assert.True(_teams.Apply(stranger));
        Assert.True(_teams.Apply(stranger));
        Assert.Single(_host.Kicked);
        _teams.Forget(stranger.Slot);
        Assert.True(_teams.Apply(stranger));
        Assert.Equal(2, _host.Kicked.Count);
    }

    [Fact]
    public void BotsAreNotPlacedOrKicked()
    {
        var bot = Fixture.Bot(Side.T);
        _teams.OnClientAuthorized(bot);
        Assert.False(_teams.Apply(bot));
        _teams.OnPlayerTeam(bot, Side.T);
        Assert.Empty(_host.Kicked);
        Assert.Empty(_host.SwitchedTeams);
        Assert.Empty(_sent);
    }

    /// <summary>A client that loads before Steam answers plays as the SteamID it claims, until Steam rules on it.</summary>
    [Fact]
    public void AClaimedSteamIdIsPlacedAndKeptOnceSteamVerifiesIt()
    {
        var claimed = Fixture.Player(Fixture.Team1Player1, Side.T) with { Authorized = false };
        Assert.False(_teams.Connect(claimed));
        Assert.Equal(Side.Ct, Assert.Single(_host.SwitchedTeams).Side);

        _teams.OnClientAuthorized(claimed with { Authorized = true });
        _host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Empty(_host.Kicked);
    }

    /// <summary>SwiftlyS2 raises nothing for a rejected claim, so the timer kicks a claim Steam rejects or never answers.</summary>
    [Fact]
    public void AClaimedSteamIdSteamDoesNotVerifyInTimeIsKicked()
    {
        var claimed = Fixture.Player(Fixture.Team1Player1, Side.T) with { Authorized = false };
        Assert.False(_teams.Connect(claimed));
        Assert.False(_teams.Connect(claimed));
        _host.Advance(TeamLock.AuthorizationGraceSeconds - 1);
        Assert.Empty(_host.Kicked);
        _host.Advance(1);
        var kick = Assert.Single(_host.Kicked);
        Assert.Equal(claimed, kick.Player);
        Assert.Equal(Fixture.Messages.Get("kick.not_authorized"), kick.Reason);
    }

    [Fact]
    public void AClientThatConnectsVerifiedIsNeverKicked()
    {
        Assert.False(_teams.Connect(Fixture.Player(Fixture.Team1Player1, Side.Ct)));
        _host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Empty(_host.Kicked);
    }

    /// <summary>Placing a client that is still loading, as a map load or restore does, starts no timer.</summary>
    [Fact]
    public void APlacementBeforeTheFullConnectStartsNoTimer()
    {
        var loading = Fixture.Player(Fixture.Team1Player1, Side.T) with { Authorized = false };
        _host.PlayerList.Add(loading);
        Assert.False(_teams.Apply(loading));
        _teams.ReapplyAll();
        _host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Empty(_host.Kicked);
    }

    [Fact]
    public void ABotStartsNoTimer()
    {
        Assert.False(_teams.Connect(Fixture.Bot(Side.T)));
        _host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Empty(_host.Kicked);
    }

    /// <summary>A map change keeps the player connected, so it keeps the timer too.</summary>
    [Fact]
    public void TheTimerOutlivesAMapChangeAndEndsWithTheMatch()
    {
        var player = Fixture.Player(Fixture.Team1Player1, Side.Ct) with { Authorized = false };
        Assert.False(_teams.Connect(player));
        _host.ChangeMap("de_inferno");
        _host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Equal(player, Assert.Single(_host.Kicked).Player);

        var other = Fixture.Player(Fixture.Team2Player1, Side.T, 1) with { Authorized = false };
        Assert.False(_teams.Connect(other));
        _teams.Dispose();
        _host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Single(_host.Kicked);
    }

    [Fact]
    public void ARejectedClaimStartsNoTimer()
    {
        var stranger = Fixture.Player(Fixture.Stranger, Side.T, 5) with { Authorized = false };
        Assert.True(_teams.Connect(stranger));
        _host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Equal(Fixture.Messages.Get("kick.not_on_roster"), Assert.Single(_host.Kicked).Reason);
    }

    [Fact]
    public void AClaimedSteamIdThatLeavesBeforeSteamAnswersIsNotKickedLater()
    {
        var claimed = Fixture.Player(Fixture.Team1Player1, Side.T) with { Authorized = false };
        Assert.False(_teams.Connect(claimed));
        _teams.Forget(claimed.Slot);

        // The slot's next client gets a timer of its own, which the old one does not stand in for.
        _host.Advance(TeamLock.AuthorizationGraceSeconds / 2);
        var next = Fixture.Player(Fixture.Team2Player1, Side.T, claimed.Slot) with { Authorized = false };
        Assert.False(_teams.Connect(next));
        _host.Advance(TeamLock.AuthorizationGraceSeconds / 2);
        Assert.Empty(_host.Kicked);
        _host.Advance(TeamLock.AuthorizationGraceSeconds / 2);
        Assert.Equal(next, Assert.Single(_host.Kicked).Player);
    }

    [Fact]
    public void SpectatorsUseSpectatorChangeAndCorrectlyPlacedPlayersAreLeftAlone()
    {
        Assert.False(_teams.Apply(Fixture.Player(Fixture.Spectator, Side.Ct)));
        Assert.Equal(Side.Spec, Assert.Single(_host.ChangedTeams).Side);
        Assert.False(_teams.Apply(Fixture.Player(Fixture.Team1Player1, Side.Ct)));
        Assert.Empty(_host.SwitchedTeams);
    }

    [Theory]
    [InlineData("jointeam 2", true)]
    [InlineData("  \"JOINTEAM\" 3", true)]
    [InlineData("spectate", true)]
    [InlineData("coach ct", true)]
    [InlineData("COACH t", true)]
    [InlineData("coaching", false)]
    [InlineData("say jointeam", false)]
    [InlineData("", false)]
    public void OnlyTeamCommandsAreIntercepted(string command, bool expected) =>
        Assert.Equal(expected, TeamLock.IsTeamChangeCommand(command));

    [Fact]
    public void ManualChangesAreBlockedAndIncorrectPlacementIsCorrected()
    {
        Assert.True(_teams.BlockTeamChange(Fixture.Player(Fixture.Team1Player1, Side.T)));
        Assert.Equal(Side.Ct, Assert.Single(_host.SwitchedTeams).Side);
        Assert.True(_teams.BlockTeamChange(Fixture.Player(Fixture.Team1Player1, Side.Ct)));
        Assert.True(_teams.BlockTeamChange(Fixture.Player(Fixture.Stranger, Side.Spec)));
        Assert.True(_teams.BlockTeamChange(Fixture.Player(Fixture.Team1Player1, Side.Spec) with { SteamId = 0 }));
        Assert.False(_teams.BlockTeamChange(Fixture.Bot(Side.T)));
        Assert.Single(_host.SwitchedTeams);
        Assert.Empty(_host.Kicked);
    }

    [Fact]
    public void AMoveOntoTheRosteredSideIsLeftAlone()
    {
        var player = Fixture.Player(Fixture.Team1Player1, Side.T);
        _teams.Apply(player);
        _teams.OnPlayerTeam(player, Side.Ct);
        Assert.Single(_host.SwitchedTeams);
    }

    [Fact]
    public void AMoveOffTheRosteredSideIsCorrected()
    {
        var player = Fixture.Player(Fixture.Team1Player1, Side.T);
        _teams.Apply(player);
        _teams.OnPlayerTeam(player, Side.Spec);
        Assert.Equal(Side.Ct, _host.SwitchedTeams.Last().Side);
        Assert.Equal(2, _host.SwitchedTeams.Count);
        _teams.OnPlayerTeam(player, Side.Ct);
        Assert.Equal(2, _host.SwitchedTeams.Count);
    }

    [Fact]
    public void AMoveIsCheckedAgainstTheSidesAfterASwap()
    {
        var player = Fixture.Player(Fixture.Team1Player1, Side.T);
        _teams.Apply(player);
        _match.SwapSides();
        _match.SidesSwapped();
        _teams.OnPlayerTeam(player, Side.Ct);
        Assert.Equal(Side.T, _host.SwitchedTeams.Last().Side);
    }

    [Fact]
    public void PostLiveRejectsConnectionsAndIgnoresTeamEvents()
    {
        _match.EnterPostLive();
        var player = Fixture.Player(Fixture.Team1Player1, Side.T);
        _teams.OnClientAuthorized(player);
        Assert.Equal(Fixture.Messages.Get("kick.match_ended"), Assert.Single(_host.Kicked).Reason);
        _teams.OnPlayerTeam(player, Side.Ct);
        Assert.Empty(_sent);
        Assert.Empty(_host.SwitchedTeams);
    }

    [Fact]
    public void AStrangersTeamChangeIsNotCorrected()
    {
        var stranger = Fixture.Player(Fixture.Stranger, Side.T, 5);

        _teams.OnPlayerTeam(stranger, Side.T);

        Assert.Empty(_host.SwitchedTeams);
        Assert.Empty(_host.ChangedTeams);
        Assert.DoesNotContain(_host.Logs, log => log.Message.Contains("correcting"));
    }

    [Fact]
    public void ReapplyUsesCurrentRosterAndSidesForAllConnectedPlayers()
    {
        _host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct),
            Fixture.Player(Fixture.Team2Player1, Side.T, 1),
            Fixture.Player(Fixture.Spectator, Side.Ct, 2),
        ]);
        _match.SwapSides();
        _match.SidesSwapped();
        _teams.ReapplyAll();
        Assert.Collection(_host.SwitchedTeams,
            move => Assert.Equal(Side.T, move.Side),
            move => Assert.Equal(Side.Ct, move.Side));
        Assert.Equal(Side.Spec, Assert.Single(_host.ChangedTeams).Side);
    }

    // Team 1 starts on CT. At halftime the plugin swaps its sides at once, but the engine moves the
    // players only when it resets the round after the break (the engine's own log on a real server:
    // SwitchTeamsAtRoundReset at the round end, the moves in OnPreResetRound, then the spawns).

    [Fact]
    public void APlayerWhoJoinsDuringTheHalftimeBreakGoesToTheSideTheirTeamStillHas()
    {
        _match.SwapSides();

        _teams.Apply(Fixture.Player(Fixture.Team1Player1, Side.Spec));

        Assert.Equal(Side.Ct, Assert.Single(_host.SwitchedTeams).Side);
    }

    [Fact]
    public void ATeamChangeDuringTheHalftimeBreakIsHeldToTheSideTheirTeamStillHas()
    {
        _match.SwapSides();

        Assert.True(_teams.BlockTeamChange(Fixture.Player(Fixture.Team1Player1, Side.Ct)));
        Assert.Empty(_host.SwitchedTeams);
        _teams.BlockTeamChange(Fixture.Player(Fixture.Team1Player1, Side.T));
        Assert.Equal(Side.Ct, Assert.Single(_host.SwitchedTeams).Side);
    }

    [Fact]
    public void TheEnginesSwapAtTheRoundResetStandsAndEndsTheBreak()
    {
        var team1 = Fixture.Player(Fixture.Team1Player1, Side.Ct);
        var team2 = Fixture.Player(Fixture.Team2Player1, Side.T, 1);
        _match.SwapSides();

        // A correction here would only land after the player spawned on the wrong side of the map.
        _teams.OnPlayerTeam(team1, Side.T);
        Assert.False(_match.SidesSwapPending);
        _teams.OnPlayerTeam(team2, Side.Ct);
        Assert.Empty(_host.SwitchedTeams);

        _teams.Apply(Fixture.Player(Fixture.Team1Player2, Side.Spec, 2));
        Assert.Equal(Side.T, Assert.Single(_host.SwitchedTeams).Side);
    }

    [Fact]
    public void TheRoundStartEndsTheBreakWhenNobodyWasOnToBeMoved()
    {
        _match.SwapSides();

        _match.OnRoundStart();

        Assert.False(_match.SidesSwapPending);
        _teams.Apply(Fixture.Player(Fixture.Team1Player1, Side.Spec));
        Assert.Equal(Side.T, Assert.Single(_host.SwitchedTeams).Side);
    }
}
