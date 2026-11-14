using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// The plugin's life on a server, apart from the engine: its link to the agent, the workshop map,
/// readiness and the loaded match, and what it does as the server loads, logs on to Steam and
/// changes map. The SwiftlyS2 adapter calls these steps in order around its own, and a host
/// without the engine calls the same steps.
/// </summary>
public sealed class PluginRuntime : AgentLink.IServer
{
    /// <summary>The loaded match, as the runtime reaches it.</summary>
    public interface IMatch : AgentLink.IMatch, IDisposable
    {
        /// <summary>A map loaded: the match's map start and its warmup.</summary>
        void OnMapLoad(string mapName);
    }

    /// <summary>The lines the runtime logs through the adapter's loggers.</summary>
    public interface ILog : AgentLink.ILog
    {
        void ConfigMissing(string file);

        void ConfigInvalid(string file, string error);

        void Readiness(string message);

        void LaunchCfg(string map, string line);

        void WsUrlMissing(string variable);

        void WsUrlInvalid(string variable);

        void EventStream(string allocationId);
    }

    /// <summary>Seconds after a map load the launch cfg runs on a server without a match, as a match's warmup does: the mode's cfg has run by then.</summary>
    private const double LaunchCfgDelaySeconds = 1;

    private readonly IGameHost _host;
    private readonly string _allocationId;
    private readonly ILog _log;
    private WorkshopMapLoader _workshopMap = null!;
    private ServerReadiness _readiness = null!;

    /// <summary>Reported in every server status, so whoever launched the server learns why it has no match.</summary>
    public string? ConfigError { get; private set; }

    /// <summary>A warmup server has no match; a match server has exactly one, for its whole life.</summary>
    public IMatch? Match { get; set; }

    public AgentLink Link { get; }

    AgentLink.IMatch? AgentLink.IServer.Match => Match;

    bool AgentLink.IServer.Ready => _readiness.Ready;

    public PluginRuntime(IGameHost host, string allocationId, ILog log)
    {
        _host = host;
        _allocationId = allocationId;
        _log = log;
        Link = new AgentLink(host, allocationId, this, log);
    }

    /// <summary>Reads and decodes the launch's match config. Null for a server without a match, or
    /// with a config it refused, whose reason <see cref="ConfigError"/> then holds.</summary>
    public MatchConfig? ReadConfig()
    {
        var matchJson = ReadMatchConfig(out var readError);
        var config = MatchConfigFile.Decode(matchJson, readError, out var configError);
        if (configError is not null)
        {
            ConfigError = configError;
            _log.ConfigInvalid(MatchConfigFile.Path, configError);
        }
        return config;
    }

    /// <summary>The file's text; null when it is missing, which means no match, or cannot be read.</summary>
    private string? ReadMatchConfig(out string? error)
    {
        error = null;
        try
        {
            return File.ReadAllText(MatchConfigFile.Path);
        }
        catch (FileNotFoundException)
        {
            _log.ConfigMissing(MatchConfigFile.Path);
            return null;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            error = ex.Message;
            return null;
        }
    }

    /// <summary>Starts following the workshop map and readiness, from the launch's environment.</summary>
    public void StartReadiness(string? workshopMap, string? startMap)
    {
        _workshopMap = new WorkshopMapLoader(_host, workshopMap);
        _readiness = new ServerReadiness(_host, _workshopMap, startMap, () => Link.SendServerStatus(), message => _log.Readiness(message));
    }

    /// <summary>Dials the agent at the relay URL the launch gave, <paramref name="rawUrl"/>.</summary>
    public void StartWebSocket(string? rawUrl)
    {
        if (string.IsNullOrWhiteSpace(rawUrl))
        {
            _log.WsUrlMissing(AnkorEnv.WsUrl);
            return;
        }

        if (AnkorEnv.ParseWsUrl(rawUrl) is not { } url)
        {
            _log.WsUrlInvalid(AnkorEnv.WsUrl);
            return;
        }

        Link.Start(url);
        _log.EventStream(_allocationId);
    }

    /// <summary>A map loaded. The adapter drops its pending commands first.</summary>
    public void OnMapLoad(string mapName)
    {
        Match?.OnMapLoad(mapName);
        _readiness.OnMapLoad(mapName);
        // After the cfgs the map load queued, so one that sets sv_tags keeps its tags beside ours.
        _host.AfterCommands(TagServer);
        if (Match is null) ExecLaunchCfg(mapName);
    }

    /// <summary>Adds the allocation's tag to <c>sv_tags</c> unless it is there.</summary>
    private void TagServer()
    {
        if (ServerTags.With(_host.GetConVarString("sv_tags"), _allocationId) is { } tags) _host.SetConVarString("sv_tags", tags);
    }

    /// <summary>
    /// A server without a match is the launch's alone: its cfgs and commands run over the mode's
    /// on every map load, and Ankor's base, which locks teams for a match, does not.
    /// </summary>
    private void ExecLaunchCfg(string map) => _host.AddTimer(LaunchCfgDelaySeconds, () =>
    {
        var line = CfgNames.Exec(CfgNames.Launch);
        _log.LaunchCfg(map, line);
        _host.ExecuteCommand(line);
        _host.AfterCommands(TagServer);
    });

    /// <summary>Steam logged the server on.</summary>
    public void OnSteamApiActivated()
    {
        using var capture = Match?.BeginCapture("steam.activated");
        _workshopMap.OnSteamApiActivated();
        _readiness.OnSteamApiActivated();
    }

    /// <summary>Stops everything the runtime started, after the adapter unhooked the engine's events.</summary>
    public void Unload()
    {
        _workshopMap.Cancel();
        _readiness.Dispose();
        Match?.Dispose();
        Match = null;
        Link.DisposeAsync().AsTask().GetAwaiter().GetResult();
    }
}
