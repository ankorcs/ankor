using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class KnifeRoundTests
{
    [Fact]
    public void MorePlayersAliveWins()
    {
        var tally = KnifeRound.Decide([
            Fixture.Player(Fixture.Team1Player1, Side.T, 0, health: 1),
            Fixture.Player(Fixture.Team1Player2, Side.T, 1, health: 1),
            Fixture.Player(Fixture.Team2Player1, Side.Ct, 2, health: 100),
        ]);

        Assert.Equal(Side.T, tally.Winner);
        Assert.Equal(2, tally.TAlive);
        Assert.Equal(1, tally.CtAlive);
    }

    [Fact]
    public void EqualAliveFallsBackToTotalHealth()
    {
        var tally = KnifeRound.Decide([
            Fixture.Player(Fixture.Team1Player1, Side.T, 0, health: 100),
            Fixture.Player(Fixture.Team1Player2, Side.T, 1, health: 100),
            Fixture.Player(Fixture.Team2Player1, Side.Ct, 2, health: 100),
            Fixture.Player(Fixture.Team2Player2, Side.Ct, 3, health: 99),
        ]);

        Assert.Equal(Side.T, tally.Winner);
        Assert.Equal(200, tally.THealth);
        Assert.Equal(199, tally.CtHealth);
    }

    [Fact]
    public void IdenticalAliveAndHealthIsATie()
    {
        var tally = KnifeRound.Decide([
            Fixture.Player(Fixture.Team1Player1, Side.T, 0, health: 87),
            Fixture.Player(Fixture.Team2Player1, Side.Ct, 1, health: 87),
        ]);

        Assert.Null(tally.Winner);
        Assert.Null(tally.Winner);
    }

    [Fact]
    public void TieIsBrokenByTheRoundEndWinner()
    {
        var tally = KnifeRound.Decide([
            Fixture.Player(Fixture.Team1Player1, Side.T, 0, health: 50),
            Fixture.Player(Fixture.Team2Player1, Side.Ct, 1, health: 50),
        ]);

        Assert.Equal(Side.Ct, KnifeRound.Resolve(tally, Side.Ct));
        Assert.Equal(Side.T, KnifeRound.Resolve(tally, Side.T));
    }

    [Fact]
    public void TieWithNoUsableRoundEndWinnerFallsBackToT()
    {
        var tally = KnifeRound.Decide([]);
        Assert.Null(tally.Winner);
        Assert.Equal(Side.T, KnifeRound.Resolve(tally, Side.Spec));
    }

    [Fact]
    public void DecidedTallyIgnoresTheTieBreak()
    {
        var tally = KnifeRound.Decide([Fixture.Player(Fixture.Team1Player1, Side.T)]);
        Assert.Equal(Side.T, KnifeRound.Resolve(tally, Side.Ct));
    }

    [Fact]
    public void OnlyLivingHumansOnAPlayingSideCount()
    {
        var tally = KnifeRound.Decide([
            Fixture.Player(Fixture.Team1Player1, Side.T, 0, health: 100, alive: false),
            Fixture.Player(Fixture.Team1Player2, Side.T, 1, health: 100, alive: false),
            Fixture.Player(Fixture.Spectator, Side.Spec, 2, health: 100),
            Fixture.Bot(Side.Ct),
            Fixture.Bot(Side.Ct, 10),
            Fixture.Player(Fixture.Team2Player1, Side.Ct, 3, health: 5),
        ]);

        // Two dead Ts, a spectator and two bots: the one living CT is the whole tally.
        Assert.Equal(Side.Ct, tally.Winner);
        Assert.Equal((0, 0, 1, 5), (tally.TAlive, tally.THealth, tally.CtAlive, tally.CtHealth));
    }

    [Fact]
    public void DyingPawnsDoNotCountAsSurvivorsEvenIfTheirAliveFlagLags()
    {
        var tally = KnifeRound.Decide([
            Fixture.Player(Fixture.Team1Player1, Side.T, 0, health: -12),
            Fixture.Player(Fixture.Team2Player1, Side.Ct, 1, health: 0),
        ]);

        Assert.Null(tally.Winner);
        Assert.Equal(0, tally.THealth);
        Assert.Equal(0, tally.CtHealth);
        Assert.Equal(0, tally.TAlive);
        Assert.Equal(0, tally.CtAlive);
    }
}
