using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class WeaponIdentityTests
{
    [Fact]
    public void ZeroUsesTheDamageSourceFallbackAndNumericItemsDoNotRepeatTheirName()
    {
        Assert.Equal(new WeaponReference(null, "world"), WeaponReference.FromItem(0, "world"));
        Assert.Equal(new WeaponReference(null, "inferno"), WeaponReference.FromItem(null, "inferno"));
        Assert.Equal(new WeaponReference(23, null), WeaponReference.FromItem(23, "mp7"));
    }

    [Theory]
    [InlineData("m4a1", "weapon_m4a1_silencer", "m4a1_silencer")]
    [InlineData("mp7", "weapon_mp5sd", "mp5sd")]
    [InlineData("hkp2000", "weapon_usp_silencer", "usp_silencer")]
    [InlineData("p250", "weapon_cz75a", "cz75a")]
    [InlineData("deagle", "weapon_revolver", "revolver")]
    [InlineData("knife", "weapon_bayonet", "bayonet")]
    [InlineData("knife", "weapon_knife_kukri", "knife_kukri")]
    [InlineData("m4a1", "weapon_m4a1", "m4a1")]
    [InlineData("mp7", "weapon_mp7", "mp7")]
    [InlineData("hkp2000", "weapon_hkp2000", "hkp2000")]
    [InlineData("p250", "weapon_p250", "p250")]
    [InlineData("deagle", "weapon_deagle", "deagle")]
    [InlineData("knife", "weapon_knife", "knife")]
    [InlineData("mp7", "weapon_ak47", "mp7")]
    [InlineData("hkp2000", "weapon_p250", "hkp2000")]
    [InlineData("p250", "weapon_hkp2000", "p250")]
    [InlineData("deagle", "weapon_ak47", "deagle")]
    [InlineData("knife", "weapon_ak47", "knife")]
    [InlineData("m4a1", "m4a1_silencer", "m4a1_silencer")]
    [InlineData("inferno", "weapon_knife", "inferno")]
    [InlineData("hegrenade", "weapon_ak47", "hegrenade")]
    [InlineData("m4a1", "weapon_knife", "m4a1")]
    [InlineData("knife", null, "knife")]
    public void ResolvesOnlyEquippedVariantsOfTheReportedWeapon(string reported, string? equipped, string expected)
    {
        Assert.Equal(expected, WeaponIdentity.ForDamage(reported, equipped));
    }

    [Theory]
    [InlineData("weapon_knife", true)]
    [InlineData("knife_t", true)]
    [InlineData("weapon_bayonet", true)]
    [InlineData("weapon_knife_karambit", true)]
    [InlineData("weapon_usp_silencer", false)]
    [InlineData("weapon_hegrenade", false)]
    public void KnowsAKnifeByItsName(string weapon, bool knife)
    {
        Assert.Equal(knife, WeaponIdentity.IsKnife(weapon));
    }

    [Theory]
    [InlineData("m4a1_silencer_off", "weapon_m4a1_silencer")]
    [InlineData("usp_silencer_off", "weapon_usp_silencer")]
    [InlineData("planted_c4", "weapon_c4")]
    [InlineData("knife_default_ct", "weapon_knife")]
    [InlineData("knife_default_t", "weapon_knife_t")]
    [InlineData("ak47", "weapon_ak47")]
    [InlineData("weapon_awp", "weapon_awp")]
    public void NamesTheItemClassnameOfADamageSource(string resolved, string expected)
    {
        Assert.Equal(expected, WeaponIdentity.Classname(resolved));
    }

    [Theory]
    [InlineData("weapon_flashbang", GrenadeKind.Flashbang)]
    [InlineData("hegrenade", GrenadeKind.HeGrenade)]
    [InlineData("smokegrenade", GrenadeKind.SmokeGrenade)]
    [InlineData("molotov", GrenadeKind.Molotov)]
    [InlineData("weapon_incgrenade", GrenadeKind.IncGrenade)]
    [InlineData("decoy", GrenadeKind.Decoy)]
    [InlineData("ak47", null)]
    [InlineData(null, null)]
    public void ReadsAGrenadeFromItsClassname(string? weapon, GrenadeKind? expected)
    {
        Assert.Equal(expected, WeaponIdentity.Grenade(weapon));
    }

    /// <summary>A grenade the contract adds needs its engine name, or it is never recognised.</summary>
    [Fact]
    public void EveryGrenadeTheContractNamesHasAnEngineName()
    {
        Assert.Equal(Enum.GetValues<GrenadeKind>().ToHashSet(), WeaponIdentity.EngineGrenades.Values.ToHashSet());
    }

    private static readonly Dictionary<string, int> Items = new()
    {
        ["weapon_ak47"] = 7,
        ["weapon_m4a1"] = 16,
        ["weapon_m4a1_silencer"] = 60,
        ["weapon_hegrenade"] = 44,
        ["weapon_c4"] = 49,
    };

    private static int? Item(string classname) => Items.TryGetValue(classname, out var id) ? id : null;

    private static int? Unlooked(string classname) => throw new InvalidOperationException($"{classname} was looked up");

    [Fact]
    public void TheEquippedItemNamesTheWeaponItFired()
    {
        Assert.Equal(new WeaponReference(7, null), WeaponIdentity.Resolve("ak47", 7, "weapon_ak47", Unlooked));
    }

    [Fact]
    public void AnEquippedVariantOfTheReportedWeaponKeepsItsOwnItem()
    {
        Assert.Equal(new WeaponReference(60, null), WeaponIdentity.Resolve("m4a1", 60, "weapon_m4a1_silencer", Unlooked));
    }

    [Fact]
    public void AGrenadeThatHurtsWhileAnotherWeaponIsHeldIsLookedUp()
    {
        Assert.Equal(new WeaponReference(44, null), WeaponIdentity.Resolve("hegrenade", 7, "weapon_ak47", Item));
    }

    [Fact]
    public void WithoutAnEquippedItemTheReportedWeaponIsLookedUp()
    {
        Assert.Equal(new WeaponReference(16, null), WeaponIdentity.Resolve("m4a1", null, null, Item));
        Assert.Equal(new WeaponReference(49, null), WeaponIdentity.Resolve("planted_c4", null, null, Item));
    }

    [Fact]
    public void AWeaponTheItemTableDoesNotKnowKeepsItsReportedName()
    {
        Assert.Equal(new WeaponReference(null, "inferno"), WeaponIdentity.Resolve("inferno", null, null, Item));
        Assert.Equal(new WeaponReference(null, null), WeaponIdentity.Resolve("", null, null, Item));
    }

    [Theory]
    [InlineData("weapon_ak47", "ak47")]
    [InlineData("ak47", "ak47")]
    [InlineData("knife_weapon_ak47", "knife_weapon_ak47")]
    [InlineData("weapon_", "")]
    [InlineData("", "")]
    public void OnlyALeadingPrefixIsStripped(string name, string expected)
    {
        Assert.Equal(expected, WeaponIdentity.WithoutPrefix(name));
    }

    /// <summary>
    /// player_hurt names the shared class and weapon_fire the classname, and both take the held
    /// item without a lookup; player_death names the kill-feed variant, which is looked up.
    /// </summary>
    [Theory]
    [InlineData("m4a1", false)]
    [InlineData("weapon_m4a1_silencer", false)]
    [InlineData("m4a1_silencer_off", true)]
    public void HurtDeathAndFireNamesOfOneWeaponResolveToItsItem(string reported, bool lookedUp)
    {
        Assert.Equal(new WeaponReference(60, null),
            WeaponIdentity.Resolve(WeaponIdentity.Fired(reported), 60, "weapon_m4a1_silencer", lookedUp ? Item : Unlooked));
    }

    [Theory]
    [InlineData("weapon_ak47", "ak47")]
    [InlineData("ak47", "ak47")]
    [InlineData("", "")]
    [InlineData(null, "")]
    public void AFiredWeaponIsNamedWithoutItsPrefix(string? weapon, string expected)
    {
        Assert.Equal(expected, WeaponIdentity.Fired(weapon));
    }
}
