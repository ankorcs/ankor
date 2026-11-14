using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;
namespace Ankor.Tests;

public sealed class WsClientTests
{
    private static WsClient Build(string allocationId = "0199a0b4-5c00-7000-8000-00000000b001") =>
        new(new Uri("ws://127.0.0.1:4500/ws/plugin?slot_id=a&key=secret"), allocationId, _ => { }, (_, _) => { });
    private static PluginLog Event(string message) => new() { timestamp = 1700000000000, level = LogLevel.Info, message = message };

    [Fact]
    public async Task BacklogAgeTracksTheOldestUnacknowledgedEventAndClearsAfterAck()
    {
        await using var ws = Build();
        Assert.Null(ws.OldestPendingAgeSeconds);
        ws.Enqueue(Event("first"));
        ws.Enqueue(Event("second"));
        var pending = ws.PendingAfter(-1);
        void AssertAge(long enqueuedAt)
        {
            // Bracket the read with the monotonic clock; no sleeps or timing tolerances.
            var before = System.Diagnostics.Stopwatch.GetElapsedTime(enqueuedAt).TotalSeconds;
            var age = ws.OldestPendingAgeSeconds;
            var after = System.Diagnostics.Stopwatch.GetElapsedTime(enqueuedAt).TotalSeconds;
            Assert.NotNull(age);
            Assert.InRange(age.Value, before, after);
        }
        AssertAge(pending[0].EnqueuedAt);
        ws.MarkSent(1);
        ws.Ack(0);
        AssertAge(pending[1].EnqueuedAt);
        ws.Ack(1);
        Assert.Null(ws.OldestPendingAgeSeconds);
    }

    [Fact]
    public void EventsHaveMonotonicSequencesAndNameNoServer()
    {
        var ws = Build();
        ws.Enqueue(Event("a")); ws.Enqueue(Event("b")); ws.Enqueue(Event("c"));
        Assert.Equal([0, 1, 2], ws.PendingAfter(-1).Select(e => e.Seq));
        foreach (var entry in ws.PendingAfter(-1))
        {
            using var json = JsonDocument.Parse(entry.Line);
            Assert.Equal("event", json.RootElement.GetProperty("type").GetString());
            Assert.False(json.RootElement.TryGetProperty("allocation_id", out _));
            Assert.Equal(3, json.RootElement.EnumerateObject().Count());
        }
    }
    [Fact]
    public void OnlyAnAckForTransmittedEventsCanTrimTheBuffer()
    {
        var ws = Build();
        for (var i = 0; i < 5; i++) ws.Enqueue(Event($"frame-{i}"));
        Assert.Throws<InvalidDataException>(() => ws.Ack(0));
        ws.MarkSent(3); ws.Ack(3);
        Assert.Equal(1, ws.PendingCount);
        ws.Ack(1);
        Assert.Equal(1, ws.PendingCount);
        Assert.Throws<InvalidDataException>(() => ws.Ack(4));
    }
    [Fact]
    public void GameTicksAndSameTickOrderSurviveReplayAndWallClockChanges()
    {
        var ws = Build();
        var host = new FakeGameHost { TickCount = 8192 };
        var match = new MatchState(host, Fixture.Config(), new SocketOutput(ws), Fixture.Messages, () => { });
        match.LogEvent(LogLevel.Info, "first");
        host.NowMs -= 1000;
        match.LogEvent(LogLevel.Info, "same tick");
        host.TickCount++;
        match.LogEvent(LogLevel.Info, "next tick");

        var pending = ws.PendingAfter(-1).ToArray();
        Assert.Equal([0, 1, 2], pending.Select(entry => entry.Seq));
        Assert.Equal([8192, 8192, 8193], pending.Select(entry =>
        {
            using var json = JsonDocument.Parse(entry.Line);
            return json.RootElement.GetProperty("event").GetProperty("tick").GetInt32();
        }));

        ws.MarkSent(2);
        ws.Ack(0);
        host.TickCount = 9000;
        Assert.Equal(pending.Skip(1).Select(entry => entry.Line), ws.PendingAfter(-1).Select(entry => entry.Line));
    }

