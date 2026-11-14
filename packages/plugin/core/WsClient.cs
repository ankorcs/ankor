using System.Diagnostics;
using System.Net.WebSockets;
using System.Runtime.CompilerServices;
using System.Text;
using Ankor.Core.Wire;

[assembly: InternalsVisibleTo("Ankor.Tests")]
namespace Ankor.Core;

/// <summary>Stream to the local agent; retain a bounded memory buffer until its durable ack.</summary>
public sealed class WsClient : IAsyncDisposable
{
    private const int MaxFrameBytes = WireLimits.MaxFrameBytes;
    /// <summary>Several matches with no agent: a frame is 300–600 bytes, so this is ~130 MB of heap at the very worst.</summary>
    private const int MaxBufferedBytes = 64 << 20;
    private const int SendWindow = 128;
    internal const int MaxBufferedEvents = 65536;
    private static readonly TimeSpan MinBackoff = TimeSpan.FromMilliseconds(100);
    private static readonly TimeSpan MaxBackoff = TimeSpan.FromSeconds(5);
    private static readonly TimeSpan HealthySessionDuration = TimeSpan.FromSeconds(10);
    private readonly Uri _url;
    private readonly Action<string> _onInbound;
    private readonly Action<LogLevel, string> _log;
    private readonly Action? _onConnected;
    /// <summary>Called with the toll once a full buffer takes events again, so the record can say so.</summary>
    private readonly Action<long>? _onRecovered;
    private readonly SemaphoreSlim _changed = new(0, 1);
    private readonly CancellationTokenSource _stop = new();
    private readonly Lock _gate = new();
    private readonly LinkedList<BufferedEvent> _pending = new();
    // Serialize individual socket writes; this does not guarantee FIFO or status priority.
    private readonly SemaphoreSlim _sendGate = new(1, 1);
    private ClientWebSocket? _socket;
    private CancellationToken _sessionToken;
    private Task _statusSend = Task.CompletedTask;
    private int _pendingControlWrites;
    private const int MaxPendingControlWrites = 128;
    private long _seq;
    private long _acked = -1;
    private long _sentThrough = -1;
    private int _bufferedBytes;
    private long _dropped;
    /// <summary>Inside an overflow: the first drop is an error, the recovery a warning, the rest a count.</summary>
    private bool _overflowing;
    private bool _disposed;
    private Task? _pump;
    private Task? _disposal;
    // The transport seam lets tests hold an actual write open and observe attempted overlaps.
    internal Func<ClientWebSocket, string, CancellationToken, Task> SendFrameAsync { get; init; } =
        static (socket, json, ct) => socket.SendAsync(Encoding.UTF8.GetBytes(json), WebSocketMessageType.Text, true, ct);

    internal sealed record BufferedEvent(long Seq, string Line, int Bytes, long EnqueuedAt);
    public WsClient(Uri url, string allocationId, Action<string> onInbound, Action<LogLevel, string> log,
        Action? onConnected = null, Action<long>? onRecovered = null)
    {
        _url = url; AllocationId = allocationId; _onInbound = onInbound; _log = log; _onConnected = onConnected; _onRecovered = onRecovered;
    }
    public string AllocationId { get; }
    /// <summary>Capture when scheduling inbound work; discard it if this connection ends before execution.</summary>
    public CancellationToken ConnectionClosed
    {
        get { lock (_gate) return _socket is null ? new CancellationToken(true) : _sessionToken; }
    }
    public long PendingCount { get { lock (_gate) return _pending.Count; } }
    /// <summary>How long the oldest unacknowledged event has been waiting; null with nothing pending.</summary>
    public double? OldestPendingAgeSeconds
    {
        get
        {
            lock (_gate)
            {
                return _pending.First is { } first
                    ? Stopwatch.GetElapsedTime(first.Value.EnqueuedAt).TotalSeconds
                    : null;
            }
        }
    }
    public void Start()
    {
        lock (_gate)
        {
            if (_disposed) return;
            _pump ??= Task.Run(() => PumpAsync(_stop.Token));
        }
    }

