namespace Ankor.Core;

/// <summary>
/// A molotov detonation is followed synchronously by its inferno spawn. Match by the native
/// owner when available, otherwise by exact origin, within that engine tick. Ambiguous matches
/// are never guessed; a recycled player slot cannot transfer a grenade to a different thrower.
/// </summary>
public sealed class PendingFireTracker
{
    private sealed record Pending(int Entity, int Tick, int? Owner, WorldPosition Position);
    private readonly List<Pending> _pending = [];

    public void Record(int entity, int tick, int? owner, WorldPosition position)
    {
        _pending.RemoveAll(p => p.Tick != tick || p.Entity == entity);
        _pending.Add(new(entity, tick, owner, position));
    }

    public int? Take(int tick, int? owner, WorldPosition? position)
    {
        _pending.RemoveAll(p => p.Tick != tick);
        var candidates = _pending.Where(p =>
            owner is not null && p.Owner is not null
                ? p.Owner == owner
                : position is not null && p.Position == position.Value).ToArray();
        if (candidates.Length != 1) return null;
        _pending.Remove(candidates[0]);
        return candidates[0].Entity;
    }

    public void Remove(int entity) => _pending.RemoveAll(p => p.Entity == entity);
    public void Clear() => _pending.Clear();
}
