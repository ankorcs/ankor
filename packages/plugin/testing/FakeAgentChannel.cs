using Ankor.Core;
using Ankor.Core.Wire;

namespace Ankor.Tests;

/// <summary>
/// An agent channel a test reads and sets by hand: it keeps what the match sends, and counts how
/// often the plugin reads the backlog.
/// </summary>
public sealed class FakeAgentChannel(List<PluginMessage>? sent = null, List<MatchReport>? reports = null) : IAgentChannel
{
    private long _pending;

    public List<PluginMessage> Sent { get; } = sent ?? [];

    public List<MatchReport> Reports { get; } = reports ?? [];

    /// <summary>Runs on each quitting message; completed immediately by default.</summary>
    public Func<Task> Quitting { get; init; } = () => Task.CompletedTask;

    public void Send(PluginMessage message) => Sent.Add(message);

    public void ReportMatch(MatchReport report) => Reports.Add(report);

    public Task SendQuitting() => Quitting();

    public long PendingCount
    {
        get
        {
            Reads++;
            return _pending;
        }
        set => _pending = value;
    }

    public double? OldestPendingAgeSeconds { get; set; }

    /// <summary>How often <see cref="PendingCount"/> was read: once per backlog check or drain poll.</summary>
    public int Reads { get; private set; }

    public void SetBacklog(long pending, double? oldestAgeSeconds)
    {
        _pending = pending;
        OldestPendingAgeSeconds = oldestAgeSeconds;
    }
}
