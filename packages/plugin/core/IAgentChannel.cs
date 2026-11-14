using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// The plugin's events that the agent has not acknowledged yet. <see cref="AgentLink"/> reads them
/// off its socket's queue, which keeps each event until the agent acknowledges it.
/// </summary>
public interface IEventBacklog
{
    /// <summary>Events sent to the agent and not yet acknowledged.</summary>
    long PendingCount { get; }

    /// <summary>Seconds the oldest of them has waited; null when none waits.</summary>
    double? OldestPendingAgeSeconds { get; }
}

/// <summary>What a match sends its agent.</summary>
public interface IMatchOutput
{
    /// <summary>Sends a gameplay event.</summary>
    void Send(PluginMessage message);

    /// <summary>Sends a phase, score or backup report.</summary>
    void ReportMatch(MatchReport report);
}

/// <summary>A match's connection to its agent: what it sends, and the events not acknowledged yet.</summary>
public interface IAgentChannel : IMatchOutput, IEventBacklog
{
    /// <summary>Tells the agent the server is about to quit; completes once that is sent.</summary>
    Task SendQuitting();
}
