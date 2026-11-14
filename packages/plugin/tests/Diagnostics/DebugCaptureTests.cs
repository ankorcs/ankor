using Ankor.Diagnostics;
using System.Text.Json;
using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public sealed class DebugCaptureTests : IDisposable
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "ankor-capture-" + Guid.NewGuid());

    private JsonElement[] Records(string launchId) => File.ReadAllLines(Path.Combine(_directory, launchId + ".jsonl"))
        .Select(line => JsonSerializer.Deserialize<JsonElement>(line)).ToArray();

    [Fact]
    public void NestedCallbacksAndReadsKeepTheirOrder()
    {
        var errors = new List<string>();
        using (var capture = new DebugCapture(_directory, "launch-a", new { map = "test" }, errors.Add))
        {
            using (capture.Begin("damage.pre"))
            {
                capture.Observe("inventory", new[] { 43, 43 });
                using (capture.Begin("death")) { }
            }
        }
        var records = Records("launch-a");
        Assert.Equal(["capture_start", "enter", "observation", "enter", "exit", "exit", "capture_end"],
            records.Select(record => record.GetProperty("phase").GetString()));
        Assert.Equal(Enumerable.Range(0, 6).Select(i => (long)i),
            records[..^1].Select(record => record.GetProperty("sequence").GetInt64()));
        Assert.Equal(1, records[3].GetProperty("parentCallbackId").GetInt64());
        Assert.Equal(3, records[4].GetProperty("callbackId").GetInt64());
        Assert.True(records[^1].GetProperty("complete").GetBoolean());
        Assert.Empty(errors);
    }

    [Fact]
    public void QuitWaitsForNestedCallbacksAndFileCompletion()
    {
        using var capture = new DebugCapture(_directory, "launch-a", new { }, _ => { });
        var quit = false;
        using (capture.Begin("timer"))
        {
            using (capture.Begin("nested"))
            {
                capture.Observe("host.command", "quit");
                capture.CompleteBefore(() =>
                {
                    var records = Records("launch-a");
                    Assert.Equal(["capture_start", "enter", "enter", "observation", "exit", "exit", "capture_end"],
                        records.Select(record => record.GetProperty("phase").GetString()));
                    Assert.True(records[^1].GetProperty("complete").GetBoolean());
                    quit = true;
                });
                Assert.False(quit);
            }
            Assert.False(quit);
        }
        Assert.True(quit);
        Assert.False(capture.Enabled);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void QuitWithoutAnActiveCallbackStillRunsAfterCompletion(bool failed)
    {
        using var capture = new DebugCapture(_directory, "launch-a", new { }, _ => { });
        if (failed) capture.Observe("oversized", new string('x', 17 << 20));
        var quit = false;
        capture.CompleteBefore(() =>
        {
            Assert.Equal(!failed, Records("launch-a")[^1].GetProperty("complete").GetBoolean());
            quit = true;
        });
        Assert.True(quit);
    }

    [Fact]
    public void LaunchesHaveSeparateFilesAndAnExistingCaptureCannotBeOverwritten()
    {
        using (var first = new DebugCapture(_directory, "launch-a", new { }, _ => { }))
            first.Observe("first", 1);
        var original = File.ReadAllText(Path.Combine(_directory, "launch-a.jsonl"));
        Assert.Throws<IOException>(() => new DebugCapture(_directory, "launch-a", new { }, _ => { }));
        using (var second = new DebugCapture(_directory, "launch-b", new { }, _ => { }))
            second.Observe("second", 2);
        Assert.Equal(original, File.ReadAllText(Path.Combine(_directory, "launch-a.jsonl")));
        Assert.Equal("second", Records("launch-b")[1].GetProperty("data").GetProperty("name").GetString());
        Assert.Throws<ArgumentException>(() => new DebugCapture(_directory, "../escape", new { }, _ => { }));
    }

    [Fact]
    public void OverflowStopsCaptureAndMarksTheFileIncomplete()
    {
        var errors = new List<string>();
        using (var capture = new DebugCapture(_directory, "launch-a", new { }, errors.Add))
        {
            capture.Observe("oversized", new string('x', 17 << 20));
            Assert.False(capture.Enabled);
            Assert.Null(capture.Begin("ignored"));
            capture.Observe("ignored", 0);
        }
        Assert.Single(errors);
        var records = Records("launch-a");
        Assert.False(records[^1].GetProperty("complete").GetBoolean());
        Assert.Contains("incomplete", records[^1].GetProperty("error").GetString());
        Assert.Equal(2, records.Length);
    }

    [Fact]
    public void UnserializableObservationsDisableCaptureWithoutThrowingIntoTheHandler()
    {
        var errors = new List<string>();
        using (var capture = new DebugCapture(_directory, "launch-a", new { }, errors.Add))
        {
            var cycle = new Dictionary<string, object>();
            cycle["self"] = cycle;
            capture.Observe("invalid", cycle);
            Assert.False(capture.Enabled);
        }
        Assert.Single(errors);
        Assert.False(Records("launch-a")[^1].GetProperty("complete").GetBoolean());
    }

    private sealed class PausedStream(bool ignoreCancellation = false) : MemoryStream
    {
        public readonly TaskCompletionSource Started = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource Closed = new(TaskCreationOptions.RunContinuationsAsynchronously);

        public override async ValueTask WriteAsync(ReadOnlyMemory<byte> buffer, CancellationToken cancellationToken = default)
        {
            Started.TrySetResult();
            await Release.Task.WaitAsync(ignoreCancellation ? CancellationToken.None : cancellationToken);
            await base.WriteAsync(buffer, ignoreCancellation ? CancellationToken.None : cancellationToken);
        }

        protected override void Dispose(bool disposing)
        {
            base.Dispose(disposing);
            Closed.TrySetResult();
        }
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task StalledCaptureCannotPreventQuitEvenIfTheStreamIgnoresCancellation(bool ignoreCancellation)
    {
        var errors = new System.Collections.Concurrent.ConcurrentQueue<string>();
        var stream = new PausedStream(ignoreCancellation);
        using var capture = new DebugCapture(stream, "launch-a", new { }, errors.Enqueue);
        try
        {
            await stream.Started.Task.WaitAsync(TimeSpan.FromSeconds(5));
            capture.Observe("pending", 1);
            var quit = false;
            await Task.Run(() => capture.CompleteBefore(() => quit = true))
                .WaitAsync(TimeSpan.FromSeconds(5));

            Assert.True(quit);
            Assert.False(capture.Enabled);
            Assert.False(stream.Release.Task.IsCompleted);
            Assert.Contains("timed out", Assert.Single(errors));
            // Repeated disposal must not wait on the abandoned writer again.
            await Task.Run(capture.Dispose).WaitAsync(TimeSpan.FromSeconds(1));
        }
        finally
        {
            stream.Release.TrySetResult();
            await stream.Closed.Task.WaitAsync(TimeSpan.FromSeconds(5));
        }

        var records = System.Text.Encoding.UTF8.GetString(stream.ToArray())
            .Split('\n', StringSplitOptions.RemoveEmptyEntries);
        Assert.DoesNotContain(records, line => line.Contains("capture_end"));
        Assert.Throws<InvalidDataException>(() => GameplayReplay.ReadCapture(records));
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task StartupBurstsAreBoundedByBytesWhileDiskIsStalled(bool exceedByteBudget)
    {
        var errors = new List<string>();
        var stream = new PausedStream();
        using var capture = new DebugCapture(stream, "launch-a", new { }, errors.Add);
        var count = exceedByteBudget ? 20 : 5000;
        var value = new string('x', exceedByteBudget ? 1 << 20 : 32);
        try
        {
            await stream.Started.Task.WaitAsync(TimeSpan.FromSeconds(5));
            for (var i = 0; i < count; i++) capture.Observe("burst", new { i, value });
            Assert.Equal(!exceedByteBudget, capture.Enabled);
        }
        finally { stream.Release.TrySetResult(); }
        capture.Dispose();

        var records = System.Text.Encoding.UTF8.GetString(stream.ToArray()).Split('\n', StringSplitOptions.RemoveEmptyEntries)
            .Select(line => JsonSerializer.Deserialize<JsonElement>(line)).ToArray();
        Assert.Equal(!exceedByteBudget, records[^1].GetProperty("complete").GetBoolean());
        Assert.Equal(exceedByteBudget ? 1 : 0, errors.Count);
        Assert.Equal(Enumerable.Range(0, records.Length - 1).Select(i => (long)i),
            records[..^1].Select(record => record.GetProperty("sequence").GetInt64()));
        if (!exceedByteBudget)
        {
            Assert.Equal(count + 2, records.Length);
            Assert.Equal(Enumerable.Range(0, count), records[1..^1]
                .Select(record => record.GetProperty("data").GetProperty("value").GetProperty("i").GetInt32()));
        }
    }

    public void Dispose()
    {
        if (Directory.Exists(_directory)) Directory.Delete(_directory, recursive: true);
    }
}
