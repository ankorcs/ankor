using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>Own physical grenade associations in the game; emit each grenade's throw, detonation and end as they happen.</summary>
public sealed class GrenadeTracker(Action<PluginMessage> emit)
{
    /// <summary>
    /// How far a live smoke's detonation point may be from a fire it put out. The engine
    /// extinguishes flames inside the smoke's voxel cloud (about 150 units across) and a fire
    /// spreads from its own origin, so this bounds the two together with margin. Nothing on
    /// either entity names the other; nearest live smoke within this radius is credited.
    /// </summary>
    public const float SmokeExtinguishRadius = 300f;

    private sealed class Pending(long id, int round, double thrownRoundTime, GrenadeThrow throwEvent)
    {
        public readonly long Id = id;
        public readonly int Round = round;
        public readonly double ThrownRoundTime = thrownRoundTime;
        public readonly GrenadeThrow Throw = throwEvent;
        public bool Detonated;
        public WirePosition? DetonatePos;
        public FireEnd? FireEnd;
        public long? ExtinguishedBy;
        public readonly HashSet<int> Entities = [];
        /// <summary>When the fire went out; the engine keeps the dead inferno entity until the round resets.</summary>
        public double? FireEndedAt;
    }

    private readonly Dictionary<int, Pending> _entities = [];
    // One tracker serves one server, so ids stay unique within it.
    private long _nextId;

    public void Throw(int entityId, int round, double roundTime, GrenadeThrow throwEvent)
    {
        // A reused entity id ends whatever it still named, no later than now.
        RemoveEntity(entityId, roundTime);
        throwEvent.grenadeId = _nextId++;
        var pending = new Pending(throwEvent.grenadeId, round, roundTime, throwEvent);
        pending.Entities.Add(entityId);
        _entities.Add(entityId, pending);
        emit(throwEvent);
    }

    /// <summary>The throw-time identity survives disconnection and follows an attached inferno.</summary>
    public PlayerRef? Owner(int entityId) =>
        _entities.TryGetValue(entityId, out var grenade) ? grenade.Throw.player : null;

    /// <summary>The grenade an entity belongs to; its throw went out when it was thrown.</summary>
    public long? Reference(int entityId) => _entities.TryGetValue(entityId, out var grenade) ? grenade.Id : null;

    /// <summary>The first detonation wins; a projectile detonates once.</summary>
    public void Detonate(int entityId, WorldPosition position)
    {
        if (!_entities.TryGetValue(entityId, out var grenade)) return;
        SendDetonate(grenade, WirePosition.From(position));
    }

    /// <summary>A fire that attaches has lit; how it ends is refined by the inferno events and its removal.</summary>
    public void AttachFire(int projectileId, int fireId)
    {
        if (!_entities.TryGetValue(projectileId, out var grenade)) return;
        SendDetonate(grenade, null);
        grenade.FireEnd ??= FireEnd.Expired;
        grenade.Entities.Add(fireId);
        _entities[fireId] = grenade;
    }

    /// <summary>
    /// inferno_extinguish names the fire. Null when the fire is not tracked, otherwise whether a
    /// live smoke within <see cref="SmokeExtinguishRadius"/> took the credit. Smothered stays
    /// smothered: the engine may report the extinguish for a fire that never burned.
    /// </summary>
    public bool? Extinguish(int fireId, double roundTime, WorldPosition position)
    {
        if (!_entities.TryGetValue(fireId, out var fire)) return null;
        if (fire.FireEnd != FireEnd.Smothered) fire.FireEnd = FireEnd.Extinguished;
        fire.FireEndedAt ??= roundTime;
        fire.ExtinguishedBy ??= CreditSmoke(position);
        return fire.ExtinguishedBy is not null;
    }

    /// <summary>inferno_expire: the fire burned out on its own.</summary>
    public void Expire(int fireId, double roundTime)
    {
        if (!_entities.TryGetValue(fireId, out var fire)) return;
        fire.FireEndedAt ??= roundTime;
    }

    /// <summary>
    /// The engine sets m_bWasCreatedInSmoke in the inferno's first think, after it spawned, so the
    /// flag is read when the entity goes: a fire that lit inside a smoke never burned.
    /// </summary>
    public void Smother(int fireId, double roundTime, WorldPosition? origin)
    {
        if (!_entities.TryGetValue(fireId, out var fire)) return;
        fire.FireEnd = FireEnd.Smothered;
        fire.FireEndedAt = roundTime;
        if (origin is { } at) fire.ExtinguishedBy ??= CreditSmoke(at);
    }

    private long? CreditSmoke(WorldPosition at)
    {
        Pending? best = null;
        var bestDistance = double.MaxValue;
        foreach (var pending in _entities.Values.Distinct())
        {
            if (pending.Throw.grenade != GrenadeKind.SmokeGrenade || pending.DetonatePos is not { } pos) continue;
            var distance = Math.Sqrt(Math.Pow(pos.x - at.X, 2) + Math.Pow(pos.y - at.Y, 2) + Math.Pow(pos.z - at.Z, 2));
            if (distance > SmokeExtinguishRadius || distance >= bestDistance) continue;
            best = pending;
            bestDistance = distance;
        }
        return best?.Id;
    }

    /// <summary>
    /// A hit dealt by a tracked grenade that has not detonated: the projectile itself striking a
    /// player. The scoreboard's utility damage does not count it, so <c>player_hurt</c> says so.
    /// </summary>
    public bool IsProjectileImpact(int entityId) =>
        _entities.TryGetValue(entityId, out var grenade) && !grenade.Detonated;

    /// <summary>
    /// The last entity going ends the effect at this round time. A removal timed before the throw
    /// means the round clock has already reset for the next round: the engine cleared the entity
    /// when the round closed, which is <paramref name="roundClosesAt"/> on the old clock.
    /// </summary>
    public void RemoveEntity(int entityId, double roundTime, double? roundClosesAt = null)
    {
        if (!_entities.Remove(entityId, out var grenade)) return;
        grenade.Entities.Remove(entityId);
        if (grenade.Entities.Count > 0) return;
        var end = roundClosesAt is { } closed && roundTime < grenade.ThrownRoundTime ? closed : roundTime;
        Complete(grenade, grenade.FireEndedAt ?? end);
    }

    /// <summary>
    /// Forgets every grenade without ending it: a restore cut their round off, and nothing of it
    /// is sent. Ids keep counting, so a later grenade never reuses one.
    /// </summary>
    public void Drop() => _entities.Clear();

    /// <summary>
    /// The round closed on whatever is still alive, at this time on the round's own clock. A reset
    /// can pass a clock that already restarted, so an end never precedes its throw.
    /// </summary>
    public void Flush(double roundClosesAt)
    {
        var remaining = _entities.Values.Distinct().ToArray();
        _entities.Clear();
        foreach (var grenade in remaining)
            Complete(grenade, Math.Max(grenade.ThrownRoundTime, grenade.FireEndedAt ?? roundClosesAt));
    }

    private void SendDetonate(Pending grenade, WirePosition? position)
    {
        if (grenade.Detonated) return;
        grenade.Detonated = true;
        grenade.DetonatePos = position;
        emit(new GrenadeDetonate { grenadeId = grenade.Id, player = grenade.Throw.player, detonatePos = position });
    }

    private void Complete(Pending grenade, double endRoundTime)
    {
        emit(new GrenadeEnd
        {
            round = grenade.Round,
            grenadeId = grenade.Id,
            player = grenade.Throw.player,
            endRoundTime = endRoundTime,
            fireEnd = grenade.FireEnd,
            extinguishedBy = grenade.ExtinguishedBy,
        });
    }
}
