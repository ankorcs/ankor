using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// Watches the event buffer for a backlog: the agent acknowledges once its own spool holds the
/// events, whatever the controller is doing, so a backlog means an agent that hangs with its socket still open,
/// and nothing else would say so until the buffer overflows a whole match later. When the oldest unacknowledged event has waited <see cref="BacklogAfterSeconds"/> the
/// monitor reports once, reminds every <see cref="RemindEverySeconds"/>, and reports the peak and
/// the duration when the backlog clears. Reports go wherever the caller sends them: the server log
/// and, as a log event, the match's own record — which is delivered once the backlog drains, so
/// the record shows when and for how long it was blind.
/// </summary>
public sealed class BacklogMonitor(
    IGameHost host,
    IEventBacklog backlog,
    Action<LogLevel, string> report) : IDisposable
{
    public const double CheckIntervalSeconds = 10;
    public const double BacklogAfterSeconds = 30;
    public const double RemindEverySeconds = 60;

    private readonly OwnedTimer _timer = new(host, stopOnMapChange: false);
    private bool _disposed;
    private bool _backlogged;
    private double _since;
    private double _lastReport;
    private long _peak;

    public void Start()
    {
        if (_disposed || _timer.IsPending) return;
        Schedule();
    }

    private void Schedule() => _timer.ScheduleRepeating(CheckIntervalSeconds, CheckIntervalSeconds, Check);

    private void Check()
    {
        var (pending, oldest) = (backlog.PendingCount, backlog.OldestPendingAgeSeconds);
        // This monitor survives map changes, so its duration clock must also survive them.
        var now = host.NowMs / 1000d;

        if (oldest is { } age && age >= BacklogAfterSeconds)
        {
            _peak = Math.Max(_peak, pending);
            if (!_backlogged)
            {
                _backlogged = true;
                _since = now - age;
                _lastReport = now;
                report(LogLevel.Warn,
                    $"event backlog: {pending} events waiting, the oldest for {age:0}s; the agent is not acknowledging");
            }
            else if (now - _lastReport >= RemindEverySeconds)
            {
                _lastReport = now;
                report(LogLevel.Warn,
                    $"event backlog continues: {pending} events waiting, the oldest for {age:0}s");
            }
        }
        else if (_backlogged)
        {
            _backlogged = false;
            report(LogLevel.Info,
                $"event backlog cleared after {now - _since:0}s; it peaked at {_peak} events waiting");
            _peak = 0;
        }
    }

    public void Dispose()
    {
        _disposed = true;
        _timer.Dispose();
    }
}