    /// <summary>
    /// Queue one event; false when it was dropped instead. An event that does not fit — the agent
    /// has not acknowledged anything for most of a match, or a single frame is absurdly large — is
    /// lost rather than taking the server down with it: the match matters more than its record.
    /// The first drop of an overflow is an error, every 500th a reminder, the recovery a warning.
    /// </summary>
    public bool Enqueue(PluginMessage message)
    {
        string? reason = null;
        long dropped;
        bool announce, recovered = false;
        lock (_gate)
        {
            if (_disposed) return false;
            var line = new PluginEventFrame { seq = _seq, @event = message }.ToJson();
            var bytes = Encoding.UTF8.GetByteCount(line);
            if (bytes > MaxFrameBytes)
            {
                reason = $"a {bytes}-byte frame is larger than the agent accepts";
                announce = true;
            }
            else if (_pending.Count >= MaxBufferedEvents || _bufferedBytes + bytes > MaxBufferedBytes)
            {
                reason = $"the buffer is full ({_pending.Count} events, {_bufferedBytes} bytes unacknowledged)";
                announce = !_overflowing;
                _overflowing = true;
            }
            else
            {
                _pending.AddLast(new BufferedEvent(_seq++, line, bytes, Stopwatch.GetTimestamp()));
                _bufferedBytes += bytes;
                recovered = _overflowing;
                _overflowing = false;
                announce = false;
            }
            dropped = reason is null ? _dropped : ++_dropped;
        }
        if (reason is not null)
        {
            if (announce || dropped % 500 == 0)
                _log(LogLevel.Error, $"dropping {message.type} ({dropped} dropped so far): {reason}");
            return false;
        }
        Signal();
        if (recovered)
        {
            _log(LogLevel.Warn, $"the event buffer has room again; {dropped} events were dropped and are lost");
            _onRecovered?.Invoke(dropped);
        }
        return true;
    }
    public void ReportServerStatus(string json) => SendControl(json);

    public void ReportCommandResult(string json) => SendControl(json);

    /// <summary>The match's phase, score or backup; like a status, lost when there is no connection.</summary>
    public void ReportMatch(MatchReport report) => SendControl(report.ToJson());

    /// <summary>`Quitting` in plugin-socket.ts; completes once written, or at once when it cannot be sent.</summary>
    public Task SendQuitting() => SendControl("""{"type":"quitting"}""");

    /// <summary>Bound transient writes and never retain them across connections or wait for event ACKs.</summary>
    private Task SendControl(string json)
    {
        lock (_gate)
        {
            if (_disposed || _socket is null || _pendingControlWrites >= MaxPendingControlWrites) return Task.CompletedTask;
            _pendingControlWrites++;
            var send = SendStatusAsync(_socket, json, _sessionToken);
            _statusSend = _statusSend.IsCompleted ? send : Task.WhenAll(_statusSend, send);
            return send;
        }
    }
    private async Task SendStatusAsync(ClientWebSocket socket, string json, CancellationToken ct)
    {
        try
        {
            await _sendGate.WaitAsync(ct).ConfigureAwait(false);
            try
            {
                // The gate can be handed over by a writer the cancellation just unblocked, before the same
                // cancellation reaches this waiter; a session that ended while queueing gets no frame.
                ct.ThrowIfCancellationRequested();
                await SendFrameAsync(socket, json, ct).ConfigureAwait(false);
            }
            finally { _sendGate.Release(); }
        }
        catch (Exception ex) when (ex is WebSocketException or OperationCanceledException or InvalidOperationException)
        {
            socket.Abort();
        }
        finally { lock (_gate) _pendingControlWrites--; }
    }

