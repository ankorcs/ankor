using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public class PendingFireTrackerTests
{
    [Fact]
    public void MissingOwnerMatchesByExactPositionAndCanOnlyAttachOnce()
    {
        var tracker = new PendingFireTracker();
        var at = new WorldPosition(10, 20, 30);
        tracker.Record(50, 100, null, at);
        Assert.Equal(50, tracker.Take(100, null, at));
        Assert.Null(tracker.Take(100, null, at));
    }

    [Fact]
    public void LiveOwnerStillMatchesWhenInfernoOriginDiffers()
    {
        var tracker = new PendingFireTracker();
        tracker.Record(50, 100, 7, new WorldPosition(10, 20, 30));
        Assert.Equal(50, tracker.Take(100, 7, new WorldPosition(11, 20, 30)));
    }

    [Fact]
    public void DifferentOriginsStaySeparateAndAmbiguousOrStaleSourcesAreNotGuessed()
    {
        var tracker = new PendingFireTracker();
        var first = new WorldPosition(10, 20, 30);
        var second = new WorldPosition(100, 200, 300);
        tracker.Record(50, 100, null, first);
        tracker.Record(51, 100, null, second);
        Assert.Equal(51, tracker.Take(100, null, second));
        tracker.Record(52, 100, null, first);
        Assert.Null(tracker.Take(100, null, first));
        tracker.Remove(52);
        Assert.Null(tracker.Take(101, null, first));
        tracker.Record(53, 101, null, first);
        tracker.Clear();
        Assert.Null(tracker.Take(101, null, first));
    }
}
