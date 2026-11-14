using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class TeamNumberMappingTests
{
    [Theory]
    // 2 and 3 are CS2's TEAM_TERRORIST / TEAM_CT, and SwiftlyS2's Players.Team has the same values.
    [InlineData(2, Side.T)]
    [InlineData(3, Side.Ct)]
    // Team.None and Team.Spectator: not playing. Anything else is not a team at all.
    [InlineData(0, Side.Spec)]
    [InlineData(1, Side.Spec)]
    [InlineData(4, Side.Spec)]
    [InlineData(-1, Side.Spec)]
    public void MapsTeamNumbersToSides(int teamNum, Side expected)
    {
        Assert.Equal(expected, SideExtensions.FromTeamNum(teamNum));
    }

    [Fact]
    public void OppositeFlipsPlayingSidesAndLeavesSpectatorAlone()
    {
        Assert.Equal(Side.Ct, Side.T.Opposite());
        Assert.Equal(Side.T, Side.Ct.Opposite());
        Assert.Equal(Side.Spec, Side.Spec.Opposite());
    }
}

public class HostPlayerReportabilityTests
{
    private static HostPlayer Player(ulong steamId, bool isBot) => new(
        Slot: 3,
        SteamId: steamId,
        Name: "someone",
        Side: Side.T,
        Health: 100,
        IsAlive: true,
        IsBot: isBot);

    [Theory]
    [InlineData(76561198000000001UL, false, true)]  // Steam vouched for the ticket: a player.
    [InlineData(0UL, true, false)]                 // A bot has no SteamID64; the controller rejects steam_id "0".
    [InlineData(76561198000000001UL, true, false)] // A bot is never a player, whatever the engine hands back.
    [InlineData(0UL, false, false)]                // Mid-connect human: reported once they authorize.
    public void OnlyAnAuthorizedHumanIsReportable(ulong steamId, bool isBot, bool expected)
    {
        var player = Player(steamId, isBot);

        Assert.Equal(expected, player.IsReportable);
    }

    [Fact]
    public void ADepartedPlayerIsADeadSpectatorWithTheEventsIdentity()
    {
        var player = HostPlayer.Departed(4, 76561198000000001, "alice");

        Assert.Equal(new HostPlayer(4, 76561198000000001, "alice", Side.Spec, 0, false, false), player);
        Assert.True(player.IsReportable);
    }

    [Fact]
    public void ADepartedPlayerWithoutASteamIdIsABotAndWithoutANameIsBlank()
    {
        var player = HostPlayer.Departed(4, 0, null);

        Assert.Equal("", player.Name);
        Assert.True(player.IsBot);
        Assert.False(player.IsReportable);
    }
}
