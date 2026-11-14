using Ankor.Core.Wire;

namespace Ankor.Core;

// A projectile's weapon is kept by entity id because its fire keeps dealing damage after the
// projectile is deleted.
public sealed class GrenadeAttribution
{
    private const string ProjectileSuffix = "_projectile";

    /// <summary>The engine's fire entity, which a molotov or incendiary leaves behind.</summary>
    public const string Fire = "inferno";

    /// <summary>The projectile of both the molotov and the incendiary.</summary>
    public const string MolotovProjectile = "molotov_projectile";

    // A planted bomb has no item of its own, so its damage is scored under the C4 item, the same
    // id the engine's item table gives weapon_c4. CS2 names no attacker for it.
    public const int C4Item = 49;

    private readonly Dictionary<int, WeaponReference> _weapons = [];
    private readonly PendingFireTracker _pendingFires = new();

    public static bool IsTracked(string designerName) =>
        GrenadeName(designerName) is not null || designerName == Fire;

    // Molotov and incendiary share molotov_projectile.
    public static GrenadeKind? Kind(string projectileName, Func<bool> isIncGrenade) =>
        projectileName == MolotovProjectile && isIncGrenade()
            ? GrenadeKind.IncGrenade
            : WeaponIdentity.Grenade(GrenadeName(projectileName));

    /// <summary>The grenade name in a projectile's designer name, as in <c>hegrenade</c>; null for other entities.</summary>
    private static string? GrenadeName(string designerName) =>
        designerName.EndsWith(ProjectileSuffix, StringComparison.Ordinal) ? designerName[..^ProjectileSuffix.Length] : null;

    public WeaponReference? Source(int entityId, string designerName, Func<int> projectileItem)
    {
        if (GrenadeName(designerName) is { } grenade && WeaponIdentity.Grenade(grenade) is not null)
        {
            var source = WeaponReference.FromItem(projectileItem(), grenade);
            _weapons[entityId] = source;
            return source;
        }
        if (_weapons.TryGetValue(entityId, out var retained)) return retained;
        return designerName switch
        {
            "planted_c4" => new WeaponReference(C4Item, null),
            Fire or "trigger_hurt" or "worldspawn" => new WeaponReference(null, designerName),
            _ => null,
        };
    }

    public void Detonated(int projectileId, int tick, int? ownerId, WorldPosition position) =>
        _pendingFires.Record(projectileId, tick, ownerId, position);

    public int? FireSpawned(int fireId, int tick, int? ownerId, WorldPosition? origin)
    {
        if (_pendingFires.Take(tick, ownerId, origin) is not { } projectileId) return null;
        if (_weapons.TryGetValue(projectileId, out var weapon))
            _weapons[fireId] = weapon;
        return projectileId;
    }

    public void Deleted(int entityId)
    {
        _weapons.Remove(entityId);
        _pendingFires.Remove(entityId);
    }

    public void Clear()
    {
        _pendingFires.Clear();
        _weapons.Clear();
    }
}
