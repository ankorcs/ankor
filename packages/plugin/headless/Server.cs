using System.Text.Json.Nodes;
using Ankor.Core;
using Ankor.Core.Wire;
using Ankor.Tests;

namespace Ankor.Headless;

/// <summary>A loaded match as the runtime reaches it, the way LoadedMatch hands it the engine's map loads.</summary>
internal sealed class HeadlessMatch(MatchSession session, IGameHost host, string version) : PluginRuntime.IMatch
{
    public MatchSession Session => session;
    public MatchState State => session.Match;
    public IDisposable? BeginCapture(string name) => null;
    public string? RestoreRound(int round) => session.Backups.Restore(round);
    public string? StartMatch() => session.Match.StartFromController();

    public void OnMapLoad(string mapName)
    {
        if (session.OnMapLoad(mapName, version, host.FrameworkVersion, () => host.ServerVersion))
            session.Callbacks.Handle(new MapLoadInput());
    }

    public void Dispose() => session.Dispose();
}

/// <summary>The runtime's lines, on the console as a server prints them.</summary>
internal sealed class ConsoleLog(GameThreadHost host) : PluginRuntime.ILog
{
    public void ConfigMissing(string file) => host.Log(LogLevel.Info, $"{file} does not exist; running without a match");
    public void ConfigInvalid(string file, string error) => host.Log(LogLevel.Error, $"{file} is invalid ({error}); running without a match");
    public void Readiness(string message) => host.Log(LogLevel.Info, message);
    public void LaunchCfg(string map, string line) => host.Log(LogLevel.Debug, $"cfg on {map} without a match: {line}");
    public void WsUrlMissing(string variable) => host.Log(LogLevel.Warn, $"{variable} is not set; events will not be forwarded");
    public void WsUrlInvalid(string variable) => host.Log(LogLevel.Error, $"{variable} is not a valid absolute URL");
    public void EventStream(string allocationId) => host.Log(LogLevel.Info, $"event stream for allocation {allocationId}");
    public void NoSocket(string frame) => host.Log(LogLevel.Debug, $"(no websocket) {frame}");
    public void Exec(string command) => host.Log(LogLevel.Debug, $"exec from the controller: {command}");
    public void SkippingType(string type) => host.Log(LogLevel.Info, $"skipping {type} frames, which this plugin does not know");
    public void IgnoredFrame(string excerpt) => host.Log(LogLevel.Warn, $"ignoring a frame from the agent that is not one of ours: {excerpt}");
}

/// <summary>
/// The game server: the engine, played by the instructions it is given, and the plugin, which
/// runs as shipped. Everything here runs on the game thread.
/// </summary>
internal sealed class Server(GameThreadHost host, string? allocationId, RelayProxy? proxy, Action<JsonObject> report)
{
    private static readonly string Version = typeof(Server).Assembly.GetName().Version is { } v ? $"{v.Major}.{v.Minor}.{v.Build}" : "0.0.0";
    private readonly FakeGameHost _engine = host.Engine;
    private PluginRuntime? _runtime;
    private bool _booted;
    private int _nextSlot;

    private MatchSession? Session => (_runtime?.Match as HeadlessMatch)?.Session;

    /// <summary>Carries out one instruction from whoever drives the server.</summary>
    public void Apply(JsonObject op)
    {
        switch ((string?)op["op"])
        {
            case "connect":
                if (proxy is not null) proxy.Hold = false;
                if (_runtime is null) Load();
                break;
            case "hold":
                if (proxy is not null) proxy.Hold = true;
                break;
            case "join":
                Join((string?)op["steam_id"] ?? "", (string?)op["name"] ?? "", Side((string?)op["side"]));
                break;
            case "round_start":
                RoundStart();
                break;
            case "round_end":
                RoundEnd(Side((string?)op["winner"]), (int?)op["reason"] ?? 8);
                break;
            case "advance":
                _engine.Advance((double?)op["seconds"] ?? 0);
                break;
            default:
                report(new JsonObject { ["unknown_op"] = op.DeepClone() });
                break;
        }
    }

