using System.Collections.Concurrent;
using System.Diagnostics;
using System.Net.WebSockets;
using System.Text.Json;
using System.Threading.Channels;
using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;
using static Ankor.Tests.MockEventAgent;

namespace Ankor.Tests;

public sealed class WsClientTransportTests
{
    private sealed class Client : IAsyncDisposable
    {
        public readonly Channel<string> Inbound = Channel.CreateUnbounded<string>();
        public readonly ConcurrentQueue<(LogLevel Level, string Message)> Logs = new();
        public readonly TaskCompletionSource ConnectionFailed = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly WsClient Socket;

        /// <summary>Without <paramref name="send"/>, frames go out through the client's own writer.</summary>
        public Client(Uri url, Func<ClientWebSocket, string, CancellationToken, Task>? send = null, Action? onConnected = null)
        {
            void Log(LogLevel level, string message)
            {
                Logs.Enqueue((level, message));
                if (level == LogLevel.Warn && message.StartsWith("agent connection failed:"))
                    ConnectionFailed.TrySetResult();
            }
            const string allocation = "0199a0b4-5c00-7000-8000-00000000b001";
            Socket = send is null
                ? new WsClient(url, allocation, text => Inbound.Writer.TryWrite(text), Log, onConnected)
                : new WsClient(url, allocation, text => Inbound.Writer.TryWrite(text), Log, onConnected) { SendFrameAsync = send };
            Socket.Start();
        }

        public void Enqueue(int count)
        {
            for (var i = 0; i < count; i++)
                Assert.True(Socket.Enqueue(new PluginLog { message = $"message-{i}" }));
        }

        public async Task BarrierAsync(WebSocket peer)
        {
            const string marker = "{\"type\":\"get_server_status\",\"request_id\":\"barrier\"}";
            await SendAsync(peer, marker);
            Assert.Equal(marker, await Inbound.Reader.ReadAsync().AsTask().WaitAsync(OperationTimeout));
        }

        public ValueTask DisposeAsync() => Socket.DisposeAsync();
    }

