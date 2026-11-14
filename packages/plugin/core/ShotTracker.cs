using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// Count every shot fired per player and weapon, so accuracy has a denominator to go with the hits
/// already carried by player_hurt. One message for each live round when it closes, also when nobody
/// fired, with the players who did.
/// </summary>
public sealed class ShotTracker
{
    private sealed class Tally(PlayerRef shooter, WeaponReference weapon)
    {
        public readonly PlayerRef Shooter = shooter;
        public readonly WeaponReference Weapon = weapon;
        public readonly List<double> Ticks = [];
    }

    /// <summary>Each open round's tallies, by shooter and weapon.</summary>
    private readonly SortedDictionary<int, Dictionary<(string SteamId, int? Weapon, string? Fallback), Tally>> _rounds = [];

    /// <summary>Shots counted for each shooter of each open round, over all their weapons.</summary>
    private readonly Dictionary<(int Round, string SteamId), int> _counted = [];

    /// <summary>A live round started: it gets its message when it closes, whoever fires.</summary>
    public void Begin(int round) => _rounds.TryAdd(round, []);

    /// <summary>The round is the one the shot was fired in: the counter has already moved on by the
    /// time the closing round start flushes these.
    /// </summary>
    public void Record(PlayerRef shooter, WeaponReference weapon, int round, double tick)
    {
        // A round_shots past the contract's limit is rejected whole, so a player's shots past it go uncounted.
        var counted = _counted.GetValueOrDefault((round, shooter.steamId));
        if (counted >= RoundShots.MaxShots) return;
        _counted[(round, shooter.steamId)] = counted + 1;

        if (!_rounds.TryGetValue(round, out var tallies)) _rounds[round] = tallies = [];
        var key = (shooter.steamId, weapon.Id, weapon.Fallback);
        if (!tallies.TryGetValue(key, out var tally)) tallies[key] = tally = new Tally(shooter, weapon);
        tally.Ticks.Add(tick);
    }

    public void Clear()
    {
        _rounds.Clear();
        _counted.Clear();
    }

    /// <summary>One message for each open round, in round order, leaving the tracker empty.</summary>
    public List<RoundShots> Flush()
    {
        var pending = _rounds.Select(round => new RoundShots
        {
            round = round.Key,
            players = round.Value.Values
                .GroupBy(tally => tally.Shooter.steamId, StringComparer.Ordinal)
                .OrderBy(shooter => shooter.Key, StringComparer.Ordinal)
                .Select(shooter => new PlayerShots
                {
                    player = shooter.First().Shooter,
                    weapons = shooter
                        .OrderBy(tally => tally.Weapon.Id ?? 0)
                        .ThenBy(tally => tally.Weapon.Fallback ?? "", StringComparer.Ordinal)
                        .Select(tally => new WeaponShots
                        {
                            weapon = Weapon.Of(tally.Weapon),
                            shots = tally.Ticks.Count,
                            ticks = tally.Ticks.ToArray(),
                        })
                        .ToList(),
                })
                .ToList(),
        }).ToList();

        Clear();
        return pending;
    }
}
