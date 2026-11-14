namespace Ankor.Core;

/// <summary>
/// A match server quits after its map. Keep the result visible, then drain to the receiver
/// before exiting; a map change must not cancel the delay or acknowledgement checks.
/// With GOTV on, it also stays up until the broadcast, tv_delay behind, has shown the map's end,
/// whether a demo records or not.
/// The agent hears of the quit first, which stops a game that hangs on its way out.
/// </summary>
public sealed class MatchShutdown(IGameHost host, IAgentChannel agent, Func<bool> recordingFlushed, IMessages messages)
    : IDisposable
{
    /// <summary>
    /// Quitting ends the process, which drops a message still unsent. A socket that cannot send it
    /// does not hold the server.
    /// </summary>
    private const double QuittingSendSeconds = 2;
    /// <summary>How often the drain looks again, after the kick.</summary>
    private const double DrainPollSeconds = 0.1;
    private const double KickDelaySeconds = 60;
    /// <summary>Long enough to read the end screen and the chat line saying the match was canceled.</summary>
    private const double UnplayedKickDelaySeconds = 10;

    private readonly OwnedTimer _timer = new(host, stopOnMapChange: false);
    private bool _started;
    /// <summary>When the broadcast has shown the map's end; at the start without GOTV.</summary>
    private long _broadcastShownAtMs;
    private bool _disposed;
    private Task? _quitting;
    /// <summary>When to quit without the agent's word that the quitting message went out.</summary>
    private long _quittingGivesUpAtMs;

    /// <param name="unplayed">The ready deadline ended a match that never started: the engine ends
    /// the match as aborted, the players are kicked sooner, and there is no broadcast to wait for.</param>
    public void Start(bool unplayed = false)
    {
        if (_started || _disposed) return;
        _started = true;
        host.ExecuteCommand("mp_match_restart_delay 86400");
        host.ExecuteCommand("sv_hibernate_when_empty 0");
        if (unplayed) host.AbortMatch();
        var broadcastSeconds = !unplayed && host.GetConVarBool("tv_enable") == true
            ? Math.Max(0, host.GetConVarInt("tv_delay") ?? 0) + DemoRecorder.StopMargin
            : 0;
        _broadcastShownAtMs = host.NowMs + broadcastSeconds * 1000L;
        var kick = unplayed ? messages.Get("kick.ready_timeout") : messages.Get("kick.match_ended");
        _timer.Schedule(unplayed ? UnplayedKickDelaySeconds : KickDelaySeconds, () =>
        {
            foreach (var player in host.Players) host.Kick(player, kick);
            // The native host queues the kicks, so the drain starts polling a tick later.
            _timer.ScheduleRepeating(DrainPollSeconds, DrainPollSeconds, Drain);
        });
    }

    /// <summary>Polled until the events are sent, the demo is flushed and the broadcast has shown the end, then quits.</summary>
    private void Drain()
    {
        if (agent.PendingCount > 0 || !recordingFlushed() || host.NowMs < _broadcastShownAtMs) return;
        if (_quitting is null)
        {
            _quitting = agent.SendQuitting();
            _quittingGivesUpAtMs = host.NowMs + (long)(QuittingSendSeconds * 1000);
        }
        if (!_quitting.IsCompleted && host.NowMs < _quittingGivesUpAtMs) return;
        _timer.Cancel();
        host.ExecuteCommand("quit");
    }

    public void Dispose()
    {
        _disposed = true;
        _timer.Dispose();
    }
}
