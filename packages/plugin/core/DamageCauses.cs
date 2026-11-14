using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// What caused damage or a death, from what the engine reports of it. Tested in CS2 in November
/// 2026: a fall comes as no attacker and the "worldent" weapon, the planted bomb as no attacker and
/// the C4 item, and a player who leaves alive dies right after their disconnect, credited to
/// themselves with the "world" weapon.
/// </summary>
public static class DamageCauses
{
    /// <param name="victimId">The victim's SteamID64.</param>
    /// <param name="attackerId">The credited attacker's SteamID64; null when no player dealt it.</param>
    /// <param name="weapon">The weapon as the event carries it.</param>
    /// <param name="disconnected">The victim disconnected in this round.</param>
    public static DamageCause Of(string victimId, string? attackerId, WeaponReference weapon, bool disconnected)
    {
        var byVictim = attackerId == victimId;
        if (disconnected && (attackerId is null || byVictim)) return DamageCause.Disconnect;
        if (attackerId is not null && !byVictim) return DamageCause.Player;
        // A player's own grenade or fire carries its item; the world's own kill of them carries none.
        if (byVictim) return weapon.Id is null ? DamageCause.Other : DamageCause.Self;
        if (weapon.Id == GrenadeAttribution.C4Item) return DamageCause.Bomb;
        return weapon.Fallback == "worldent" ? DamageCause.Fall : DamageCause.Other;
    }
}
