using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class MatchShutdownTests
{
    [Fact]
    public void KicksAfterSixtySecondsAndWaitsForAgentAcknowledgements()
    {
        var host = new FakeGameHost();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct));
        var agent = new FakeAgentChannel { PendingCount = 2 };
        using var shutdown = new MatchShutdown(host, agent, () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(59);
        Assert.Empty(host.Kicked);
        shutdown.Start(); // Duplicate result callbacks must not reset the deadline.
        host.Advance(1);
        Assert.Single(host.Kicked);
        host.Advance(0.1);
        host.Advance(120);
        Assert.DoesNotContain("quit", host.CommandNames);
        agent.PendingCount = 0;
        host.Advance(1);
        Assert.Single(host.Commands, c => c.Command == "quit");
    }

    [Fact]
    public void AnUnplayedMatchAbortsKicksAfterTenSecondsWithTheReadyTimeoutReasonAndWaitsForNoBroadcast()
    {
        var host = new FakeGameHost();
        host.ConVars["tv_enable"] = 1;
        host.ConVars["tv_delay"] = 90;
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct));
        using var shutdown = new MatchShutdown(host, new FakeAgentChannel(), () => true, Fixture.Messages);

        shutdown.Start(unplayed: true);
        Assert.Equal(1, host.Aborted);
        Assert.Equal("mp_match_restart_delay 86400", host.CommandNames.First());
        host.Advance(9.9);
        Assert.Empty(host.Kicked);
        host.Advance(0.1);
        Assert.Equal("Ready-up time ran out", Assert.Single(host.Kicked).Reason);
        host.Advance(1);

        Assert.Single(host.Commands, c => c.Command == "quit");
    }

    [Fact]
    public void AlreadyDrainedStillShowsResultForSixtySecondsAndKicksBeforeQuit()
    {
        var host = new FakeGameHost();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct));
        using var shutdown = new MatchShutdown(host, new FakeAgentChannel(), () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(60);
        Assert.Single(host.Kicked);
        Assert.DoesNotContain("quit", host.CommandNames);
        host.Advance(0.1);
        Assert.Contains("quit", host.CommandNames);
    }

    [Fact]
    public void QuitWaitsForTheDemoFileToBeFlushed()
    {
        var host = new FakeGameHost();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct));
        var flushed = false;
        using var shutdown = new MatchShutdown(host, new FakeAgentChannel(), () => flushed, Fixture.Messages);
        shutdown.Start();
        host.Advance(60);
        Assert.Single(host.Kicked);
        host.Advance(120);
        Assert.DoesNotContain("quit", host.CommandNames);
        flushed = true;
        host.Advance(1);
        Assert.Single(host.Commands, c => c.Command == "quit");
    }

    [Fact]
    public void WithGotvOnStaysUpUntilTheBroadcastShowedTheEndWithoutARecording()
    {
        var host = new FakeGameHost();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct));
        host.ConVars["tv_enable"] = 1;
        host.ConVars["tv_delay"] = 90;
        using var shutdown = new MatchShutdown(host, new FakeAgentChannel(), () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(60);
        Assert.Single(host.Kicked);
        host.Advance(90 + DemoRecorder.StopMargin - 60 - 0.5);
        Assert.DoesNotContain("quit", host.CommandNames);
        host.Advance(1.5);
        Assert.Single(host.Commands, c => c.Command == "quit");
    }

    [Fact]
    public void WithGotvOffIgnoresTvDelay()
    {
        var host = new FakeGameHost();
        host.ConVars["tv_enable"] = 0;
        host.ConVars["tv_delay"] = 90;
        using var shutdown = new MatchShutdown(host, new FakeAgentChannel(), () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(60.1);
        Assert.Contains("quit", host.CommandNames);
    }

    [Theory]
    [InlineData(10)]
    [InlineData(60)]
    [InlineData(61)]
    public void MapChangesPreserveResultDelayAndDrainUntilAcknowledged(double elapsed)
    {
        var host = new FakeGameHost();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct));
        var agent = new FakeAgentChannel { PendingCount = 1 };
        using var shutdown = new MatchShutdown(host, agent, () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(elapsed);
        host.ChangeMap("de_nuke");
        shutdown.Start(); // Neither map changes nor duplicate requests restart the result delay.
        if (elapsed < 60)
        {
            host.Advance(59 - elapsed);
            Assert.Empty(host.Kicked);
            host.Advance(1);
        }
        Assert.Single(host.Kicked);
        host.Advance(120);
        Assert.DoesNotContain("quit", host.CommandNames);
        agent.PendingCount = 0;
        host.Advance(1);
        Assert.Single(host.Commands, c => c.Command == "quit");
        host.ChangeMap("de_mirage");
        shutdown.Start();
        host.Advance(61);
        Assert.Single(host.Commands, c => c.Command == "quit");
    }

    [Theory]
    [InlineData(0)]
    [InlineData(60)]
    [InlineData(61)]
    public void UnloadCancelsShutdownEvenIfATimerCallbackWasAlreadyQueued(double elapsed)
    {
        var host = new FakeGameHost();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct));
        var agent = new FakeAgentChannel { PendingCount = 1 };
        var shutdown = new MatchShutdown(host, agent, () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(elapsed);
        var timer = Assert.Single(host.Timers, t => !t.Fired && !t.Cancelled);
        var kicks = host.Kicked.Count;
        shutdown.Dispose();
        agent.PendingCount = 0;
        timer.Callback();
        shutdown.Start();
        host.Advance(61);
        Assert.DoesNotContain("quit", host.CommandNames);
        Assert.Equal(kicks, host.Kicked.Count);
        Assert.DoesNotContain(host.Timers, t => !t.Fired && !t.Cancelled);
    }

    [Fact]
    public void TellsTheAgentOnceAndQuitsWhenThatIsSent()
    {
        var host = new FakeGameHost();
        var sends = 0;
        var sent = new TaskCompletionSource();
        using var shutdown = new MatchShutdown(host, new FakeAgentChannel
        {
            Quitting = () =>
            {
                sends++;
                return sent.Task;
            },
        }, () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(60);
        host.Advance(0.1);
        Assert.Equal(1, sends);
        host.Advance(0.5);
        Assert.DoesNotContain("quit", host.CommandNames);
        sent.SetResult();
        host.Advance(0.1);
        Assert.Single(host.Commands, c => c.Command == "quit");
        Assert.Equal(1, sends);
        Assert.DoesNotContain(host.Timers, t => !t.Fired && !t.Cancelled);
    }

    [Fact]
    public void QuitsWithinTwoSecondsWhenTheAgentCannotBeTold()
    {
        var host = new FakeGameHost();
        using var shutdown = new MatchShutdown(host, new FakeAgentChannel { Quitting = () => new TaskCompletionSource().Task }, () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(60);
        host.Advance(0.1);
        for (var i = 0; i < 19; i++) host.Advance(0.1);
        Assert.DoesNotContain("quit", host.CommandNames);
        host.Advance(0.1);
        Assert.Single(host.Commands, c => c.Command == "quit");
        Assert.DoesNotContain(host.Timers, t => !t.Fired && !t.Cancelled);
    }

    [Fact]
    public void UnloadWhileWaitingToTellTheAgentNeverQuits()
    {
        var host = new FakeGameHost();
        var shutdown = new MatchShutdown(host, new FakeAgentChannel { Quitting = () => new TaskCompletionSource().Task }, () => true, Fixture.Messages);
        shutdown.Start();
        host.Advance(60);
        host.Advance(0.5);
        shutdown.Dispose();
        host.Advance(5);
        Assert.DoesNotContain("quit", host.CommandNames);
        Assert.DoesNotContain(host.Timers, t => !t.Fired && !t.Cancelled);
    }
}
