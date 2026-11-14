using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class DamageCausesTests
{
    private const string Victim = "76561198000000001";
    private const string Other = "76561198000000002";

    [Theory]
    [InlineData(Other, 7, null, false, DamageCause.Player)]
    [InlineData(null, GrenadeAttribution.C4Item, null, false, DamageCause.Bomb)]
    [InlineData(null, null, "worldent", false, DamageCause.Fall)]
    [InlineData(Victim, 46, null, false, DamageCause.Self)]
    [InlineData(Victim, null, "world", true, DamageCause.Disconnect)]
    [InlineData(null, null, "world", true, DamageCause.Disconnect)]
    [InlineData(Victim, null, "world", false, DamageCause.Other)]
    [InlineData(null, null, "trigger_hurt", false, DamageCause.Other)]
    // A player who left and came back is killed by another as anyone is.
    [InlineData(Other, 7, null, true, DamageCause.Player)]
    public void NamesTheCauseFromWhatTheEngineReports(string? attacker, int? weapon, string? fallback, bool disconnected, DamageCause cause)
    {
        Assert.Equal(cause, DamageCauses.Of(Victim, attacker, new WeaponReference(weapon, fallback), disconnected));
    }
}
