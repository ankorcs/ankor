using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class ServerReadinessTests
{
    private const string AimRedline = "3744399267";

    private sealed class Harness
    {
        public FakeGameHost Host { get; } = new();
        public WorkshopMapLoader Workshop { get; }
        public ServerReadiness Readiness { get; }
        public int Pushed { get; private set; }

        public Harness(string? workshopId = null)
        {
            Workshop = new WorkshopMapLoader(Host, workshopId);
            Readiness = new ServerReadiness(Host, Workshop, "de_mirage", () => Pushed++, message => Host.Log(LogLevel.Info, message));
        }

        public void Activate()
        {
            Workshop.OnSteamApiActivated();
            Readiness.OnSteamApiActivated();
        }

        public string[] InfoLogs => Host.Logs.Where(log => log.Level == LogLevel.Info).Select(log => log.Message).ToArray();
    }

    [Fact]
    public void AStockMapIsNotReadyBeforeTheSteamLogonAndReadyOnceLoggedOn()
    {
        var harness = new Harness();
        harness.Readiness.OnMapLoad("de_mirage");
        harness.Activate();
        harness.Host.Advance(5);

        Assert.False(harness.Readiness.Ready);
        Assert.Equal(0, harness.Pushed);
        Assert.Equal(["map de_mirage loaded; waiting for the Steam logon"], harness.InfoLogs);

        harness.Host.SteamLoggedOn = true;
        harness.Host.Advance(1);

        Assert.True(harness.Readiness.Ready);
        Assert.Equal(1, harness.Pushed);
        Assert.Empty(harness.Host.Timers);
        Assert.Equal("game server ready: logged on to Steam, map de_mirage loaded", harness.InfoLogs[^1]);

        harness.Readiness.OnMapLoad("de_nuke");
        harness.Readiness.OnSteamApiActivated();
        harness.Host.Advance(5);
        Assert.Equal(1, harness.Pushed);
        Assert.Equal(2, harness.InfoLogs.Length);
    }

    [Fact]
    public void AStockMapLoggedOnBeforeItsMapLoadsWaitsForTheMap()
    {
        var harness = new Harness { Host = { SteamLoggedOn = true } };
        harness.Activate();
        harness.Host.Advance(3);

        Assert.False(harness.Readiness.Ready);
        Assert.Equal(["logged on to Steam; waiting for map de_mirage"], harness.InfoLogs);

        harness.Readiness.OnMapLoad("de_mirage");

        Assert.True(harness.Readiness.Ready);
        Assert.Equal(1, harness.Pushed);
        Assert.Empty(harness.Host.Timers);
    }

    [Fact]
    public void NothingIsCheckedBeforeTheSteamApiIsUp()
    {
        var harness = new Harness { Host = { SteamLoggedOn = true } };
        harness.Readiness.OnMapLoad("de_mirage");
        harness.Host.Advance(10);

        Assert.False(harness.Readiness.Ready);
        Assert.Empty(harness.Host.Timers);
        Assert.Empty(harness.Host.Logs);

        harness.Readiness.OnSteamApiActivated();
        // The first check waits a tick past the activation callback.
        Assert.False(harness.Readiness.Ready);
        harness.Host.Advance(1);
        Assert.True(harness.Readiness.Ready);
    }

    [Fact]
    public void AWorkshopLaunchWaitsForTheWorkshopMapAfterTheBootMap()
    {
        var harness = new Harness(AimRedline) { Host = { SteamLoggedOn = true, DeferNextFrames = true } };
        harness.Readiness.OnMapLoad("de_dust2");
        harness.Activate();
        harness.Host.Advance(3);

        Assert.False(harness.Readiness.Ready);
        Assert.Equal([$"logged on to Steam; waiting for workshop map {AimRedline}"], harness.InfoLogs);

        harness.Host.RunNextFrame();
        harness.Host.ChangeMap("aim_redline");
        harness.Host.Advance(3);
        Assert.False(harness.Readiness.Ready);

        harness.Readiness.OnMapLoad("aim_redline");

        Assert.True(harness.Readiness.Ready);
        Assert.Equal(1, harness.Pushed);
        Assert.Empty(harness.Host.Timers);
        Assert.Equal("game server ready: logged on to Steam, map aim_redline loaded", harness.InfoLogs[^1]);
    }

    [Fact]
    public void DisposingStopsTheCheck()
    {
        var harness = new Harness();
        harness.Activate();
        Assert.Single(harness.Host.Timers);

        harness.Readiness.Dispose();
        Assert.Empty(harness.Host.Timers);

        harness.Host.SteamLoggedOn = true;
        harness.Readiness.OnMapLoad("de_mirage");
        harness.Host.Advance(5);
        Assert.False(harness.Readiness.Ready);
        Assert.Equal(0, harness.Pushed);
    }
}
