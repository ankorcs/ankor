namespace Ankor.Core;

/// <summary>
/// One game-thread timer slot per owner. Replacing, cancelling or disposing the slot invalidates
/// callbacks already selected by the scheduler. SwiftlyS2 owns final plugin/map scheduler cleanup;
/// this slot enforces the shorter match and phase lifetimes described in packages/plugin/README.md.
/// Work that repeats uses <see cref="ScheduleRepeating"/>: a <see cref="Schedule"/> callback that
/// schedules itself again fires a frame late each time, and the lateness adds up.
/// </summary>
internal sealed class OwnedTimer(IGameHost host, bool stopOnMapChange = true) : IDisposable
{
    private IHostTimer? _pending;
    private int _revision;
    private bool _disposed;

    public bool IsPending => _pending is not null && !_pending.IsCancellationRequested;

    public void Schedule(double seconds, Action callback)
    {
        if (_disposed) return;
        Cancel();
        var revision = _revision;
        _pending = host.AddTimer(seconds, () =>
        {
            if (_disposed || revision != _revision || !IsPending) return;
            Cancel();
            callback();
        }, stopOnMapChange);
    }

    /// <summary>
    /// Every <paramref name="periodSeconds"/> after <paramref name="delaySeconds"/>, until the slot is
    /// cancelled, replaced or disposed. A callback that is done cancels the slot itself.
    /// </summary>
    public void ScheduleRepeating(double delaySeconds, double periodSeconds, Action callback)
    {
        if (_disposed) return;
        Cancel();
        var revision = _revision;
        _pending = host.AddRepeatingTimer(delaySeconds, periodSeconds, () =>
        {
            if (_disposed || revision != _revision || !IsPending) return;
            callback();
        }, stopOnMapChange);
    }

    public void Cancel()
    {
        ++_revision;
        var pending = _pending;
        _pending = null;
        pending?.Cancel();
    }

    public void Dispose()
    {
        _disposed = true;
        Cancel();
    }
}
