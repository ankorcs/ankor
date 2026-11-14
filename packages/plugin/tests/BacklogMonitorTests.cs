using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class BacklogMonitorTests
{
    private static (FakeGameHost Host, List<(LogLevel Level, string Message)> Reports, FakeAgentChannel Agent) Build()
    {
        var host = new FakeGameHost();
        var reports = new List<(LogLevel, string)>();
        var agent = new FakeAgentChannel();
        var monitor = new BacklogMonitor(host, agent, (level, message) => reports.Add((level, message)));
        monitor.Start();
        return (host, reports, agent);
    }

    [Fact]
    public void ReportingCanDisposeTheMonitorFromInsideItsCheck()
    {
        var host = new FakeGameHost();
        var agent = new FakeAgentChannel();
        BacklogMonitor? monitor = null;
        monitor = new BacklogMonitor(host, agent, (_, _) => monitor!.Dispose());
        monitor.Start();
        Assert.Single(host.Timers);
        agent.SetBacklog(1, null);
        host.Advance(BacklogMonitor.CheckIntervalSeconds);
        Assert.Equal(1, agent.Reads);
        Assert.Single(host.Timers);
        agent.SetBacklog(2, 90);
        host.Advance(BacklogMonitor.CheckIntervalSeconds);
        Assert.Equal(2, agent.Reads);
        Assert.Empty(host.Timers);
    }

    [Fact]
    public void StartingAgainWhileACheckIsPendingKeepsItsSchedule()
    {
        var host = new FakeGameHost();
        var agent = new FakeAgentChannel();
        using var monitor = new BacklogMonitor(host, agent, (_, _) => { });
        monitor.Start();
        host.Advance(BacklogMonitor.CheckIntervalSeconds / 2);

        monitor.Start();
        host.Advance(BacklogMonitor.CheckIntervalSeconds / 2);

        Assert.Equal(1, agent.Reads);
        Assert.Single(host.Timers);
    }

    [Fact]
    public void AShortWaitIsNormalAndSaysNothing()
    {
        var (host, reports, agent) = Build();
        agent.SetBacklog(40, 12);
        host.Advance(BacklogMonitor.CheckIntervalSeconds * 6);

        Assert.Empty(reports);
    }

    [Fact]
    public void ABacklogIsReportedOnceRemindedEveryMinuteAndClosedWithItsPeak()
    {
        var (host, reports, agent) = Build();

        agent.SetBacklog(120, BacklogMonitor.BacklogAfterSeconds);
        host.Advance(BacklogMonitor.CheckIntervalSeconds);
        var opened = Assert.Single(reports);
        Assert.Equal(LogLevel.Warn, opened.Level);
        Assert.Contains("120 events waiting", opened.Message);

        // Still backlogged, more queued: silence until the reminder is due.
        agent.SetBacklog(900, 80);
        host.Advance(BacklogMonitor.CheckIntervalSeconds * 5);
        Assert.Single(reports);
        host.Advance(BacklogMonitor.CheckIntervalSeconds);
        Assert.Equal(2, reports.Count);
        Assert.Contains("900 events waiting", reports[1].Message);

        // The agent catches up: one closing line with how long it lasted and how deep it got.
        agent.SetBacklog(0, null);
        host.Advance(BacklogMonitor.CheckIntervalSeconds);
        Assert.Equal(3, reports.Count);
        Assert.Equal(LogLevel.Info, reports[2].Level);
        Assert.Contains("peaked at 900 events", reports[2].Message);

        // And nothing more while things stay healthy.
        host.Advance(BacklogMonitor.CheckIntervalSeconds * 12);
        Assert.Equal(3, reports.Count);
    }

    [Fact]
    public void DisposingStopsTheChecks()
    {
        var host = new FakeGameHost();
        var reports = new List<(LogLevel, string)>();
        var monitor = new BacklogMonitor(host, new FakeAgentChannel { PendingCount = 500, OldestPendingAgeSeconds = 90 },
            (level, message) => reports.Add((level, message)));
        monitor.Start();
        monitor.Dispose();

        host.Advance(BacklogMonitor.CheckIntervalSeconds * 3);
        Assert.Empty(reports);
        Assert.Empty(host.Timers);
    }

    [Fact]
    public void MonitoringSurvivesMapChangesIncludingRemindersRecoveryAndDisposal()
    {
        var host = new FakeGameHost { Clock = 1000 };
        var reports = new List<(LogLevel Level, string Message)>();
        var agent = new FakeAgentChannel { PendingCount = 500, OldestPendingAgeSeconds = 90 };
        using var monitor = new BacklogMonitor(host, agent, (level, message) => reports.Add((level, message)));
        monitor.Start();
        host.ChangeMap("de_nuke"); // Includes the initial stock-to-workshop transition.
        host.Advance(10);
        Assert.Single(reports);

        host.ChangeMap("de_mirage"); // A reminder must survive the map clock resetting too.
        host.Advance(60);
        Assert.Equal(2, reports.Count);
        Assert.Contains("backlog continues", reports[1].Message);
        agent.SetBacklog(0, null);
        host.Advance(10);
        Assert.Equal(3, reports.Count);
        Assert.Equal(LogLevel.Info, reports[2].Level);
        Assert.Contains("after 160s", reports[2].Message);

        monitor.Dispose();
        agent.SetBacklog(500, 90);
        host.ChangeMap("de_dust2");
        host.Advance(60);
        Assert.Equal(3, reports.Count);
        Assert.Empty(host.Timers);
    }
}
