using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.Json.Nodes;

namespace Ankor.Headless;

/// <summary>
/// Stands between the plugin's socket and the agent's relay, on loopback, and passes every byte on
/// unchanged. It reads the WebSocket frames as they pass, to report the socket's opening, its
/// frames and how it ended. While held, it refuses the plugin's dials, as a relay the plugin
/// cannot reach would.
/// </summary>
internal sealed class RelayProxy(Uri relay, Action<JsonObject> report) : IDisposable
{
    private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
    private readonly CancellationTokenSource _stop = new();
    private volatile bool _unreachableReported;

    /// <summary>Refuses the plugin's dials until it is let go again.</summary>
    public volatile bool Hold;

    /// <summary>The relay URL with the proxy as its host, for the plugin to dial.</summary>
    public Uri Url => new UriBuilder(relay) { Host = "127.0.0.1", Port = ((IPEndPoint)_listener.LocalEndpoint).Port }.Uri;

    public void Start()
    {
        _listener.Start();
        _ = Task.Run(AcceptAsync);
    }

    private async Task AcceptAsync()
    {
        while (!_stop.IsCancellationRequested)
        {
            TcpClient plugin;
            try { plugin = await _listener.AcceptTcpClientAsync(_stop.Token); }
            catch (OperationCanceledException) { return; }
            _ = Task.Run(() => ServeAsync(plugin));
        }
    }

    private async Task ServeAsync(TcpClient plugin)
    {
        using var _ = plugin;
        if (Hold) return;

        using var agent = new TcpClient();
        try
        {
            await agent.ConnectAsync(relay.Host, relay.Port, _stop.Token);
        }
        catch (Exception error) when (error is SocketException or OperationCanceledException)
        {
            // Once per outage, because how many dials fail depends on how long it lasts.
            if (!_unreachableReported) report(new JsonObject { ["relay"] = "unreachable", ["error"] = error.Message });
            _unreachableReported = true;
            return;
        }

        _unreachableReported = false;
        var session = new Session(report);
        using var ended = CancellationTokenSource.CreateLinkedTokenSource(_stop.Token);
        var up = PumpAsync(plugin.GetStream(), agent.GetStream(), session.FromPlugin, ended.Token);
        var down = PumpAsync(agent.GetStream(), plugin.GetStream(), session.FromAgent, ended.Token);
        await Task.WhenAny(up, down);
        await ended.CancelAsync();
        session.Ended();
    }

    private static async Task PumpAsync(NetworkStream from, NetworkStream to, Action<byte[], int> observe, CancellationToken stop)
    {
        var buffer = new byte[64 * 1024];
        var observing = true;
        try
        {
            while (true)
            {
                var read = await from.ReadAsync(buffer, stop);
                if (read == 0) return;
                // The bytes pass on whatever the reading makes of them; a stream it cannot read is
                // still forwarded, just no longer reported.
                if (observing)
                {
                    try { observe(buffer, read); }
                    catch (Exception error) when (error is ArgumentException or InvalidOperationException) { observing = false; }
                }
                await to.WriteAsync(buffer.AsMemory(0, read), stop);
            }
        }
        catch (Exception error) when (error is IOException or OperationCanceledException or ObjectDisposedException) { }
    }

    public void Dispose()
    {
        _stop.Cancel();
        _listener.Stop();
    }

    /// <summary>One socket, read in both directions: the HTTP upgrade, then frames.</summary>
    private sealed class Session(Action<JsonObject> report)
    {
        private readonly FrameReader _fromPlugin = new();
        private readonly FrameReader _fromAgent = new();
        private bool _opened;
        private bool _closed;
        private readonly Lock _gate = new();

        public void FromPlugin(byte[] data, int count)
        {
            lock (_gate)
                foreach (var frame in _fromPlugin.Read(data, count))
                    if (frame.Text is { } text) report(new JsonObject { ["plugin_frame"] = Parse(text) });
        }

