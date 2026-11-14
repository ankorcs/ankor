using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// Players can join once the server is logged on to Steam and has loaded the map its launch asked
/// for: the first map, or on a workshop launch the first map after <c>host_workshop_map</c> ran.
/// Checked once a second from the Steam API's activation until both hold; ready stays ready.
/// With a token that Steam refuses, CS2 keeps retrying that token and never logs on anonymously
/// (tested on CS2 in October 2026), so the agent reads ready as Steam accepting the slot's token.
/// </summary>
/// <param name="log">Readiness is logged on every server, also one without a match, whose host log is off.</param>
public sealed class ServerReadiness(IGameHost host, WorkshopMapLoader workshopMap, string? startMap, Action onReady, Action<string> log) : IDisposable
{
    private enum Waiting { None, Map, Logon }

    // The workshop map loads on a later level, so the check outlives map changes.
    private readonly OwnedTimer _check = new(host, stopOnMapChange: false);
    private bool _activated;
    private string? _loadedMap;
    private Waiting _logged;
    private bool _disposed;

    public bool Ready { get; private set; }

    public void OnMapLoad(string mapName)
    {
        if (workshopMap.WorkshopId is null || workshopMap.Switched) _loadedMap ??= mapName;
        Check();
    }

    public void OnSteamApiActivated()
    {
        _activated = true;
        // Not from inside the engine's activation: a Steam API call there hangs the server.
        if (!_disposed && !Ready) _check.ScheduleRepeating(1, 1, Check);
    }

    public void Dispose()
    {
        _disposed = true;
        _check.Dispose();
    }

    private void Check()
    {
        // The Steam API answers nothing useful before it is up.
        if (Ready || _disposed || !_activated) return;

        var loggedOn = host.SteamLoggedOn;
        if (loggedOn && _loadedMap is not null)
        {
            Ready = true;
            _check.Cancel();
            log($"game server ready: logged on to Steam, map {_loadedMap} loaded");
            onReady();
            return;
        }

        if (loggedOn)
            LogOnce(Waiting.Map, workshopMap.WorkshopId is { } id
                ? $"logged on to Steam; waiting for workshop map {id}"
                : $"logged on to Steam; waiting for map {startMap}");
        else if (_loadedMap is not null)
            LogOnce(Waiting.Logon, $"map {_loadedMap} loaded; waiting for the Steam logon");
    }

    private void LogOnce(Waiting state, string message)
    {
        if (_logged == state) return;
        _logged = state;
        log(message);
    }
}
