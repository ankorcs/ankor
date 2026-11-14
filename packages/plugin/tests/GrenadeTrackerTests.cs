using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class GrenadeTrackerTests
{
    private static PlayerAt Player(string id) => new() { steamId = id };

    private static void Throw(GrenadeTracker tracker, int entityId, GrenadeKind kind = GrenadeKind.HeGrenade,
        PlayerAt? thrower = null, double thrownRoundTime = 0) =>
        tracker.Throw(entityId, 1, thrownRoundTime, new GrenadeThrow { player = thrower ?? Player("76561198000000001"), grenade = kind });

    private static GrenadeEnd[] Ends(List<PluginMessage> events) => [.. events.OfType<GrenadeEnd>()];

    [Fact]
    public void OwnerReferencesFollowEffectLifetimeAndNeverLeakAcrossEntityReuse()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        var first = Player("76561198000000001");
        var second = Player("76561198000000002");
        Throw(tracker, 10, GrenadeKind.Molotov, first);
        tracker.AttachFire(10, 20);
        tracker.RemoveEntity(10, 1);
        Assert.Null(tracker.Owner(10));
        Assert.Same(first, tracker.Owner(20));
        tracker.RemoveEntity(20, 2);
        Assert.Null(tracker.Owner(20));
        Assert.Single(Ends(events));
        Throw(tracker, 10, thrower: first);
        Throw(tracker, 10, thrower: second);
        Assert.Same(second, tracker.Owner(10));
        tracker.Flush(3);
        Assert.Null(tracker.Owner(10));
        Throw(tracker, 30, thrower: first);
        tracker.Flush(4);
        Assert.Null(tracker.Owner(30));
    }

    [Fact]
    public void FireEventsForAnUntrackedEntityAreIgnored()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);

        tracker.AttachFire(10, 20);
        tracker.Expire(20, 1);
        tracker.Smother(20, 1, new WorldPosition(0, 0, 0));
        tracker.Flush(2);

        Assert.Empty(events);
        Assert.Null(tracker.Owner(20));
    }

    [Fact]
    public void IdsCountUpPerTrackerAndSurviveRounds()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 10);
        tracker.Flush(5);
        Throw(tracker, 10);
        tracker.Flush(5);
        Throw(tracker, 11);
        tracker.Flush(5);
        Assert.Equal([0L, 1L, 2L], Ends(events).Select(end => end.grenadeId));
    }

    [Fact]
    public void AFlushOnARestartedClockNeverEndsBeforeTheThrow()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        tracker.Throw(10, 1, 30, new GrenadeThrow { grenade = GrenadeKind.HeGrenade });
        tracker.Flush(0);
        Assert.Equal(30, Ends(events).Single().endRoundTime);
    }

    [Fact]
    public void TheThrowGoesOutWhenThrownAndPrecedesEverythingThatReferencesIt()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 10, GrenadeKind.IncGrenade);
        var thrown = Assert.IsType<GrenadeThrow>(Assert.Single(events));
        Assert.Equal((0L, GrenadeKind.IncGrenade), (thrown.grenadeId, thrown.grenade));

        Throw(tracker, 11, GrenadeKind.Flashbang);
        Assert.Equal(1L, tracker.Reference(11));
        Assert.Null(tracker.Reference(99));

        Throw(tracker, 12, GrenadeKind.Decoy);
        tracker.Detonate(12, new WorldPosition(0, 0, 0));
        tracker.RemoveEntity(12, 4);
        Assert.Equal(["grenade_throw", "grenade_throw", "grenade_throw", "grenade_detonate", "grenade_end"],
            events.Select(message => message.type));
    }

    [Fact]
    public void ADetonationIsSentOnceWithItsPositionAndAnUndetonatedProjectileSendsNone()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 10, GrenadeKind.Molotov);
        Assert.True(tracker.IsProjectileImpact(10));
        tracker.Detonate(10, new WorldPosition(1, 2, 3));
        Assert.False(tracker.IsProjectileImpact(10));
        tracker.AttachFire(10, 20);
        tracker.RemoveEntity(10, 0);
        tracker.RemoveEntity(20, 0);
        // A fire that lit without a detonation report has no position.
        Throw(tracker, 11, GrenadeKind.IncGrenade);
        tracker.AttachFire(11, 21);
        tracker.Flush(5);
        // Removed without detonating.
        Throw(tracker, 12, GrenadeKind.Flashbang);
        tracker.RemoveEntity(12, 0);
        var detonations = events.OfType<GrenadeDetonate>().ToArray();
        Assert.Equal([0L, 1L], detonations.Select(d => d.grenadeId));
        Assert.Equal((1f, 2f, 3f), (detonations[0].detonatePos!.x, detonations[0].detonatePos!.y, detonations[0].detonatePos!.z));
        Assert.Null(detonations[1].detonatePos);
        Assert.Equal(3, Ends(events).Length);
    }

    [Fact]
    public void OverlappingFiresFromOneThrowerKeepTheirOwnIds()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 10, GrenadeKind.Molotov);
        Throw(tracker, 11, GrenadeKind.Molotov);
        tracker.AttachFire(11, 21);
        tracker.AttachFire(10, 20);
        tracker.RemoveEntity(10, 0);
        tracker.RemoveEntity(11, 0);
        Assert.Empty(Ends(events));
        Assert.Equal(0L, tracker.Reference(20));
        Assert.Equal(1L, tracker.Reference(21));
        tracker.RemoveEntity(20, 0);
        Assert.Equal(0L, Assert.Single(Ends(events)).grenadeId);
        tracker.Flush(30);
        Assert.Equal(1L, Ends(events)[1].grenadeId);
    }

    [Fact]
    public void AFirePutOutByASmokeNamesThatSmokeAndSaysHowItEnded()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 30, GrenadeKind.SmokeGrenade);
        tracker.Detonate(30, new WorldPosition(100, 100, 0));
        Throw(tracker, 31, GrenadeKind.SmokeGrenade);
        tracker.Detonate(31, new WorldPosition(900, 900, 0)); // far away, must not be credited
        Throw(tracker, 10, GrenadeKind.Molotov);
        tracker.Detonate(10, new WorldPosition(120, 110, 0));
        tracker.AttachFire(10, 20);
        tracker.RemoveEntity(10, 6);
        Assert.True(tracker.Extinguish(20, 6.5, new WorldPosition(125, 112, 0)));
        // The engine keeps the dead inferno until the round resets; the fire ended when it was put out.
        tracker.RemoveEntity(20, 0, 67);
        tracker.RemoveEntity(30, 20);
        tracker.RemoveEntity(31, 21);
        var ends = Ends(events);
        Assert.Equal((2L, FireEnd.Extinguished, 0L, 6.5), (ends[0].grenadeId, ends[0].fireEnd, ends[0].extinguishedBy, ends[0].endRoundTime));
        Assert.Equal((0L, null, null, 20.0), (ends[1].grenadeId, ends[1].fireEnd, ends[1].extinguishedBy, ends[1].endRoundTime));
        Assert.Equal((1L, null, null, 21.0), (ends[2].grenadeId, ends[2].fireEnd, ends[2].extinguishedBy, ends[2].endRoundTime));
    }

    [Fact]
    public void AFireLitInsideASmokeIsSmotheredWhenItGoesAndStaysSoAfterAnExtinguishReport()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 30, GrenadeKind.SmokeGrenade);
        tracker.Detonate(30, new WorldPosition(0, 0, 0));
        Throw(tracker, 10, GrenadeKind.IncGrenade);
        tracker.Detonate(10, new WorldPosition(10, 0, 0));
        tracker.AttachFire(10, 20);
        tracker.RemoveEntity(10, 6);
        tracker.Smother(20, 6, new WorldPosition(10, 0, 0));
        Assert.True(tracker.Extinguish(20, 6, new WorldPosition(10, 0, 0)));
        tracker.RemoveEntity(20, 6);
        tracker.RemoveEntity(30, 20);
        var fire = Ends(events)[0];
        Assert.Equal((FireEnd.Smothered, 0L, 6.0), (fire.fireEnd, fire.extinguishedBy, fire.endRoundTime));
    }

    [Fact]
    public void AFireThatBurnedOutEndsAtItsExpiryNotAtTheEntityRemoval()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 10, GrenadeKind.Molotov, thrownRoundTime: 10);
        tracker.Detonate(10, new WorldPosition(0, 0, 0));
        tracker.AttachFire(10, 20);
        tracker.RemoveEntity(10, 10.5);
        tracker.Expire(20, 17.5);
        tracker.RemoveEntity(20, 0, 70);
        Assert.Equal((FireEnd.Expired, 17.5), (Ends(events)[0].fireEnd, Ends(events)[0].endRoundTime));
    }

    [Fact]
    public void AnExtinguishWithNoLiveSmokeReportsFalseAndAnUntrackedFireReportsNull()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 10, GrenadeKind.Molotov);
        tracker.Detonate(10, new WorldPosition(0, 0, 0));
        tracker.AttachFire(10, 20);
        Assert.False(tracker.Extinguish(20, 8, new WorldPosition(0, 0, 0)));
        Assert.Null(tracker.Extinguish(99, 8, new WorldPosition(0, 0, 0)));
        // A fire that burned out on its own, and a projectile flushed by the round closing.
        tracker.RemoveEntity(10, 6);
        tracker.RemoveEntity(20, 13);
        Throw(tracker, 11, GrenadeKind.Molotov);
        tracker.Flush(30);
        var ends = Ends(events);
        Assert.Equal((FireEnd.Extinguished, null, 8.0), (ends[0].fireEnd, ends[0].extinguishedBy, ends[0].endRoundTime));
        Assert.Equal((null, 30.0), (ends[1].fireEnd, ends[1].endRoundTime));
    }

    [Fact]
    public void ARemovalAfterTheRoundClockResetEndsTheEffectWhenTheRoundClosed()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 10, GrenadeKind.Molotov, thrownRoundTime: 117);
        tracker.Detonate(10, new WorldPosition(0, 0, 0));
        tracker.AttachFire(10, 20);
        // Burned through the restart delay: the projectile went while the clock still ran.
        tracker.RemoveEntity(10, 118, 122);
        // The fire was cleared by the reset; the new round's clock reads 0.
        tracker.RemoveEntity(20, 0, 122);
        var end = Assert.Single(Ends(events));
        Assert.Equal((122.0, 1), (end.endRoundTime, end.round));
        Throw(tracker, 11, GrenadeKind.Flashbang, thrownRoundTime: 5);
        tracker.RemoveEntity(11, 6.5, 122);
        Assert.Equal(6.5, Ends(events)[1].endRoundTime);
    }

    [Fact]
    public void AReusedEntityGetsANewIdAndEndsTheGrenadeItReplaced()
    {
        var events = new List<PluginMessage>();
        var tracker = new GrenadeTracker(events.Add);
        Throw(tracker, 10, GrenadeKind.Flashbang);
        tracker.Detonate(10, new WorldPosition(0, 0, 0));
        Assert.Equal(0L, tracker.Reference(10));
        Throw(tracker, 10, GrenadeKind.Flashbang);
        Assert.Equal(0L, Assert.Single(Ends(events)).grenadeId);
        Assert.Equal(1L, tracker.Reference(10));
    }
}
