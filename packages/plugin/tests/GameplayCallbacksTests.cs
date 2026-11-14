using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;
using Session = Ankor.Tests.GameplayTestSession;

namespace Ankor.Tests;

public sealed class GameplayCallbacksTests
{
    [Fact]
    public void FreezeEndSamplesPlayersWhoReconnectedAfterRoundStart()
    {
        var session = new Session(record: false);
        session.Engine.RoundsPlayed = 0;
        var reconnect = session.Engine.PlayerList[1] with { Cash = 0, StartCash = 3200, CashSpent = 3200 };
        session.Engine.PlayerList.RemoveAt(1);
        session.Send(new RoundStartInput());
        Assert.Single(session.Sent.OfType<RoundStart>().Single().players);
        Assert.Empty(session.Sent.OfType<RoundFreezeEnd>());
        session.Engine.PlayerList.Add(reconnect);
        session.Send(new RoundFreezeEndInput());
        var snapshot = Assert.Single(session.Sent.OfType<RoundFreezeEnd>());
        var player = Assert.Single(snapshot.players, p => p.player.steamId == reconnect.SteamId.ToString());
        Assert.Equal(3200, player.startCash);
        Assert.Equal(0, player.cash);
        Assert.Equal(3200, player.cashSpent);
    }

    [Theory]
    [InlineData("complete")]
    [InlineData("reset")]
    [InlineData("phase_change")]
    [InlineData("everyone_left")]
    public void KnifeCompletionSamplesNextFrameAndRejectsStaleCallbacks(string transition)
    {
        var session = new Session(record: false);
        session.Engine.DeferNextFrames = true;
        session.Match.StartKnifeRound();
        session.Send(new RoundStartInput());
        if (transition == "everyone_left")
        {
            var leaving = session.T;
            session.Engine.PlayerList.Clear();
            session.Send(new PlayerDisconnectInput(leaving, "disconnect_by_user"));
        }
        session.Send(new RoundEndInput(Side.T, 9));
        Assert.Empty(session.Sent.OfType<KnifeEnd>());
        // The engine callback happens before all death-side state has settled.
        if (transition != "everyone_left") session.Engine.PlayerList[1] = session.T with { Health = 0, IsAlive = false };
        if (transition == "reset") session.Callbacks.Reset();
        if (transition == "phase_change") session.Match.EnterWarmup();
        session.Engine.RunNextFrame();
        if (transition == "complete")
        {
            Assert.Equal(Side.Ct, Assert.Single(session.Sent.OfType<KnifeEnd>()).winnerSide);
            Assert.Equal(MatchPhase.SideChoice, session.Match.Phase);
        }
        else Assert.Empty(session.Sent.OfType<KnifeEnd>());
        Assert.Empty(session.Sent.OfType<RoundEnd>());
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void DeferredHalftimeDoesNotSwapAfterResetOrLeavingLive(bool reset)
    {
        var session = new Session(maxRounds: 2, record: false);
        session.StartRound(1);
        session.Send(new DamageEnterInput(100));
        session.Send(new RoundEndInput(Side.Ct, 8));
        var cleared = session.Cleared;
        if (reset) session.Callbacks.Reset();
        else session.Match.EnterPostLive();
        session.Send(new DamageExitInput());
        Assert.Equal(Side.Ct, session.Match.Team1Side);
        Assert.Equal(cleared, session.Cleared);
    }

    [Theory]
    [InlineData(17)]
    [InlineData(18)]
    public void SpecialRoundEndReasonsFinishOnceAfterDamage(int reason)
    {
        var session = new Session(record: false);
        session.Engine.DeferNextFrames = true;
        session.StartRound(1);
        session.Send(new DamageEnterInput(100));
        session.Send(new RoundEndInput(Side.Ct, reason));
        session.Send(new MapEndInput());
        Assert.Empty(session.Sent.OfType<MapEnd>());
        session.Send(new DamageExitInput());
        Assert.Empty(session.Sent.OfType<MapEnd>());
        session.Send(new RoundMvpInput(session.Ct, 1));
        session.Engine.RunNextFrame();
        Assert.Equal(MapEndReasons.Played, Assert.Single(session.Sent.OfType<MapEnd>()).reason);
        var complete = Assert.Single(session.Sent.OfType<RoundComplete>());
        Assert.True(session.Sent.FindIndex(e => e is RoundMvp) < session.Sent.IndexOf(complete));
        Assert.True(session.Sent.IndexOf(complete) < session.Sent.FindIndex(e => e is MapEnd));
        Assert.Equal(MatchPhase.PostLive, session.Match.Phase);
        session.Callbacks.Reset();
        // Resetting the latch must not reopen a finished map.
        var leaving = session.T;
        session.Engine.PlayerList.Clear();
        session.Send(new PlayerDisconnectInput(leaving, "disconnect_by_user"));
        Assert.Single(session.Sent.OfType<MapEnd>());
    }

    [Fact]
    public void WarmupNonLiveAndSpectatorResultsCannotScoreOrEndTheMap()
    {
        var session = new Session(record: false);
        session.StartRound(1);
        session.Send(new RoundEndInput(Side.Spec, 0));
        session.Engine.IsWarmup = true;
        session.Send(new RoundEndInput(Side.Ct, 8));
        session.Engine.IsWarmup = false;
        session.Match.EnterWarmup();
        session.Send(new RoundEndInput(Side.Ct, 8));
        session.Send(new MapEndInput());
        Assert.Empty(session.Sent.OfType<RoundEnd>());
        Assert.Empty(session.Sent.OfType<MapEnd>());
    }

    [Fact]
    public void EveryoneLeavingTheKnifeRoundEmitsResultsAndStartsShutdownOnce()
    {
        var host = new FakeGameHost { DeferNextFrames = true };
        var sent = new List<PluginMessage>();
        using var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(sent), Fixture.Messages, () => { });
        var forwarder = new EventForwarder(match, host);
        var shutdowns = 0;
        var cleared = 0;
        var callbacks = new GameplayCallbacks(match, host, forwarder, new DamageTracker(forwarder),
            new TeamLock(match, host, Fixture.Messages),
            () => cleared++, () =>
            {
                Assert.Equal(MatchPhase.PostLive, match.Phase);
                Assert.Single(sent.OfType<MapEnd>());
                shutdowns++;
            }, () => { }, () => { });
        match.StartKnifeRound();

        callbacks.Handle(new MapEndInput()); // A normal win-panel callback cannot end a pre-live match.
        Assert.Equal(0, shutdowns);
        var leaving = Fixture.Player(Fixture.Team1Player1, Side.Ct);
        callbacks.Handle(new PlayerDisconnectInput(leaving, "disconnect_by_user"));
        callbacks.Handle(new PlayerDisconnectInput(leaving, "disconnect_by_user"));
        host.RunNextFrame();
        callbacks.Handle(new MapEndInput());

        Assert.Empty(sent.OfType<MapEnd>());
        Assert.Equal(0, shutdowns);
        host.RunNextFrame();
        Assert.Equal(MatchPhase.PostLive, match.Phase);
        Assert.Single(sent.OfType<MapEnd>());
        Assert.Equal(1, shutdowns);
        Assert.Equal(1, cleared);
    }