    [Fact]
    public async Task PhaseAndScoreBypassTheDurableEventSequenceAndNeedNoAcknowledgement()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        await client.BarrierAsync(peer);
        client.Socket.ReportMatch(new PluginPhaseChange { phase = MatchPhase.SideChoice });
        Assert.Equal("""{"type":"phase_change","phase":"side_choice"}""", await ReceiveAsync(peer));
        client.Socket.ReportMatch(new PluginScore { team1Score = 13, team2Score = 9 });
        Assert.Equal("""{"type":"score","team1_score":13,"team2_score":9}""", await ReceiveAsync(peer));
        Assert.Equal(0, client.Socket.PendingCount);
    }

    [Fact]
    public async Task QuittingIsSentOutsideTheEventSequenceAndCompletesOnceWritten()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        await client.BarrierAsync(peer);

        await client.Socket.SendQuitting().WaitAsync(OperationTimeout);

        Assert.Equal("""{"type":"quitting"}""", await ReceiveAsync(peer));
        Assert.Equal(0, client.Socket.PendingCount);
    }

    [Fact]
    public async Task AFailingConnectedCallbackIsLoggedAndTheConnectionCarriesOn()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url, onConnected: () => throw new InvalidOperationException("no hello"));
        var peer = await agent.AcceptAsync();
        await client.BarrierAsync(peer);

        Assert.Contains(client.Logs, entry => entry.Level == LogLevel.Error && entry.Message == "hello callback failed: no hello");
        client.Enqueue(1);
        Assert.Equal(0, Sequence(await ReceiveAsync(peer)));
        await AckAsync(peer, 0);
    }

    [Fact]
    public async Task DisposalDuringAConnectAttemptEndsItWithoutAFailure()
    {
        // Accepts the TCP connection and never answers the upgrade, so the attempt stays open.
        using var listener = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, 0);
        listener.Start();
        var url = new Uri($"ws://127.0.0.1:{((System.Net.IPEndPoint)listener.LocalEndpoint).Port}/ws/plugin");
        var client = new Client(url);
        using var connection = await listener.AcceptTcpClientAsync().WaitAsync(OperationTimeout);

        await client.Socket.DisposeAsync().AsTask().WaitAsync(OperationTimeout);

        Assert.DoesNotContain(client.Logs, entry => entry.Message.StartsWith("agent connection failed:"));
    }

    [Fact]
    public async Task AnEnqueuedEventIsSequencedHistory()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        await client.BarrierAsync(peer);
        client.Socket.Enqueue(new MapEnd { winnerTeam = 1 });
        var durable = await ReceiveAsync(peer);
        using var envelope = JsonDocument.Parse(durable);
        Assert.Equal("map_end", envelope.RootElement.GetProperty("event").GetProperty("type").GetString());
        Assert.Equal(0, Sequence(durable));
        Assert.Equal(1, client.Socket.PendingCount);
        await AckAsync(peer, 0);
        await client.BarrierAsync(peer);
        Assert.Equal(0, client.Socket.PendingCount);
    }

    [Fact]
    public async Task ServerStatusBypassesTheDurableEventSequenceAndNeedsNoAcknowledgement()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        await client.BarrierAsync(peer);
        const string queriedStatus = "{\"type\":\"server_status\",\"request_id\":\"inspect-1\",\"map_name\":\"aim_map\",\"players\":[]}";
        client.Socket.ReportServerStatus(queriedStatus);
        Assert.Equal(queriedStatus, await ReceiveAsync(peer));
        Assert.Equal(0, client.Socket.PendingCount);
        client.Enqueue(1);
        Assert.Equal(0, Sequence(await ReceiveAsync(peer)));
        await AckAsync(peer, 0);
    }

    [Fact]
    public async Task ConcurrentStatusRepliesAndEventsShareTheConnectionWithoutDroppingReplies()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        await client.BarrierAsync(peer);
        client.Enqueue(32);
        Parallel.For(0, 32, i => client.Socket.ReportServerStatus(
            JsonSerializer.Serialize(new { type = "server_status", request_id = i.ToString() })));

        var replies = new HashSet<string>();
        var events = new List<long>();
        for (var i = 0; i < 64; i++)
        {
            using var frame = JsonDocument.Parse(await ReceiveAsync(peer));
            if (frame.RootElement.TryGetProperty("seq", out var seq)) events.Add(seq.GetInt64());
            else Assert.True(replies.Add(frame.RootElement.GetProperty("request_id").GetString()!));
        }
        Assert.Equal(Enumerable.Range(0, 32).Select(i => (long)i), events);
        Assert.Equal(32, replies.Count);
        await AckAsync(peer, 31);
    }

    private sealed class HeldWrite
    {
        public readonly TaskCompletionSource Entered = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public int Calls;
        public async Task SendAsync(ClientWebSocket socket, string json, CancellationToken ct)
        {
            if (Interlocked.Increment(ref Calls) == 1)
            {
                Entered.TrySetResult();
                await Release.Task.WaitAsync(ct);
            }
            await socket.SendAsync(System.Text.Encoding.UTF8.GetBytes(json), WebSocketMessageType.Text, true, ct);
        }
    }

    [Fact]
    public async Task StatusWaitsForHeldEventWriteWhileReceivesContinue()
    {
        await using var agent = await MockEventAgent.StartAsync();
        var held = new HeldWrite();
        await using var client = new Client(agent.Url, held.SendAsync);
        var peer = await agent.AcceptAsync();
        await client.BarrierAsync(peer);
        client.Enqueue(1);
        await held.Entered.Task.WaitAsync(OperationTimeout);
        try
        {
            client.Socket.ReportServerStatus("first-status");
            client.Socket.ReportServerStatus("second-status");
            Assert.Equal(1, Volatile.Read(ref held.Calls));
            // Receiving must not need the write lock, even with several blocked senders.
            await client.BarrierAsync(peer);
        }
        finally { held.Release.TrySetResult(); }
        Assert.Equal(0, Sequence(await ReceiveAsync(peer)));
        var replies = new HashSet<string> { await ReceiveAsync(peer), await ReceiveAsync(peer) };
        Assert.True(replies.SetEquals(["first-status", "second-status"]));
        await AckAsync(peer, 0);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task DisconnectCancelsHeldAndWaitingStatusWrites(bool dispose)
    {
        await using var agent = await MockEventAgent.StartAsync();
        var held = new HeldWrite();
        await using var client = new Client(agent.Url, held.SendAsync);
        var peer = await agent.AcceptAsync();
        await client.BarrierAsync(peer);
        var oldConnection = client.Socket.ConnectionClosed;
        Assert.False(oldConnection.IsCancellationRequested);
        client.Socket.ReportServerStatus("old-active");
        await held.Entered.Task.WaitAsync(OperationTimeout);
        client.Socket.ReportServerStatus("old-waiting");
        Assert.Equal(1, Volatile.Read(ref held.Calls));
        if (dispose)
        {
            await client.Socket.DisposeAsync().AsTask().WaitAsync(OperationTimeout);
            Assert.True(client.Socket.ConnectionClosed.IsCancellationRequested);
            Assert.True(oldConnection.IsCancellationRequested);
            client.Socket.Start();
            Assert.True(client.Socket.ConnectionClosed.IsCancellationRequested);
        }
        else
        {
            await peer.CloseOutputAsync(WebSocketCloseStatus.NormalClosure, "reconnect", CancellationToken.None);
            var next = await agent.AcceptAsync();
            await client.BarrierAsync(next);
            Assert.True(oldConnection.IsCancellationRequested);
            Assert.False(client.Socket.ConnectionClosed.IsCancellationRequested);
            client.Socket.ReportServerStatus("fresh");
            Assert.Equal("fresh", await ReceiveAsync(next));
            Assert.Equal(2, Volatile.Read(ref held.Calls));
        }
    }

    private static long Sequence(string frame)
    {
        using var json = JsonDocument.Parse(frame);
        return json.RootElement.GetProperty("seq").GetInt64();
    }

    [Fact]
    public async Task InitialConnectionFailureRetainsQueuedDataUntilTheAgentIsReady()
    {
        await using var agent = await MockEventAgent.StartAsync(acceptingConnections: false);
        await using var client = new Client(agent.Url);
        client.Enqueue(3);
        var queued = client.Socket.PendingAfter(-1).Select(entry => entry.Line).ToArray();
        await client.ConnectionFailed.Task.WaitAsync(OperationTimeout);
        Assert.True(client.Socket.ConnectionClosed.IsCancellationRequested);
        client.Socket.ReportServerStatus("{\"type\":\"server_status\",\"request_id\":\"stale\"}");
        Assert.Equal(3, client.Socket.PendingCount);

        agent.AcceptingConnections = true;
        var peer = await agent.AcceptAsync();
        foreach (var frame in queued) Assert.Equal(frame, await ReceiveAsync(peer));
        await AckAsync(peer, 2);
        await client.BarrierAsync(peer);
        Assert.Equal(0, client.Socket.PendingCount);
    }

    [Fact]
    public async Task DisposalCancelsConnectionRetriesWhileTheAgentRemainsUnavailable()
    {
        await using var agent = await MockEventAgent.StartAsync(acceptingConnections: false);
        await using var client = new Client(agent.Url);
        await client.ConnectionFailed.Task.WaitAsync(OperationTimeout);

        await client.Socket.DisposeAsync().AsTask().WaitAsync(TimeSpan.FromSeconds(1));

        Assert.True(client.Socket.ConnectionClosed.IsCancellationRequested);
        Assert.DoesNotContain(client.Logs, entry => entry.Level == LogLevel.Error);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task ReconnectResendsOnlyUnacknowledgedFramesWithIdenticalPayloads(bool abrupt)
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var first = await agent.AcceptAsync();
        client.Enqueue(4);
        var original = new List<string>();
        for (var i = 0; i < 4; i++) original.Add(await ReceiveAsync(first));
        Assert.Equal([0L, 1L, 2L, 3L], original.Select(Sequence));
        await AckAsync(first, 1);
        await client.BarrierAsync(first);
        Assert.Equal(2, client.Socket.PendingCount);

        if (abrupt) first.Abort();
        else await first.CloseOutputAsync(WebSocketCloseStatus.NormalClosure, "reconnect", CancellationToken.None);

        var second = await agent.AcceptAsync();
        Assert.Equal(original[2], await ReceiveAsync(second));
        Assert.Equal(original[3], await ReceiveAsync(second));
        await AckAsync(second, 3);
        await client.BarrierAsync(second);
        Assert.Equal(0, client.Socket.PendingCount);
    }

    [Fact]
    public async Task SendWindowWaitsForAcknowledgementsAndReleasesOnlyTheAvailableSlots()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        client.Enqueue(130);
        for (var i = 0; i < 128; i++) Assert.Equal(i, Sequence(await ReceiveAsync(peer)));

        var next = ReceiveAsync(peer);
        Assert.NotSame(next, await Task.WhenAny(next, Task.Delay(250)));
        await AckAsync(peer, 0);
        Assert.Equal(128, Sequence(await next));
        next = ReceiveAsync(peer);
        Assert.NotSame(next, await Task.WhenAny(next, Task.Delay(250)));
        await AckAsync(peer, 1);
        Assert.Equal(129, Sequence(await next));
        await AckAsync(peer, 129);
        await client.BarrierAsync(peer);
        Assert.Equal(0, client.Socket.PendingCount);
    }

    [Fact]
    public async Task FragmentedMessagesAreAssembledAndMalformedJsonCannotAcknowledgeData()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        client.Enqueue(1);
        await ReceiveAsync(peer);
        await SendAsync(peer, "{\"type\":\"exec\",", endOfMessage: false);
        await SendAsync(peer, "\"command\":\"status\",\"request_id\":\"r1\"}");
        var text = await client.Inbound.Reader.ReadAsync().AsTask().WaitAsync(OperationTimeout);
        Assert.Equal("status", Assert.IsType<AgentMessage.Exec>(WireJson.ParseInbound(text)).command);

        await SendAsync(peer, "{invalid");
        text = await client.Inbound.Reader.ReadAsync().AsTask().WaitAsync(OperationTimeout);
        Assert.Null(WireJson.ParseInbound(text));
        Assert.Equal(1, client.Socket.PendingCount);
        await SendAsync(peer, "{\"type\":\"ack\",", endOfMessage: false);
        await SendAsync(peer, "\"to_seq\":0}");
        await client.BarrierAsync(peer);
        Assert.Equal(0, client.Socket.PendingCount);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task InvalidFramesDisconnectWithoutDiscardingPendingData(bool oversized)
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        client.Enqueue(1);
        var original = await ReceiveAsync(peer);
        if (oversized)
        {
            var chunk = new string('x', 1 << 16);
            for (var i = 0; i < WireLimits.MaxFrameBytes / chunk.Length; i++) await SendAsync(peer, chunk, endOfMessage: false);
            await SendAsync(peer, "x");
        }
        else await SendAsync(peer, "binary", WebSocketMessageType.Binary);

        var reconnect = await agent.AcceptAsync();
        Assert.Equal(original, await ReceiveAsync(reconnect));
        Assert.Contains(client.Logs, entry => entry.Level == LogLevel.Warn &&
            entry.Message.Contains(oversized ? "Oversized agent frame" : "Non-text agent frame"));
        await AckAsync(reconnect, 0);
        await client.BarrierAsync(reconnect);
        Assert.Equal(0, client.Socket.PendingCount);
    }

    [Fact]
    public async Task DisposalAllowsPendingDataToBeAcknowledgedBeforeDisconnecting()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        client.Enqueue(1);
        await ReceiveAsync(peer);

        var disposal = client.Socket.DisposeAsync().AsTask();
        Assert.False(disposal.IsCompleted);
        var secondDisposal = client.Socket.DisposeAsync().AsTask();
        Assert.Same(disposal, secondDisposal);
        Assert.False(client.Socket.Enqueue(new PluginLog { message = "too late" }));
        await AckAsync(peer, 0);
        await disposal.WaitAsync(OperationTimeout);
        Assert.Equal(0, client.Socket.PendingCount);
        Assert.True(client.Socket.ConnectionClosed.IsCancellationRequested);
        Assert.DoesNotContain(client.Logs, entry => entry.Level == LogLevel.Error);
    }

    [Fact]
    public async Task UnresponsiveAgentCannotPreventDisposalAndTheLostCountIsReported()
    {
        await using var agent = await MockEventAgent.StartAsync();
        await using var client = new Client(agent.Url);
        var peer = await agent.AcceptAsync();
        client.Enqueue(1);
        await ReceiveAsync(peer);

        var started = Stopwatch.GetTimestamp();
        await client.Socket.DisposeAsync().AsTask().WaitAsync(TimeSpan.FromSeconds(5));
        Assert.True(Stopwatch.GetElapsedTime(started) >= TimeSpan.FromSeconds(2));
        Assert.True(client.Socket.ConnectionClosed.IsCancellationRequested);
        Assert.Contains(client.Logs, entry => entry.Level == LogLevel.Error &&
            entry.Message.Contains("unloading with 1 events not acknowledged"));
    }
}
