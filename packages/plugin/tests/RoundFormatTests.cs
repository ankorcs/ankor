using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public class RoundFormatTests
{
    /// <summary>MR24 or MR30 with six-round overtimes, halftime on and no win limit.</summary>
    private static RoundFormat Format(int maxRounds = 24, bool overtime = true, int overtimeRounds = 6,
        bool halftime = true, int winLimit = 0, bool canClinch = true) =>
        new(maxRounds, overtime, overtimeRounds, halftime, winLimit, canClinch);

    [Theory]
    [InlineData(24, 23, false)]
    [InlineData(24, 24, true)] // Regulation ends; overtime 1 starts.
    [InlineData(24, 25, false)]
    [InlineData(24, 29, false)]
    [InlineData(24, 30, true)] // Overtime 1 ends; overtime 2 starts.
    [InlineData(24, 36, true)]
    [InlineData(30, 30, true)]
    [InlineData(30, 33, false)]
    [InlineData(30, 36, true)]
    public void AnOvertimeStartsAfterRegulationAndAfterEachOvertime(int maxRounds, int round, bool starts)
    {
        Assert.Equal(starts, Format(maxRounds).StartsOvertimeAfter(round));
    }

    [Fact]
    public void NoOvertimeStartsWhenOvertimeIsOffOrUnbounded()
    {
        Assert.False(Format(overtime: false).StartsOvertimeAfter(24));
        Assert.False(Format(overtimeRounds: 0).StartsOvertimeAfter(24));
        Assert.False(Format(maxRounds: 0).StartsOvertimeAfter(24));
    }

    [Theory]
    [InlineData(24, 11, false)]
    [InlineData(24, 12, true)] // Regulation's halftime.
    [InlineData(24, 13, false)]
    [InlineData(24, 24, false)] // Overtime 1 keeps regulation's second-half sides.
    [InlineData(24, 26, false)]
    [InlineData(24, 27, true)] // Overtime 1's halftime: rounds 25 to 27, then 28 to 30.
    [InlineData(24, 30, false)] // No swap between two overtimes.
    [InlineData(24, 33, true)]
    [InlineData(30, 15, true)]
    [InlineData(30, 33, true)]
    [InlineData(30, 36, false)]
    public void SidesSwapHalfwayThroughRegulationAndEachOvertime(int maxRounds, int played, bool swaps)
    {
        Assert.Equal(swaps, Format(maxRounds).IsHalftimeBoundary(played));
    }

    [Fact]
    public void SidesNeverSwapWithHalftimeOffOrNoRegulation()
    {
        Assert.False(Format(halftime: false).IsHalftimeBoundary(12));
        Assert.False(Format(maxRounds: 0).IsHalftimeBoundary(0));
        Assert.False(Format(overtimeRounds: 0).IsHalftimeBoundary(27));
    }

    [Theory]
    [InlineData(12, 11, false)]
    [InlineData(13, 10, true)] // Clinched: more than half of regulation.
    [InlineData(12, 12, false)] // A tie at regulation's end goes to overtime.
    [InlineData(15, 13, false)] // Overtime 1 needs more than 12 + 3.
    [InlineData(16, 14, true)]
    [InlineData(15, 15, false)] // A tie after overtime 1 goes to overtime 2.
    [InlineData(18, 16, false)]
    [InlineData(19, 17, true)] // Overtime 2 needs more than 12 + 6.
    public void TheMatchEndsWhenATeamWinsRegulationOrAnOvertime(int team1, int team2, bool final)
    {
        Assert.Equal(final, Format().IsFinalRound(team1, team2));
        Assert.Equal(final, Format().IsFinalRound(team2, team1));
    }

    [Fact]
    public void WithoutClinchingEveryRegulationRoundIsPlayed()
    {
        Assert.False(Format(canClinch: false).IsFinalRound(13, 10));
        Assert.True(Format(canClinch: false).IsFinalRound(13, 11));
    }

    [Fact]
    public void ATieAtRegulationsEndIsFinalWithoutOvertime()
    {
        Assert.True(Format(overtime: false).IsFinalRound(12, 12));
    }

    [Fact]
    public void AWinLimitEndsTheMatchAtOnce()
    {
        Assert.True(Format(winLimit: 2).IsFinalRound(2, 0));
        Assert.False(Format(winLimit: 2).IsFinalRound(1, 1));
    }

    [Fact]
    public void WithNoRegulationOnlyAWinLimitEndsTheMatch()
    {
        Assert.False(Format(maxRounds: 0).IsFinalRound(40, 0));
    }

    [Fact]
    public void ItReadsTheServersCvars()
    {
        var host = new FakeGameHost();
        host.ConVars["mp_maxrounds"] = 24;
        host.ConVars["mp_overtime_enable"] = 1;
        host.ConVars["mp_overtime_maxrounds"] = 6;
        host.ConVars["mp_halftime"] = 1;
        host.ConVars["mp_winlimit"] = 0;
        host.ConVars["mp_match_can_clinch"] = 0;

        Assert.Equal(new RoundFormat(24, true, 6, true, 0, false), RoundFormat.Read(host));
    }
}