        public void FromAgent(byte[] data, int count)
        {
            lock (_gate)
            {
                foreach (var frame in _fromAgent.Read(data, count))
                {
                    if (frame.Text is { } text) report(new JsonObject { ["relay_frame"] = Parse(text) });
                    else if (frame.Close is { } close && !_closed)
                    {
                        _closed = true;
                        report(new JsonObject { ["relay"] = "closed", ["code"] = close.Code, ["reason"] = close.Reason });
                    }
                }
                if (!_opened && _fromAgent.Head is { } head)
                {
                    _opened = true;
                    report(head.StartsWith("HTTP/1.1 101", StringComparison.Ordinal)
                        ? new JsonObject { ["relay"] = "connected" }
                        : new JsonObject { ["relay"] = "refused", ["status"] = head.Split(' ').ElementAtOrDefault(1) });
                }
            }
        }

        public void Ended()
        {
            lock (_gate)
                if (_opened && !_closed && _fromAgent.Head?.StartsWith("HTTP/1.1 101", StringComparison.Ordinal) == true)
                {
                    _closed = true;
                    report(new JsonObject { ["relay"] = "closed" });
                }
        }

        private static JsonNode? Parse(string text)
        {
            try { return JsonNode.Parse(text); }
            catch (System.Text.Json.JsonException) { return text; }
        }
    }

    internal sealed record Frame(string? Text, (int Code, string Reason)? Close);

    /// <summary>Reads WebSocket frames out of a byte stream, after its HTTP head.</summary>
    internal sealed class FrameReader
    {
        private readonly List<byte> _buffer = [];
        private readonly List<byte> _message = [];
        private int _messageOpcode;

        /// <summary>The first line of the HTTP head, once it has passed.</summary>
        public string? Head { get; private set; }

        public IEnumerable<Frame> Read(byte[] data, int count)
        {
            _buffer.AddRange(data.AsSpan(0, count));
            var frames = new List<Frame>();
            // After a refused upgrade the rest is an HTTP body, not frames.
            if (Head is { } passed && !passed.StartsWith("HTTP/1.1 101", StringComparison.Ordinal) && !passed.StartsWith("GET ", StringComparison.Ordinal))
                return frames;
            if (Head is null)
            {
                var end = IndexOf(_buffer, "\r\n\r\n"u8.ToArray());
                if (end < 0) return frames;
                var head = Encoding.ASCII.GetString(_buffer.GetRange(0, end).ToArray());
                Head = head.Split("\r\n")[0];
                _buffer.RemoveRange(0, end + 4);
            }
            while (TryFrame(out var frame)) if (frame is not null) frames.Add(frame);
            return frames;
        }

        private bool TryFrame(out Frame? frame)
        {
            frame = null;
            if (_buffer.Count < 2) return false;
            var fin = (_buffer[0] & 0x80) != 0;
            var opcode = _buffer[0] & 0x0f;
            var masked = (_buffer[1] & 0x80) != 0;
            long length = _buffer[1] & 0x7f;
            var offset = 2;
            if (length == 126)
            {
                if (_buffer.Count < 4) return false;
                length = (_buffer[2] << 8) | _buffer[3];
                offset = 4;
            }
            else if (length == 127)
            {
                if (_buffer.Count < 10) return false;
                length = 0;
                for (var i = 2; i < 10; i++) length = (length << 8) | _buffer[i];
                offset = 10;
            }
            var maskAt = offset;
            if (masked) offset += 4;
            if (_buffer.Count < offset + length) return false;

            var payload = _buffer.GetRange(offset, (int)length).ToArray();
            if (masked)
                for (var i = 0; i < payload.Length; i++) payload[i] ^= _buffer[maskAt + i % 4];
            _buffer.RemoveRange(0, offset + (int)length);

            if (opcode == 0x8)
            {
                var code = payload.Length >= 2 ? (payload[0] << 8) | payload[1] : 1005;
                frame = new Frame(null, (code, payload.Length > 2 ? Encoding.UTF8.GetString(payload, 2, payload.Length - 2) : ""));
                return true;
            }
            if (opcode is 0x1 or 0x2) { _message.Clear(); _messageOpcode = opcode; }
            if (opcode is 0x0 or 0x1 or 0x2)
            {
                _message.AddRange(payload);
                if (fin && _messageOpcode == 0x1) frame = new Frame(Encoding.UTF8.GetString(_message.ToArray()), null);
            }
            return true;
        }

        private static int IndexOf(List<byte> haystack, byte[] needle)
        {
            for (var i = 0; i + needle.Length <= haystack.Count; i++)
            {
                var match = true;
                for (var j = 0; j < needle.Length && match; j++) match = haystack[i + j] == needle[j];
                if (match) return i;
            }
            return -1;
        }
    }
}
