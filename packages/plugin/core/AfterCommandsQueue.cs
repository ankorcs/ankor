using Ankor.Core.Wire;

namespace Ankor.Core;

public sealed class AfterCommandsQueue(Action<string> execute, Func<double, Action, IHostTimer> addTimer,
    Action<int> applied, Action<int> timedOut)
{
    // The marker runs in the same frame as the cfgs before it, so after this it was lost.
    public const double TimeoutSeconds = 2;

    private sealed class Pending(int id, Action action)
    {
        public int Id { get; } = id;
        public Action Action { get; } = action;
        public bool Ran { get; set; }
    }

    // The marker comes out of a cfg, which cannot carry an id, so markers settle in queue order. One
    // that timed out keeps its place until its own marker arrives, or that marker would settle the next.
    private readonly Queue<Pending> _pending = new();
    private int _count;

    public void Add(Action action)
    {
        var pending = new Pending(++_count, action);
        _pending.Enqueue(pending);
        execute($"exec {CfgNames.Applied}");
        addTimer(TimeoutSeconds, () =>
        {
            if (pending.Ran) return;
            pending.Ran = true;
            timedOut(pending.Id);
            pending.Action();
        });
    }

    public void Applied()
    {
        if (!_pending.TryDequeue(out var pending) || pending.Ran) return;
        pending.Ran = true;
        applied(pending.Id);
        pending.Action();
    }

    public void Clear()
    {
        foreach (var pending in _pending) pending.Ran = true;
        _pending.Clear();
    }
}
