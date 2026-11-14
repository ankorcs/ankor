using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class DemoRecorderTests
{
    private const string Stamp = "20231114-221320";

    private static (DemoRecorder Recorder, FakeGameHost Host, List<(LogLevel Level, string Message)> Logs)
        Build(MatchConfig? config = null)
    {
        var host = new FakeGameHost();
        var logs = new List<(LogLevel, string)>();
        return (new DemoRecorder(host, config ?? Fixture.Config(), (level, message) => logs.Add((level, message))),
            host, logs);
    }

    [Fact]
    public void AnnouncesCompletionOnlyAfterTheRecordingHasFlushed()
    {
        var host = new FakeGameHost();
        var completed = new List<string>();
        using var recorder = new DemoRecorder(host, Fixture.Config(), (_, _) => { }, completed.Add);
        recorder.Start();
        recorder.ScheduleStop();
        Assert.Empty(completed);
        host.Advance(15);
        Assert.Empty(completed);
        host.Advance(5);
        Assert.Equal([recorder.Name], completed);
        Assert.True(recorder.Flushed);
    }

    [Fact]
    public void AMatchThatRecordsNoDemoNeverStartsOneAndIsFlushedAtOnce()
    {
        var (recorder, host, _) = Build(Fixture.Config(recordDemo: false));
        using var _ = recorder;

        recorder.Start();
        recorder.ScheduleStop();

        Assert.Empty(host.CommandNames);
        Assert.True(recorder.Flushed);
    }

    [Fact]
    public void RecordingStartsOncePerMap()
    {
        var (recorder, host, logs) = Build();
        using var _ = recorder;

        recorder.Start();
        recorder.Start();

        Assert.Equal([$"tv_record {Demos.RecordDir}/{Stamp}_0199a0b4-5c00-7000-8000-00000000abc1_de_mirage"], host.CommandNames.ToArray());
        Assert.Equal([(LogLevel.Info, $"demo recording started: {Stamp}_0199a0b4-5c00-7000-8000-00000000abc1_de_mirage")], logs.ToArray());
    }

    [Theory]
    [InlineData("0199a0b4-5c00-7000-8000-00000000abc1", "de_mirage", "0199a0b4-5c00-7000-8000-00000000abc1_de_mirage")]
    [InlineData("0199a0b4-5c00-7000-8000-00000000abc1", "de mirage; quit", "0199a0b4-5c00-7000-8000-00000000abc1_demiragequit")]
    // The agent splits the name on underscores, so only the map part may carry them.
    [InlineData("m_abc 123", "de_mirage", "mabc123_de_mirage")]
    public void TheFileNameCarriesTheMatchIdentityAndOnlySafeCharacters(
        string matchId, string mapName, string tail)
    {
        var config = Fixture.Config();
        config.matchId = matchId;
        var expected = $"{Stamp}_{tail}";
        var (recorder, host, _) = Build(config);
        using var _dispose = recorder;
        host.MapName = mapName;

        recorder.Start();

        Assert.Equal(expected, recorder.Name);
        Assert.Equal([$"tv_record {Demos.RecordDir}/{expected}"], host.CommandNames.ToArray());
    }

    [Fact]
    public void AServerWithoutAMatchIdStillRecordsWithAnEmptyIdentity()
    {
        var config = Fixture.Config();
        config.matchId = null;
        var (recorder, host, _) = Build(config);
        using var _dispose = recorder;

        recorder.Start();

        Assert.Equal($"{Stamp}__de_mirage", recorder.Name);
        Assert.Equal([$"tv_record {Demos.RecordDir}/{Stamp}__de_mirage"], host.CommandNames.ToArray());
    }

    [Theory]
    [InlineData(0)]
    [InlineData(15)]
    [InlineData(30)]
    [InlineData(60)]
    [InlineData(90)]
    public void TheStopWaitsForTheBroadcastDelayAndTheFileThenCloses(int delay)
    {
        var (recorder, host, logs) = Build();
        using var _ = recorder;
        host.ConVars["tv_delay"] = delay;
        recorder.Start();
        host.Advance(delay);
        Assert.Contains(host.CommandNames, c => c.StartsWith("tv_record "));

        recorder.ScheduleStop();
        host.Advance(delay + 14);
        Assert.DoesNotContain("tv_stoprecord", host.CommandNames);
        Assert.False(recorder.Flushed);

        host.Advance(1);
        Assert.Contains("tv_stoprecord", host.CommandNames);
        Assert.Contains((LogLevel.Info, $"demo recording stopped: {recorder.Name}"), logs);
        Assert.False(recorder.Flushed);

        host.Advance(4);
        Assert.False(recorder.Flushed);
        host.Advance(1);
        Assert.True(recorder.Flushed);
    }

    [Fact]
    public void AMatchThatNeverRecordedIsAlreadyFlushed()
    {
        var (recorder, host, logs) = Build();
        using var _ = recorder;

        Assert.True(recorder.Flushed);
        recorder.ScheduleStop();
        host.Advance(120);

        Assert.True(recorder.Flushed);
        Assert.Empty(host.Commands);
        Assert.Empty(logs);
    }

    [Fact]
    public void TheStopSurvivesAMapChange()
    {
        var (recorder, host, _) = Build();
        using var _dispose = recorder;
        recorder.Start();
        recorder.ScheduleStop();

        host.ChangeMap("de_nuke");
        host.Advance(20);

        Assert.Contains("tv_stoprecord", host.CommandNames);
        Assert.True(recorder.Flushed);
    }

    [Fact]
    public void DisposalCancelsAPendingStopAndItsAlreadySelectedCallback()
    {
        var (recorder, host, _) = Build();
        recorder.Start();
        recorder.ScheduleStop();
        // The stop and the flush after it, each on its own timer.
        var pending = host.Timers.ToList();
        Assert.Equal(2, pending.Count);

        recorder.Dispose();
        foreach (var timer in pending) timer.Callback();
        host.Advance(120);

        Assert.DoesNotContain("tv_stoprecord", host.CommandNames);
        Assert.False(recorder.Flushed);
        Assert.Empty(host.Timers);
    }

    [Theory]
    [InlineData(SidesMode.Team1Ct, true)]
    [InlineData(SidesMode.Knife, false)]
    public void RecordingStartsWhenTheMatchGoesLive(SidesMode sides, bool recorded)
    {
        var host = new FakeGameHost();
        var config = Fixture.Config(sides);
        using var recorder = new DemoRecorder(host, config, (_, _) => { });
        using var match = new MatchState(host, config, new FakeAgentChannel(), Fixture.Messages, recorder.Start);
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0),
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 1),
            Fixture.Player(Fixture.Team2Player1, Side.T, 2),
            Fixture.Player(Fixture.Team2Player2, Side.T, 3),
        ]);

        match.EnterWarmup();
        match.MaybeStart();
        host.Advance(14);
        Assert.Equal(recorded ? MatchPhase.Warmup : MatchPhase.Knife, match.Phase);
        host.IsWarmup = false;
        match.OnRoundStart();

        Assert.Equal(recorded ? MatchPhase.Live : MatchPhase.Knife, match.Phase);
        Assert.Equal(recorded, host.CommandNames.Any(c => c.StartsWith("tv_record ")));
    }

    private static bool Recorded(FakeGameHost host) => host.CommandNames.Any(c => c.StartsWith("tv_record "));

    [Fact]
    public void RecordingStartsTvDelayAfterGoingLiveSoTheDemoOpensOnLivePlay()
    {
        var (recorder, host, _) = Build();
        using var _ = recorder;
        host.ConVars["tv_delay"] = 30;

        recorder.Start();
        host.Advance(10);
        // A second go-live inside the delay neither records nor pushes the start back.
        recorder.Start();
        host.Advance(19.9);
        Assert.False(Recorded(host));
        Assert.True(recorder.Flushed);

        host.Advance(0.1);
        Assert.True(Recorded(host));
        Assert.False(recorder.Flushed);
    }

    [Fact]
    public void AMatchEndingInsideTheStartDelayRecordsNothingAndIsFlushedAtOnce()
    {
        var (recorder, host, _) = Build();
        using var _ = recorder;
        host.ConVars["tv_delay"] = 30;

        recorder.Start();
        host.Advance(5);
        recorder.ScheduleStop();
        recorder.Start();
        host.Advance(300);

        Assert.Empty(host.CommandNames);
        Assert.True(recorder.Flushed);
    }

    [Fact]
    public void DisposingCancelsAPendingStart()
    {
        var (recorder, host, _) = Build();
        host.ConVars["tv_delay"] = 30;

        recorder.Start();
        recorder.Dispose();
        host.Advance(300);

        Assert.Empty(host.CommandNames);
    }

    [Fact]
    public void AMapChangeDropsAPendingStartAndTheNextGoLiveSchedulesItAgain()
    {
        var (recorder, host, _) = Build();
        using var _ = recorder;
        host.ConVars["tv_delay"] = 30;

        recorder.Start();
        host.Advance(10);
        host.ChangeMap("de_inferno");
        host.Advance(300);
        Assert.False(Recorded(host));

        recorder.Start();
        host.Advance(30);
        Assert.Single(host.CommandNames, c => c.StartsWith("tv_record ") && c.EndsWith("_de_inferno"));
    }

    // The live cfgs set the GOTV delay, which must not change once a recording runs.
    [Fact]
    public void RecordingWaitsForTheLiveCfgsToHaveRun()
    {
        var host = new FakeGameHost();
        var config = Fixture.Config(SidesMode.Team1Ct);
        using var recorder = new DemoRecorder(host, config, (_, _) => { });
        using var match = new MatchState(host, config, new FakeAgentChannel(), Fixture.Messages, recorder.Start);
        host.CfgFiles["gamemode_competitive.cfg"] = "tv_delay 105";
        host.DeferNextFrames = true;

        match.GoLive();
        host.RunNextFrame();

        var ran = host.Console.Ran;
        Assert.True(ran.IndexOf("tv_delay 105") < ran.IndexOf("tv_delay 30"));
        Assert.True(ran.IndexOf("tv_delay 30") < ran.FindIndex(c => c.StartsWith("tv_record ")));
        Assert.Equal("30", host.Console.Values["tv_delay"]);
    }
}

