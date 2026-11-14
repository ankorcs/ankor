using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public sealed class OwnedTimerTests
{
    [Fact]
    public void ReplacementAndRearmingRejectPreviouslySelectedCallbacks()
    {
        var host = new FakeGameHost();
        using var timer = new OwnedTimer(host);
        var calls = 0;
        timer.Schedule(1, () => calls += 100);
        var replaced = Assert.Single(host.Timers);
        timer.Schedule(2, () =>
        {
            Assert.False(timer.IsPending);
            calls++;
            timer.Schedule(3, () => calls++);
        });
        var current = Assert.Single(host.Timers);
        replaced.Callback();
        Assert.True(timer.IsPending);
        Assert.Equal(0, calls);
        host.Advance(2);
        Assert.Equal(1, calls);
        current.Callback();
        Assert.True(timer.IsPending);
        Assert.Single(host.Timers);
        host.Advance(3);
        Assert.Equal(2, calls);
        Assert.False(timer.IsPending);
        Assert.Empty(host.Timers);
    }

    [Fact]
    public void ARepeatingSlotKeepsItsCadenceUntilItsCallbackCancelsIt()
    {
        var host = new FakeGameHost();
        using var timer = new OwnedTimer(host);
        var at = new List<double>();
        timer.ScheduleRepeating(1, 2, () =>
        {
            at.Add(host.CurrentTime);
            if (at.Count == 3) timer.Cancel();
        });

        host.Advance(20);

        Assert.Equal([1d, 3d, 5d], at);
        Assert.False(timer.IsPending);
        Assert.Empty(host.Timers);
    }

    [Fact]
    public void ReplacingOrDisposingARepeatingSlotStopsIt()
    {
        var host = new FakeGameHost();
        var timer = new OwnedTimer(host);
        var calls = 0;
        timer.ScheduleRepeating(1, 1, () => calls += 100);
        var replaced = Assert.Single(host.Timers);
        timer.ScheduleRepeating(1, 1, () => calls++);

        replaced.Callback();
        host.Advance(3);
        Assert.Equal(3, calls);

        timer.Dispose();
        host.Advance(3);
        Assert.Equal(3, calls);
        Assert.Empty(host.Timers);
    }

    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public void FrameworkMapCancellationAppliesToAlreadySelectedCallbacks(bool stopOnMapChange)
    {
        var host = new FakeGameHost();
        using var timer = new OwnedTimer(host, stopOnMapChange);
        var calls = 0;
        timer.Schedule(1, () => calls++);
        var selected = Assert.Single(host.Timers);
        host.ChangeMap("de_nuke");
        Assert.Equal(!stopOnMapChange, timer.IsPending);
        selected.Callback();
        Assert.Equal(stopOnMapChange ? 0 : 1, calls);
        Assert.False(timer.IsPending);
    }

    [Fact]
    public void DisposalInsideCallbackPreventsRearming()
    {
        var host = new FakeGameHost();
        var timer = new OwnedTimer(host);
        timer.Schedule(1, () =>
        {
            timer.Dispose();
            timer.Schedule(1, () => Assert.Fail("Disposed owner ran"));
        });
        host.Advance(2);
        timer.Dispose();
        Assert.False(timer.IsPending);
        Assert.Empty(host.Timers);
    }

    [Fact]
    public void CancelledAndDisposedCallbacksCannotRun()
    {
        var host = new FakeGameHost();
        var timer = new OwnedTimer(host);
        timer.Schedule(1, () => Assert.Fail("Cancelled owner ran"));
        var cancelled = Assert.Single(host.Timers);
        timer.Cancel();
        timer.Cancel();
        cancelled.Callback();
        timer.Schedule(1, () => Assert.Fail("Disposed owner ran"));
        var disposed = Assert.Single(host.Timers);
        timer.Dispose();
        disposed.Callback();
        Assert.Empty(host.Timers);
    }

    [Fact]
    public void ThrowingCallbackStillConsumesItsSlot()
    {
        var host = new FakeGameHost();
        using var timer = new OwnedTimer(host);
        timer.Schedule(1, () => throw new InvalidOperationException());
        var selected = Assert.Single(host.Timers);
        Assert.Throws<InvalidOperationException>(() => selected.Callback());
        selected.Callback();
        Assert.False(timer.IsPending);
        Assert.Empty(host.Timers);
    }
}
