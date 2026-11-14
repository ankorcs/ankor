using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class EventForwarderTests
{
    private static (MatchState Match, EventForwarder Forwarder, FakeGameHost Host, List<PluginMessage> Sent) Build()
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        var config = Fixture.Config(sides: SidesMode.Team1Ct);
        var match = new MatchState(host, config, new FakeAgentChannel(sent), Fixture.Messages, () => { });
        return (match, new EventForwarder(match, host), host, sent);
    }

    /// <summary>Fixed sides: the match goes live as soon as the roster is complete.</summary>
    private static void StartLive(MatchState match, FakeGameHost host)
    {
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0, position: new WorldPosition(0, 0, 0), angles: new ViewAngles(0, 0)),
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 1, position: new WorldPosition(0, 0, 0), angles: new ViewAngles(0, 0)),
            Fixture.Player(Fixture.Team2Player1, Side.T, 2, position: new WorldPosition(0, 0, 0), angles: new ViewAngles(0, 0)),
            Fixture.Player(Fixture.Team2Player2, Side.T, 3, position: new WorldPosition(0, 0, 0), angles: new ViewAngles(0, 0)),
        ]);
        match.MaybeStart();
        host.Advance(14);
    }

    /// <summary>Deliver the engine round-start callback after leaving warmup.</summary>
    private static void GoLive(MatchState match, FakeGameHost host)
    {
        StartLive(match, host);
        host.IsWarmup = false;
        match.OnRoundStart();
    }

    /// <summary>Start a round and end its freeze time, which opens the buy-time clock.</summary>
    private static void StartRound(EventForwarder forwarder)
    {
        forwarder.OnRoundStart();
        forwarder.OnRoundFreezeEnd();
    }

    private static void Death(EventForwarder forwarder, HostPlayer victim, HostPlayer attacker) =>
        forwarder.OnPlayerDeath(victim, attacker, null, new WeaponReference(7, null), false, 0, false, false, false, false, false, victimEquipmentValue: 0, victimGrenadesHeld: []);

    /// <summary>The engine kills a living player who leaves right after the disconnect, credited to themselves.</summary>
    [Fact]
    public void NamesADeathRightAfterItsVictimLeftADisconnectAndOneInTheNextRoundAFall()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var victim = host.Players.First(player => player.Side == Side.T);

        forwarder.OnPlayerDisconnect(victim, "disconnect_by_user");
        forwarder.OnPlayerDeath(victim, victim, null, new WeaponReference(null, "world"), false, 0, false, false, false, false, false, victimEquipmentValue: 0, victimGrenadesHeld: []);
        Assert.Equal(DamageCause.Disconnect, sent.OfType<PlayerDeath>().Last().cause);

        StartRound(forwarder);
        forwarder.OnPlayerDeath(victim, null, null, new WeaponReference(null, "worldent"), false, 0, false, false, false, false, false, victimEquipmentValue: 0, victimGrenadesHeld: []);
        Assert.Equal(DamageCause.Fall, sent.OfType<PlayerDeath>().Last().cause);
    }

    [Fact]
    public void PreviousBuyWindowCannotCloseTheNextRoundOrOutliveDisposal()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var old = Assert.Single(host.Timers);
        StartRound(forwarder);
        var current = Assert.Single(host.Timers);
        old.Callback();
        Assert.True(forwarder.BuyWindowOpen);
        Assert.Empty(sent.OfType<RoundBuyEnd>());
        forwarder.Dispose();
        current.Callback();
        StartRound(forwarder);
        Assert.False(forwarder.BuyWindowOpen);
        Assert.Empty(host.Timers);
        Assert.Empty(sent.OfType<RoundBuyEnd>());
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void ResetAndMapEndReleaseOwnersAndCancelBuyCallbacks(bool endMap)
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var oldBuyWindow = Assert.Single(host.Timers);
        var thrower = host.PlayerList[0] with { Position = new WorldPosition(0, 0, 0), Angles = new ViewAngles(0, 0) };
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.Flashbang, 50);
        forwarder.OnPlayerBlind(host.PlayerList[2], null, 20, 50);
        if (endMap) forwarder.OnMapEnd();
        else forwarder.Reset();
        Assert.False(forwarder.IsTrackedGrenade(50));
        Assert.False(forwarder.BuyWindowOpen);
        oldBuyWindow.Callback();
        host.Advance(FakeGameHost.BuyTimeSeconds);
        Assert.Empty(sent.OfType<RoundBuyEnd>());
        Death(forwarder, host.PlayerList[2], host.PlayerList[1]);
        Assert.Null(Assert.Single(sent.OfType<PlayerDeath>()).victimBlindBy);
        Assert.Single(sent.OfType<GrenadeEnd>());
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void DisconnectedThrowerOwnsGrenadeDamageDeathsAndVictims(bool friendly)
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var thrower = host.PlayerList[0] with { Position = new WorldPosition(0, 0, 0), Angles = new ViewAngles(0, 0) };
        var victim = host.PlayerList[friendly ? 1 : 2] with
        { Health = 0, IsAlive = false, Position = new WorldPosition(100, 0, 0), Angles = new ViewAngles(0, 0) };
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.HeGrenade, 50);
        forwarder.OnPlayerDisconnect(thrower, "left");
        host.PlayerList.RemoveAt(0);
        forwarder.OnGrenadeDetonate(50, new WorldPosition(100, 0, 0));
        forwarder.OnPlayerHurt(victim, null, new WeaponReference(44, null), 120, 0, 0, 80, 50);
        forwarder.OnPlayerDeath(victim, null, null, new WeaponReference(44, null),
            false, 0, false, false, false, false, false, 0, [], damageEntityId: 50);
        forwarder.OnGrenadeRemoved(50);
        var hurt = Assert.Single(sent.OfType<PlayerHurt>());
        var death = Assert.Single(sent.OfType<PlayerDeath>());
        var grenade = Assert.Single(sent.OfType<GrenadeThrow>());
        Assert.Equal(thrower.SteamIdString, hurt.attacker!.steamId);
        Assert.Equal(thrower.SteamIdString, death.attacker!.steamId);
        Assert.Equal(thrower.SteamIdString, grenade.player.steamId);
        Assert.Equal(friendly, hurt.friendlyFire);
        Assert.Equal(friendly, death.friendlyFire);
        Assert.Equal(grenade.grenadeId, hurt.grenadeId);
        Assert.False(hurt.projectileImpact);
        Assert.Equal(grenade.grenadeId, Assert.Single(sent.OfType<GrenadeEnd>()).grenadeId);
        Assert.Null(hurt.attacker.position);
        Assert.Null(death.attacker.position);
    }

    [Fact]
    public void DisconnectedFlashOwnerStillBlindsAndNamesTheBlinderOnDeath()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var thrower = host.PlayerList[0] with { Position = new WorldPosition(0, 0, 0), Angles = new ViewAngles(0, 0) };
        var victim = host.PlayerList[2];
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.Flashbang, 50);
        forwarder.OnPlayerDisconnect(thrower, "left");
        forwarder.OnGrenadeDetonate(50, new WorldPosition(100, 0, 0));
        forwarder.OnPlayerBlind(victim, null, 3, 50);
        forwarder.OnPlayerBlind(host.PlayerList[1], null, 2, 50);
        forwarder.OnGrenadeRemoved(50);
        forwarder.OnPlayerDeath(victim, host.PlayerList[1], null, new WeaponReference(7, null),
            false, 0, false, false, false, true, false, 0, []);
        Assert.All(sent.OfType<PlayerBlind>(), blind => Assert.Equal(thrower.SteamIdString, blind.attacker.steamId));
        Assert.Equal([false, true], sent.OfType<PlayerBlind>().Select(blind => blind.friendlyFire));
        var death = Assert.Single(sent.OfType<PlayerDeath>());
        Assert.Equal(thrower.SteamIdString, death.victimBlindBy!.steamId);
        Assert.Equal(thrower.SteamIdString, death.assister!.steamId);
        Assert.All(sent.OfType<PlayerBlind>(), blind => Assert.Equal(0L, blind.grenadeId));
    }

    /// <summary>The engine credits the flasher with an assist on the death a disconnect causes, so the blind stays on it.</summary>
    [Fact]
    public void FlashedVictimWhoLeavesNamesTheBlinderOnTheDisconnectDeath()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var thrower = host.PlayerList[0] with { Position = new WorldPosition(0, 0, 0), Angles = new ViewAngles(0, 0) };
        var victim = host.PlayerList[2];
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.Flashbang, 50);
        forwarder.OnGrenadeDetonate(50, new WorldPosition(100, 0, 0));
        forwarder.OnPlayerBlind(victim, null, 3, 50);
        forwarder.OnPlayerDisconnect(victim, "disconnect_by_user");
        forwarder.OnPlayerDeath(victim, victim, thrower, new WeaponReference(null, "world"),
            false, 0, false, false, false, true, false, 0, []);
        var death = Assert.Single(sent.OfType<PlayerDeath>());
        Assert.Equal(DamageCause.Disconnect, death.cause);
        Assert.Equal(thrower.SteamIdString, death.victimBlindBy!.steamId);
        Assert.True(death.victimBlindRemaining > 0);
        Assert.Equal(thrower.SteamIdString, death.assister!.steamId);
    }

    [Fact]
    public void FireKeepsThrowerAfterProjectileDeletionAndIgnoresAReplacementAttacker()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var thrower = host.PlayerList[0] with { Position = new WorldPosition(0, 0, 0), Angles = new ViewAngles(0, 0) };
        var victim = host.PlayerList[2] with { Health = 90, Position = new WorldPosition(100, 0, 0), Angles = new ViewAngles(0, 0) };
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.Molotov, 50);
        forwarder.OnPlayerDisconnect(thrower, "left");
        forwarder.OnGrenadeFire(50, 60);
        forwarder.OnGrenadeRemoved(50);
        var replacement = host.PlayerList[3] with { Slot = thrower.Slot, Position = new WorldPosition(999, 999, 999) };
        forwarder.OnPlayerHurt(victim, replacement, new WeaponReference(46, null), 10, 0, 0, 100, 60);
        forwarder.OnGrenadeRemoved(60);
        var hurt = Assert.Single(sent.OfType<PlayerHurt>());
        Assert.Equal(thrower.SteamIdString, hurt.attacker!.steamId);
        Assert.Null(hurt.attacker.position);
        Assert.False(hurt.friendlyFire);
        Assert.Equal(0L, hurt.grenadeId);
        Assert.Single(sent.OfType<GrenadeEnd>());
        Assert.False(forwarder.IsTrackedGrenade(60));
        forwarder.OnPlayerBlind(victim, null, 3, 60);
        Assert.Empty(sent.OfType<PlayerBlind>());
    }

    [Fact]
    public void HurtPreservesRawOverkillAndPreHitHealth()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var victim = Fixture.Player(Fixture.Team2Player1, Side.T, 2, position: new WorldPosition(0, 0, 0), angles: new ViewAngles(0, 0)) with { Health = 0 };
        var attacker = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);
        forwarder.OnPlayerHurt(victim, attacker, new WeaponReference(59, null), 180, 0, 0, healthBefore: 35);
        var hurt = Assert.Single(sent.OfType<PlayerHurt>());
        Assert.Equal(180, hurt.dmgHealth);
        Assert.Equal(35, hurt.healthBefore);
        Assert.Equal(0, hurt.healthAfter);
    }

    [Fact]
    public void HurtCarriesSnapshotPositionsAndAnglesOfBothPlayers()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var victim = Fixture.Player(Fixture.Team2Player1, Side.T, 2,
            position: new WorldPosition(-1200.5f, 640, 12), angles: new ViewAngles(3.5f, -170));
        var attacker = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0,
            position: new WorldPosition(300, -80.25f, 64), angles: new ViewAngles(-2, 15));
        forwarder.OnPlayerHurt(victim, attacker, new WeaponReference(7, null), 27, 5, 2);
        var hurt = Assert.Single(sent.OfType<PlayerHurt>());
        var victimAt = hurt.victim.position!;
        var attackerAt = hurt.attacker!.position!;
        Assert.Equal((-1200.5f, 640f, 12f, 3.5f, -170f), (victimAt.x, victimAt.y, victimAt.z, victimAt.pitch, victimAt.yaw));
        Assert.Equal((300f, -80.25f, 64f, -2f, 15f), (attackerAt.x, attackerAt.y, attackerAt.z, attackerAt.pitch, attackerAt.yaw));
    }

    [Fact]
    public void AHurtVictimWithoutAPawnIsDroppedAndReported()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnPlayerHurt(Fixture.Player(Fixture.Team2Player1, Side.T, 2), null, new WeaponReference(null, "world"), 50, 0, 0);
        Assert.Empty(sent.OfType<PlayerHurt>());
        Assert.Equal(LogLevel.Warn, Assert.Single(sent.OfType<PluginLog>()).level);
    }

    [Fact]
    public void ADeathVictimWithoutAPawnIsDroppedAndReported()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnPlayerDeath(Fixture.Player(Fixture.Team2Player1, Side.T, 2), null, null,
            new WeaponReference(null, "world"), false, 0, false, false, false, false, false, 0, []);
        Assert.Empty(sent.OfType<PlayerDeath>());
        Assert.Equal(LogLevel.Warn, Assert.Single(sent.OfType<PluginLog>()).level);
    }

    [Fact]
    public void BlindAndBombEventsPlaceTheirPlayers()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var flasher = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0,
            position: new WorldPosition(1, 2, 3), angles: new ViewAngles(4, 5));
        var victim = Fixture.Player(Fixture.Team2Player1, Side.T, 2,
            position: new WorldPosition(6, 7, 8), angles: new ViewAngles(9, 10));
        forwarder.OnPlayerBlind(victim, flasher, 2);
        forwarder.OnBombPlanted(victim, BombSite.A);
        var blind = Assert.Single(sent.OfType<PlayerBlind>());
        Assert.Equal(3f, blind.attacker.position!.z);
        Assert.Equal(9f, blind.victim.position!.pitch);
        Assert.Equal(6f, Assert.Single(sent.OfType<BombPlanted>()).player.position!.x);
    }

    [Fact]
    public void BlindAndBombEventsWithoutAPawnAreDroppedAndReported()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var unplaced = Fixture.Player(Fixture.Team2Player1, Side.T, 2);
        forwarder.OnPlayerBlind(unplaced, host.PlayerList[0], 2);
        forwarder.OnBombPlanted(unplaced, BombSite.A);
        Assert.Empty(sent.OfType<PlayerBlind>());
        Assert.Empty(sent.OfType<BombPlanted>());
        Assert.Equal(2, sent.OfType<PluginLog>().Count(log => log.level == LogLevel.Warn));
    }

    [Fact]
    public void WorldDamageHurtHasNoAttacker()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var victim = Fixture.Player(Fixture.Team2Player1, Side.T, 2,
            position: new WorldPosition(1, 2, 3), angles: new ViewAngles(0, 90));
        forwarder.OnPlayerHurt(victim, null, new WeaponReference(null, "world"), 50, 0, 0);
        var hurt = Assert.Single(sent.OfType<PlayerHurt>());
        Assert.Null(hurt.attacker);
    }

    [Fact]
    public void RoundScopedEventsAreOnlyForwardedOnceLive()
    {
        var (match, forwarder, host, sent) = Build();
        var victim = Fixture.Player(Fixture.Team2Player1, Side.T, 2,
            position: new WorldPosition(1, 2, 3), angles: new ViewAngles(0, 90));
        var attacker = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);

        // Warmup deathmatch: nothing round-scoped goes out, and no connection either.
        forwarder.OnRoundStart();
        Death(forwarder, victim, attacker);
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        forwarder.OnPlayerConnect(victim, rejected: false);
        forwarder.OnPlayerConnect(Fixture.Player(Fixture.Stranger, Side.T, 5), rejected: true);
        Assert.DoesNotContain(sent, m => m is RoundScopedMessage);
        Assert.DoesNotContain(sent, m => m is PlayerConnect);

        // Until the engine confirms the live round start, round stats remain gated.
        StartLive(match, host);
        forwarder.OnRoundStart();
        Assert.DoesNotContain(sent, m => m is RoundScopedMessage);

        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Contains(sent, m => m is GoingLive);

        forwarder.OnRoundStart();
        Death(forwarder, victim, attacker);
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        Assert.Equal(
            ["round_start", "player_death", "round_buy_end", "round_end"],
            sent.OfType<RoundScopedMessage>().Select(m => m.type).ToList());

        forwarder.OnPlayerConnect(victim, rejected: false);
        Assert.Contains(sent, m => m is PlayerConnect);
    }

    [Fact]
    public void FreezeEndMvpAndBombExplosionAreOnlyForwardedOnceLive()
    {
        var (match, forwarder, host, sent) = Build();
        StartLive(match, host);

        forwarder.OnRoundFreezeEnd();
        forwarder.OnRoundMvp(host.PlayerList[0], 1);
        forwarder.OnBombExploded();
        Assert.DoesNotContain(sent, m => m is RoundScopedMessage);

        host.IsWarmup = false;
        match.OnRoundStart();
        forwarder.OnRoundFreezeEnd();
        forwarder.OnRoundMvp(host.PlayerList[0], 1);
        forwarder.OnBombExploded();
        Assert.Equal(["round_freeze_end", "round_mvp", "bomb_exploded"],
            sent.OfType<RoundScopedMessage>().Select(m => m.type).ToList());
    }

    [Fact]
    public void ABuyWindowClosingAfterABackupLoadBeganReportsNoBuy()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        host.ConVars["mp_buytime"] = 5;
        StartRound(forwarder);
        host.LoadBackup(match);
        Assert.False(match.IsLive);

        host.Advance(5);

        Assert.Empty(sent.OfType<RoundBuyEnd>());
    }

    [Fact]
    public void HalftimeAfterABackupLoadBeganFlushesNoShots()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        forwarder.OnWeaponFire(host.PlayerList[0], new WeaponReference(7, null));
        host.LoadBackup(match);
        Assert.False(match.IsLive);

        forwarder.OnHalftime(match.RoundTime);

        Assert.Empty(sent.OfType<RoundShots>());
    }

    [Fact]
    public void ADamageCallThatOutlivesItsRoundStartFinishesWithoutASample()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var player = host.PlayerList[0];
        Assert.True(forwarder.OnPlayerDamageStarting(player with { EquipmentValue = 4700 }));
        // The next round start clears the round's damage samples while the call is still open.
        StartRound(forwarder);

        Assert.Null(Record.Exception(() => forwarder.OnPlayerDamageFinished(player.SteamId)));

        host.PlayerList[0] = player with { EquipmentValue = 1000 };
        host.Advance(FakeGameHost.BuyTimeSeconds);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>());
        Assert.Equal(1000, Assert.Single(buy.players, p => p.player.steamId == player.SteamIdString).equipmentValue);
    }

    [Theory]
    [InlineData(Side.Spec, Side.Spec, false)]
    [InlineData(Side.T, Side.T, true)]
    [InlineData(Side.T, Side.Ct, false)]
    public void UnrosteredPlayersAreTeammatesOnlyOnTheSamePlayingSide(Side attackerSide, Side victimSide, bool friendly)
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var attacker = Fixture.Player("76561198000000098", attackerSide, 6,
            position: new WorldPosition(0, 0, 0), angles: new ViewAngles(0, 0));
        var victim = Fixture.Player(Fixture.Stranger, victimSide, 5,
            position: new WorldPosition(1, 0, 0), angles: new ViewAngles(0, 0));

        forwarder.OnPlayerHurt(victim, attacker, new WeaponReference(7, null), 10, 0, 0);

        Assert.Equal(friendly, Assert.Single(sent.OfType<PlayerHurt>()).friendlyFire);
    }

    [Fact]
    public void RoundStartNamesBothTeamsAndActualAliveStateWithoutSpectators()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        host.PlayerList[1] = host.PlayerList[1] with { IsAlive = false, Health = 0 };
        host.PlayerList.Add(Fixture.Player("76561198099999999", Side.Spec, 5));
        forwarder.OnRoundStart();
        var start = Assert.Single(sent.OfType<RoundStart>());
        Assert.Equal(4, start.players.Count);
        Assert.Equal([1, 1, 2, 2], start.players.Select(p => p.player.team));
        Assert.Equal([true, false, true, true], start.players.Select(p => p.alive));
    }

    [Fact]
    public void RoundStartRecordsMoneyWithoutReadingInventory()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var player = host.PlayerList[0];
        host.PlayerList[0] = player with { EquipmentValue = 5700, Cash = 1000, StartCash = 6700, CashSpent = 5700 };
        host.Inventories[player.SteamId] = new PlayerInventory
        {
            items = [new() { weapon = 7, quantity = 1, unitValue = 2700 },
                     new() { weapon = 43, quantity = 2, unitValue = 200 }],
            armor = 83, helmet = true, defuseKit = true,
        };
        forwarder.OnRoundStart();
        var start = Assert.Single(sent.OfType<RoundStart>());
        var own = Assert.Single(start.players, p => p.player.steamId == player.SteamIdString);
        Assert.Equal((5700, 1000, 6700, 5700), (own.equipmentValue, own.cash, own.startCash, own.cashSpent));
        Assert.Equal(0, host.InventoryReads);
        Assert.Null(start.carryover);
    }

    [Fact]
    public void CarryoverCapturesExitDeathsAndPickupsBeforeRespawnsAndNewPurchases()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.TargetBombed);
        // A winner picks up a rifle and a loser dies after the result, before the reset.
        host.PlayerList[0] = host.PlayerList[0] with { EquipmentValue = 4750, Cash = 2100 };
        host.PlayerList[2] = host.PlayerList[2] with { IsAlive = false, Health = 0, EquipmentValue = 0 };
        host.Inventories[host.PlayerList[0].SteamId] = new PlayerInventory
        {
            items = [new() { weapon = 9, quantity = 1, unitValue = 4750 }],
        };
        host.TickCount = 1000;
        forwarder.OnRoundPrestart();
        // The reset and purchases must not mutate the saved snapshot.
        host.PlayerList[0] = host.PlayerList[0] with { EquipmentValue = 5700, Cash = 1150 };
        host.Inventories[host.PlayerList[0].SteamId] = new PlayerInventory { armor = 100 };
        host.PlayerList[2] = host.PlayerList[2] with { IsAlive = true, Health = 100, EquipmentValue = 200 };
        host.RoundsPlayed = 1;
        host.TickCount = 1001;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        var next = sent.OfType<RoundStart>().Last();
        Assert.Equal(2, next.round);
        Assert.Equal(1, next.carryover!.fromRound);
        Assert.Equal(1000, next.carryover.sampledTick);
        Assert.Equal(4750, next.carryover.players[0].equipmentValue);
        Assert.Equal(2100, next.carryover.players[0].cash);
        Assert.Equal(9, Assert.Single(next.carryover.players[0].inventory!.items).weapon);
        Assert.False(next.carryover.players[2].alive);
        Assert.Empty(next.carryover.players[2].inventory!.items);
        Assert.Equal(0, next.carryover.players[2].inventory!.armor);
        Assert.False(next.carryover.players[2].inventory!.helmet);
        Assert.False(next.carryover.players[2].inventory!.defuseKit);
        Assert.True(next.players[2].alive);
        Assert.Equal(5700, next.players[0].equipmentValue);
        Assert.Equal(3, host.InventoryReads);
        // It must not be attached again if another round starts without a boundary snapshot.
        host.RoundsPlayed = 2;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        Assert.Null(sent.OfType<RoundStart>().Last().carryover);
    }

    [Theory]
    [InlineData(24)]
    [InlineData(30)]
    public void CarryoverIsExcludedWhenAnOvertimeStartsWithoutASideSwap(int endedRound)
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        host.ConVars["mp_maxrounds"] = 24;
        host.ConVars["mp_overtime_maxrounds"] = 6;
        host.ConVars["mp_overtime_enable"] = 1;
        host.RoundsPlayed = endedRound - 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.TargetBombed);
        forwarder.OnRoundPrestart();
        host.RoundsPlayed = endedRound;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        Assert.Null(sent.OfType<RoundStart>().Last().carryover);
        Assert.Equal(0, host.InventoryReads);
    }

    [Fact]
    public void CarryoverIsDiscardedIfSidesSwapAfterThePrestartSnapshot()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.TargetBombed);
        forwarder.OnRoundPrestart();
        Assert.Equal(4, host.InventoryReads);
        match.SwapSides();
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        Assert.Null(sent.OfType<RoundStart>().Last().carryover);
    }

    [Fact]
    public void CarryoverIsExcludedAcrossHalftime()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.TargetBombed);
        match.SwapSides();
        host.PlayerList[0] = host.PlayerList[0] with { Side = Side.T };
        host.PlayerList[2] = host.PlayerList[2] with { Side = Side.Ct };
        forwarder.OnRoundPrestart();
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        var start = sent.OfType<RoundStart>().Last();
        Assert.Null(start.carryover);
        Assert.Equal(0, host.InventoryReads);
        Assert.Equal(Side.T, start.players[0].player.side);
    }

    [Fact]
    public void CarryoverIsAbsentWithoutACompletedLiveRound()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundPrestart();
        forwarder.OnRoundStart();
        Assert.Null(Assert.Single(sent.OfType<RoundStart>()).carryover);
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.TargetBombed);
        match.EnterPostLive();
        forwarder.OnRoundPrestart();
        Assert.Single(sent.OfType<RoundStart>());
    }

    [Fact]
    public void EarlyDeathInsideBuyZonePreservesPreDamageEconomyEvenAfterDisconnect()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var victim = host.PlayerList[0] with
        {
            EquipmentValue = 4700, Cash = 300, StartCash = 5000, CashSpent = 4700, InBuyZone = true,
        };
        host.Advance(1);
        forwarder.OnPlayerDamageStarting(victim);
        var sampledTick = host.SimulationTick;
        var dead = victim with { EquipmentValue = 0, Cash = 900, IsAlive = false, Health = 0 };
        Death(forwarder, dead, host.PlayerList[2]);
        forwarder.OnPlayerDamageFinished(victim.SteamId);
        forwarder.OnPlayerDisconnect(dead, "left");
        host.PlayerList.RemoveAt(0);
        host.Advance(FakeGameHost.BuyTimeSeconds);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>());
        var own = Assert.Single(buy.players, p => p.player.steamId == victim.SteamIdString);
        Assert.Equal((4700, 300, 5000, 4700), (own.equipmentValue, own.cash, own.startCash, own.cashSpent));
        Assert.Equal(sampledTick, own.sampledTick);
        Assert.Equal(4700, buy.team1EquipmentValue);
    }

    [Fact]
    public void RoundEndInsideDamageUsesPreDamageEconomyBeforeTheDeathCallback()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var victim = host.PlayerList[0] with { EquipmentValue = 3500, Cash = 200, InBuyZone = true };
        forwarder.OnPlayerDamageStarting(victim);
        host.PlayerList[0] = victim with { EquipmentValue = 0, Cash = 1600, IsAlive = false, Health = 0 };
        forwarder.OnRoundEnd(Side.T, RoundEndReason.TargetBombed);
        Death(forwarder, host.PlayerList[0], host.PlayerList[2]);
        forwarder.OnPlayerDamageFinished(victim.SteamId);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>());
        Assert.Equal(3500, buy.team1EquipmentValue);
        Assert.Equal(200, Assert.Single(buy.players, p => p.player.steamId == victim.SteamIdString).cash);
        host.Advance(FakeGameHost.BuyTimeSeconds);
        Assert.Single(sent.OfType<RoundBuyEnd>());
    }

    [Fact]
    public void DeathWithoutDamageContextAndDisconnectStillRetainIndividualBuys()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var victim = host.PlayerList[0] with { Cash = 100, InBuyZone = true, IsAlive = false };
        forwarder.OnPlayerDeath(victim, null, null, new WeaponReference(null, "worldent"),
            false, 0, false, false, false, false, false, 4200, []);
        var quitter = host.PlayerList[2] with { EquipmentValue = 1800, Cash = 200 };
        forwarder.OnPlayerDisconnect(quitter, "left");
        host.PlayerList.RemoveAt(2);
        host.Advance(FakeGameHost.BuyTimeSeconds);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>());
        Assert.Equal(4200, buy.team1EquipmentValue);
        Assert.Equal(1800, buy.team2EquipmentValue);
        Assert.Equal(100, Assert.Single(buy.players, p => p.player.steamId == victim.SteamIdString).cash);
    }

    [Fact]
    public void BuyZoneReentryAndDeathUseLaterPurchasesButOutsideDeathKeepsTheExit()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var first = host.PlayerList[0] with { EquipmentValue = 800, Cash = 4200 };
        forwarder.OnExitBuyZone(first);
        first = first with { EquipmentValue = 4500, Cash = 500, InBuyZone = true };
        forwarder.OnPlayerDamageStarting(first);
        Death(forwarder, first with { IsAlive = false, InBuyZone = false }, host.PlayerList[2]);
        forwarder.OnPlayerDamageFinished(first.SteamId);
        var second = host.PlayerList[1] with { EquipmentValue = 5000, Cash = 0 };
        forwarder.OnExitBuyZone(second);
        forwarder.OnPlayerDamageStarting(second with { EquipmentValue = 4000, Cash = 300 });
        Death(forwarder, second, host.PlayerList[2]);
        forwarder.OnPlayerDamageFinished(second.SteamId);
        host.Advance(FakeGameHost.BuyTimeSeconds);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>());
        Assert.Equal(9500, buy.team1EquipmentValue);
        Assert.Equal(500, buy.players.Where(p => p.player.team == 1).Sum(p => p.cash));
    }

    [Fact]
    public void CompletedDamageDoesNotFreezeALaterPurchaseAndTheNextRoundResetsBuys()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        var player = host.PlayerList[0] with { EquipmentValue = 800, InBuyZone = true };
        forwarder.OnPlayerDamageStarting(player);
        forwarder.OnPlayerDamageStarting(player with { EquipmentValue = 700 });
        forwarder.OnPlayerDamageFinished(player.SteamId);
        forwarder.OnPlayerDamageFinished(player.SteamId);
        host.PlayerList[0] = player with { EquipmentValue = 4500 };
        host.Advance(FakeGameHost.BuyTimeSeconds);
        Assert.Equal(4500, Assert.Single(sent.OfType<RoundBuyEnd>()).team1EquipmentValue);
        sent.Clear();
        host.PlayerList[0] = player with { EquipmentValue = 0 };
        match.OnRoundStart();
        StartRound(forwarder);
        host.Advance(FakeGameHost.BuyTimeSeconds);
        Assert.Equal(0, Assert.Single(sent.OfType<RoundBuyEnd>()).team1EquipmentValue);
    }

    [Fact]
    public void BuyEndSumsWhatEachLobbyTeamCarriesWhenBuyTimeCloses()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        host.ConVars["mp_buytime"] = 20;
        StartRound(forwarder);
        for (var i = 0; i < host.PlayerList.Count; i++)
            host.PlayerList[i] = host.PlayerList[i] with { EquipmentValue = 1000 * (i + 1), InBuyZone = true };
        host.Advance(19);
        Assert.Empty(sent.OfType<RoundBuyEnd>());
        host.Advance(1);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>());
        var team1 = host.PlayerList.Where(p => match.Roster.LobbyTeam(p.SteamIdString) == 1).Sum(p => p.EquipmentValue);
        var team2 = host.PlayerList.Where(p => match.Roster.LobbyTeam(p.SteamIdString) == 2).Sum(p => p.EquipmentValue);
        Assert.True(team1 > 0 && team2 > 0);
        Assert.Equal((team1, team2), (buy.team1EquipmentValue, buy.team2EquipmentValue));
    }

    [Fact]
    public void BuyEndReadsAPlayerOutsideTheBuyZoneAtTheirLastExit()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        // Team 1 player 1 buys, leaves, and is dead with nothing by the time buy time closes.
        forwarder.OnExitBuyZone(host.PlayerList[0] with { EquipmentValue = 4700 });
        host.PlayerList[0] = host.PlayerList[0] with { EquipmentValue = 0, IsAlive = false };
        // Team 1 player 2 left with a pistol, came back and bought a rifle: the live figure wins.
        forwarder.OnExitBuyZone(host.PlayerList[1] with { EquipmentValue = 800 });
        host.PlayerList[1] = host.PlayerList[1] with { EquipmentValue = 3500, InBuyZone = true };
        host.Advance(FakeGameHost.BuyTimeSeconds);
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>());
        Assert.Equal(4700 + 3500, buy.team1EquipmentValue);
        Assert.Equal(0, buy.team2EquipmentValue);
    }

    [Fact]
    public void ARoundEndingInsideBuyTimeReportsItsBuyFirst()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        host.PlayerList[2] = host.PlayerList[2] with { EquipmentValue = 650, InBuyZone = true };
        host.Advance(5);
        forwarder.OnRoundEnd(Side.T, RoundEndReason.TerroristsWin);
        Assert.Equal(
            ["round_start", "round_freeze_end", "round_buy_end", "round_end"],
            sent.OfType<RoundScopedMessage>().Select(m => m.type).ToList());
        Assert.Equal(650, Assert.Single(sent.OfType<RoundBuyEnd>()).team2EquipmentValue);
        host.Advance(FakeGameHost.BuyTimeSeconds);
        Assert.Single(sent.OfType<RoundBuyEnd>());
    }

    [Fact]
    public void BuyTimeCountsFromFreezeEndAndFreezeTimeBuysCount()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        Assert.True(forwarder.BuyWindowOpen);
        // A player buys and leaves the zone during freeze time.
        forwarder.OnExitBuyZone(host.PlayerList[0] with { EquipmentValue = 4700 });
        host.Advance(FakeGameHost.BuyTimeSeconds * 2);
        Assert.True(forwarder.BuyWindowOpen);
        Assert.Empty(sent.OfType<RoundBuyEnd>());

        forwarder.OnRoundFreezeEnd();
        host.Advance(FakeGameHost.BuyTimeSeconds - 1);
        Assert.Empty(sent.OfType<RoundBuyEnd>());
        host.Advance(1);
        Assert.False(forwarder.BuyWindowOpen);
        Assert.Equal(4700, Assert.Single(sent.OfType<RoundBuyEnd>()).team1EquipmentValue);
    }

    [Fact]
    public void ABuyZoneExitAfterBuyTimeChangesNothing()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        host.Advance(FakeGameHost.BuyTimeSeconds);
        forwarder.OnExitBuyZone(host.PlayerList[0] with { EquipmentValue = 4700 });
        var buy = Assert.Single(sent.OfType<RoundBuyEnd>());
        Assert.Equal(0, buy.team1EquipmentValue);
    }

    [Fact]
    public void BombAttemptsCarryTheirPlayerSiteAndKit()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var planter = host.PlayerList[2];
        var defuser = host.PlayerList[0];
        forwarder.OnBombBeginPlant(planter, BombSite.B);
        forwarder.OnBombAbortPlant(planter, BombSite.B);
        forwarder.OnBombBeginDefuse(defuser, hasKit: true);
        forwarder.OnBombAbortDefuse(defuser);
        Assert.Equal(
            ["bomb_begin_plant", "bomb_abort_plant", "bomb_begin_defuse", "bomb_abort_defuse"],
            sent.OfType<RoundScopedMessage>().Select(m => m.type).ToList());
        Assert.Equal(BombSite.B, Assert.Single(sent.OfType<BombBeginPlant>()).site);
        Assert.Equal(BombSite.B, Assert.Single(sent.OfType<BombAbortPlant>()).site);
        Assert.True(Assert.Single(sent.OfType<BombBeginDefuse>()).hasKit);
        Assert.Equal(defuser.SteamIdString, Assert.Single(sent.OfType<BombAbortDefuse>()).player.steamId);
    }

    [Fact]
    public void TheRoundNumberIsLatchedAtRoundStart()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);

        host.RoundsPlayed = 4; // round 5 begins; the plugin calls both on round_start
        match.OnRoundStart();
        forwarder.OnRoundStart();
        host.RoundsPlayed = 5; // the game counts the round before round_end fires
        forwarder.OnRoundEnd(Side.T, RoundEndReason.TerroristsWin);

        var rounds = sent.OfType<RoundScopedMessage>().Select(m => m.round).ToList();
        Assert.Equal([5, 5, 5], rounds);
    }

    [Fact]
    public void IncendiaryThrowCarriesItsKindAndEndsAfterItsFire()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        var thrower = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0,
            position: new WorldPosition(100, -200, 64), angles: new ViewAngles(-10, 135));
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.IncGrenade, 50);
        forwarder.OnGrenadeDetonate(50, new WorldPosition(500, 600, 70));
        forwarder.OnGrenadeFire(50, 60);
        forwarder.OnGrenadeRemoved(50);
        Assert.Empty(sent.OfType<GrenadeEnd>());
        forwarder.OnGrenadeRemoved(60);
        Assert.Equal(["grenade_throw", "grenade_detonate", "grenade_end"], sent.Select(message => message.type).Where(type => type.StartsWith("grenade_")));
        var grenade = Assert.Single(sent.OfType<GrenadeThrow>());
        Assert.Equal(GrenadeKind.IncGrenade, grenade.grenade);
        var thrownAt = grenade.player.position!;
        Assert.Equal((100f, -200f, 64f, -10f, 135f), (thrownAt.x, thrownAt.y, thrownAt.z, thrownAt.pitch, thrownAt.yaw));
        Assert.Equal(500f, Assert.Single(sent.OfType<GrenadeDetonate>()).detonatePos!.x);
        var end = Assert.Single(sent.OfType<GrenadeEnd>());
        Assert.Equal((FireEnd.Expired, null), (end.fireEnd, end.extinguishedBy));
    }

    [Theory]
    [InlineData(0, 1)] // competitive
    [InlineData(0, 0)] // casual
    [InlineData(0, 2)] // wingman
    public void ShotsAreTalliedInEveryMatchMode(int gameType, int gameMode)
    {
        var (match, forwarder, host, sent) = Build();
        host.ConVars["game_type"] = gameType;
        host.ConVars["game_mode"] = gameMode;
        GoLive(match, host);
        forwarder.OnRoundStart();

        forwarder.OnWeaponFire(host.PlayerList[0], new WeaponReference(7, null));
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        forwarder.OnRoundStart();

        Assert.NotEmpty(sent.OfType<RoundShots>());
    }

    [Fact]
    public void HalftimeFlushesShotsAndGrenadesWithoutWaitingForRoundStart()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        host.ConVars["mp_round_restart_delay"] = 7;
        forwarder.OnRoundStart();
        var thrower = host.PlayerList[0] with { Position = new WorldPosition(1, 1, 1), Angles = new ViewAngles(0, 0) };
        forwarder.OnWeaponFire(thrower, new WeaponReference(7, null));
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.Molotov, 50);
        forwarder.OnGrenadeFire(50, 60);
        var damage = new DamageTracker(forwarder);
        damage.Enter(1);
        host.TeamScores[Side.Ct] = 1;
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        var length = sent.OfType<RoundEnd>().Single().roundLength;
        damage.RunAfterDamage(() => forwarder.OnHalftime(length));
        Assert.Empty(sent.OfType<GrenadeEnd>());
        forwarder.OnPlayerHurt(host.PlayerList[2] with { Health = 0,
            Position = new WorldPosition(2, 2, 2), Angles = new ViewAngles(0, 0) }, thrower,
            new WeaponReference(46, null), 100, 0, 0, healthBefore: 100, damageEntityId: 60);
        var thrown = Assert.Single(sent.OfType<GrenadeThrow>());
        Assert.Equal(Side.Ct, thrown.player.side);
        Assert.Equal(thrown.grenadeId, Assert.Single(sent.OfType<PlayerHurt>()).grenadeId);
        sent.Clear();
        host.Advance(0.1);
        damage.Exit();
        Assert.Equal(["grenade_end", "round_shots"], sent.Select(message => message.type));
        Assert.Equal(length, Assert.Single(sent.OfType<GrenadeEnd>()).endRoundTime);
        Assert.Equal(Side.Ct, Assert.Single(Assert.Single(sent.OfType<RoundShots>()).players).player.side);
        Assert.False(forwarder.IsTrackedGrenade(60));
        forwarder.CompleteRound(length);
        Assert.Equal(1, Assert.Single(sent.OfType<RoundComplete>()).round);
        match.SwapSides();
        sent.Clear();
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        Assert.Single(sent);
        Assert.IsType<RoundStart>(sent[0]);
    }

    [Theory]
    [InlineData("halftime")]
    [InlineData("map_end")]
    [InlineData("round_end")]
    public void LateProjectileCannotReopenFrozenRoundButOrdinaryExitPlayRemains(string boundary)
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        var thrower = host.PlayerList[0] with { Position = new WorldPosition(1, 1, 1), Angles = new ViewAngles(0, 0) };
        // Live capture: weapon_fire at 56080.890625, round_end at 56083, spawn at 56088.
        forwarder.OnWeaponFire(thrower, new WeaponReference(45, null));
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        if (boundary == "halftime") forwarder.OnHalftime(match.RoundTime);
        if (boundary == "map_end") forwarder.OnMapEnd();
        sent.Clear();
        host.Advance(0.078);
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.SmokeGrenade, 50);
        Assert.Equal(boundary == "round_end", forwarder.IsTrackedGrenade(50));
        forwarder.OnGrenadeDetonate(50, new WorldPosition(1, 1, 1));
        forwarder.OnGrenadeRemoved(50);
        Assert.Equal(boundary == "round_end" ? 1 : 0, sent.OfType<GrenadeEnd>().Count());

        if (boundary == "map_end") return;
        if (boundary == "halftime") match.SwapSides();
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.SmokeGrenade, 51);
        Assert.True(forwarder.IsTrackedGrenade(51));
    }

    /// <summary>Grenades are flushed when the round closes, ahead of the next round_start.</summary>
    [Fact]
    public void PrestartWrapsUpABurningFireBeforeCompletingItsRound()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        host.ConVars["mp_round_restart_delay"] = 7;
        forwarder.OnRoundStart();
        var thrower = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0,
            position: new WorldPosition(1, 1, 1), angles: new ViewAngles(0, 0));
        var victim = Fixture.Player(Fixture.Team2Player1, Side.T, 2,
            position: new WorldPosition(2, 2, 2), angles: new ViewAngles(0, 0));
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.Molotov, 50);
        forwarder.OnGrenadeDetonate(50, new WorldPosition(500, 600, 70));
        forwarder.OnGrenadeFire(50, 60);
        forwarder.OnGrenadeRemoved(50);
        forwarder.OnPlayerHurt(victim with { Health = 92 }, thrower, new WeaponReference(46, null), 8, 0, 0, healthBefore: 100, damageEntityId: 60);
        // The fire keeps burning through the restart delay; that damage is still the old round's.
        host.TeamScores[Side.Ct] = 1;
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        forwarder.OnPlayerHurt(victim with { Health = 85 }, thrower, new WeaponReference(46, null), 7, 0, 0, healthBefore: 92, damageEntityId: 60);
        sent.Clear();

        forwarder.OnRoundPrestart();
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();

        Assert.Equal(["grenade_end", "round_shots", "round_complete", "round_start"], sent.Select(m => m.type));
        Assert.Equal(1, Assert.Single(sent.OfType<RoundComplete>()).round);
        var grenade = sent.OfType<GrenadeEnd>().Single();
        Assert.Equal(1, grenade.round);
        Assert.Equal(7, grenade.endRoundTime); // round length 0 on the fake clock, plus the restart delay
        Assert.Equal(2, sent.OfType<RoundStart>().Single().round);

        // Whatever the fire does after the wrap-up is not this grenade's, and its removal emits nothing more.
        forwarder.OnPlayerHurt(victim with { Health = 80 }, thrower, new WeaponReference(46, null), 5, 0, 0, healthBefore: 85, damageEntityId: 60);
        forwarder.OnGrenadeRemoved(60);
        Assert.Single(sent.OfType<GrenadeEnd>());
        Assert.Null(sent.OfType<PlayerHurt>().Single().grenadeId);
    }

    [Fact]
    public void AnExtinguishedFireCreditsTheSmokeAndAnUnmatchedOneIsReported()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var thrower = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0,
            position: new WorldPosition(1, 1, 1), angles: new ViewAngles(0, 0));
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.Molotov, 50);
        forwarder.OnGrenadeDetonate(50, new WorldPosition(500, 600, 70));
        forwarder.OnGrenadeFire(50, 60);
        forwarder.OnGrenadeRemoved(50);
        forwarder.OnFireExtinguished(60, new WorldPosition(505, 605, 70));
        Assert.Equal(LogLevel.Warn, Assert.Single(sent.OfType<PluginLog>()).level);

        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.SmokeGrenade, 70);
        forwarder.OnGrenadeDetonate(70, new WorldPosition(510, 610, 70));
        forwarder.OnFireExtinguished(60, new WorldPosition(505, 605, 70));
        forwarder.OnGrenadeRemoved(60);
        forwarder.OnGrenadeRemoved(70);
        var ends = sent.OfType<GrenadeEnd>().ToList();
        Assert.Equal((0L, FireEnd.Extinguished, 1L), (ends[0].grenadeId, ends[0].fireEnd, ends[0].extinguishedBy));
        Assert.Equal((1L, null), (ends[1].grenadeId, ends[1].extinguishedBy));
        Assert.Single(sent.OfType<PluginLog>());

        // A fire whose inferno goes flagged as created in smoke never burned; the smoke is credited.
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.SmokeGrenade, 80);
        forwarder.OnGrenadeDetonate(80, new WorldPosition(0, 0, 0));
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.Molotov, 90);
        forwarder.OnGrenadeDetonate(90, new WorldPosition(5, 0, 0));
        forwarder.OnGrenadeFire(90, 91);
        forwarder.OnGrenadeRemoved(90);
        forwarder.OnGrenadeRemoved(91, smothered: true, origin: new WorldPosition(5, 0, 0));
        forwarder.OnGrenadeRemoved(80);
        ends = sent.OfType<GrenadeEnd>().ToList();
        Assert.Equal((3L, FireEnd.Smothered, 2L), (ends[2].grenadeId, ends[2].fireEnd, ends[2].extinguishedBy));
        Assert.Equal(2L, ends[3].grenadeId);
    }

    [Fact]
    public void MapEndWaitsForTheFinalDeathBeforeFlushingFireAndShots()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var thrower = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0,
            position: new WorldPosition(1, 1, 1), angles: new ViewAngles(0, 0));
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.IncGrenade, 50);
        forwarder.OnGrenadeDetonate(50, new WorldPosition(500, 600, 70));
        forwarder.OnGrenadeFire(50, 60);
        forwarder.OnGrenadeRemoved(50);
        forwarder.OnWeaponFire(thrower, new WeaponReference(7, null));
        sent.Clear();

        var damage = new DamageTracker(forwarder);
        damage.Enter(1);
        damage.Capture(new DamageSnapshot(100, 60, new WeaponReference(46, null), 400,
            [new InventoryItem { weapon = 43, quantity = 2, unitValue = 200 }]), null);
        damage.RunAfterDamage(forwarder.OnMapEnd);
        Assert.Empty(sent);
        var snapshot = Assert.IsType<DamageSnapshot>(damage.Current(1));
        forwarder.OnPlayerDeath(host.PlayerList[2] with { IsAlive = false, Health = 0, EquipmentValue = 0 },
            thrower, null, Assert.IsType<WeaponReference>(snapshot.Weapon), false, 0, false, false, false, false, false,
            snapshot.EquipmentValue, snapshot.GrenadesHeld, snapshot.Inflictor);
        damage.Exit();

        Assert.Equal(["player_death", "grenade_end", "round_shots", "map_end"], sent.Select(m => m.type));
        var death = Assert.Single(sent.OfType<PlayerDeath>());
        Assert.Equal(400, death.victimEquipmentValue);
        Assert.Equal(2, Assert.Single(death.victimGrenadesHeld!).quantity);
        Assert.Equal(1, sent.OfType<GrenadeEnd>().Single().round);
        Assert.Equal(1, sent.OfType<RoundShots>().Single().round);
    }

    [Fact]
    public void AtMapEndTheRoundClosesWithItsEndWhenTheEngineClockReadsZero()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        host.FreezePeriod = false;
        var thrower = host.PlayerList[0] with { Position = new WorldPosition(1, 1, 1), Angles = new ViewAngles(0, 0) };
        host.Advance(9.422);
        forwarder.OnGrenadeSpawned(thrower, GrenadeKind.SmokeGrenade, 50);
        host.Advance(1.906);
        forwarder.OnGrenadeDetonate(50, new WorldPosition(500, 600, 70));
        host.Advance(11.307);
        host.TeamScores[Side.Ct] = 1;
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        var length = sent.OfType<RoundEnd>().Single().roundLength;
        Assert.True(length > 9.422);

        // Game over freezes the round clock at 0 in some matches.
        host.FreezePeriod = true;
        forwarder.OnMapEnd();

        Assert.Equal(length, sent.OfType<GrenadeEnd>().Single().endRoundTime);
        Assert.Equal(length, sent.OfType<RoundComplete>().Single().roundTime);
    }

    [Fact]
    public void AThrowerSnapshotWithoutAPawnIsNotTracked()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnGrenadeSpawned(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), GrenadeKind.Flashbang, 50);
        forwarder.OnGrenadeDetonate(50, new WorldPosition(1, 2, 3));
        forwarder.OnGrenadeRemoved(50);
        Assert.Empty(sent.OfType<GrenadeThrow>());
        Assert.Empty(sent.OfType<GrenadeEnd>());
    }

    [Fact]
    public void NonLiveRoundStartDiscardsPendingGrenadesWithoutEndingThem()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnGrenadeSpawned(host.PlayerList[0], GrenadeKind.Molotov, 10);
        Assert.Single(sent.OfType<GrenadeThrow>());
        match.EnterPostLive();
        forwarder.OnRoundStart();
        forwarder.OnGrenadeRemoved(10);
        Assert.Empty(sent.OfType<GrenadeEnd>());
    }

    [Fact]
    public void AnAssistGoesOnlyToAReportableThirdPlayer()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var killer = host.PlayerList[0];
        var victim = host.PlayerList[2];
        forwarder.OnPlayerDeath(victim, killer, host.PlayerList[1] with { IsBot = true }, new WeaponReference(7, null),
            false, 0, false, false, false, false, false, victimEquipmentValue: 0, victimGrenadesHeld: []);
        forwarder.OnPlayerBlind(host.PlayerList[3], killer, 5);
        forwarder.OnPlayerDeath(host.PlayerList[3], killer, null, new WeaponReference(7, null),
            false, 0, false, false, false, true, false, victimEquipmentValue: 0, victimGrenadesHeld: []);
        Assert.All(sent.OfType<PlayerDeath>(), death => Assert.Null(death.assister));
    }

    [Fact]
    public void HurtSendsOnlyValuesTheContractTakes()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var victim = Fixture.Player(Fixture.Team2Player1, Side.T, 2,
            position: new WorldPosition(0, 0, 0), angles: new ViewAngles(120, 400)) with { Health = 60 };
        forwarder.OnPlayerHurt(victim, null, new WeaponReference(null, "world"), 10, 0, -1, healthBefore: 50);
        var hurt = Assert.Single(sent.OfType<PlayerHurt>());
        Assert.Equal(0, hurt.hitgroup);
        Assert.Null(hurt.healthBefore);
        Assert.Equal((90f, 40f), (hurt.victim.position!.pitch, hurt.victim.position.yaw));
    }

    [Fact]
    public void NativeAssistAndBlindFlagsAreNotOverriddenByEarlierFlashEvents()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var killer = host.PlayerList[0];
        var assister = host.PlayerList[1];
        var victim = host.PlayerList[2];
        forwarder.OnPlayerBlind(victim, assister, 5);
        forwarder.OnPlayerBlind(killer, victim, 5);
        forwarder.OnPlayerDeath(victim, killer, assister, new WeaponReference(7, null), false, 0, false, false, false, false, false, victimEquipmentValue: 0, victimGrenadesHeld: []);
        var death = Assert.Single(sent.OfType<PlayerDeath>());
        Assert.False(death.flashAssist);
        Assert.False(death.attackerBlind);
        Assert.Equal(assister.SteamIdString, death.assister?.steamId);
    }

    [Fact]
    public void ADeathInsideTheBlindWindowNamesTheBlinderAndTheSecondsLeft()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var killer = host.PlayerList[0];
        var victim = host.PlayerList[2];

        // The engine gives no flash assist here: the killer blinded the victim himself.
        forwarder.OnPlayerBlind(victim, killer, 3.0);
        host.Advance(1.25);
        Death(forwarder, victim, killer);

        var death = Assert.Single(sent.OfType<PlayerDeath>());
        Assert.Equal(killer.SteamIdString, death.victimBlindBy?.steamId);
        Assert.Equal(1.75, death.victimBlindRemaining);
        Assert.False(death.flashAssist);
    }

    /// <summary>Any blind counts, so a brief one still names the blinder on a kill inside it.</summary>
    [Fact]
    public void ABriefBlindIsSentAndNamesTheBlinderOnAKillInsideIt()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var killer = host.PlayerList[0];
        var victim = host.PlayerList[2];

        forwarder.OnPlayerBlind(victim, killer, 0.4);
        Assert.Equal(0.4, Assert.Single(sent.OfType<PlayerBlind>()).duration);

        host.Advance(0.25);
        Death(forwarder, victim, killer);
        var death = Assert.Single(sent.OfType<PlayerDeath>());
        Assert.Equal(killer.SteamIdString, death.victimBlindBy?.steamId);
        Assert.Equal(0.15, death.victimBlindRemaining, 3);
    }

    [Fact]
    public void AnExpiredOrSelfInflictedBlindLeavesTheDeathUnblinded()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var killer = host.PlayerList[0];
        var victim = host.PlayerList[2];

        forwarder.OnPlayerBlind(victim, killer, 1.0);
        host.Advance(2.0);
        Death(forwarder, victim, killer);

        // The victim's own flashbang is nobody's assist.
        forwarder.OnPlayerBlind(victim, victim, 5.0);
        Death(forwarder, victim, killer);

        Assert.All(sent.OfType<PlayerDeath>(), death =>
        {
            Assert.Null(death.victimBlindBy);
            Assert.Equal(0, death.victimBlindRemaining);
        });
    }

    [Fact]
    public void ARoundStartClearsTheBlindsOfThePreviousRound()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnPlayerBlind(host.PlayerList[2], host.PlayerList[0], 60);

        host.RoundsPlayed = 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        Death(forwarder, host.PlayerList[2], host.PlayerList[0]);

        Assert.Null(Assert.Single(sent.OfType<PlayerDeath>()).victimBlindBy);
    }

    [Fact]
    public void ShotsGoOutWithTheRoundsCompletionUnderTheRoundTheyWereFiredIn()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnRoundStart();
        forwarder.OnWeaponFire(host.PlayerList[0], new WeaponReference(7, null));
        forwarder.OnWeaponFire(host.PlayerList[0], new WeaponReference(7, null));
        forwarder.OnWeaponFire(host.PlayerList[1], new WeaponReference(60, null));
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        sent.Clear();

        forwarder.OnRoundPrestart();
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();

        Assert.Equal(["round_shots", "round_complete", "round_start"], sent.Select(m => m.type));
        var shots = Assert.Single(sent.OfType<RoundShots>());
        Assert.Equal(1, shots.round);
        var first = Assert.Single(shots.players[0].weapons);
        var second = Assert.Single(shots.players[1].weapons);
        Assert.Equal((Fixture.Team1Player1, 7, 2), (shots.players[0].player.steamId, first.weapon.id, first.shots));
        Assert.Equal((Fixture.Team1Player2, 60, 1), (shots.players[1].player.steamId, second.weapon.id, second.shots));
        Assert.Equal(2, sent.OfType<RoundStart>().Single().round);
    }

    /// <summary>The engine restarts a round a team left, with no round_end: the aborted attempt has no round_shots.</summary>
    [Fact]
    public void SendsNoRoundShotsForARoundThatStartsAgainWithoutEnding()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        StartRound(forwarder);
        forwarder.OnWeaponFire(host.PlayerList[0], new WeaponReference(7, null));

        StartRound(forwarder);

        Assert.Empty(sent.OfType<RoundShots>());
        Assert.Equal(2, sent.OfType<RoundStart>().Count());
    }

    [Fact]
    public void ShotsFiredWhileNotLiveAreNeitherCountedNorEmitted()
    {
        var (match, forwarder, host, sent) = Build();

        // Warmup deathmatch: the shots are dropped, and the round start emits nothing.
        forwarder.OnWeaponFire(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), new WeaponReference(7, null));
        forwarder.OnRoundStart();
        Assert.Empty(sent.OfType<RoundShots>());

        // They must not reappear in the first live round's message, which has nobody who fired.
        GoLive(match, host);
        forwarder.OnRoundStart();
        forwarder.OnRoundEnd(Side.Ct, RoundEndReason.CtsWin);
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        forwarder.OnRoundStart();
        Assert.Empty(Assert.Single(sent.OfType<RoundShots>()).players);
    }

    [Fact]
    public void NativeAirborneFlagReachesTheDeathEvent()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        forwarder.OnPlayerDeath(host.PlayerList[2], host.PlayerList[0], null, new WeaponReference(40, null),
            false, 1, true, true, true, false, attackerInAir: true, victimEquipmentValue: 0,
            victimGrenadesHeld: []);
        var death = Assert.Single(sent.OfType<PlayerDeath>());
        Assert.Equal(0, death.victimEquipmentValue);
        Assert.Empty(death.victimGrenadesHeld!);
        Assert.True(death.attackerInAir);
        Assert.True(death.thruSmoke);
        Assert.True(death.noScope);
        Assert.True(death.attackerBlind);
        Assert.Equal(1, death.penetrated);
    }

    [Fact]
    public void UnthrownGrenadesPreserveQuantitiesAndUnitPricesAndDistinguishUnknown()
    {
        var (match, forwarder, host, sent) = Build();
        GoLive(match, host);
        var held = new List<InventoryItem>
        {
            new() { weapon = 43, quantity = 2, unitValue = 200 },
            new() { weapon = 45, quantity = 1, unitValue = 300 },
        };
        forwarder.OnPlayerDeath(host.PlayerList[2], host.PlayerList[0], null, new WeaponReference(40, null),
            false, 0, false, false, false, false, false, victimEquipmentValue: 4750, victimGrenadesHeld: held);
        held.Clear(); // Later inventory mutation must not change the death snapshot.
        forwarder.OnPlayerDeath(host.PlayerList[3], host.PlayerList[0], null, new WeaponReference(40, null),
            false, 0, false, false, false, false, false, victimEquipmentValue: 0, victimGrenadesHeld: []);
        forwarder.OnPlayerDeath(host.PlayerList[3], host.PlayerList[0], null, new WeaponReference(40, null),
            false, 0, false, false, false, false, false, victimEquipmentValue: 0, victimGrenadesHeld: null);
        var deaths = sent.OfType<PlayerDeath>().ToArray();
        Assert.Equal(3, deaths[0].victimGrenadesHeld!.Sum(item => item.quantity));
        Assert.Equal(700, deaths[0].victimGrenadesHeld!.Sum(item => item.quantity * item.unitValue));
        Assert.Empty(deaths[1].victimGrenadesHeld!);
        Assert.Null(deaths[2].victimGrenadesHeld);
    }
}