    [Fact]
    public void AFullBufferDropsTheNewEventKeepsTheOldOnesAndSaysSoOnce()
    {
        var logs = new List<(LogLevel Level, string Message)>();
        long? recoveredAfter = null;
        var ws = new WsClient(new Uri("ws://127.0.0.1:4500/ws/plugin"), "0199a0b4-5c00-7000-8000-00000000b001", _ => { },
            (level, message) => logs.Add((level, message)), onRecovered: dropped => recoveredAfter = dropped);
        for (var i = 0; i < WsClient.MaxBufferedEvents; i++) Assert.True(ws.Enqueue(Event($"frame-{i}")));

        Assert.False(ws.Enqueue(Event("overflow-1")));
        Assert.False(ws.Enqueue(Event("overflow-2")));

        // Nothing already queued is touched, the dropped events got no sequence, one error line.
        Assert.Equal(WsClient.MaxBufferedEvents, ws.PendingCount);
        Assert.Equal(0, ws.PendingAfter(-1)[0].Seq);
        Assert.Equal(LogLevel.Error, Assert.Single(logs).Level);
        Assert.Contains("dropping log (1 dropped so far)", logs[0].Message);

        // An ack frees room: the next event is taken and the recovery is one warning with the toll.
        ws.MarkSent(9);
        ws.Ack(9);
        Assert.True(ws.Enqueue(Event("after")));
        Assert.Equal(WsClient.MaxBufferedEvents, Assert.Single(ws.PendingAfter(WsClient.MaxBufferedEvents - 1)).Seq);
        Assert.Equal(2, logs.Count);
        Assert.Equal(LogLevel.Warn, logs[1].Level);
        Assert.Contains("2 events were dropped", logs[1].Message);
        Assert.Equal(2, recoveredAfter);
    }

    [Fact]
    public void TheByteLimitPreservesQueuedEventsAndAcknowledgementsReleaseCapacity()
    {
        var ws = Build();
        var config = Fixture.Config();
        var backup = new RoundBackup
        {
            round = 1,
            team1Side = Side.Ct,
            rules = config.rules,
            roster = config.roster,
            file = new string('x', RoundBackup.MaxFileChars),
        };
        // Half-MiB backups reach the byte limit long before the event limit, whatever the framing adds.
        var fitted = 0;
        while (ws.Enqueue(backup)) fitted++;
        Assert.InRange(fitted, 1, WsClient.MaxBufferedEvents - 1);
        var pending = ws.PendingAfter(-1, int.MaxValue).ToArray();
        Assert.Equal(fitted, pending.Length);
        Assert.Equal(fitted, ws.PendingCount);

        Assert.False(ws.Enqueue(backup));
        Assert.Equal(pending, ws.PendingAfter(-1, int.MaxValue));

        ws.MarkSent(0);
        ws.Ack(0);
        Assert.True(ws.Enqueue(backup));
        var afterAck = ws.PendingAfter(-1, int.MaxValue).ToArray();
        Assert.Equal(pending.Skip(1), afterAck.Take(fitted - 1));
        // A refused event took no sequence number.
        Assert.Equal(fitted, afterAck[^1].Seq);
        Assert.False(ws.Enqueue(backup));
        Assert.Equal(afterAck, ws.PendingAfter(-1, int.MaxValue));
    }

    [Fact]
    public void OversizedEventsAreDroppedBeforeAssigningASequence()
    {
        var ws = Build();
        Assert.False(ws.Enqueue(Event(new string('x', WireLimits.MaxFrameBytes))));
        Assert.True(ws.Enqueue(Event("next")));
        Assert.Equal(0, Assert.Single(ws.PendingAfter(-1)).Seq);
    }
    [Fact]
    public async Task EnqueueAfterDisposeIsANoOp()
    {
        var ws = Build(); await ws.DisposeAsync(); Assert.False(ws.Enqueue(Event("a"))); Assert.Equal(0, ws.PendingCount);
    }
    [Fact]
    public void ConcurrentEnqueuesPreserveWireOrder()
    {
        var ws = Build();
        Parallel.For(0, 6, thread => { for (var i = 0; i < 250; i++) ws.Enqueue(Event($"{thread}/{i}")); });
        Assert.Equal(Enumerable.Range(0, 1500).Select(i => (long)i), ws.PendingAfter(-1, 2000).Select(e => e.Seq));
    }
    [Theory]
    [InlineData(true, 0.2, 0.1)]
    [InlineData(false, 30, 0.1)]
    [InlineData(false, 0.2, 4)]
    public void BackoffResetsOnlyAfterAHealthySession(bool ack, double duration, double expected) =>
        Assert.Equal(TimeSpan.FromSeconds(expected), WsClient.BackoffAfterSession(TimeSpan.FromSeconds(4), ack, TimeSpan.FromSeconds(duration)));
    [Fact]
    public void CredentialsAreStrippedFromLoggedUrls() =>
        Assert.Equal("ws://localhost/ws/plugin", WsClient.Redact(new Uri("ws://localhost/ws/plugin?slot_id=a&key=secret")));

    /// <summary>The match's events, straight onto the socket's queue.</summary>
    private sealed class SocketOutput(WsClient ws) : IMatchOutput
    {
        public void Send(PluginMessage message) => ws.Enqueue(message);
        public void ReportMatch(MatchReport report) { }
    }
}
