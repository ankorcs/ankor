using Ankor.Core.Wire;

namespace Ankor.Core;

public readonly record struct DamageSnapshot(
    int Health,
    int Inflictor,
    WeaponReference? Weapon,
    int EquipmentValue,
    IReadOnlyList<InventoryItem>? GrenadesHeld);

/// <summary>
/// Native damage calls nest. Every pre/post pair owns a frame, even without a player snapshot,
/// so a skipped inner call cannot consume the outer call's snapshot. Round results wait until
/// the outermost post has released its inventory and buy samples.
/// </summary>
public sealed class DamageTracker(EventForwarder forwarder)
{
    private sealed class DamageCall(nint entity)
    {
        public nint Entity { get; } = entity;
        public DamageSnapshot? Snapshot;
        public ulong? BuyPlayerId;
    }

    private readonly Stack<DamageCall> _calls = new();
    private readonly Queue<Action> _pending = new();

    public void Enter(nint entity) => _calls.Push(new DamageCall(entity));

    public void Capture(DamageSnapshot snapshot, HostPlayer? buyPlayer)
    {
        var call = _calls.Peek();
        call.Snapshot = snapshot;
        if (buyPlayer is not null && forwarder.OnPlayerDamageStarting(buyPlayer))
            call.BuyPlayerId = buyPlayer.SteamId;
    }

    public DamageSnapshot? Current(nint entity)
    {
        foreach (var call in _calls)
            if (call.Entity == entity) return call.Snapshot;
        return null;
    }

    /// <summary>
    /// A reported hit leaves the victim at <paramref name="health"/>: an outer call on the same
    /// victim still open reads its next hit from there, not from before the inner one.
    /// </summary>
    public void Hurt(nint entity, int health)
    {
        foreach (var call in _calls)
            if (call.Entity == entity && call.Snapshot is { } snapshot) call.Snapshot = snapshot with { Health = health };
    }

    public void RunAfterDamage(Action action)
    {
        if (_calls.Count == 0) action();
        else _pending.Enqueue(action);
    }

    public void Exit()
    {
        if (!_calls.TryPop(out var call)) return;
        Release(call);
        while (_calls.Count == 0 && _pending.TryDequeue(out var action)) action();
    }

    public void Reset()
    {
        _pending.Clear();
        while (_calls.TryPop(out var call)) Release(call);
    }

    private void Release(DamageCall call)
    {
        if (call.BuyPlayerId is { } steamId) forwarder.OnPlayerDamageFinished(steamId);
    }
}
