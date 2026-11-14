using Xunit;

namespace Ankor.Tests;

public class FakeGameHostTests
{
    [Fact]
    public void MapChangeCancelsMapTimersAndPreservesRemainingDelayOfOtherTimers()
    {
        var host = new FakeGameHost { Clock = 100 };
        var calls = new List<string>();
        host.AddTimer(10, () => calls.Add("map"));
        host.AddTimer(10, () => calls.Add("persistent"), stopOnMapChange: false);
        host.Advance(4);
        var wallTime = host.NowMs;
        host.ChangeMap("de_nuke");
        Assert.Equal(0, host.CurrentTime);
        Assert.Equal(wallTime, host.NowMs);
        host.Advance(5);
        Assert.Empty(calls);
        host.Advance(1);
        Assert.Equal(["persistent"], calls);
    }

    [Fact]
    public void PersistentTimerCanStillBeExplicitlyCancelled()
    {
        var host = new FakeGameHost();
        var fired = false;
        var timer = host.AddTimer(1, () => fired = true, stopOnMapChange: false);
        host.ChangeMap("de_nuke");
        timer.Cancel();
        host.Advance(2);
        Assert.False(fired);
    }

    [Fact]
    public void AdvancingRunsChainedTimersAtTheirDeadlinesBeforeReachingTheTarget()
    {
        var host = new FakeGameHost { Clock = 10, NowMs = 10_000 };
        var calls = new List<(double, long)>();
        host.AddTimer(2, () =>
        {
            calls.Add((host.Clock, host.NowMs));
            host.AddTimer(1, () => calls.Add((host.Clock, host.NowMs)));
        });

        host.Advance(5);

        Assert.Equal([(12d, 12_000L), (13d, 13_000L)], calls);
        Assert.Equal(15, host.Clock);
        Assert.Equal(15_000, host.NowMs);
        host.Advance(5);
        Assert.Equal(2, calls.Count);
    }

    [Fact]
    public void ACallbackCanCancelAnotherTimerDueInTheSameAdvance()
    {
        var host = new FakeGameHost();
        var calls = new List<string>();
        var cancelled = host.AddTimer(2, () => calls.Add("cancelled"));
        host.AddTimer(1, cancelled.Cancel);
        host.AddTimer(2, () => calls.Add("first"));
        host.AddTimer(2, () => calls.Add("second"));

        host.Advance(3);

        Assert.Equal(["first", "second"], calls);
    }
}
