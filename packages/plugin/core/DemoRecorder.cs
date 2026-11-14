using System.Globalization;
using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// The recording lags live play by tv_delay, so the start waits that long to open on live play, the
/// stop waits past the last round and the file needs a moment to close.
/// </summary>
public sealed class DemoRecorder : IDisposable
{
    /// <summary>Seconds past tv_delay before the broadcast and the recording have shown the map's end.</summary>
    internal const int StopMargin = 15;
    private const int FlushMargin = 5;

    private readonly IGameHost _host;
    private readonly Action<LogLevel, string> _log;
    private readonly OwnedTimer _flushTimer;
    private readonly OwnedTimer _stopTimer;
    // A map change drops a pending start; the next go-live schedules it again.
    private readonly OwnedTimer _startTimer;
    private readonly Action<string>? _onFlushed;
    private readonly bool _enabled;
    private readonly string _identity;
    private bool _started;
    private bool _stopping;
    private bool _flushed;
    private bool _disposed;

    public DemoRecorder(IGameHost host, MatchConfig config, Action<LogLevel, string> log, Action<string>? onFlushed = null)
    {
        _host = host;
        _enabled = config.recordDemo;
        _onFlushed = onFlushed;
        _log = log;
        _stopTimer = new OwnedTimer(host, stopOnMapChange: false);
        _flushTimer = new OwnedTimer(host, stopOnMapChange: false);
        _startTimer = new OwnedTimer(host);
        _identity = Sanitize(config.matchId ?? "", underscores: false);
    }

    // A marker file, unlike an event, survives a lost connection and an agent restart.
    public static void MarkReady(string directory, string name, Action<LogLevel, string> log)
    {
        try
        {
            var demo = Path.Combine(directory, name + ".dem");
            if (!File.Exists(demo)) return;
            File.WriteAllText(demo + Demos.ReadySuffix + ".tmp", "");
            File.Move(demo + Demos.ReadySuffix + ".tmp", demo + Demos.ReadySuffix, overwrite: true);
        }
        catch (Exception error) { log(LogLevel.Error, $"Could not mark demo ready: {error.Message}"); }
    }

    /// <summary>Set when recording starts, from the map the server has loaded then.</summary>
    public string Name { get; private set; } = "";

    public bool Flushed => !_started || _flushed;

    /// <summary>Records once per match, tv_delay after going live, unless the match ends first.</summary>
    public void Start()
    {
        if (_disposed || _started || _stopping || !_enabled || _startTimer.IsPending) return;
        var delay = Math.Max(0, _host.GetConVarInt("tv_delay") ?? 0);
        if (delay == 0) Record();
        else _startTimer.Schedule(delay, Record);
    }

    private void Record()
    {
        if (_disposed || _started || _stopping) return;
        _started = true;
        var stamp = DateTimeOffset.FromUnixTimeMilliseconds(_host.NowMs).UtcDateTime
            .ToString(Demos.TimestampFormat, CultureInfo.InvariantCulture);
        Name = $"{stamp}_{_identity}_{Sanitize(_host.MapName, underscores: true)}";
        _host.ExecuteCommand($"tv_record {Demos.RecordDir}/{Name}");
        _log(LogLevel.Info, $"demo recording started: {Name}");
    }

    public void ScheduleStop()
    {
        if (_disposed || _stopping) return;
        _stopping = true;
        // A match that ends inside the start delay has recorded nothing.
        _startTimer.Cancel();
        if (!_started) return;
        var delay = Math.Max(0, _host.GetConVarInt("tv_delay")!.Value);
        _stopTimer.Schedule(delay + StopMargin, () =>
        {
            _host.ExecuteCommand("tv_stoprecord");
            _log(LogLevel.Info, $"demo recording stopped: {Name}");
        });
        // Its own timer, from the same moment, rather than one the stop schedules when it fires.
        _flushTimer.Schedule(delay + StopMargin + FlushMargin, () =>
        {
            _flushed = true;
            _onFlushed?.Invoke(Name);
        });
    }

    public void Dispose()
    {
        _disposed = true;
        _startTimer.Dispose();
        _stopTimer.Dispose();
        _flushTimer.Dispose();
    }

    /// <summary>The name becomes a console argument and a file name the agent splits on
    /// underscores, so only the trailing map part may keep them.</summary>
    private static string Sanitize(string value, bool underscores) =>
        new(value.Where(c => char.IsAsciiLetterOrDigit(c) || c == '-' || (underscores && c == '_')).ToArray());
}
