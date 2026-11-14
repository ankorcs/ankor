using Session = Ankor.Tests.GameplayTestSession;
using Ankor.Diagnostics;
using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class GameplayReplayTests
{
    /// <summary>
    /// The committed fixture is what the scenario's replay sends now. A change to what the plugin
    /// sends fails here: fix it, or, when it is meant, run <c>scripts/plugin.sh replay</c> and
    /// commit the fixture's diff with the change.
    /// </summary>
    [Fact]
    public void TheCommittedReplayFixtureIsWhatTheScenarioSends()
    {
        var committed = File.ReadAllLines(Path.Combine(Fixture.PluginRoot(), "tests", "fixtures", "plugin-replay.jsonl"));
        var sent = GameplayScenario.Run().Replay.Sent.Select(WireJson.Serialize).ToArray();
        var differs = Enumerable.Range(0, Math.Max(committed.Length, sent.Length))
            .FirstOrDefault(line => committed.ElementAtOrDefault(line) != sent.ElementAtOrDefault(line), -1);

        Assert.True(differs < 0, $"tests/fixtures/plugin-replay.jsonl is out of date from line {differs + 1}: "
            + $"the fixture has {committed.ElementAtOrDefault(differs) ?? "nothing"}, the scenario sends {sent.ElementAtOrDefault(differs) ?? "nothing"}. "
            + "Fix the change, or run scripts/plugin.sh replay and review the fixture's diff.");
    }

    [Fact]
    public void TheKnifeWinnersSideChoiceReplays()
    {
        var live = new Session(sides: SidesMode.Knife);
        live.Send(new RoundStartInput());
        live.Send(new RoundEndInput(Side.Ct, 8));
        // The knife round's end takes the engine back to warmup, where the winners choose.
        live.Engine.IsWarmup = true;
        live.Send(new SideChoiceInput(live.Engine.PlayerList[0], SideChoiceKind.Swap));
        Assert.Equal(SideChoiceKind.Swap, Assert.Single(live.Sent.OfType<SideChoice>()).choice);

        var replay = new Session(replay: true, sides: SidesMode.Knife);
        GameplayReplay.Run(live.Tape, replay.Callbacks, replay.Replay!);
        Assert.Equal(live.Sent.Select(WireJson.Serialize), replay.Sent.Select(WireJson.Serialize));
    }

    [Fact]
    public void MatchWinningExplosionKeepsLateDamageBeforeDeferredResultsInLiveAndReplay()
    {
        var live = new Session(maxRounds: 2);
        live.Engine.DeferNextFrames = true;
        live.StartRound(1);
        live.EndRound(Side.Ct, 8);
        live.StartRound(2);
        live.Send(new BombPlantedInput(live.T, BombSite.A));
        live.Send(new BombExplodedInput());
        live.EndRound(Side.T, 1); // Team 1 wins 2-0 after the halftime swap.
        live.Send(new MapEndInput()); // The win panel must share the pending finalization.
        Assert.True(live.Match.IsLive);
        Assert.Empty(live.Sent.OfType<MapEnd>());

        live.Kill(live.Ct, live.T, 49);
        live.Send(new DamageExitInput());
        Assert.Empty(live.Sent.OfType<MapEnd>());
        live.Engine.RunNextFrame();
        live.Send(new MapEndInput());
        live.Engine.RunNextFrame();
        live.Engine.Advance(61);

        var replay = new Session(replay: true, maxRounds: 2);
        GameplayReplay.Run(live.Tape, replay.Callbacks, replay.Replay!);
        foreach (var session in new[] { live, replay })
        {
            Assert.Equal(MatchPhase.PostLive, session.Match.Phase);
            var hurt = Assert.Single(session.Sent.OfType<PlayerHurt>());
            var death = Assert.Single(session.Sent.OfType<PlayerDeath>());
            Assert.Equal(49, hurt.weapon.id);
            Assert.Equal(49, death.weapon.id);
            Assert.Equal(2, hurt.round);
            Assert.Equal(2, death.round);
            var end = Assert.Single(session.Sent.OfType<MapEnd>());
            Assert.Equal(2, end.team1Score);
            Assert.Equal(0, end.team2Score);
            Assert.True(session.Sent.IndexOf(hurt) < session.Sent.IndexOf(end));
            Assert.True(session.Sent.IndexOf(death) < session.Sent.IndexOf(end));
            Assert.Single(session.Engine.Commands, c => c.Command == "mp_match_restart_delay 86400");
            Assert.Single(session.Engine.Commands, c => c.Command == "quit");
        }
    }

    [Fact]
    public void EveryoneLeavingEndsTheMapInLiveAndReplay()
    {
        var live = new Session(maxRounds: 2);
        live.StartRound(1);
        var leaving = live.Engine.PlayerList.ToList();
        live.Engine.PlayerList.Clear();
        foreach (var player in leaving) live.Send(new PlayerDisconnectInput(player, "disconnect_by_user"));
        live.Engine.Advance(1);

        var replay = new Session(replay: true, maxRounds: 2);
        GameplayReplay.Run(live.Tape, replay.Callbacks, replay.Replay!);
        foreach (var session in new[] { live, replay })
        {
            Assert.Equal(MatchPhase.PostLive, session.Match.Phase);
            // The first departure already empties the server; the map ends before the second is reported.
            Assert.Single(session.Sent.OfType<PlayerDisconnect>());
            var end = Assert.Single(session.Sent.OfType<MapEnd>());
            // An abandoned live map keeps its score and names no winner; the controller decides.
            Assert.Equal(MapEndReasons.Abandoned, end.reason);
            Assert.Null(end.winnerTeam);
            Assert.Null(end.teams);
        }
    }

    [Fact]
    public void CapturedHalftimeOrderingUsesTheLiveCoordinator()
    {
        var session = new Session(maxRounds: 2);
        session.StartRound(1);
        var fixture = JsonDocument.Parse(File.ReadAllText(Path.Combine(Fixture.PluginRoot(), "tests", "fixtures", "halftime-projectile.json")));
        using (fixture)
        foreach (var step in fixture.RootElement.GetProperty("steps").EnumerateArray())
        {
            session.Engine.Clock = step.GetProperty("tick").GetDouble() / 64;
            session.Send(GameplayRecording.Decode(step.GetProperty("record")), step.GetProperty("tick").GetDouble());
        }
        Assert.Equal(Side.T, session.Match.Team1Side);
        Assert.False(session.Forwarder.IsTrackedGrenade(256));
        Assert.Empty(session.Sent.OfType<GrenadeThrow>());
        var replay = new Session(replay: true, maxRounds: 2);
        GameplayReplay.Run(session.Tape, replay.Callbacks, replay.Replay!);
        Assert.Equal(Side.T, replay.Match.Team1Side);
        Assert.Empty(replay.Sent.OfType<GrenadeThrow>());
        Assert.Single(replay.Sent.OfType<RoundEnd>());
    }

    [Fact]
    public void FullGameplayReplayCoversBombOutcomesGrenadesHalftimeAndShutdown()
    {
        var (live, replay) = GameplayScenario.Run();
        AssertMatch(live);
        AssertMatch(replay);
    }

    private static void AssertMatch(Session session)
    {
        Assert.Equal(MatchPhase.PostLive, session.Match.Phase);
        Assert.Equal(6, session.Sent.OfType<RoundEnd>().Count());
        Assert.Equal(Enumerable.Range(1, 6), session.Sent.OfType<RoundComplete>().Select(e => e.round));
        foreach (var complete in session.Sent.OfType<RoundComplete>())
        {
            var index = session.Sent.IndexOf(complete);
            Assert.DoesNotContain(session.Sent.Skip(index + 1),
                e => e is RoundScopedMessage scoped && scoped.round == complete.round);
            Assert.True(index < session.Sent.FindIndex(e => e is MapEnd));
        }
        var end = Assert.Single(session.Sent.OfType<MapEnd>());
        Assert.Equal(2, end.team1Score);
        Assert.Equal(4, end.team2Score);
        Assert.Equal(2, session.Sent.OfType<BombDefused>().Count());
        Assert.Single(session.Sent.OfType<BombExploded>());
        Assert.Equal(3, session.Sent.OfType<PlayerDeath>().Count(d => d.round <= 2));
        var throws = session.Sent.OfType<GrenadeThrow>().ToArray();
        var ends = session.Sent.OfType<GrenadeEnd>().ToDictionary(end => end.grenadeId);
        Assert.Equal(7, throws.Length);
        Assert.Equal(throws.Select(t => t.grenadeId).Order(), ends.Keys.Order());
        Assert.Contains(ends.Values, end => end.fireEnd == FireEnd.Extinguished && end.extinguishedBy is not null);
        Assert.Equal(6, throws.Select(t => t.grenade).Distinct().Count());
        Assert.All(throws, t => Assert.True(ends[t.grenadeId].endRoundTime >= t.roundTime));
        Assert.All(throws, t => Assert.True(session.Sent.IndexOf(t) < session.Sent.IndexOf(ends[t.grenadeId])));
        Assert.True(session.Sent.FindLastIndex(m => m is GrenadeEnd) < session.Sent.FindIndex(m => m is MapEnd));
        Assert.Single(session.Engine.Commands, c => c.Command == "quit");
    }

    [Fact]
    public void CancellationBuyExitFireExpiryAndPrestartReplayThroughHandle()
    {
        var live = new Session();
        live.StartRound(1);
        live.Send(new ExitBuyZoneInput(live.Ct with { EquipmentValue = 4200, Cash = 800 }));
        live.Send(new BombBeginPlantInput(live.T, BombSite.B));
        live.Send(new BombAbortPlantInput(live.T, BombSite.B));
        live.Send(new BombPlantedInput(live.T, BombSite.A));
        live.Send(new BombBeginDefuseInput(live.Ct, true));
        live.Send(new BombAbortDefuseInput(live.Ct));
        live.Send(new GrenadeSpawnedInput(live.T, GrenadeKind.Molotov, 50));
        live.Send(new GrenadeDetonateInput(50, new(1, 1, 1)));
        live.Send(new GrenadeFireInput(50, 51));
        live.Send(new GrenadeRemovedInput(50));
        live.Engine.Clock += 7;
        live.Send(new FireExpiredInput(51));
        live.Send(new GrenadeRemovedInput(51));
        live.EndRound(Side.Ct, 7);
        live.Send(new RoundPrestartInput());
        live.StartRound(2);

        var replay = new Session(replay: true);
        GameplayReplay.Run(live.Tape, replay.Callbacks, replay.Replay!);
        foreach (var session in new[] { live, replay })
        {
            var plant = Assert.Single(session.Sent.OfType<BombAbortPlant>());
            Assert.Equal(BombSite.B, plant.site);
            Assert.Equal(Fixture.Team2Player1, plant.player.steamId);
            Assert.Equal(Fixture.Team1Player1, Assert.Single(session.Sent.OfType<BombAbortDefuse>()).player.steamId);
            Assert.Empty(session.Sent.OfType<BombDefused>());
            var buy = Assert.Single(session.Sent.OfType<RoundBuyEnd>()).players.Single(p => p.player.steamId == Fixture.Team1Player1);
            Assert.Equal(4200, buy.equipmentValue);
            Assert.Equal(800, buy.cash);
            var grenade = Assert.Single(session.Sent.OfType<GrenadeEnd>());
            Assert.Equal(FireEnd.Expired, grenade.fireEnd);
            Assert.Equal(7, grenade.endRoundTime);
            var next = session.Sent.OfType<RoundStart>().Last();
            Assert.NotNull(next.carryover);
            Assert.Equal(1, next.carryover.fromRound);
        }
    }

}
