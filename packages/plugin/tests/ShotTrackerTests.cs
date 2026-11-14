using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class ShotTrackerTests
{
    private const string Ct1 = "76561198000000001";
    private const string Ct2 = "76561198000000002";

    private static PlayerRef Player(string id) => new() { steamId = id, name = "shooter" };

    [Fact]
    public void ARoundsShotsGoOutAsOneMessageByPlayerAndWeapon()
    {
        var shots = new ShotTracker();
        shots.Begin(3);
        shots.Record(Player(Ct1), new WeaponReference(7, null), 3, 640.125);
        shots.Record(Player(Ct1), new WeaponReference(7, null), 3, 640.25);
        shots.Record(Player(Ct1), new WeaponReference(60, null), 3, 640.125);
        shots.Record(Player(Ct2), new WeaponReference(7, null), 3, 640.125);

        var message = Assert.Single(shots.Flush());
        Assert.Equal(3, message.round);
        Assert.Equal([Ct1, Ct2], message.players.Select(p => p.player.steamId));
        var ct1 = message.players[0].weapons;
        Assert.Equal([(7, 2), (60, 1)], ct1.Select(w => (w.weapon.id!.Value, w.shots)));
        Assert.Equal([640.125, 640.25], ct1[0].ticks);
        Assert.Equal(1, Assert.Single(message.players[1].weapons).shots);
    }

    [Fact]
    public void ARoundNobodyFiredInGoesOutWithNoPlayers()
    {
        var shots = new ShotTracker();
        shots.Begin(2);

        var message = Assert.Single(shots.Flush());
        Assert.Equal(2, message.round);
        Assert.Empty(message.players);
    }

    [Fact]
    public void APlayersShotsStopAtTheContractsLimitOverAllTheirWeapons()
    {
        var shots = new ShotTracker();
        for (var i = 0; i < RoundShots.MaxShots - 1; i++) shots.Record(Player(Ct1), new WeaponReference(7, null), 3, 640 + i);
        shots.Record(Player(Ct1), new WeaponReference(60, null), 3, 9000);
        shots.Record(Player(Ct1), new WeaponReference(60, null), 3, 9001);
        shots.Record(Player(Ct2), new WeaponReference(7, null), 3, 9002);

        var players = Assert.Single(shots.Flush()).players;
        Assert.Equal(RoundShots.MaxShots, players[0].weapons.Sum(w => w.shots));
        Assert.Equal([9000.0], players[0].weapons.Single(w => w.weapon.id == 60).ticks);
        Assert.Equal(1, Assert.Single(players[1].weapons).shots);
    }

    [Fact]
    public void FallbackNamedWeaponsAreTheirOwnTally()
    {
        var shots = new ShotTracker();
        shots.Record(Player(Ct1), new WeaponReference(null, "taser"), 1, 640.125);
        shots.Record(Player(Ct1), new WeaponReference(null, "taser"), 1, 640.125);

        var weapon = Assert.Single(Assert.Single(Assert.Single(shots.Flush()).players).weapons);
        Assert.Null(weapon.weapon.id);
        Assert.Equal("taser", weapon.weapon.fallback);
        Assert.Equal(2, weapon.shots);
    }

    [Fact]
    public void TheFlushOrderIsRoundThenPlayerThenWeapon()
    {
        var shots = new ShotTracker();
        shots.Record(Player(Ct2), new WeaponReference(7, null), 2, 640.125);
        shots.Record(Player(Ct1), new WeaponReference(60, null), 2, 640.125);
        shots.Record(Player(Ct1), new WeaponReference(7, null), 2, 640.125);
        shots.Record(Player(Ct1), new WeaponReference(7, null), 1, 640.125);

        Assert.Equal(
            [(1, Ct1, 7), (2, Ct1, 7), (2, Ct1, 60), (2, Ct2, 7)],
            shots.Flush().SelectMany(m => m.players.SelectMany(p => p.weapons.Select(w => (m.round, p.player.steamId, w.weapon.id)))).ToArray());
    }

    [Fact]
    public void FlushAndClearBothLeaveTheTrackerEmpty()
    {
        var shots = new ShotTracker();
        shots.Begin(1);
        shots.Record(Player(Ct1), new WeaponReference(7, null), 1, 640.125);
        Assert.Single(shots.Flush());
        Assert.Empty(shots.Flush());

        shots.Begin(2);
        shots.Record(Player(Ct1), new WeaponReference(7, null), 2, 640.125);
        shots.Clear();
        Assert.Empty(shots.Flush());
    }
}
