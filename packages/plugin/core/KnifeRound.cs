using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>Alive-player and health tally; Winner is null on an exact tie.</summary>
public sealed record KnifeTally(Side? Winner, int TAlive, int CtAlive, int THealth, int CtHealth);

/// <summary>Compare alive count, then health. Do not use the engine’s objective-based winner except to break a tie.</summary>
public static class KnifeRound
{
    public static KnifeTally Decide(IEnumerable<HostPlayer> players)
    {
        int tAlive = 0, ctAlive = 0, tHealth = 0, ctHealth = 0;

        foreach (var p in players)
        {
            if (p.IsBot || !p.IsAlive || p.Health <= 0) continue;

            var health = p.Health;

            switch (p.Side)
            {
                case Side.T:
                    tAlive++;
                    tHealth += health;
                    break;
                case Side.Ct:
                    ctAlive++;
                    ctHealth += health;
                    break;
            }
        }

        Side? winner = null;
        if (tAlive != ctAlive) winner = tAlive > ctAlive ? Side.T : Side.Ct;
        else if (tHealth != ctHealth) winner = tHealth > ctHealth ? Side.T : Side.Ct;

        return new KnifeTally(winner, tAlive, ctAlive, tHealth, ctHealth);
    }

    /// <summary>Resolve an exact tie using the supplied fallback side.</summary>
    public static Side Resolve(KnifeTally tally, Side tieBreak) =>
        tally.Winner ?? (tieBreak == Side.Spec ? Side.T : tieBreak);
}