    /// <summary>
    /// The server starts: the plugin loads as SwiftlyS2 loads it and dials the relay. The start map
    /// loads in <see cref="Boot"/>, once the plugin has spoken on the relay, as a real server takes
    /// seconds to load the map after the plugin connects.
    /// </summary>
    private void Load()
    {
        if (allocationId is null)
        {
            host.Log(LogLevel.Error, $"{AnkorEnv.AllocationId} is not a lowercase UUID, so this server has no identity for its events; Ankor does nothing. The agent sets it on every launch");
            return;
        }

        var runtime = _runtime = new PluginRuntime(host, allocationId, new ConsoleLog(host));
        var config = runtime.ReadConfig();
        runtime.StartReadiness(Environment.GetEnvironmentVariable(Cs2Env.WorkshopMap), Environment.GetEnvironmentVariable(Cs2Env.StartMap));
        if (config is not null)
            runtime.Match = new HeadlessMatch(new MatchSession(host, host, config, MatchFiles.Server, runtime.Link,
                Fixture.Messages, clearGrenades: () => { }, beforeShutdown: _ => { }), host, Version);
        runtime.StartWebSocket(proxy?.Url.ToString() ?? Environment.GetEnvironmentVariable(AnkorEnv.WsUrl));
        if (proxy is null) Boot();
    }

    /// <summary>The start map loads, Steam logs the server on, and two seconds of boot pass.</summary>
    public void Boot()
    {
        if (_runtime is not { } runtime || _booted) return;
        _booted = true;
        // The launch cfg the agent rendered and mounted, which exec reads as the real server does.
        var launchCfg = Path.Combine(ServerPaths.GameDir, "cfg", CfgNames.Launch + ".cfg");
        if (File.Exists(launchCfg)) _engine.CfgFiles[CfgNames.Launch + ".cfg"] = File.ReadAllText(launchCfg);

        var map = Environment.GetEnvironmentVariable(Cs2Env.StartMap) is { Length: > 0 } start ? start : "de_dust2";
        _engine.ChangeMap(map);
        runtime.OnMapLoad(map);
        _engine.SteamLoggedOn = true;
        _engine.ServerSteamId = ServerSteamId;
        runtime.OnSteamApiActivated();
        // The server boots for a couple of seconds, in which warmup starts and the plugin finds the
        // server ready, whether or not the clock runs by itself.
        _engine.Advance(BootSeconds);
    }

    private const double BootSeconds = 2;

    /// <summary>The persistent game server account <c>[G:1:1]</c>, which Steam would give the slot's token.</summary>
    private const string ServerSteamId = "85568392920039425";

    /// <summary>The plugin unloads as the server stops.</summary>
    public void Unload() => _runtime?.Unload();

    private void Handle(GameplayInput input) => Session?.Callbacks.Handle(input);

    private static Side Side(string? name) => name switch
    {
        "ct" => Core.Wire.Side.Ct,
        "t" => Core.Wire.Side.T,
        _ => Core.Wire.Side.Spec,
    };

    /// <summary>A player connects, is authorised by Steam, and joins a side.</summary>
    private void Join(string steamId, string name, Side side)
    {
        var player = Fixture.Player(steamId, Core.Wire.Side.Spec, _nextSlot++, name: name);
        _engine.PlayerList.Add(player);
        Handle(new ClientAuthorizedInput(player));
        Handle(new PlayerConnectInput(player));
        if (side == Core.Wire.Side.Spec) return;
        var joined = player with { Side = side };
        _engine.PlayerList[_engine.PlayerList.IndexOf(player)] = joined;
        Handle(new PlayerTeamInput(joined, Core.Wire.Side.Spec, side));
    }

    /// <summary>The engine starts a round: out of warmup, through freeze time. It writes the round's
    /// backup as it starts, as `mp_backup_round_auto` has it do.</summary>
    private void RoundStart()
    {
        Handle(new RoundPrestartInput());
        _engine.IsWarmup = false;
        _engine.RoundStartTime = _engine.Clock;
        _engine.FreezePeriod = true;
        WriteBackup(_engine.RoundsPlayed + 1);
        Handle(new RoundStartInput());
        _engine.FreezePeriod = false;
        Handle(new RoundFreezeEndInput());
    }

    /// <summary>The engine scores the round on its scoreboard, then ends it.</summary>
    private void RoundEnd(Side winner, int reason)
    {
        if (Session?.Match is { IsLive: true } match && winner is Core.Wire.Side.Ct or Core.Wire.Side.T)
            _engine.ScoreRound(match, winner);
        _engine.RoundsPlayed++;
        Handle(new RoundEndInput(winner, reason));
    }

    private void WriteBackup(int round)
    {
        var path = Path.Combine(Core.Wire.Backups.Path, Core.Wire.Backups.FileNameForRound(round));
        try
        {
            Directory.CreateDirectory(Core.Wire.Backups.Path);
            File.WriteAllText(path, Fixture.EmptyBackup);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            host.Log(LogLevel.Error, $"could not write {path}: {error.Message}");
        }
    }
}
