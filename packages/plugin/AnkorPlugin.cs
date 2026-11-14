using Microsoft.Extensions.Logging;
using Ankor.Core;
using Ankor.Core.Wire;
using Ankor.GameHost;
using SwiftlyS2.Shared;
using SwiftlyS2.Shared.Events;
using SwiftlyS2.Shared.Misc;
using SwiftlyS2.Shared.Plugins;

namespace Ankor;

[PluginMetadata(
    Id = "Ankor",
    Name = "Ankor",
    Version = AnkorPlugin.Version,
    Author = "Ankor",
    Description = "Streams CS2 match events to the Ankor controller, locks teams by SteamID, runs the knife round.")]
public sealed class AnkorPlugin : BasePlugin
{
    public const string Version = AnkorVersion.Value;

    private SwiftlyGameHost _host = null!;
    private PluginRuntime? _runtime;
    private LoadedMatch? LoadedMatch => _runtime?.Match as LoadedMatch;
    /// <summary>Set once <see cref="Load"/> has started the plugin; a refused load registers nothing to undo.</summary>
    private bool _started;
    private string _allocationId = "";
    private ILogger Logger => _host.DiagnosticsEnabled ? Core.Logger : Microsoft.Extensions.Logging.Abstractions.NullLogger.Instance;

    public AnkorPlugin(ISwiftlyCore core) : base(core) { }

    public override void Load(bool hotReload)
    {
        // Events are numbered from 0 per allocation, so a second run of the plugin on the same server
        // would number them again, and the agent would take them for replays and drop them.
        if (hotReload)
        {
            Core.Logger.LogError("Ankor was reloaded on a running server, which it does not support, and does nothing; recreate the container");
            return;
        }

        if (AnkorEnv.ParseAllocationId(Environment.GetEnvironmentVariable(AnkorEnv.AllocationId)) is not { } allocationId)
        {
            Core.Logger.LogError("{Variable} is not a lowercase UUID, so this server has no identity for its events; Ankor does nothing. The agent sets it on every launch",
                AnkorEnv.AllocationId);
            return;
        }

        _started = true;
        _allocationId = allocationId;
        _host = new SwiftlyGameHost(Core);
        var runtime = _runtime = new PluginRuntime(_host, allocationId, new RuntimeLog(this));
        var config = runtime.ReadConfig();
        _host.DiagnosticsEnabled = config is not null;
        runtime.StartReadiness(Environment.GetEnvironmentVariable(Cs2Env.WorkshopMap), Environment.GetEnvironmentVariable(Cs2Env.StartMap));
        var settings = PluginConfig.Load(Core, Core.Logger);
        // A warmup server has no match; a match server has exactly one, for its whole life.
        if (config is not null)
            runtime.Match = new LoadedMatch(Core, _host, config, runtime.Link, settings);
        Core.Event.OnMapLoad += OnMapLoad;
        Core.Event.OnCommandExecuteHook += OnCommandExecute;
        runtime.StartWebSocket(Environment.GetEnvironmentVariable(AnkorEnv.WsUrl));
        Core.Event.OnSteamAPIActivated += OnSteamApiActivated;
    }

    /// <summary>A backup load from any console, this plugin's included, is followed from before the engine acts on it.</summary>
    private void OnCommandExecute(IOnCommandExecuteHookEvent @event)
    {
        if (@event.HookMode != HookMode.Pre || LoadedMatch is not { } match) return;
        var command = @event.Command;
        if (command.ArgC < 1 || !Backups.IsLoadCommand(command.Arg(0))) return;
        // Only the engine's command is cancelled; another plugin's hook on it still runs.
        if (!match.OnBackupLoad(command.ArgC > 1 ? command.Arg(1) ?? "" : "")) @event.Result = HookResult.CancelOriginal;
    }

    public override void Unload()
    {
        if (!_started) return;
        Core.Event.OnSteamAPIActivated -= OnSteamApiActivated;
        Core.Event.OnCommandExecuteHook -= OnCommandExecute;
        Core.Event.OnMapLoad -= OnMapLoad;
        _runtime!.Unload();
        _host.UnregisterCommands();
        _host.DiagnosticsEnabled = false;
    }

    private void OnMapLoad(SwiftlyS2.Shared.Events.IOnMapLoadEvent @event)
    {
        _host.DropPendingCommands();
        _runtime!.OnMapLoad(@event.MapName);
    }

    private void OnSteamApiActivated() => _runtime!.OnSteamApiActivated();

    /// <summary>The runtime's lines, through this plugin's loggers: the diagnostics-gated one for the
    /// agent link, and SwiftlyS2's for the launch.</summary>
    private sealed class RuntimeLog(AnkorPlugin plugin) : PluginRuntime.ILog
    {
        public void ConfigMissing(string file) =>
            plugin.Core.Logger.LogInformation("{File} does not exist; running without a match", file);

        public void ConfigInvalid(string file, string error) =>
            plugin.Core.Logger.LogError("{File} is invalid ({Error}); running without a match", file, error);

        public void Readiness(string message) => plugin.Core.Logger.LogInformation("{Message}", message);

        public void LaunchCfg(string map, string line) => plugin.Core.Logger.LogDebug("cfg on {Map} without a match: {Line}", map, line);

        public void WsUrlMissing(string variable) =>
            plugin.Logger.LogWarning("{Variable} is not set; events will not be forwarded", variable);

        public void WsUrlInvalid(string variable) => plugin.Logger.LogError("{Variable} is not a valid absolute URL", variable);

        public void EventStream(string allocationId) =>
            plugin.Logger.LogInformation("event stream for allocation {AllocationId}", allocationId);

        public void NoSocket(string frame) => plugin.Logger.LogDebug("(no websocket) {Frame}", frame);

        // The controller keeps a record of who ran what; at info this would fill the server log.
        public void Exec(string command) => plugin.Logger.LogDebug("exec from the controller: {Command}", command);

        public void SkippingType(string type) =>
            plugin.Logger.LogInformation("skipping {Type} frames, which this plugin does not know", type);

        public void IgnoredFrame(string excerpt) =>
            plugin.Logger.LogWarning("ignoring a frame from the agent that is not one of ours: {Frame}", excerpt);
    }
}
