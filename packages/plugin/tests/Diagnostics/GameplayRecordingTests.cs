using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Ankor.Diagnostics;

using Xunit;

namespace Ankor.Tests;

public sealed class GameplayRecordingTests
{
    [Fact]
    public void FailedSerializationPreservesHostValueAndMakesCaptureUnusable()
    {
        var stream = new MemoryStream();
        using (var capture = new DebugCapture(stream, "fixture", new { }, _ => { }))
        {
            var engine = new FakeGameHost { Clock = double.NaN };
            var host = GameplayRecording.Wrap(engine, capture.Observe, capture.Stop);
            Assert.True(double.IsNaN(host.CurrentTime));
            Assert.False(capture.Enabled);
            engine.Clock = 42;
            Assert.Equal(42, host.CurrentTime);
        }
        var lines = System.Text.Encoding.UTF8.GetString(stream.ToArray()).Split('\n', StringSplitOptions.RemoveEmptyEntries);
        Assert.Throws<InvalidDataException>(() => GameplayReplay.ReadCapture(lines));
    }

    [Fact]
    public void FailedObserverAndReporterCannotSuppressScheduledGameplay()
    {
        var engine = new FakeGameHost { DeferNextFrames = true };
        var attempts = 0;
        var host = GameplayRecording.Wrap(engine, (_, _) =>
        {
            attempts++;
            throw new InvalidOperationException("observer");
        }, _ => throw new InvalidOperationException("reporter"));
        var ran = 0;
        host.NextFrame(() => ran++);
        host.AddTimer(1, () => ran++);
        engine.RunNextFrame();
        engine.Advance(1);
        Assert.Equal(2, ran);
        Assert.Equal(1, attempts);
        var error = new InvalidOperationException("gameplay");
        host.NextFrame(() => throw error);
        Assert.Same(error, Assert.Throws<InvalidOperationException>(engine.RunNextFrame));
    }

    [Fact]
    public void InputSchemaAllowsAdditionsButRejectsMissingRequiredValuesAndUnknownKinds()
    {
        var valid = """{"input":{"$input":"GrenadeRemoved","entityId":12,"futureField":true}}""";
        using var json = JsonDocument.Parse(valid);
        Assert.Equal(new GrenadeRemovedInput(12), GameplayRecording.Decode(json.RootElement));
        foreach (var invalid in new[] {
            """{"input":{"$input":"GrenadeRemoved"}}""",
            """{"input":{"$input":"unknown"}}""",
            """{"input":null}""" })
        {
            using var document = JsonDocument.Parse(invalid);
            Assert.Throws<JsonException>(() => GameplayRecording.Decode(document.RootElement));
        }
    }

    [Fact]
    public void NewRequiredEngineReadFailsClearlyInsteadOfSubstitutingZero()
    {
        var (host, replay) = GameplayReplayHost.Create(new());
        replay.Replaying = true;
        Assert.Throws<InvalidDataException>(() => host.SimulationTick);
    }

    [Fact]
    public void CaptureLoaderRejectsTruncationGapsAndUnclosedCallbacks()
    {
        var stream = new MemoryStream();
        using (var capture = new DebugCapture(stream, "fixture", new { }, _ => { }))
        using (capture.Begin("round.start"))
            capture.Observe("gameplay.input", new GameplayRecord( new RoundStartInput()));
        var lines = System.Text.Encoding.UTF8.GetString(stream.ToArray()).Split('\n', StringSplitOptions.RemoveEmptyEntries);
        var input = Assert.Single(GameplayReplay.ReadCapture(lines));
        Assert.IsType<RoundStartInput>(GameplayRecording.Decode(input.Value));
        Assert.Throws<InvalidDataException>(() => GameplayReplay.ReadCapture(lines[..^1]));
        Assert.Throws<InvalidDataException>(() => GameplayReplay.ReadCapture(lines.Where((_, i) => i != 2)));
        Assert.Throws<InvalidDataException>(() => GameplayReplay.ReadCapture([.. lines[..3], lines[^1]]));
    }
}