    [Fact]
    public void AdministrativeTerminationWaitsForOutermostDamageBeforeClosingAndShuttingDown()
    {
        var session = new Session(record: false);
        session.Engine.DeferNextFrames = true;
        session.StartRound(1);
        session.Send(new DamageEnterInput(99));
        session.Send(new DamageEnterInput(100));
        session.Send(new MapEndInput());
        session.Send(new MapEndInput());
        session.Send(new DamageExitInput());
        session.Engine.RunNextFrame();
        Assert.True(session.Match.IsLive);
        Assert.Empty(session.Sent.OfType<MapEnd>());
        Assert.DoesNotContain(session.Engine.CommandNames, c => c == "mp_match_restart_delay 86400");

        // Events within the active damage operation must still be emitted before the map closes.
        session.Send(new PlayerHurtInput(session.T, session.Ct, new(7, null), 20, 0, 1, 100));
        session.Send(new DamageExitInput());
        session.Send(new MapEndInput());

        Assert.Equal(MatchPhase.PostLive, session.Match.Phase);
        Assert.Single(session.Sent.OfType<MapEnd>());
        Assert.Single(session.Sent.OfType<PlayerHurt>());
        Assert.True(session.Sent.FindIndex(e => e is PlayerHurt) < session.Sent.FindIndex(e => e is MapEnd));
        Assert.Empty(session.Sent.OfType<RoundComplete>()); // An interrupted round has no result.
        Assert.Single(session.Engine.Commands, c => c.Command == "mp_match_restart_delay 86400");
        session.Engine.Advance(61);
        Assert.Single(session.Engine.Commands, c => c.Command == "quit");
    }

