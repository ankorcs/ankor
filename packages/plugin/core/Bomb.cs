using Ankor.Core.Wire;

namespace Ankor.Core;

public static class Bomb
{
    public static BombSite? PlantedSite(int? site) => site switch { 0 => BombSite.A, 1 => BombSite.B, _ => null };

    public static double DefuseTimeRemaining(double blowTime, double now) => Math.Max(0, blowTime - now);
}
