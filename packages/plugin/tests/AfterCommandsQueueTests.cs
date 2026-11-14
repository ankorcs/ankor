using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public class AfterCommandsQueueTests
{
    private sealed class Harness
    {
        public readonly FakeGameHost Host = new();
        public readonly List<string> Executed = [];
        public readonly List<int> Applied = [];
        public readonly List<int> TimedOut = [];
        public readonly AfterCommandsQueue Queue;

        public Harness() => Queue = new AfterCommandsQueue(Executed.Add,
            (seconds, callback) => Host.AddTimer(seconds, callback), Applied.Add, TimedOut.Add);
    }

    [Fact]
    public void EachActionExecsTheMarkerCfg()
    {
        var harness = new Harness();
        harness.Queue.Add(() => { });
        harness.Queue.Add(() => { });

        Assert.Equal(["exec ankor/applied", "exec ankor/applied"], harness.Executed);
    }

    [Fact]
    public void MarkersSettleTheActionsInTheOrderTheyWereQueued()
    {
        var harness = new Harness();
        var runs = new List<string>();
        harness.Queue.Add(() => runs.Add("first"));
        harness.Queue.Add(() => runs.Add("second"));

        harness.Queue.Applied();
        Assert.Equal(["first"], runs);
        harness.Queue.Applied();
        harness.Queue.Applied();
        harness.Host.Advance(AfterCommandsQueue.TimeoutSeconds);

        Assert.Equal(["first", "second"], runs);
        Assert.Equal([1, 2], harness.Applied);
        Assert.Empty(harness.TimedOut);
    }

    [Fact]
    public void AnActionWhoseMarkerNeverRunsRunsOnceAfterTheTimeout()
    {
        var harness = new Harness();
        var runs = 0;
        harness.Queue.Add(() => runs++);

        harness.Host.Advance(AfterCommandsQueue.TimeoutSeconds - 0.1);
        Assert.Equal(0, runs);
        harness.Host.Advance(0.1);

        Assert.Equal(1, runs);
        Assert.Equal([1], harness.TimedOut);
    }

    [Fact]
    public void ALateMarkerSettlesTheActionThatTimedOutAndNotTheNext()
    {
        var harness = new Harness();
        var runs = new List<string>();
        harness.Queue.Add(() => runs.Add("late"));
        harness.Host.Advance(AfterCommandsQueue.TimeoutSeconds);
        harness.Queue.Add(() => runs.Add("next"));

        harness.Queue.Applied();
        Assert.Equal(["late"], runs);
        harness.Queue.Applied();

        Assert.Equal(["late", "next"], runs);
        Assert.Equal([2], harness.Applied);
    }

    [Fact]
    public void AMarkerWithNothingQueuedIsNotKeptForTheNextAction()
    {
        var harness = new Harness();
        harness.Queue.Applied();

        var runs = 0;
        harness.Queue.Add(() => runs++);
        Assert.Equal(0, runs);

        harness.Queue.Applied();
        Assert.Equal(1, runs);
    }

    [Fact]
    public void ClearDropsWaitingActionsAndTheirTimeouts()
    {
        var harness = new Harness();
        var runs = 0;
        harness.Queue.Add(() => runs++);

        harness.Queue.Clear();
        harness.Queue.Applied();
        harness.Host.Advance(AfterCommandsQueue.TimeoutSeconds);

        Assert.Equal(0, runs);
        Assert.Empty(harness.TimedOut);
    }
}
