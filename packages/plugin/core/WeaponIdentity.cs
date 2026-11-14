using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>Some damage events name the shared base class rather than the equipped variant.</summary>
public static class WeaponIdentity
{
    /// <summary>What every engine item classname starts with.</summary>
    public const string Prefix = "weapon_";

    public static string WithoutPrefix(string name) =>
        name.StartsWith(Prefix, StringComparison.Ordinal) ? name[Prefix.Length..] : name;

    public static string ForDamage(string reported, string? equipped)
    {
        if (equipped is null) return reported;
        equipped = WithoutPrefix(equipped);

        var sameFamily = reported switch
        {
            "m4a1" => equipped is "m4a1" or "m4a1_silencer",
            "mp7" => equipped is "mp7" or "mp5sd",
            "hkp2000" => equipped is "hkp2000" or "usp_silencer",
            "p250" => equipped is "p250" or "cz75a",
            "deagle" => equipped is "deagle" or "revolver",
            "knife" => IsKnife(equipped),
            _ => false,
        };
        // A grenade/fire can hurt someone while its thrower holds another weapon.
        return sameFamily ? equipped : reported;
    }

    public static WeaponReference Resolve(string reported, int? equippedId, string? equippedClassname,
        Func<string, int?> itemByClassname)
    {
        var equippedName = equippedClassname is null ? null : WithoutPrefix(equippedClassname);
        var resolved = ForDamage(reported, equippedName);
        // An active grenade/fire can deal damage while another weapon is held: only take the
        // equipped item's ID when its classname matches the resolved damage source.
        if (equippedName == resolved && equippedId is > 0)
            return WeaponReference.FromItem(equippedId, reported);
        return WeaponReference.FromItem(itemByClassname(Classname(resolved)), reported);
    }

    /// <summary>
    /// Whether a weapon name, with or without its <c>weapon_</c> prefix, is a knife: the CT and T
    /// defaults, the bayonet and every <c>knife_</c> model.
    /// </summary>
    public static bool IsKnife(string weapon)
    {
        var name = WithoutPrefix(weapon);
        return name is "knife" or "bayonet" || name.StartsWith("knife_", StringComparison.Ordinal);
    }

    // weapon_fire names the full classname but player_hurt doesn't, and both must resolve to the
    // same weapon.
    public static string Fired(string? weapon) => WithoutPrefix(weapon ?? "");

    /// <summary>The item classname of a resolved damage source, as the engine's item table names it.</summary>
    public static string Classname(string resolved)
    {
        var canonical = resolved switch
        {
            "m4a1_silencer_off" => "m4a1_silencer",
            "usp_silencer_off" => "usp_silencer",
            "planted_c4" => "c4",
            "knife_default_ct" => "knife",
            "knife_default_t" => "knife_t",
            _ => resolved,
        };
        return canonical.StartsWith(Prefix, StringComparison.Ordinal) ? canonical : Prefix + canonical;
    }

    /// <summary>
    /// The engine's grenade names, as in <c>weapon_hegrenade</c> and <c>hegrenade_projectile</c>.
    /// They are kept apart from the wire names of <see cref="GrenadeKind"/>, which the contract may change.
    /// </summary>
    public static readonly IReadOnlyDictionary<string, GrenadeKind> EngineGrenades = new Dictionary<string, GrenadeKind>
    {
        ["flashbang"] = GrenadeKind.Flashbang,
        ["hegrenade"] = GrenadeKind.HeGrenade,
        ["smokegrenade"] = GrenadeKind.SmokeGrenade,
        ["molotov"] = GrenadeKind.Molotov,
        ["incgrenade"] = GrenadeKind.IncGrenade,
        ["decoy"] = GrenadeKind.Decoy,
    };

    /// <summary>The grenade a weapon or projectile name stands for, with or without its <c>weapon_</c> prefix.</summary>
    public static GrenadeKind? Grenade(string? weapon) =>
        weapon is not null && EngineGrenades.TryGetValue(WithoutPrefix(weapon), out var kind) ? kind : null;
}

/// <summary>Native item identity; zero is not an item and retains its damage-source name.</summary>
public readonly record struct WeaponReference(int? Id, string? Fallback)
{
    public static WeaponReference FromItem(int? id, string reported) =>
        id is > 0 ? new(id, null) : new(null, string.IsNullOrWhiteSpace(reported) ? null : reported);
}
