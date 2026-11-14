using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// `+host_workshop_map` in the launch arguments runs before the Steam API is up and is dropped, so
/// a workshop container boots on a stock map and the plugin hosts the real map once Steam activates.
/// One shot per process: a later map change arrives as an exec frame from the agent.
/// </summary>
public sealed class WorkshopMapLoader(IGameHost host, string? workshopId)
{
    private bool _handled;
    private bool _cancelled;

    /// <summary>
    /// The id to host, or null when this container launched on a stock map. The agent sets it from a
    /// launch config its contract validated as a decimal file id.
    /// </summary>
    public string? WorkshopId { get; } = string.IsNullOrEmpty(workshopId) ? null : workshopId;

    /// <summary>The map change ran; the next map to load is the workshop map.</summary>
    public bool Switched { get; private set; }

    /// <summary>Returns whether this call queued the map change.</summary>
    public bool OnSteamApiActivated()
    {
        if (_handled) return false;
        _handled = true;

        if (WorkshopId is null) return false;

        // Leave the activation callback before changing map.
        host.NextFrame(() =>
        {
            if (_cancelled) return;
            Switched = true;
            host.ExecuteCommand($"host_workshop_map {WorkshopId}");
            host.Log(LogLevel.Info, $"switching to workshop map {WorkshopId}");
        });
        return true;
    }

    /// <summary>A runtime match request supersedes the launch-time workshop bootstrap.</summary>
    public void Cancel()
    {
        _cancelled = true;
        _handled = true;
    }

}
