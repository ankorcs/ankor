using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class BombTests
{
    [Theory]
    [InlineData(0, BombSite.A)]
    [InlineData(1, BombSite.B)]
    [InlineData(2, null)]
    [InlineData(null, null)]
    public void ReadsThePlantedSiteNumber(int? site, BombSite? expected)
    {
        Assert.Equal(expected, Bomb.PlantedSite(site));
    }

    [Theory]
    [InlineData(140.5, 100, 40.5)]
    [InlineData(100, 140.5, 0)]
    public void DefuseTimeRemainingIsNeverNegative(double blowTime, double now, double expected)
    {
        Assert.Equal(expected, Bomb.DefuseTimeRemaining(blowTime, now));
    }
}
