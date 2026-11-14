using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class PluginRuntimeTests
{
    private const string AllocationId = "0199a0b4-5c00-7000-8000-00000000b001";

    private sealed class Log(List<string> steps) : PluginRuntime.ILog
    {
        public void ConfigMissing(string file) => steps.Add($"config missing {file}");
        public void ConfigInvalid(string file, string error) => steps.Add($"config invalid {error}");
        public void Readiness(string message) => steps.Add($"readiness {message}");
        public void LaunchCfg(string map, string line) => steps.Add($"launch cfg {map}: {line}");
        public void WsUrlMissing(string variable) => steps.Add($"missing {variable}");
        public void WsUrlInvalid(string variable) => steps.Add($"invalid {variable}");
        public void EventStream(string allocationId) => steps.Add($"event stream {allocationId}");
        public void NoSocket(string frame) => steps.Add("no socket");
        public void Exec(string command) => steps.Add($"exec {command}");
        public void SkippingType(string type) => steps.Add($"skipping {type}");
        public void IgnoredFrame(string excerpt) => steps.Add($"ignored {excerpt}");
    }

    /// <summary>A loaded match that records what the runtime asks of it.</summary>
    private sealed class Match(MatchState state, List<string> steps) : PluginRuntime.IMatch
    {
        public MatchState State => state;
        public IDisposable? BeginCapture(string name)
        {
            steps.Add($"capture {name}");
            return null;
        }
        public string? RestoreRound(int round) => null;
        public string? StartMatch() => null;
        public void OnMapLoad(string mapName) => steps.Add($"match map {mapName}");
        public void Dispose() => steps.Add("match disposed");
    }

    private static (PluginRuntime Runtime, FakeGameHost Host, List<string> Steps) Start(bool withMatch)
    {
        var host = new FakeGameHost();
        var steps = new List<string>();
        var runtime = new PluginRuntime(host, AllocationId, new Log(steps));
        runtime.StartReadiness(null, "de_dust2");
        if (withMatch) runtime.Match = new Match(new MatchState(host, Fixture.Config(), new FakeAgentChannel(), Fixture.Messages, () => { }), steps);
        return (runtime, host, steps);
    }

    [Fact]
    public void RunsTheLaunchCfgASecondAfterEachMapLoadOnAServerWithoutAMatch()
    {
        var (runtime, host, steps) = Start(withMatch: false);

        runtime.OnMapLoad("de_dust2");
        Assert.DoesNotContain(host.CommandNames, command => command == CfgNames.Exec(CfgNames.Launch));
        host.Advance(1);

        Assert.Contains(host.CommandNames, command => command == CfgNames.Exec(CfgNames.Launch));
        Assert.Contains($"launch cfg de_dust2: {CfgNames.Exec(CfgNames.Launch)}", steps);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void TagsTheServerWithItsAllocationAfterTheTagsItsCfgsSet(bool withMatch)
    {
        var (runtime, host, _) = Start(withMatch);
        host.StringConVars["sv_tags"] = "league";

        runtime.OnMapLoad("de_dust2");
        host.Advance(2);

        Assert.Equal($"league,{ServerTags.Tag(AllocationId)}", host.StringConVars["sv_tags"]);
    }

    [Fact]
    public void HandsAMapLoadToTheMatchBeforeReadinessAndRunsNoLaunchCfgForIt()
    {
        var (runtime, host, steps) = Start(withMatch: true);

        runtime.OnMapLoad("de_dust2");
        host.Advance(2);

        Assert.Equal("match map de_dust2", steps.First());
        Assert.DoesNotContain(host.CommandNames, command => command == CfgNames.Exec(CfgNames.Launch));
    }

    [Fact]
    public void NamesSteamsLogonInTheMatchsCapture()
    {
        var (runtime, _, steps) = Start(withMatch: true);

        runtime.OnSteamApiActivated();

        Assert.Equal("capture steam.activated", steps.First());
    }

    [Theory]
    [InlineData(null, "missing ANKOR_WS_URL")]
    [InlineData(" ", "missing ANKOR_WS_URL")]
    [InlineData("not a url", "invalid ANKOR_WS_URL")]
    public void DialsNothingWithoutAUsableRelayUrl(string? url, string logged)
    {
        var (runtime, _, steps) = Start(withMatch: false);

        runtime.StartWebSocket(url);

        Assert.Equal([logged], steps);
    }

    [Fact]
    public void UnloadsTheMatchAndForgetsIt()
    {
        var (runtime, _, steps) = Start(withMatch: true);

        runtime.Unload();

        Assert.Contains("match disposed", steps);
        Assert.Null(runtime.Match);
    }
}
