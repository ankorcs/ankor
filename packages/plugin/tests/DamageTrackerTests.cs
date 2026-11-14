using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class DamageTrackerTests
{
    private static (DamageTracker Damage, EventForwarder Forwarder, MatchState Match,
        FakeGameHost Host, List<PluginMessage> Sent) Build()
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        var match = new MatchState(host, Fixture.Config(SidesMode.Team1Ct),
            new FakeAgentChannel(sent), Fixture.Messages, () => { });
        var forwarder = new EventForwarder(match, host);
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0) with { Cash = 800, InBuyZone = true },
            Fixture.Player(Fixture.Team2Player1, Side.T, 1),
        ]);
        Assert.Null(match.StartFromController());
        host.Advance(14);
        host.IsWarmup = false;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        return (new DamageTracker(forwarder), forwarder, match, host, sent);
    }

    private static DamageSnapshot Snapshot(int health = 100) => new(health, 50,
        new WeaponReference(44, null), 1200,
        [new InventoryItem { weapon = 43, quantity = 2, unitValue = 200 }]);

    [Theory]
    [InlineData(1)]
    [InlineData(2)]
    public void NestedDamageRestoresTheOuterSnapshotAndReleasesBothFrames(int innerEntity)
    {
        var (damage, _, _, _, _) = Build();
        var outer = Snapshot();
        var inner = Snapshot(40) with { Inflictor = 60, GrenadesHeld = null };
        damage.Enter(1);
        damage.Capture(outer, null);
        damage.Enter(innerEntity);
        damage.Capture(inner, null);

        Assert.Equal(inner, damage.Current(innerEntity));
        if (innerEntity != 1) Assert.Equal(outer, damage.Current(1));
        damage.Exit();
        Assert.Equal(outer, damage.Current(1));
        damage.Exit();
        Assert.Null(damage.Current(1));
        Assert.Null(damage.Current(innerEntity));
    }

    [Fact]
    public void AReportedInnerHitIsTheHealthTheOuterHitTakesFrom()
    {
        var (damage, _, _, _, _) = Build();
        damage.Enter(1);
        damage.Capture(Snapshot(), null);
        damage.Enter(2);
        damage.Capture(Snapshot(80), null);
        damage.Enter(1);
        damage.Capture(Snapshot(), null);
        damage.Hurt(1, 70);
        damage.Exit();

        Assert.Equal(70, damage.Current(1)?.Health);
        Assert.Equal(80, damage.Current(2)?.Health);
        damage.Exit();
        Assert.Equal(70, damage.Current(1)?.Health);
        damage.Exit();
    }

    [Fact]
    public void SkippedInnerSnapshotCannotExposeOrRemoveTheOuterSnapshot()
    {
        var (damage, _, _, _, _) = Build();
        var outer = Snapshot();
        damage.Enter(1);
        damage.Capture(outer, null);
        damage.Enter(1);
        Assert.Null(damage.Current(1));
        damage.Exit();
        Assert.Equal(outer, damage.Current(1));
        damage.Exit();
        Assert.Null(damage.Current(1));
    }

    [Fact]
    public void CompletionWaitsForSnapshotlessCallsAndAllNestedDamage()
    {
        var (damage, _, _, _, _) = Build();
        var calls = new List<string>();
        damage.Enter(99);
        damage.Enter(1);
        damage.Capture(Snapshot(), null);
        damage.RunAfterDamage(() =>
        {
            Assert.Null(damage.Current(1));
            calls.Add("round summaries");
        });
        calls.Add("nested death");
        damage.Exit();
        damage.RunAfterDamage(() => calls.Add("map end"));
        Assert.Equal(["nested death"], calls);
        damage.Exit();
        damage.Exit();
        damage.RunAfterDamage(() => calls.Add("outside damage"));
        Assert.Equal(["nested death", "round summaries", "map end", "outside damage"], calls);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void ExitOrResetReleasesBuySamplesBeforeLaterRoundHandling(bool reset)
    {
        var (damage, forwarder, _, host, sent) = Build();
        damage.Enter(1);
        damage.Capture(Snapshot(), host.PlayerList[0]);
        damage.Enter(1);
        damage.Capture(Snapshot(40), host.PlayerList[0] with { Cash = 400 });
        host.PlayerList[0] = host.PlayerList[0] with { Cash = 200 };
        var completed = false;
        damage.RunAfterDamage(() => completed = true);
        if (reset)
        {
            damage.Reset();
            damage.Exit();
        }
        else
        {
            damage.Exit();
            damage.Exit();
        }

        Assert.Equal(!reset, completed);
        Assert.Null(damage.Current(1));
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>()).players.Single(p => p.player.team == 1);
        Assert.Equal(200, buy.cash);
        damage.RunAfterDamage(() => completed = true);
        Assert.True(completed);
    }

    [Fact]
    public void NestedDamageKeepsTheOuterBuySampleForAPrematureRoundEnd()
    {
        var (damage, forwarder, _, host, sent) = Build();
        damage.Enter(1);
        damage.Capture(Snapshot(), host.PlayerList[0]);
        damage.Enter(1);
        damage.Capture(Snapshot(40), host.PlayerList[0] with { Cash = 400 });
        damage.Exit();
        host.PlayerList[0] = host.PlayerList[0] with { Cash = 0, IsAlive = false };
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>()).players.Single(p => p.player.team == 1);
        Assert.Equal(800, buy.cash);
        damage.Exit();
    }

    [Fact]
    public void ARejectedBuySampleCannotPopAnOuterPlayersBuySample()
    {
        var (damage, forwarder, _, host, sent) = Build();
        damage.Enter(1);
        damage.Capture(Snapshot(), host.PlayerList[0]);
        damage.Enter(1);
        damage.Capture(Snapshot(40), host.PlayerList[0] with { Side = Side.Spec, Cash = 400 });
        damage.Exit();
        host.PlayerList[0] = host.PlayerList[0] with { Cash = 0 };
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        Assert.Equal(800, Assert.Single(sent.OfType<RoundBuyEnd>()).players.Single(p => p.player.team == 1).cash);
        damage.Exit();
    }

    [Fact]
    public void AThrowingCompletionCannotRetainTheCompletedDamageSnapshot()
    {
        var (damage, _, _, _, _) = Build();
        damage.Enter(1);
        damage.Capture(Snapshot(), null);
        damage.RunAfterDamage(() => throw new InvalidOperationException("completion failed"));
        Assert.Throws<InvalidOperationException>(damage.Exit);
        Assert.Null(damage.Current(1));
        var completed = false;
        damage.RunAfterDamage(() => completed = true);
        Assert.True(completed);
    }
}
