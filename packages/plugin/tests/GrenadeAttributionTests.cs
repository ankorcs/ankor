using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class GrenadeAttributionTests
{
    private static readonly WorldPosition At = new(10, 20, 30);

    private static int Unread() => throw new InvalidOperationException("the projectile item was read");

    [Theory]
    [InlineData("hegrenade_projectile", true)]
    [InlineData("molotov_projectile", true)]
    [InlineData("inferno", true)]
    [InlineData("player", false)]
    [InlineData("weapon_hegrenade", false)]
    public void TracksProjectilesAndInfernos(string name, bool tracked)
    {
        Assert.Equal(tracked, GrenadeAttribution.IsTracked(name));
    }

    [Fact]
    public void MolotovProjectileIsAnIncendiaryOnlyWhenTheEngineSaysSo()
    {
        Assert.Equal(GrenadeKind.IncGrenade, GrenadeAttribution.Kind("molotov_projectile", () => true));
        Assert.Equal(GrenadeKind.Molotov, GrenadeAttribution.Kind("molotov_projectile", () => false));
    }

    [Fact]
    public void OtherProjectilesAreNamedWithoutReadingTheIncendiaryFlag()
    {
        bool Unreadable() => throw new InvalidOperationException("IsIncGrenade was read");
        Assert.Equal(GrenadeKind.Flashbang, GrenadeAttribution.Kind("flashbang_projectile", Unreadable));
        Assert.Equal(GrenadeKind.Decoy, GrenadeAttribution.Kind("decoy_projectile", Unreadable));
        Assert.Null(GrenadeAttribution.Kind("rocket_projectile", Unreadable));
    }

    [Fact]
    public void WorldDamageAndThePlantedBombHaveFixedSources()
    {
        var attribution = new GrenadeAttribution();
        Assert.Equal(new WeaponReference(49, null), attribution.Source(1, "planted_c4", Unread));
        Assert.Equal(new WeaponReference(null, "inferno"), attribution.Source(2, "inferno", Unread));
        Assert.Equal(new WeaponReference(null, "trigger_hurt"), attribution.Source(3, "trigger_hurt", Unread));
        Assert.Equal(new WeaponReference(null, "worldspawn"), attribution.Source(0, "worldspawn", Unread));
        Assert.Null(attribution.Source(4, "player", Unread));
        Assert.Null(attribution.Source(5, "rocket_projectile", Unread));
    }

    [Fact]
    public void AProjectileIsNamedByItsItemAndKeptForItsId()
    {
        var attribution = new GrenadeAttribution();
        Assert.Equal(new WeaponReference(44, null), attribution.Source(50, "hegrenade_projectile", () => 44));
        Assert.Equal(new WeaponReference(44, null), attribution.Source(50, "", Unread));
    }

    [Fact]
    public void AProjectileWithoutAnItemFallsBackToItsGrenadeName()
    {
        var attribution = new GrenadeAttribution();
        Assert.Equal(new WeaponReference(null, "hegrenade"), attribution.Source(50, "hegrenade_projectile", () => 0));
    }

    [Fact]
    public void AnInfernoTakesOverItsProjectilesWeapon()
    {
        var attribution = new GrenadeAttribution();
        attribution.Source(50, "molotov_projectile", () => 48);
        attribution.Detonated(50, 100, 7, At);

        Assert.Equal(50, attribution.FireSpawned(60, 100, 7, At));
        attribution.Deleted(50);
        Assert.Equal(new WeaponReference(48, null), attribution.Source(60, "inferno", Unread));
    }

    [Fact]
    public void AnInfernoWithoutADetonationThatTickHasNoSource()
    {
        var attribution = new GrenadeAttribution();
        attribution.Source(50, "molotov_projectile", () => 48);
        attribution.Detonated(50, 100, 7, At);

        Assert.Null(attribution.FireSpawned(60, 101, 7, At));
        Assert.Equal(new WeaponReference(null, "inferno"), attribution.Source(60, "inferno", Unread));
    }

    [Fact]
    public void DeletionForgetsTheWeaponAndThePendingFire()
    {
        var attribution = new GrenadeAttribution();
        attribution.Source(50, "molotov_projectile", () => 48);
        attribution.Detonated(50, 100, 7, At);
        attribution.Deleted(50);

        Assert.Null(attribution.FireSpawned(60, 100, 7, At));
        Assert.Null(attribution.Source(50, "", Unread));
    }

    [Fact]
    public void ClearForgetsEverything()
    {
        var attribution = new GrenadeAttribution();
        attribution.Source(50, "molotov_projectile", () => 48);
        attribution.Detonated(50, 100, 7, At);
        attribution.Clear();

        Assert.Null(attribution.FireSpawned(60, 100, 7, At));
        Assert.Null(attribution.Source(50, "", Unread));
    }
}