    [Theory]
    [InlineData(false, false)]
    [InlineData(false, true)]
    [InlineData(true, false)]
    [InlineData(true, true)]
    public void ResetOrUnloadInvalidatesFinalizationAtEitherDeferredBoundary(bool unload, bool waitingForDamage)
    {
        var session = new Session(record: false);
        session.Engine.DeferNextFrames = true;
        session.StartRound(1);
        var cleared = session.Cleared;
        session.Send(new DamageEnterInput(100));
        session.Send(new MapEndInput());
        if (waitingForDamage) session.Engine.RunNextFrame();

        if (unload) session.Match.Dispose();
        else session.Callbacks.Reset();
        session.Engine.RunNextFrame();
        session.Send(new DamageExitInput());

        Assert.Empty(session.Sent.OfType<MapEnd>());
        Assert.Equal(cleared, session.Cleared);
        Assert.DoesNotContain("mp_match_restart_delay 86400", session.Engine.CommandNames);
    }

    [Fact]
    public void PrestartCompletesExitPlayBeforeNextRoundExactlyOnce()
    {
        var session = new Session(record: false);
        session.StartRound(1);
        session.Send(new WeaponFireInput(session.Ct, new(7, null)));
        session.Send(new RoundEndInput(Side.Ct, 8));
        Assert.Empty(session.Sent.OfType<RoundComplete>());
        session.Send(new PlayerHurtInput(session.T, session.Ct, new(7, null), 20, 0, 1, 100));
        session.Send(new RoundMvpInput(session.Ct, 1));
        session.Send(new RoundPrestartInput());
        var complete = Assert.Single(session.Sent.OfType<RoundComplete>());
        Assert.Equal(1, complete.round);
        Assert.True(session.Sent.FindIndex(e => e is PlayerHurt) < session.Sent.IndexOf(complete));
        Assert.True(session.Sent.FindIndex(e => e is RoundMvp) < session.Sent.IndexOf(complete));
        Assert.True(session.Sent.FindIndex(e => e is RoundShots) < session.Sent.IndexOf(complete));
        session.StartRound(2);
        Assert.Single(session.Sent.OfType<RoundComplete>());
        Assert.True(session.Sent.IndexOf(complete) < session.Sent.FindIndex(e => e is RoundStart { round: 2 }));
    }

    [Fact]
    public void HalftimeCompletesAfterMvpWithoutWaitingForNextRoundAndDoesNotCompleteTwice()
    {
        var session = new Session(maxRounds: 2, record: false);
        session.Engine.DeferNextFrames = true;
        session.StartRound(1);
        session.Send(new DamageEnterInput(100));
        session.Send(new RoundEndInput(Side.Ct, 8));
        session.Send(new RoundMvpInput(session.Ct, 1));
        session.Engine.RunNextFrame();
        Assert.Empty(session.Sent.OfType<RoundComplete>());
        session.Send(new PlayerHurtInput(session.T, session.Ct, new(7, null), 20, 0, 1, 100));
        session.Send(new DamageExitInput());
        var complete = Assert.Single(session.Sent.OfType<RoundComplete>());
        Assert.Equal(1, complete.round);
        Assert.True(session.Sent.FindIndex(e => e is PlayerHurt) < session.Sent.IndexOf(complete));
        Assert.True(session.Sent.FindIndex(e => e is RoundMvp) < session.Sent.IndexOf(complete));
        session.Send(new RoundPrestartInput());
        session.StartRound(2);
        Assert.Single(session.Sent.OfType<RoundComplete>());
    }

    private sealed record UnsupportedInput : GameplayInput;

