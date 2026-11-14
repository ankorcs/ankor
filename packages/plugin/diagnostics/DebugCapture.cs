using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading.Channels;

namespace Ankor.Diagnostics;

/// <summary>
/// Copy and serialize on the game thread; only immutable JSON crosses to the file writer.
/// Stop on overflow instead of silently producing a recording with missing callbacks.
/// </summary>
public sealed class DebugCapture : IDisposable
{
    private const int MaxQueuedBytes = 16 << 20;
    private static readonly TimeSpan DrainTimeout = TimeSpan.FromSeconds(2);
    // A map-load burst can contain thousands of tiny records before the writer gets CPU.
    // Bound serialized bytes in Write, not record count; never block the game thread on disk.
    private readonly Channel<byte[]> _queue = Channel.CreateUnbounded<byte[]>(new UnboundedChannelOptions
    {
        SingleReader = true,
        AllowSynchronousContinuations = false,
    });
    private readonly Action<string> _report;
    private readonly Task _writer;
    private readonly CancellationTokenSource _stopWriter = new();
    private int _queuedBytes;
    private string? _failure;
    private bool _disposed;
    private long _sequence;
    private long? _callback;
    private Action? _afterScopes;
    private static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        TypeInfoResolver = GameplaySerialization.Resolver,
        NumberHandling = JsonNumberHandling.AllowNamedFloatingPointLiterals,
    };

    public DebugCapture(string directory, string launchId, object metadata, Action<string> report)
        : this(OpenFile(directory, launchId), launchId, metadata, report) { }

    /// <summary>Own the stream; accepting one lets tests hold disk writes during a startup burst.</summary>
    internal DebugCapture(Stream stream, string launchId, object metadata, Action<string> report)
    {
        _report = report;
        _writer = Task.Run(() => WriteAsync(stream));
        Write(CaptureNames.Phase.Start, null, null, new { launchId, startedAt = DateTimeOffset.UtcNow, metadata });
    }

    private static FileStream OpenFile(string directory, string launchId)
    {
        if (string.IsNullOrEmpty(launchId) || launchId.Length > 100 ||
            launchId.Any(c => !char.IsAsciiLetterOrDigit(c) && c is not '-' and not '_'))
            throw new ArgumentException("Invalid capture launch id", nameof(launchId));
        Directory.CreateDirectory(directory);
        return new FileStream(Path.Combine(directory, launchId + ".jsonl"),
            FileMode.CreateNew, FileAccess.Write, FileShare.Read, 64 * 1024, FileOptions.Asynchronous);
    }

    public bool Enabled => !_disposed && _failure is null;

    public IDisposable? Begin(string name)
    {
        if (!Enabled) return null;
        var parent = _callback;
        var id = _sequence;
        _callback = id;
        Write(CaptureNames.Phase.Enter, id, parent, new { name });
        return new Scope(this, id, parent, name);
    }

    public void Observe(string name, object? value)
    {
        if (Enabled) Write(CaptureNames.Phase.Observation, _callback, null, new { name, value });
    }

    private sealed class Scope(DebugCapture owner, long id, long? parent, string name) : IDisposable
    {
        private bool _disposed;
        public void Dispose()
        {
            if (_disposed) return;
            _disposed = true;
            if (owner.Enabled) owner.Write(CaptureNames.Phase.Exit, id, parent, new { name });
            owner._callback = parent;
            if (parent is null)
            {
                var afterScopes = owner._afterScopes;
                owner._afterScopes = null;
                afterScopes?.Invoke();
            }
        }
    }

    /// <summary>
    /// An engine quit does not unload the plugin, because SwiftlyS2 (1.5.0) unloads plugins only on
    /// an unload or reload command, a hot reload or a failed load. Close the current callback scopes
    /// and allow a bounded file drain before issuing quit. A stalled diagnostic writer must
    /// not keep the game process alive; cancellation may leave the capture incomplete.
    /// Called on the game thread, like Begin and scope disposal.
    /// </summary>
    public void CompleteBefore(Action action)
    {
        void Complete()
        {
            Dispose();
            action();
        }
        if (_callback is null) Complete();
        else _afterScopes += Complete;
    }

    private void Write(string phase, long? callbackId, long? parentCallbackId, object data)
    {
        try
        {
            var bytes = JsonSerializer.SerializeToUtf8Bytes(new
            {
                sequence = _sequence++, phase, callbackId, parentCallbackId, data,
            }, Json);
            if (Interlocked.Add(ref _queuedBytes, bytes.Length) > MaxQueuedBytes || !_queue.Writer.TryWrite(bytes))
            {
                Interlocked.Add(ref _queuedBytes, -bytes.Length);
                Stop("Capture queue exceeded its limit; the stream is incomplete");
            }
        }
        catch (Exception ex) { Stop($"Capture serialization failed: {ex.Message}"); }
    }

    public void Stop(string reason)
    {
        if (Interlocked.CompareExchange(ref _failure, reason, null) is not null) return;
        _queue.Writer.TryComplete();
        _report(reason);
    }

    private async Task WriteAsync(Stream stream)
    {
        var ct = _stopWriter.Token;
        try
        {
            await using (stream)
            {
                await foreach (var bytes in _queue.Reader.ReadAllAsync(ct))
                {
                    await WriteLineAsync(stream, bytes, ct);
                    Interlocked.Add(ref _queuedBytes, -bytes.Length);
                }
                var end = JsonSerializer.SerializeToUtf8Bytes(new
                {
                    phase = CaptureNames.Phase.End, complete = _failure is null, error = _failure,
                }, Json);
                await WriteLineAsync(stream, end, ct);
            }
        }
        catch (Exception ex)
        {
            Stop($"Capture file write failed: {ex.Message}");
            while (_queue.Reader.TryRead(out _)) { }
        }
    }

    private static async Task WriteLineAsync(Stream stream, byte[] bytes, CancellationToken ct)
    {
        ct.ThrowIfCancellationRequested();
        await stream.WriteAsync(bytes, ct);
        // Some streams cannot interrupt an in-flight write. Stop when it eventually returns.
        ct.ThrowIfCancellationRequested();
        await stream.WriteAsync("\n"u8.ToArray(), ct);
        await stream.FlushAsync(ct);
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        _queue.Writer.TryComplete();
        try
        {
            _writer.WaitAsync(DrainTimeout).GetAwaiter().GetResult();
            _stopWriter.Dispose();
        }
        catch (TimeoutException)
        {
            Stop("Capture drain timed out; abandoning the writer and the capture may be incomplete");
            _ = CancelWriterAsync();
        }
    }

    private async Task CancelWriterAsync()
    {
        // Cancellation callbacks and stream disposal must not extend the game-thread deadline.
        try
        {
            await _stopWriter.CancelAsync().ConfigureAwait(false);
            await _writer.ConfigureAwait(false);
        }
        catch (Exception ex) { Stop($"Capture writer cleanup failed: {ex.Message}"); }
        finally { _stopWriter.Dispose(); }
    }
}
