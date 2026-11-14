using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class EventCollectionTests
{
    private static IEnumerable<string> Reported(GameplayTestSession session) => session.Progress.Select(report => report switch
    {
        PluginPhaseChange change => change.phase.ToString(),
        PluginScore score => $"{score.team1Score}-{score.team2Score}",
        PluginBackup backup => $"backup {backup.backup.round}",
        _ => throw new InvalidOperationException(),
    });

    [Fact]
    public void AMatchWithoutStatisticsPlaysThroughHalftimeAndReportsOnlyItsPhaseScoreAndBackups()
    {
        var session = new GameplayTestSession(maxRounds: 2, record: false, collectEvents: false);
        Assert.False(session.Match.Config.sendStatistics);
        session.StartRound(1);
        session.EndRound(session.Match.Team1Side, 1);
        session.Engine.RunNextFrame();
        session.StartRound(2);
        session.EndRound(session.Match.Team1Side, 1);
        session.Engine.RunNextFrame();
        Assert.Equal(MatchPhase.PostLive, session.Match.Phase);
        Assert.Equal(2, session.Match.Team1Score);
        Assert.Empty(session.Sent);
        Assert.Equal(["Live", "backup 1", "1-0", "backup 2", "2-0", "PostLive"], Reported(session));
        using var status = JsonDocument.Parse(ServerStatus.Serialize(session.Engine, true, session.Match));
        var match = status.RootElement.GetProperty("match");
        Assert.Equal("post_live", match.GetProperty("phase").GetString());
        session.Engine.Advance(61);
        Assert.Contains(session.Engine.Commands, command => command.Command == "quit");
    }

    [Fact]
    public void AMatchWithStatisticsReportsItsPhaseAndScoreBesideItsEvents()
    {
        var session = new GameplayTestSession(maxRounds: 2, record: false);
        session.StartRound(1);
        session.EndRound(session.Match.Team1Side, 1);
        session.Engine.RunNextFrame();

        Assert.Equal(["Live", "1-0"], Reported(session));
        Assert.Contains(session.Sent, message => message is RoundEnd { team1Score: 1 });
    }
}
