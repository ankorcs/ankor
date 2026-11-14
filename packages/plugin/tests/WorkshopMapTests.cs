using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class WorkshopMapTests
{
    private const string AimRedline = "3744399267";

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void ANewMatchOrUnloadCancelsTheLaunchTimeWorkshopCommand(bool alreadyQueued)
    {
        var host = new FakeGameHost { DeferNextFrames = true };
        var loader = new WorkshopMapLoader(host, AimRedline);
        if (alreadyQueued) loader.OnSteamApiActivated();
        loader.Cancel();
        host.RunNextFrame();
        Assert.False(loader.OnSteamApiActivated());
        Assert.Empty(host.Commands);
        Assert.Empty(host.Logs);
    }

    [Fact]
    public void SteamActivationHostsTheWorkshopMapExactlyOnce()
    {
        var host = new FakeGameHost();
        var loader = new WorkshopMapLoader(host, AimRedline);

        Assert.Equal(AimRedline, loader.WorkshopId);
        Assert.True(loader.OnSteamApiActivated());
        Assert.Equal([$"host_workshop_map {AimRedline}"], host.CommandNames.ToArray());
        Assert.Contains((LogLevel.Info, $"switching to workshop map {AimRedline}"), host.Logs);

        // Steam can activate again on a later map; the plugin must not bounce the server back.
        Assert.False(loader.OnSteamApiActivated());
        Assert.False(loader.OnSteamApiActivated());
        Assert.Single(host.CommandNames);
    }

    [Fact]
    public void TheMapCommandRunsOnTheNextFrame()
    {
        var host = new FakeGameHost { DeferNextFrames = true };
        var loader = new WorkshopMapLoader(host, AimRedline);

        Assert.True(loader.OnSteamApiActivated());
        Assert.Empty(host.CommandNames);
        Assert.Empty(host.Logs);

        host.RunNextFrame();
        Assert.Equal([$"host_workshop_map {AimRedline}"], host.CommandNames.ToArray());
        Assert.Contains((LogLevel.Info, $"switching to workshop map {AimRedline}"), host.Logs);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    public void AStockMapContainerHostsNothingAndSaysNothing(string? value)
    {
        var host = new FakeGameHost();
        var loader = new WorkshopMapLoader(host, value);

        Assert.Null(loader.WorkshopId);
        Assert.False(loader.OnSteamApiActivated());
        Assert.Empty(host.CommandNames);
        Assert.Empty(host.Logs);
    }
}