    [Fact]
    public void WarmupCommandInputsReachTheirHandlers()
    {
        var host = new FakeGameHost();
        var match = new MatchState(host, Fixture.Config(readyMode: ReadyMode.Manual), new FakeAgentChannel(), Fixture.Messages, () => { });
        var forwarder = new EventForwarder(match, host);
        var callbacks = new GameplayCallbacks(match, host, forwarder, new DamageTracker(forwarder),
            new TeamLock(match, host, Fixture.Messages), () => { }, () => { }, () => { }, () => { });
        match.EnterWarmup();
        var captain = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0, name: "cap");
        host.PlayerList.AddRange([captain, Fixture.Player(Fixture.Team2Player1, Side.T, 1)]);

        callbacks.Handle(new ClientAuthorizedInput(Fixture.Player(Fixture.Stranger, Side.Spec, 5)));
        Assert.Equal(Fixture.Stranger, Assert.Single(host.Kicked).Player.SteamIdString);

        callbacks.Handle(new ReadyInput(captain, true));
        Assert.Contains(host.Chat, line => line.Contains("cap") && line.Contains("is ready"));

        callbacks.Handle(new SideChoiceForSideInput(captain, Side.T));
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        callbacks.Handle(new ForceReadyInput(captain));
        Assert.Contains(host.Chat, line => line.Contains("force-readied"));
    }

    [Fact]
    public void AClientThatFullyConnectsUnverifiedIsKickedWhenSteamDoesNotVerifyIt()
    {
        var host = new FakeGameHost();
        var match = new MatchState(host, Fixture.Config(SidesMode.Team1Ct), new FakeAgentChannel(), Fixture.Messages, () => { });
        var forwarder = new EventForwarder(match, host);
        var callbacks = new GameplayCallbacks(match, host, forwarder, new DamageTracker(forwarder),
            new TeamLock(match, host, Fixture.Messages), () => { }, () => { }, () => { }, () => { });
        match.EnterWarmup();
        var claimed = Fixture.Player(Fixture.Team1Player1, Side.T) with { Authorized = false };
        host.PlayerList.Add(claimed);

        callbacks.Handle(new PlayerConnectInput(claimed));
        host.Advance(TeamLock.AuthorizationGraceSeconds);
        Assert.Equal(claimed, Assert.Single(host.Kicked).Player);
    }

    [Fact]
    public void AReportedHitLeavesTheOpenDamageCallAtTheReportedHealth()
    {
        var host = new FakeGameHost();
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(), Fixture.Messages, () => { });
        var forwarder = new EventForwarder(match, host);
        var damage = new DamageTracker(forwarder);
        var callbacks = new GameplayCallbacks(match, host, forwarder, damage,
            new TeamLock(match, host, Fixture.Messages), () => { }, () => { }, () => { }, () => { });

        callbacks.Handle(new DamageEnterInput(100));
        callbacks.Handle(new DamageSnapshotInput(new DamageSnapshot(100, 0, new WeaponReference(7, null), 200, []), null));
        callbacks.Handle(new DamageReportedInput(100, 40));

        Assert.Equal(40, damage.Current(100)?.Health);
    }

    [Fact]
    public void AMapLoadWhileTheMatchEndsResetsNothingAndStartsNoWarmup()
    {
        var session = new Session(record: false);
        session.StartRound(1);
        session.Match.BeginEnding();
        var cleared = session.Cleared;
        var phase = session.Match.Phase;
        session.Engine.Commands.Clear();

        session.Send(new MapLoadInput());
        session.Engine.Advance(5);

        Assert.Equal(cleared, session.Cleared);
        Assert.Equal(phase, session.Match.Phase);
        Assert.Empty(session.Engine.Commands);
    }

    [Fact]
    public void AMatchThatEndsBeforeTheLoadedMapsRulesAreReadPlacesNobody()
    {
        var session = new Session(record: false);
        session.StartRound(1);
        session.Send(new MapLoadInput());
        session.Match.BeginEnding();
        var misplaced = session.Engine.PlayerList[0] with { Side = Side.T };
        session.Engine.PlayerList[0] = misplaced;

        session.Engine.Advance(1);

        Assert.Empty(session.Engine.SwitchedTeams);
        Assert.Empty(session.Engine.Kicked);
    }

    [Fact]
    public void UnknownInMemoryInputsFailRatherThanSilentlySkippingAHandler() =>
        Assert.Throws<ArgumentOutOfRangeException>(() => new Session(record: false).Callbacks.Handle(new UnsupportedInput()));

}
