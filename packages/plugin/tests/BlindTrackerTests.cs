using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class BlindTrackerTests
{
    private const string Ct1 = "76561198000000001";
    private const string Ct2 = "76561198000000002";
    private const string T1 = "76561198000000011";

    private static PlayerRef Player(string id) => new() { steamId = id };

    [Fact]
    public void ARunningBlindNamesItsBlinderAndTheSecondsItHasLeft()
    {
        var blinds = new BlindTracker();
        blinds.Record(T1, Player(Ct1), 3.2, 10.0);

        var found = blinds.Lookup(T1, 11.0);
        Assert.Equal(Ct1, found?.Blinder.steamId);
        Assert.Equal(2.2, found!.Value.Remaining, 3);

        // Remaining time is rounded to the millisecond, like every duration on the wire.
        Assert.Equal(0.077, blinds.Lookup(T1, 13.1234567)?.Remaining);
    }

    [Fact]
    public void AnExpiredBlindIsGone()
    {
        var blinds = new BlindTracker();
        blinds.Record(T1, Player(Ct1), 1.5, 10.0);

        Assert.Null(blinds.Lookup(T1, 11.5));
        Assert.Null(blinds.Lookup(T1, 20.0));
    }

    [Fact]
    public void SelfBlindsAndZeroLengthBlindsAreNobodysAssist()
    {
        var blinds = new BlindTracker();
        blinds.Record(T1, Player(T1), 4.0, 10.0);
        blinds.Record(T1, Player(Ct1), 0, 10.0);

        Assert.Null(blinds.Lookup(T1, 10.0));
        Assert.Null(blinds.Lookup(T1, 11.0));
    }

    [Fact]
    public void TheBlindThatEndsLatestWins()
    {
        var blinds = new BlindTracker();
        blinds.Record(T1, Player(Ct1), 4.0, 10.0);

        // A shorter flash a moment later does not take the victim's sight back sooner.
        blinds.Record(T1, Player(Ct2), 0.5, 11.0);
        Assert.Equal(Ct1, blinds.Lookup(T1, 11.0)?.Blinder.steamId);

        blinds.Record(T1, Player(Ct2), 5.0, 11.0);
        Assert.Equal(Ct2, blinds.Lookup(T1, 11.0)?.Blinder.steamId);
    }

    [Fact]
    public void NobodyIsBlindedByDefaultAndResetClearsTheRound()
    {
        var blinds = new BlindTracker();
        Assert.Null(blinds.Lookup(T1, 0));

        blinds.Record(T1, Player(Ct1), 4.0, 10.0);
        blinds.Reset();
        Assert.Null(blinds.Lookup(T1, 11.0));
    }
}
