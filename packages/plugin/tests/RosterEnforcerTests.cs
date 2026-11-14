using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class RosterEnforcerTests
{
    private static RosterEnforcer Locked() => new(Fixture.Config().roster);

    [Fact]
    public void LobbyTeamLookup()
    {
        var roster = Locked();
        Assert.Equal(1, roster.LobbyTeam(Fixture.Team1Player1));
        Assert.Equal(1, roster.LobbyTeam(Fixture.Team1Player2));
        Assert.Equal(2, roster.LobbyTeam(Fixture.Team2Player1));
        Assert.Null(roster.LobbyTeam(Fixture.Stranger));
    }

    [Fact]
    public void TeamNameNamesEachLobbyTeam()
    {
        var roster = Locked();
        Assert.Equal(roster.Team1Name, roster.TeamName(1));
        Assert.Equal(roster.Team2Name, roster.TeamName(2));
        Assert.NotEqual(roster.Team1Name, roster.Team2Name);
    }

    [Theory]
    [InlineData(0)]
    [InlineData(3)]
    public void OnlyLobbyTeamsHaveNames(int team)
    {
        Assert.Throws<ArgumentOutOfRangeException>(() => Locked().TeamName(team));
    }

    [Theory]
    [InlineData(1, Side.Ct, Side.Ct)]
    [InlineData(2, Side.Ct, Side.T)]
    [InlineData(1, Side.T, Side.T)]
    [InlineData(2, Side.T, Side.Ct)]
    public void SideForTeamFollowsTheCurrentHalf(int team, Side team1Side, Side expected) =>
        Assert.Equal(expected, RosterEnforcer.SideForTeam(team, team1Side));

    [Theory]
    [InlineData(Side.Ct, Side.Ct, 1)]
    [InlineData(Side.T, Side.Ct, 2)]
    [InlineData(Side.T, Side.T, 1)]
    [InlineData(Side.Ct, Side.T, 2)]
    public void TeamForSideIsTheInverse(Side side, Side team1Side, int expected) =>
        Assert.Equal(expected, RosterEnforcer.TeamForSide(side, team1Side));

    [Fact]
    public void SpectatorSideHasNoLobbyTeam() =>
        Assert.Null(RosterEnforcer.TeamForSide(Side.Spec, Side.Ct));

    [Fact]
    public void RosteredPlayerIsPlacedOnItsTeamsSide()
    {
        var roster = Locked();

        var placement = roster.Resolve(Fixture.Team2Player1, team1Side: Side.Ct);
        Assert.Equal(RosterVerdict.Place, placement.Verdict);
        Assert.Equal(Side.T, placement.Side);

        var afterHalftime = roster.Resolve(Fixture.Team2Player1, team1Side: Side.T);
        Assert.Equal(Side.Ct, afterHalftime.Side);
    }

    [Fact]
    public void ListedSpectatorsGoToSpectator()
    {
        var roster = Locked();

        Assert.Equal(RosterVerdict.Spectate, roster.Resolve(Fixture.Spectator, Side.Ct).Verdict);
    }

    [Fact]
    public void StrangersAreKicked()
    {
        Assert.Equal(RosterVerdict.Kick, Locked().Resolve(Fixture.Stranger, Side.Ct).Verdict);
    }

    [Fact]
    public void BothTeamsPresentNeedsOneRosteredPlayerPerTeam()
    {
        var roster = Locked();

        Assert.False(roster.BothTeamsPresent([]));
        Assert.False(roster.BothTeamsPresent([Fixture.Player(Fixture.Team1Player1, Side.Ct)]));
        // A stranger is not on a player roster, so does not fill a team.
        Assert.False(roster.BothTeamsPresent([
            Fixture.Player(Fixture.Team1Player1, Side.Ct),
            Fixture.Player(Fixture.Stranger, Side.T),
        ]));
        Assert.False(roster.BothTeamsPresent([Fixture.Bot(Side.T), Fixture.Bot(Side.Ct, 10)]));

        Assert.True(roster.BothTeamsPresent([
            Fixture.Player(Fixture.Team1Player1, Side.Ct),
            Fixture.Player(Fixture.Team2Player2, Side.T),
        ]));
    }

    [Fact]
    public void MissingPlayersListsOnlyRosteredAbsentees()
    {
        var roster = Locked();
        var missing = roster.MissingPlayers([
            Fixture.Player(Fixture.Team1Player1, Side.Ct),
            Fixture.Player(Fixture.Stranger, Side.T),
        ]);

        Assert.Equal(3, missing.Count);
        Assert.Contains(Fixture.Team1Player2, missing);
        Assert.Contains(Fixture.Team2Player1, missing);
        Assert.Contains(Fixture.Team2Player2, missing);
        Assert.DoesNotContain(Fixture.Team1Player1, missing);
    }

    /// <summary>The lobby sends each team captain-first, so index 0 is the only captain.</summary>
    [Fact]
    public void TheFirstPlayerListedOnEachTeamIsItsCaptain()
    {
        var roster = Locked();

        Assert.Equal(Fixture.Team1Player1, roster.Captain(1));
        Assert.Equal(Fixture.Team2Player1, roster.Captain(2));
        Assert.Null(roster.Captain(0));

        Assert.Equal(1, roster.CaptainTeam(Fixture.Team1Player1));
        Assert.Equal(2, roster.CaptainTeam(Fixture.Team2Player1));
        Assert.Null(roster.CaptainTeam(Fixture.Team1Player2));
        Assert.Null(roster.CaptainTeam(Fixture.Stranger));

        Assert.True(roster.IsCaptain(Fixture.Team1Player1, 1));
        // A captain only captains their own team.
        Assert.False(roster.IsCaptain(Fixture.Team1Player1, 2));
        Assert.False(roster.IsCaptain(Fixture.Team1Player2, 1));
    }

    [Fact]
    public void CaptainLookupHandlesEmptyTeams()
    {
        var config = Fixture.Config();
        config.roster.team1.players = [Fixture.Team1Player2, Fixture.Team1Player1];
        config.roster.team2.players = [];
        var roster = new RosterEnforcer(config.roster);

        Assert.Equal(Fixture.Team1Player2, roster.Captain(1));
        Assert.Equal(1, roster.CaptainTeam(Fixture.Team1Player2));
        Assert.Null(roster.Captain(2));
        Assert.Null(roster.CaptainTeam(""));
        Assert.False(roster.IsCaptain("", 2));
    }
}