    internal IReadOnlyList<BufferedEvent> PendingAfter(long after, int limit = SendWindow)
    {
        lock (_gate) return _pending.Where(entry => entry.Seq > after).Take(limit).ToList();
    }
    internal void MarkSent(long seq) { lock (_gate) _sentThrough = Math.Max(_sentThrough, seq); }
    internal void Ack(long toSeq)
    {
        lock (_gate)
        {
            if (toSeq <= _acked) return;
            if (toSeq > _sentThrough) throw new InvalidDataException("Agent ack exceeds the last transmitted event");
            while (_pending.First is { } node && node.Value.Seq <= toSeq)
            {
                _bufferedBytes -= node.Value.Bytes;
                _pending.RemoveFirst();
            }
            _acked = toSeq;
        }
        Signal();
    }
    private void Signal()
    {
        try { _changed.Release(); } catch (SemaphoreFullException) { }
    }
    private async Task PumpAsync(CancellationToken ct)
    {
        var backoff = MinBackoff;
        while (!ct.IsCancellationRequested)
        {
            using var socket = new ClientWebSocket();
            socket.Options.KeepAliveInterval = TimeSpan.FromSeconds(15);
            socket.Options.KeepAliveTimeout = TimeSpan.FromSeconds(20);
            socket.Options.SetRequestHeader(Protocol.Header, Protocol.Version);
            try { await socket.ConnectAsync(_url, ct).ConfigureAwait(false); }
            catch (OperationCanceledException) when (ct.IsCancellationRequested) { return; }
            catch (Exception ex)
            {
                _log(LogLevel.Warn, $"agent connection failed: {ex.Message}; retaining {PendingCount} events");
                await DelayAsync(backoff, ct).ConfigureAwait(false); backoff = NextBackoff(backoff); continue;
            }
            using var session = CancellationTokenSource.CreateLinkedTokenSource(ct);
            lock (_gate) { _socket = socket; _sessionToken = session.Token; }
            var started = Stopwatch.GetTimestamp();
            var health = new SessionHealth();
            _log(LogLevel.Info, $"connected to local event agent {Redact(_url)}");
            try { _onConnected?.Invoke(); }
            catch (Exception ex) { _log(LogLevel.Error, $"hello callback failed: {ex.Message}"); }
            var reader = ReceiveLoopAsync(socket, health, session.Token);
            var writer = SendLoopAsync(socket, session.Token);
            await Task.WhenAny(reader, writer).ConfigureAwait(false);
            Task statusSend;
            lock (_gate) { _socket = null; statusSend = _statusSend; }
            await session.CancelAsync().ConfigureAwait(false);
            try { await Task.WhenAll(reader, writer, statusSend).ConfigureAwait(false); }
            catch (Exception ex) when (ex is not OperationCanceledException) { _log(LogLevel.Warn, $"agent disconnected: {ex.Message}"); }
            catch (OperationCanceledException) { }
            if (ct.IsCancellationRequested) return;
            backoff = BackoffAfterSession(backoff, health.SawAck, Stopwatch.GetElapsedTime(started));
            await DelayAsync(backoff, ct).ConfigureAwait(false); backoff = NextBackoff(backoff);
        }
    }
    private async Task SendLoopAsync(ClientWebSocket socket, CancellationToken ct)
    {
        long sent;
        lock (_gate) sent = _acked;
        while (!ct.IsCancellationRequested)
        {
            long acknowledged;
            lock (_gate) acknowledged = _acked;
            var room = SendWindow - (int)Math.Max(0, sent - acknowledged);
            var batch = PendingAfter(sent, room);
            if (batch.Count == 0) { await _changed.WaitAsync(TimeSpan.FromMilliseconds(100), ct).ConfigureAwait(false); continue; }
            foreach (var entry in batch)
            {
                await _sendGate.WaitAsync(ct).ConfigureAwait(false);
                try
                {
                    ct.ThrowIfCancellationRequested();
                    MarkSent(entry.Seq);
                    await SendFrameAsync(socket, entry.Line, ct).ConfigureAwait(false);
                }
                finally { _sendGate.Release(); }
                sent = entry.Seq;
            }
        }
    }
    private async Task ReceiveLoopAsync(ClientWebSocket socket, SessionHealth health, CancellationToken ct)
    {
        var buffer = new byte[16 * 1024];
        using var accumulated = new MemoryStream();
        while (!ct.IsCancellationRequested)
        {
            var result = await socket.ReceiveAsync(buffer, ct).ConfigureAwait(false);
            if (result.MessageType == WebSocketMessageType.Close) return;
            if (result.MessageType != WebSocketMessageType.Text) throw new InvalidDataException("Non-text agent frame");
            accumulated.Write(buffer, 0, result.Count);
            if (accumulated.Length > MaxFrameBytes) throw new InvalidDataException("Oversized agent frame");
            if (!result.EndOfMessage) continue;
            var text = Encoding.UTF8.GetString(accumulated.GetBuffer(), 0, (int)accumulated.Length);
            accumulated.SetLength(0);
            if (WireJson.ParseInbound(text) is AgentMessage.Ack ack) { Ack(ack.toSeq); health.SawAck = true; }
            else _onInbound(text);
        }
    }
    internal static TimeSpan BackoffAfterSession(TimeSpan current, bool sawAck, TimeSpan lasted) =>
        sawAck || lasted >= HealthySessionDuration ? MinBackoff : current;
    private static TimeSpan NextBackoff(TimeSpan current) => TimeSpan.FromMilliseconds(Math.Min(MaxBackoff.TotalMilliseconds, current.TotalMilliseconds * 2));
    private static async Task DelayAsync(TimeSpan delay, CancellationToken ct)
    {
        try { await Task.Delay(delay + TimeSpan.FromMilliseconds(Random.Shared.Next(0, 100)), ct).ConfigureAwait(false); }
        catch (OperationCanceledException) { }
    }
    private sealed class SessionHealth { public volatile bool SawAck; }
    public static string Redact(Uri url) => $"{url.Scheme}://{url.Authority}{url.AbsolutePath}";
    public ValueTask DisposeAsync()
    {
        lock (_gate)
        {
            if (_disposal is not null) return new ValueTask(_disposal);
            _disposed = true;
            // Do not run cancellation callbacks while holding the state lock.
            _disposal = Task.Run(DisposeCoreAsync);
            return new ValueTask(_disposal);
        }
    }
    private async Task DisposeCoreAsync()
    {
        var deadline = Stopwatch.GetTimestamp();
        while (_pump is not null && PendingCount > 0 && Stopwatch.GetElapsedTime(deadline) < TimeSpan.FromSeconds(2))
            await Task.Delay(10).ConfigureAwait(false);
        if (PendingCount > 0) _log(LogLevel.Error, $"unloading with {PendingCount} events not acknowledged by the local agent");
        await _stop.CancelAsync().ConfigureAwait(false);
        if (_pump is not null) await _pump.ConfigureAwait(false);
        _stop.Dispose();
    }
}
