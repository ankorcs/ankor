using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class MatchSessionTests
{
    private static MatchSession Build(FakeGameHost host, MatchConfig config, List<PluginMessage> sent, List<string> steps) =>
        new(host, host, config, MatchFiles.Server, new FakeAgentChannel(sent), Fixture.Messages,
            () => steps.Add("grenades cleared"), match => steps.Add($"before shutdown {match.Config.matchId}"));

    [Fact]
    public void EndsTheSteamVerificationTimersWithTheMatch()
    {
        var host = new FakeGameHost();
        var session = Build(host, Fixture.Config(SidesMode.Team1Ct), [], []);
        session.TeamLock.Connect(Fixture.Player(Fixture.Team1Player1, Side.Ct) with { Authorized = false });

        session.Dispose();
        host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Empty(host.Kicked);
    }

    [Fact]
    public void HandsTheMatchToTheAdapterBeforeTheShutdownIsBuilt()
    {
        var steps = new List<string>();
        using var _ = Build(new FakeGameHost(), Fixture.Config(), [], steps);

        Assert.Equal(["before shutdown 0199a0b4-5c00-7000-8000-00000000abc1"], steps);
    }

    [Fact]
    public void WatchesTheBacklogOnlyForAMatchThatSendsStatistics()
    {
        var config = Fixture.Config();
        using var counted = Build(new FakeGameHost(), config, [], []);
        config.sendStatistics = false;
        using var quiet = Build(new FakeGameHost(), config, [], []);

        Assert.NotNull(counted.Backlog);
        Assert.Null(quiet.Backlog);
    }

    [Fact]
    public void LogsOnTheServerAndIntoTheMatchsRecord()
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        using var session = Build(host, Fixture.Config(), sent, []);

        session.LogBoth(LogLevel.Warn, "disk is slow");

        Assert.Contains((LogLevel.Warn, "disk is slow"), host.Logs);
        Assert.Contains(sent, message => message is PluginLog { message: "disk is slow" });
    }

    [Fact]
    public void AReadyTimeoutAbortsTheMatchThenKicksEveryoneAndQuits()
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        using var session = Build(host, Fixture.Config(readyMode: ReadyMode.Manual, readyTimeoutSeconds: 30), sent, []);
        session.Match.EnterWarmup();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0));
        host.PlayerList.Add(Fixture.Player(Fixture.Team2Player1, Side.T, 1));

        host.Advance(30);
        Assert.Empty(sent.OfType<MapEnd>());
        host.IsWarmup = false;
        session.Callbacks.Handle(new RoundStartInput());
        host.Advance(1);

        Assert.Equal(MapEndReasons.ReadyTimeout, Assert.Single(sent.OfType<MapEnd>()).reason);
        Assert.Equal(1, host.Aborted);
        Assert.Empty(host.Kicked);
        host.Advance(10);
        Assert.Equal(2, host.Kicked.Count);
        Assert.Contains("quit", host.CommandNames);
    }
}