public sealed class DemoReadyMarkerTests : IDisposable
{
    private readonly string _directory = Directory.CreateTempSubdirectory("ankor-demos-").FullName;
    private readonly List<(LogLevel Level, string Message)> _logs = [];

    private void Mark(string name) => DemoRecorder.MarkReady(_directory, name, (level, message) => _logs.Add((level, message)));

    [Fact]
    public void AFlushedRecordingGetsItsMarkerAndNoTemporaryFile()
    {
        File.WriteAllText(Path.Combine(_directory, "match.dem"), "demo");

        Mark("match");

        Assert.Equal(["match.dem", "match.dem.ready"], Directory.GetFiles(_directory).Select(Path.GetFileName).Order());
        Assert.Empty(_logs);
    }

    [Fact]
    public void AMissingRecordingGetsNoMarker()
    {
        Mark("match");

        Assert.Empty(Directory.GetFileSystemEntries(_directory));
        Assert.Empty(_logs);
    }

    [Fact]
    public void AMarkerThatCannotBeWrittenIsAnError()
    {
        File.WriteAllText(Path.Combine(_directory, "match.dem"), "demo");
        Directory.CreateDirectory(Path.Combine(_directory, "match.dem.ready"));

        Mark("match");

        Assert.Equal(LogLevel.Error, Assert.Single(_logs).Level);
    }

    public void Dispose() => Directory.Delete(_directory, recursive: true);
}
