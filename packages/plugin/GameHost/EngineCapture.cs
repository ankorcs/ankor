using Ankor.Diagnostics;
using Microsoft.Extensions.Logging;
using Ankor.Core.Wire;
using SwiftlyS2.Shared;
using SwiftlyS2.Shared.GameEvents;

namespace Ankor.GameHost;

/// <summary>Names each engine callback in the debug capture, around the replayable observations.</summary>
public sealed class EngineCapture : IDisposable
{
    private readonly DebugCapture? _stream;

    public EngineCapture(ISwiftlyCore core, object metadata, bool enabled = true)
    {
        if (!enabled) return;
        var directory = Environment.GetEnvironmentVariable(AnkorEnv.DebugCaptureDir);
        if (string.IsNullOrWhiteSpace(directory)) return;
        try
        {
            var captureId = Guid.NewGuid().ToString("N");
            _stream = new DebugCapture(directory, captureId, metadata,
                message => core.Logger.LogError("{Message}", message));
            core.Logger.LogInformation("Debug event capture enabled: {CaptureId}", captureId);
        }
        catch (Exception ex) { core.Logger.LogError(ex, "Could not start debug event capture"); }
    }

    public IDisposable? Begin(string name) => _stream?.Begin(name);

    public void Stop(string reason) => _stream?.Stop(reason);

    public void Observe(string name, object? value) => _stream?.Observe(name, value);

    public SwiftlyS2.Shared.Misc.HookResult GameEvent<T>(string phase, T value,
        IGameEventService.GameEventHandler<T> handler) where T : IGameEvent<T>
    {
        using var scope = Begin($"{typeof(T).Name}.{phase}");
        return handler(value);
    }

    public void CompleteBefore(Action action)
    {
        if (_stream is null) action();
        else _stream.CompleteBefore(action);
    }

    public void Dispose() => _stream?.Dispose();
}
