using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class MatchStateTests
{
    private static (MatchState Match, FakeGameHost Host, List<PluginMessage> Sent) Build(
        MatchConfig? config = null)
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        var match = new MatchState(host, config ?? Fixture.Config(), new FakeAgentChannel(sent), Fixture.Messages, () => { });
        return (match, host, sent);
    }

    /// <summary>The start countdown, told from warmup's one-second panel ticker by its length.</summary>
    private static Predicate<FakeGameHost.FakeTimer> Countdown(FakeGameHost host) =>
        timer => !timer.Fired && !timer.Cancelled && timer.DueAt > host.Clock + 1;

    private static void ConnectFullRoster(FakeGameHost host)
    {
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0),
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 1),
            Fixture.Player(Fixture.Team2Player1, Side.T, 2),
            Fixture.Player(Fixture.Team2Player2, Side.T, 3),
        ]);
    }

    [Fact]
    public void RestartedWarmupIgnoresPreviousCountdownCallback()
    {
        var (match, host, _) = Build();
        ConnectFullRoster(host);
        match.EnterWarmup();
        match.MaybeStart();
        var old = host.Timers.Find(Countdown(host))!;
        host.Advance(5);
        match.EnterWarmup();
        match.MaybeStart();
        old.Callback();
        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Single(host.Timers, Countdown(host));
        host.Advance(13);
        Assert.Equal(MatchPhase.Warmup, match.Phase);
        host.Advance(1);
        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    // Warmup and the go-live gate

    [Theory]
    [InlineData(SidesMode.Team1Ct, "Alpha", "Bravo")]
    [InlineData(SidesMode.Team1T, "Bravo", "Alpha")]
    public void TeamNamesAreInitializedButNotRewrittenAtEngineSideSwaps(SidesMode sides, string ct, string t)
    {
        var (match, host, _) = Build(Fixture.Config(sides: sides));
        match.EnterWarmup();
        Assert.Equal(ct, host.StringConVars["mp_teamname_1"]);
        Assert.Equal(t, host.StringConVars["mp_teamname_2"]);
        // Regulation and overtime swaps are owned by the engine, not the plugin.
        for (var half = 0; half < 4; half++)
        {
            host.StringConVars.Clear();
            match.SwapSides();
            match.OnRoundStart();
            Assert.Empty(host.StringConVars);
        }
    }

    [Fact]
    public void KnifeSwapUpdatesNames()
    {
        var (match, host, _) = AtSideChoice(knifeWinner: Side.T);
        match.TrySideChoice(Fixture.Player(Fixture.Team2Player1, Side.T), SideChoiceKind.Swap);
        host.IsWarmup = false;
        match.OnRoundStart();
        host.RunScheduledRestart();
        match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.Equal("Bravo", host.StringConVars["mp_teamname_1"]);
        Assert.Equal("Alpha", host.StringConVars["mp_teamname_2"]);
    }

    [Fact]
    public void KnifeSwapNamesWinOverTheNamesMpSwapteamsSwaps()
    {
        var (match, host, _) = AtSideChoice(knifeWinner: Side.T);
        match.TrySideChoice(Fixture.Player(Fixture.Team2Player1, Side.T), SideChoiceKind.Swap);
        host.IsWarmup = false;
        match.OnRoundStart();
        // As mp_swapteams does: the names move with the teams.
        (host.StringConVars["mp_teamname_1"], host.StringConVars["mp_teamname_2"]) =
            (host.StringConVars["mp_teamname_2"], host.StringConVars["mp_teamname_1"]);
        host.RunScheduledRestart();
        match.OnRoundStart();
        Assert.Equal("Bravo", host.StringConVars["mp_teamname_1"]);
        Assert.Equal("Alpha", host.StringConVars["mp_teamname_2"]);
    }

    [Fact]
    public void ConfigTeamNamesRemainLiteral()
    {
        var config = Fixture.Config();
        config.roster.team1.name = "Alpha \"quoted\"; quit";
        var (match, host, _) = Build(config);
        match.EnterWarmup();
        Assert.Contains(config.roster.team1.name, host.StringConVars.Values);
        Assert.DoesNotContain(host.CommandNames, command => command.Contains("quoted"));
    }

    [Fact]
    public void WarmupExecsTheWarmupCfgAndWaitsForTheRoster()
    {
        var (match, host, _) = Build();
        match.EnterWarmup();

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Contains(Fixture.WarmupCfgs, host.CommandNames);
        Assert.DoesNotContain("exec ankor/knife", host.CommandNames);
    }



    [Fact]
    public void TheKnifeRoundStartsOnceEveryRosteredPlayerIsConnected()
    {
        var (match, host, sent) = Build();
        match.EnterWarmup();

        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0));
        match.MaybeStart();
        host.Advance(14);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        host.PlayerList.Clear();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
        Assert.Contains("exec ankor/knife", host.CommandNames);
        Assert.Contains($"mp_warmuptime {MatchState.CountdownSeconds}", host.CommandNames);
        Assert.DoesNotContain("mp_restartgame 1", host.CommandNames);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Single(sent.OfType<KnifeStart>());
    }

    [Fact]
    public void KnifeAnnouncementWaitsForTheActualRoundAndOnlyHappensOnce()
    {
        var (match, host, sent) = Build();
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        match.OnRoundStart(); // A warmup callback must not announce knife.
        Assert.Empty(sent.OfType<KnifeStart>());
        Assert.DoesNotContain(host.Chat, message => message == Fixture.Messages.Prefixed("knife.start"));
        host.IsWarmup = false;
        match.OnRoundStart();
        match.OnRoundStart();
        Assert.Single(sent.OfType<KnifeStart>());
        Assert.Single(host.Chat, message => message == Fixture.Messages.Prefixed("knife.start"));
    }

    [Theory]
    [InlineData(SidesMode.Knife, "knife", "warmup.countdown_knife")]
    [InlineData(SidesMode.Team1Ct, "live", "warmup.countdown_live")]
    public void WarmupCountdownKeepsWarmupRulesUntilTheFinalSecond(SidesMode sides, string cfg, string announcement)
    {
        var (match, host, _) = Build(Fixture.Config(sides));
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        match.MaybeStart();
        Assert.Single(host.Timers, Countdown(host));
        Assert.Contains($"mp_warmuptime {MatchState.CountdownSeconds}", host.CommandNames);
        Assert.Contains(Fixture.Messages.Prefixed(announcement, MatchState.CountdownSeconds), host.Chat);
        host.Advance(MatchState.CountdownSeconds - 1.1);
        Assert.Equal(MatchPhase.Warmup, match.Phase);
        var phaseCfgs = cfg == "knife" ? $"exec {CfgNames.Knife}" : Fixture.LiveCfgs;
        Assert.DoesNotContain(phaseCfgs, host.CommandNames);
        host.Advance(0.1);
        Assert.Contains(phaseCfgs, host.CommandNames);
        // The knife cfg leaves the countdown's clock alone; going live ends warmup itself.
        Assert.Equal(cfg == "live", host.CommandNames.Contains("mp_warmup_end"));
        Assert.DoesNotContain("mp_restartgame 1", host.CommandNames);
    }

    [Theory]
    [InlineData(SidesMode.Team1Ct, MatchPhase.Live)]
    [InlineData(SidesMode.Knife, MatchPhase.Knife)]
    public void ALeaveDuringTheCountdownDoesNotStopIt(SidesMode sides, MatchPhase started)
    {
        var (match, host, _) = Build(Fixture.Config(sides: sides));
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(5);
        host.PlayerList.RemoveAt(0);
        host.Advance(10);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Equal(started, match.Phase);
    }

    [Fact]
    public void UnloadCancelsTheWarmupCountdown()
    {
        var (match, host, _) = Build();
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        match.Dispose();
        host.Advance(30);
        Assert.DoesNotContain("exec ankor/knife", host.CommandNames);
        Assert.DoesNotContain(host.Timers, t => !t.Fired);
    }

    [Fact]
    public void AnEmptyServerNeverStartsTheMatch()
    {
        var (match, host, sent) = Build();
        match.EnterWarmup();
        match.MaybeStart();
        host.Advance(14);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Empty(sent.OfType<KnifeStart>());
    }


    [Theory]
    [InlineData(SidesMode.Team1Ct, Side.Ct)]
    [InlineData(SidesMode.Team1T, Side.T)]
    public void AFixedSidesModeSkipsTheKnifeRound(SidesMode sides, Side expected)
    {
        var (match, host, sent) = Build(Fixture.Config(sides: sides));
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Equal(expected, match.Team1Side);
        Assert.Contains(Fixture.LiveCfgs, host.CommandNames);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.Empty(sent.OfType<KnifeStart>());
    }

    // Force ready (captains only, under manual ready-up)

    private static MatchConfig ManualConfig(SidesMode sides = SidesMode.Knife) =>
        Fixture.Config(sides, readyMode: ReadyMode.Manual);

    /// <summary>Warmup with one rostered player on each team — both captains — and nobody else.</summary>
    private static (MatchState Match, FakeGameHost Host, List<PluginMessage> Sent) AtOneVersusOne()
    {
        var (match, host, sent) = Build(ManualConfig());
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0),
            Fixture.Player(Fixture.Team2Player1, Side.T, 1),
        ]);

        // The normal gate is unchanged: two of four rostered players is not a full roster.
        match.MaybeStart();
        host.Advance(14);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        return (match, host, sent);
    }

    [Theory]
    [InlineData(0)] // team 1's captain first
    [InlineData(1)] // team 2's captain first: the roster's index 0 on that team
    public void BothCaptainsForceReadyingStartsTheMatchWithOnePlayerOnEachTeam(int first)
    {
        var (match, host, sent) = AtOneVersusOne();

        Assert.True(match.TryForceReady(host.PlayerList[first]));
        Assert.Contains(host.Chat, line => line.Contains("force-readied"));
        host.Advance(14);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        Assert.True(match.TryForceReady(host.PlayerList[1 - first]));
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
        Assert.Contains("exec ankor/knife", host.CommandNames);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Single(sent.OfType<KnifeStart>());
    }

    [Fact]
    public void ForceReadyHonoursTheFixedSidesPathJustLikeAFullRoster()
    {
        var (match, host, _) = Build(ManualConfig(SidesMode.Team1T));
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.T, 0),
            Fixture.Player(Fixture.Team2Player1, Side.Ct, 1),
        ]);

        Assert.True(match.TryForceReady(host.PlayerList[0]));
        Assert.True(match.TryForceReady(host.PlayerList[1]));
        host.Advance(14);
        Assert.Equal(MatchPhase.Warmup, match.Phase);
        host.IsWarmup = false;
        match.OnRoundStart();

        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.Equal(Side.T, match.Team1Side);
    }

    [Fact]
    public void ARosteredNonCaptainCannotForceReady()
    {
        var (match, host, sent) = Build(ManualConfig());
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 0),
            Fixture.Player(Fixture.Team2Player1, Side.T, 1),
        ]);

        Assert.False(match.TryForceReady(host.PlayerList[0]));

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Empty(sent.OfType<KnifeStart>());
        Assert.Contains("Only captains can force-ready their team", Assert.Single(host.DirectChat).Message);

        // The teammate who is the captain still gets through.
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct, 2));
        Assert.True(match.TryForceReady(host.PlayerList[2]));
    }

    [Fact]
    public void AnUnrosteredCallerCannotForceReady()
    {
        var (match, host, _) = AtOneVersusOne();
        var stranger = Fixture.Player(Fixture.Stranger, Side.Ct, 5);

        Assert.False(match.TryForceReady(stranger));

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Contains("Only captains can force-ready their team", Assert.Single(host.DirectChat).Message);
    }

    [Fact]
    public void AForceReadiedTeamWaitsForTheOtherTeam()
    {
        var (match, host, sent) = Build(ManualConfig());
        match.EnterWarmup();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0));

        Assert.True(match.TryForceReady(host.PlayerList[0]));
        host.Advance(60);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Empty(sent.OfType<KnifeStart>());
    }

    [Theory]
    [InlineData(0, 0, "mp_backup_round_auto 1")] // casual
    [InlineData(0, 1, "mp_backup_round_auto 1")] // competitive
    [InlineData(0, 2, "mp_backup_round_auto 1")] // wingman
    public void GoingLiveBacksRoundsUpInEveryMatchMode(int gameType, int gameMode, string command)
    {
        var (match, host, _) = Build(Fixture.Config(SidesMode.Team1Ct));
        host.ConVars["game_type"] = gameType;
        host.ConVars["game_mode"] = gameMode;
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0),
            Fixture.Player(Fixture.Team2Player1, Side.T, 1),
        ]);

        Assert.Null(match.StartFromController());
        host.Advance(14);

        Assert.Contains(command, host.CommandNames);
        Assert.Single(host.CommandNames, name => name.StartsWith("mp_backup_round_auto", StringComparison.Ordinal));
    }

    [Fact]
    public void ABackupLoadIsRefusedDuringTheSideChoice()
    {
        var (match, _, sent) = AtSideChoice();

        Assert.False(match.BeforeBackupLoad());

        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Single(sent.OfType<PluginLog>(), log => log.level == LogLevel.Warn && log.message.Contains("warmup or during live play"));
    }

    [Fact]
    public void NothingStartsASecondCountdownWhileTheMatchGoesLive()
    {
        var (match, host, _) = Build(Fixture.Config(SidesMode.Team1Ct));
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0),
            Fixture.Player(Fixture.Team2Player1, Side.T, 1),
        ]);
        Assert.Null(match.StartFromController());
        host.Advance(14);
        Assert.Equal(MatchPhase.Warmup, match.Phase);
        host.Chat.Clear();

        Assert.Equal(CommandResult.NotInWarmup, match.StartFromController());
        match.MaybeStart();

        Assert.Empty(host.Chat);
        Assert.Single(host.CommandNames, name => name == "mp_warmuptime 15");
    }

    [Fact]
    public void AutoModeRefusesForceReady()
    {
        var (match, host, _) = Build(Fixture.Config(readyMode: ReadyMode.Auto));
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0),
            Fixture.Player(Fixture.Spectator, Side.Spec, 1),
        ]);

        Assert.False(match.TryForceReady(host.PlayerList[0]));
        Assert.False(match.TryForceReady(host.PlayerList[1]));

        Assert.All(host.DirectChat, line => Assert.Contains("starts on its own", line.Message));
        Assert.Equal(2, host.DirectChat.Count);
    }

    [Fact]
    public void ACaptainForceReadiesTheirTeamOnce()
    {
        var (match, host, _) = AtOneVersusOne();

        Assert.True(match.TryForceReady(host.PlayerList[0]));
        Assert.False(match.TryForceReady(host.PlayerList[0]));

        Assert.Contains("Already force-readied", Assert.Single(host.DirectChat).Message);
    }

    [Fact]
    public void ForceReadyIsRefusedOutsideWarmup()
    {
        var (match, host, _) = AtOneVersusOne();
        Assert.True(match.TryForceReady(host.PlayerList[0]));
        Assert.True(match.TryForceReady(host.PlayerList[1]));
        host.Advance(14);
        host.DirectChat.Clear();

        // Already knifing: a second call must not restart anything.
        Assert.False(match.TryForceReady(host.PlayerList[0]));

        Assert.Equal(MatchPhase.Knife, match.Phase);
        Assert.Empty(host.DirectChat);
    }


    [Fact]
    public void ForceReadyDuringTheCountdownIsRefusedWithoutAReply()
    {
        var (match, host, _) = AtOneVersusOne();
        Assert.True(match.TryForceReady(host.PlayerList[0]));
        Assert.True(match.TryForceReady(host.PlayerList[1]));
        host.DirectChat.Clear();

        Assert.False(match.TryForceReady(host.PlayerList[0]));

        Assert.Empty(host.DirectChat);
    }

    [Fact]
    public void ANonCaptainIsToldToAskTheirConnectedCaptain()
    {
        var (match, host, _) = Build(ManualConfig());
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0, name: "cap"),
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 1),
        ]);

        Assert.False(match.TryForceReady(host.PlayerList[1]));

        Assert.Equal((1, Fixture.Messages.Prefixed("forceready.not_captain_ask", "cap")), Assert.Single(host.DirectChat));
    }

    [Fact]
    public void ReadyIsRefusedBeforeWarmupAndOnceTheMatchEnds()
    {
        var (match, host, _) = Build(ManualConfig());
        var player = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);
        host.PlayerList.Add(player);

        // The map is still loading.
        Assert.False(match.TrySetReady(player, true));
        Assert.Empty(host.Chat);
        Assert.Empty(host.DirectChat);

        match.EnterWarmup();
        match.BeginEnding();
        host.Chat.Clear();

        Assert.False(match.TrySetReady(player, true));
        Assert.Empty(host.Chat);
        Assert.Empty(host.DirectChat);
    }

    [Fact]
    public void WarmupRemindersStopOnceTheMatchLeavesWarmup()
    {
        var (match, host, _) = Build(ManualConfig());
        match.EnterWarmup();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0));
        match.StartKnifeRound();
        host.Chat.Clear();

        host.Advance(90);

        Assert.Empty(host.Chat);
    }

    [Fact]
    public void TheReadyDeadlineStopsCountingOnceTheMatchLeavesWarmup()
    {
        var (match, host, sent) = Build(Fixture.Config(readyTimeoutSeconds: 5));
        var expired = 0;
        match.ReadyExpired += () => expired++;
        match.EnterWarmup();
        match.StartKnifeRound();
        host.Chat.Clear();

        host.Advance(60);

        Assert.Equal(0, expired);
        Assert.Empty(host.Chat);
        Assert.Empty(sent.OfType<PluginLog>());
    }

    [Fact]
    public void AnEndingMatchStartsNoKnifeRoundAndDoesNotGoLive()
    {
        var (match, host, _) = Build();
        match.EnterWarmup();
        match.BeginEnding();
        host.Commands.Clear();

        match.StartKnifeRound();
        match.GoLive();

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Empty(host.Commands);
    }

    [Fact]
    public void TheWaitingAnnouncementStillCountsTheWholeRoster()
    {
        var (match, host, _) = Build();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0));

        match.EnterWarmup();

        // Force start does not change what the server tells people it is waiting for: three of
        // the four rostered players are still missing.
        Assert.Equal(Fixture.Messages.Prefixed("warmup.waiting_many", 3), Assert.Single(host.Chat));
    }

    // Restoring a launch's backup

    /// <summary>What the engine does with a loaded file: puts every rostered player on the side the round had them.</summary>
    private static void Seat(FakeGameHost host, Side team1Side)
    {
        for (var i = 0; i < host.PlayerList.Count; i++)
        {
            var player = host.PlayerList[i];
            if (player.SteamIdString is Fixture.Team1Player1 or Fixture.Team1Player2) host.PlayerList[i] = player with { Side = team1Side };
            else host.PlayerList[i] = player with { Side = team1Side.Opposite() };
        }
    }

    private static (MatchState Match, FakeGameHost Host, List<PluginMessage> Sent) AtRestoreLoad(Side team1Side = Side.T)
    {
        var (match, host, sent) = Build(Fixture.Config(sides: SidesMode.Knife, restore: Fixture.Restore(team1Side)));
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        Assert.Contains(Fixture.Messages.Prefixed("warmup.countdown_restore", MatchState.CountdownSeconds), host.Chat);
        host.Advance(14);
        return (match, host, sent);
    }

    /// <summary>
    /// A launch's restore into the second half: team 1 plays T in round 14 of 24, so it played CT in
    /// the first half, and its name goes in mp_teamname_1, the first half's CT team.
    /// </summary>
    [Fact]
    public void ALaunchRestoreIntoTheSecondHalfNamesTheTeamThatPlayedCtInTheFirstHalfFirst()
    {
        var (match, host, _) = AtRestoreLoad(Side.T);
        host.ConVars["mp_halftime"] = 1;
        host.ConVars["mp_maxrounds"] = 24;
        host.IsWarmup = false;
        match.OnRoundStart();
        host.RoundsPlayed = 13;
        Seat(host, Side.T);
        host.LoadBackup(match);
        match.OnRoundStart();

        Assert.Equal((MatchPhase.Live, Side.T), (match.Phase, match.Team1Side));
        Assert.Equal(("Alpha", "Bravo"), (host.StringConVars["mp_teamname_1"], host.StringConVars["mp_teamname_2"]));
    }

    [Fact]
    public void ARestoreSkipsTheKnifeRoundAndLoadsTheBackupFromTheFirstLiveRound()
    {
        var (match, host, sent) = AtRestoreLoad();
        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.Equal(Side.T, match.Team1Side);
        Assert.Contains(Fixture.LiveCfgs, host.CommandNames);
        Assert.DoesNotContain("exec ankor/knife", host.CommandNames);
        // The engine's round 1 before the load writes ankor_round00.txt, the reset point of a restore_round 1.
        Assert.Equal([Fixture.LiveCfgs, "mp_backup_round_auto 1", "mp_backup_round_file ankor/backups/ankor"],
            host.CommandNames.SkipWhile(command => command != Fixture.LiveCfgs).Take(3));
        Assert.DoesNotContain(host.CommandNames, command => command.StartsWith("mp_backup_restore_load_file"));

        // The engine's warmup end starts round 1; the load goes out a frame later, not inside the callback.
        host.IsWarmup = false;
        host.DeferNextFrames = true;
        match.OnRoundStart();
        Assert.DoesNotContain(host.CommandNames, command => command.StartsWith("mp_backup_restore_load_file"));
        host.RunNextFrame();
        Assert.Equal(["mp_backup_restore_load_file ankor/restore.txt"],
            host.CommandNames.Where(command => command.StartsWith("mp_backup_restore")));
        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.Empty(sent.OfType<GoingLive>());
        Assert.False(match.IsLive);

        // The restored round: the engine has read the file and seated everyone, and the plugin reads the engine.
        host.RoundsPlayed = 13;
        host.TeamScores[Side.T] = 7;
        host.TeamScores[Side.Ct] = 6;
        Seat(host, Side.T);
        host.LoadBackup(match);
        match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.True(match.IsLive);
        Assert.Equal(14, match.Round);
        Assert.Equal(1, match.Restores);
        Assert.Equal((7, 6), (match.Team1Score, match.Team2Score));
        Assert.Equal("Bravo", host.StringConVars["mp_teamname_1"]);
        Assert.Equal("Alpha", host.StringConVars["mp_teamname_2"]);
        Assert.DoesNotContain("mp_unpause_match", host.CommandNames);
        Assert.Contains("mp_warmup_end", host.CommandNames);
        host.Advance(1);
        Assert.Contains("mp_unpause_match", host.CommandNames);
        var restored = Assert.Single(sent.OfType<MatchRestored>());
        Assert.Equal((14, Side.T, 7, 6), (restored.round, restored.team1Side, restored.team1Score, restored.team2Score));
        Assert.Single(host.CommandNames, command => command.StartsWith("mp_backup_round_file"));
        Assert.Empty(sent.OfType<GoingLive>());
        Assert.Contains(Fixture.Messages.Prefixed("match.restored", 14, 7, 6), host.Chat);
        Assert.DoesNotContain(host.Timers, timer => !timer.Fired && !timer.Cancelled);

        // From here it is an ordinary live match: round 14's result counts on top of the restored score.
        match.RecordRoundResult(Side.Ct);
        Assert.Equal((7, 7), (match.Team1Score, match.Team2Score));
    }

    /// <summary>The game is left as it is: round 1 under the live cfg, which nobody reports, until a load is tried again.</summary>
    [Fact]
    public void ALoadTheEngineIgnoresLeavesTheLaunchPendingUntilALoadIsTriedAgain()
    {
        var (match, host, sent) = AtRestoreLoad();
        host.IsWarmup = false;
        match.OnRoundStart();
        host.Commands.Clear();
        host.Chat.Clear();

        host.Advance(4);
        Assert.Empty(sent.OfType<PluginLog>());
        host.Advance(1);

        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.Empty(host.Commands);
        var log = Assert.Single(sent.OfType<PluginLog>());
        Assert.Equal(LogLevel.Warn, log.level);
        Assert.Contains("did not load", log.message);
        Assert.Equal(Fixture.Messages.Prefixed("restore.failed"), Assert.Single(host.Chat));
        Assert.DoesNotContain(host.Timers, timer => !timer.Fired && !timer.Cancelled);
        Assert.False(match.IsLive);

        // Rounds the engine plays meanwhile are nobody's: not started, not scored.
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        match.RecordRoundResult(Side.T);
        Assert.Equal((0, 0), (match.Team1Score, match.Team2Score));
        Assert.Empty(sent.OfType<GoingLive>().Concat<PluginMessage>(sent.OfType<MatchRestored>()));

        // A load tried again, by an admin at the console, is followed from the hook like the first.
        Assert.True(match.BeforeBackupLoad());
        host.RoundsPlayed = 13;
        host.TeamScores[Side.T] = 7;
        host.TeamScores[Side.Ct] = 6;
        Seat(host, Side.T);
        host.LoadBackup(match);
        match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.Equal((7, 6), (match.Team1Score, match.Team2Score));
        Assert.Single(sent.OfType<MatchRestored>());
    }

    /// <summary>A restore seats the teams as the knife winners' choice left them, which is what every live round recorded.</summary>
    [Fact]
    public void ARestoreRoundSeatsTeamsAsTheKnifeChoiceLeftThem()
    {
        var (match, host, _) = AtSideChoice(knifeWinner: Side.T);
        Assert.True(match.TrySideChoice(Fixture.Player(Fixture.Team2Player1, Side.T), SideChoiceKind.Swap));
        host.RunNextFrame();
        host.IsWarmup = false;
        match.OnRoundStart();
        host.RunScheduledRestart();
        match.OnRoundStart();
        Assert.Equal(Side.T, match.Team1Side);
        Assert.True(match.IsLive);
        host.RoundsPlayed = 1;
        match.OnRoundStart();
        match.SwapSides();
        host.RoundsPlayed = 2;
        match.OnRoundStart();
        Assert.Equal(Side.Ct, match.Team1Side);

        match.RestoreRound(2, "ankor/backups/ankor_round01.txt");
        Assert.Equal(Side.Ct, match.Team1Side);
        host.LoadBackup(match);

        Assert.Equal(Side.T, match.Team1Side);
        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
    }

    /// <summary>The engine seats players by the file, not by what the launch said; the plugin takes the engine's word.</summary>
    [Fact]
    public void TheRestoredRoundTakesItsSidesFromWhereTheEngineSeatedTheTeams()
    {
        var (match, host, sent) = AtRestoreLoad(Side.T);
        host.IsWarmup = false;
        match.OnRoundStart();
        host.RoundsPlayed = 13;
        host.TeamScores[Side.Ct] = 7;
        host.TeamScores[Side.T] = 6;
        Seat(host, Side.Ct);
        host.LoadBackup(match);

        match.OnRoundStart();

        Assert.Equal(Side.Ct, match.Team1Side);
        Assert.Equal((7, 6), (match.Team1Score, match.Team2Score));
        Assert.Equal(Side.Ct, Assert.Single(sent.OfType<MatchRestored>()).team1Side);
    }

    [Fact]
    public void ScoresTheEngineCannotGiveFailTheRestore()
    {
        var (match, host, sent) = AtRestoreLoad();
        host.IsWarmup = false;
        match.OnRoundStart();
        host.TeamScores.Remove(Side.T);
        host.RoundsPlayed = 13;
        Seat(host, Side.T);
        host.LoadBackup(match);

        match.OnRoundStart();

        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.Contains("scores could not be read", Assert.Single(sent.OfType<PluginLog>()).message);
        Assert.Equal(0, match.Restores);
    }

    [Fact]
    public void ARestoredMatchHasNoKnifeAndNoSideChoice()
    {
        var (match, host, sent) = Build(Fixture.Config(sides: SidesMode.Knife, restore: Fixture.Restore(Side.Ct)));
        match.EnterWarmup();
        Assert.Equal(Side.Ct, match.Team1Side);
        Assert.Equal("Alpha", host.StringConVars["mp_teamname_1"]);
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Empty(sent.OfType<KnifeStart>());
        Assert.False(match.TrySideChoice(host.PlayerList[0], SideChoiceKind.Swap));
        Assert.Null(match.KnifeWinnerTeam);
    }

    [Fact]
    public void ABackupLoadBeforeWarmupsCfgsHaveRunKeepsTheEnginesWarmupFromStarting()
    {
        var (match, host, _) = Build();
        host.DeferNextFrames = true;
        match.EnterWarmup();
        host.LoadBackup(match);

        host.RunNextFrame();

        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.DoesNotContain("mp_warmup_start", host.CommandNames);
    }

    [Fact]
    public void ABackupLoadBeforeTheLiveCfgsHaveRunCancelsTheRestartIntoLive()
    {
        var (match, host, _) = Build(Fixture.Config(SidesMode.Team1Ct));
        match.EnterWarmup();
        host.DeferNextFrames = true;
        match.GoLive();
        host.LoadBackup(match);

        host.RunNextFrame();

        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.DoesNotContain(host.CommandNames, command => command is "mp_warmup_end" or "mp_restartgame 1");
    }

    [Fact]
    public void TheEnginesLoadingFlagIsIgnoredWhileTheMapLoadsAndOnceTheMatchEnds()
    {
        var (match, host, _) = Build();
        host.LoadingRoundBackup = true;

        match.NoticeBackupLoading();
        Assert.Equal(MatchPhase.Loading, match.Phase);

        match.EnterWarmup();
        match.BeginEnding();
        match.NoticeBackupLoading();
        Assert.Equal(MatchPhase.Warmup, match.Phase);
    }

    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public void TheLaunchsLoadIsNotIssuedOnceTheMatchEndsOrTheEngineIsAlreadyLoading(bool ending)
    {
        var (match, host, _) = AtRestoreLoad();
        host.IsWarmup = false;
        host.DeferNextFrames = true;
        match.OnRoundStart();
        if (ending) match.BeginEnding();
        else host.LoadBackup(match);

        host.RunNextFrame();

        Assert.DoesNotContain(host.CommandNames, command => command.StartsWith("mp_backup_restore_load_file"));
    }

    [Fact]
    public void AnotherLoadWhileTheEngineLoadsKeepsTheRestoreDeadline()
    {
        var (match, host, sent) = AtRestoreLoad();
        host.IsWarmup = false;
        match.OnRoundStart();
        host.LoadBackup(match);

        Assert.True(match.BeforeBackupLoad());
        host.Advance(10);

        var log = Assert.Single(sent.OfType<PluginLog>());
        Assert.Equal(LogLevel.Error, log.level);
        Assert.Contains("the engine did not restart into the restored round", log.message);
    }

    // Knife round and side choice

    /// <summary>The knife round over and the engine back in warmup, as the plugin asks it to be.</summary>
    private static (MatchState Match, FakeGameHost Host, List<PluginMessage> Sent) AtSideChoice(
        Side knifeWinner = Side.Ct,
        int timeout = 30)
    {
        var (match, host, sent) = Build(Fixture.Config(knifeDecisionSeconds: timeout));
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);

        host.IsWarmup = false;
        var tally = knifeWinner == Side.Ct
            ? new KnifeTally(Side.Ct, 1, 2, 100, 200)
            : new KnifeTally(Side.T, 2, 1, 200, 100);

        match.FinishKnifeRound(tally, knifeWinner);
        // The fake's round restart delay is 0, so warmup starts on the knife round's own tick.
        host.Advance(0);
        host.IsWarmup = true;
        return (match, host, sent);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void PartialTeamDeparturesDoNotFinishKnifeOrLive(bool live)
    {
        var config = Fixture.Config();
        config.roster.team1.players = [Fixture.Team1Player1, Fixture.Team1Player2, "76561198000000003"];
        config.roster.team2.players = [Fixture.Team2Player1, Fixture.Team2Player2, "76561198000000013"];
        var (match, host, sent) = Build(config);
        match.EnterWarmup();
        foreach (var id in config.roster.team1.players) host.PlayerList.Add(Fixture.Player(id, Side.Ct, host.PlayerList.Count));
        foreach (var id in config.roster.team2.players) host.PlayerList.Add(Fixture.Player(id, Side.T, host.PlayerList.Count));
        match.MaybeStart();
        host.Advance(14);
        host.IsWarmup = false;
        if (live) match.GoLive();
        match.OnRoundStart();
        var phase = match.Phase;
        // One, then two departures from the same team still leave a teammate connected.
        foreach (var id in config.roster.team1.players.Take(2))
        {
            host.PlayerList.RemoveAll(player => player.SteamIdString == id);
            Assert.False(match.ResolvePlayerDeparture());
            Assert.Equal(phase, match.Phase);
            Assert.Empty(sent.OfType<KnifeEnd>());
        }
        // A dead connected teammate still counts as present.
        var remaining = host.PlayerList.FindIndex(player => match.Roster.LobbyTeam(player.SteamIdString) == 1);
        host.PlayerList[remaining] = host.PlayerList[remaining] with { IsAlive = false, Health = 0 };
        host.PlayerList.RemoveAll(player => player.SteamIdString == Fixture.Team2Player1);
        Assert.False(match.ResolvePlayerDeparture());
        Assert.Equal(phase, match.Phase);
        Assert.Empty(sent.OfType<KnifeEnd>());
        // Emptying one entire team ends knife only, never the live match.
        host.PlayerList.RemoveAll(player => match.Roster.LobbyTeam(player.SteamIdString) == 1);
        Assert.False(match.ResolvePlayerDeparture());
        Assert.Equal(live ? MatchPhase.Live : MatchPhase.SideChoice, match.Phase);
        if (live) Assert.Empty(sent.OfType<KnifeEnd>());
        else Assert.Equal(2, Assert.Single(sent.OfType<KnifeEnd>()).winnerTeam);
        host.PlayerList.Clear();
        Assert.True(match.ResolvePlayerDeparture());
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void KnifeDeparturesForfeitOneTeamOrDrawWhenBothLeave(bool bothLeave)
    {
        var (match, host, sent) = Build();
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        host.IsWarmup = false;
        match.OnRoundStart();
        host.PlayerList.RemoveAll(player => bothLeave || match.Roster.LobbyTeam(player.SteamIdString) == 1);
        var end = match.ResolvePlayerDeparture();
        Assert.Equal(bothLeave, end);
        if (bothLeave)
        {
            new EventForwarder(match, host).OnMapEnd();
            var mapEnd = Assert.Single(sent.OfType<MapEnd>());
            Assert.Equal(MapEndReasons.Abandoned, mapEnd.reason);
            Assert.Null(mapEnd.winnerTeam);
            Assert.Empty(sent.OfType<KnifeEnd>());
        }
        else
        {
            Assert.Equal(MatchPhase.SideChoice, match.Phase);
            Assert.Equal(2, Assert.Single(sent.OfType<KnifeEnd>()).winnerTeam);
            Assert.Contains("mp_warmup_start", host.CommandNames);
            Assert.DoesNotContain("mp_restartgame 1", host.CommandNames);
            host.PlayerList.Clear();
            Assert.True(match.ResolvePlayerDeparture());
            match.EnterPostLive();
            host.Advance(60);
            Assert.Empty(sent.OfType<SideChoice>());
        }
    }

    [Fact]
    public void FinishingTheKnifeRoundEmitsTheWinnerAndOpensTheSideChoice()
    {
        var (match, _, sent) = AtSideChoice();

        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Equal(Side.Ct, match.KnifeWinnerSide);

        // Team 1 holds CT by default, so the CT knife win belongs to team 1.
        var knifeEnd = Assert.Single(sent.OfType<KnifeEnd>());
        Assert.Equal(1, knifeEnd.winnerTeam);
        Assert.Equal(Side.Ct, knifeEnd.winnerSide);
        Assert.Equal(2, knifeEnd.team1Alive);
        Assert.Equal(1, knifeEnd.team2Alive);
        Assert.Equal(200, knifeEnd.team1Health);
        Assert.Equal(100, knifeEnd.team2Health);
        Assert.Equal(1, match.KnifeWinnerTeam);
    }

    [Fact]
    public void KnifeCompletionReturnsToWarmupWithTheDecisionOnItsClock()
    {
        var (match, host, _) = AtSideChoice(timeout: 45);
        Assert.Equal(MatchPhase.SideChoice, match.Phase);

        var afterKnife = host.CommandNames.SkipWhile(c => c != "exec ankor/knife").Skip(1).ToList();
        Assert.Equal([CfgNames.Exec(CfgNames.Reset, "gamemode_competitive", CfgNames.Base, CfgNames.Launch, CfgNames.Warmup),
                "mp_warmuptime 45", "mp_warmup_pausetimer 0", "mp_warmup_start"],
            afterKnife.Where(c => c is not ("mp_backup_round_auto 1") && !c.StartsWith("mp_backup_round_file", StringComparison.Ordinal)));
        Assert.DoesNotContain("mp_pause_match", host.CommandNames);
    }

    [Fact]
    public void TheKnifeRoundsEndPlaysOutAndWarmupStartsASecondBeforeTheEnginesRestart()
    {
        var (match, host, _) = Build();
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        host.IsWarmup = false;
        host.ConVars["mp_round_restart_delay"] = 3;
        host.Commands.Clear();

        match.FinishKnifeRound(new KnifeTally(Side.Ct, 1, 2, 100, 200), Side.Ct);
        host.Advance(1.9);
        Assert.DoesNotContain("mp_warmup_start", host.CommandNames);
        Assert.False(match.TrySideChoice(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), SideChoiceKind.Swap));

        host.Advance(0.1);
        Assert.Contains("mp_warmup_start", host.CommandNames);
        Assert.Contains(host.Chat, line => line.Contains("!stay"));
    }

    // On a node, players spawned into the side choice with the knife round's knife alone.
    [Fact]
    public void TheSideChoiceWarmupTakesTheGameModesWeaponsBackFromTheKnifeRound()
    {
        var (match, host, _) = Build();
        host.CfgFiles["gamemode_competitive.cfg"] = "mp_ct_default_secondary weapon_hkp2000\nmp_t_default_secondary weapon_glock";
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        Assert.Equal("", host.Console.Values["mp_ct_default_secondary"]);

        host.IsWarmup = false;
        match.FinishKnifeRound(new KnifeTally(Side.Ct, 1, 2, 100, 200), Side.Ct);
        host.Advance(0);

        Assert.Equal("weapon_hkp2000", host.Console.Values["mp_ct_default_secondary"]);
        Assert.Equal("weapon_glock", host.Console.Values["mp_t_default_secondary"]);
    }

    [Fact]
    public void TheSideChoiceWarmupStartsOnItsOwnClockOnceItsCfgsHaveRun()
    {
        var (_, host, _) = AtSideChoice(timeout: 45);
        var ran = host.Console.Ran;

        Assert.Equal("45", host.Console.Values["mp_warmuptime"]);
        Assert.Equal("0", host.Console.Values["mp_warmup_pausetimer"]);
        Assert.True(ran.LastIndexOf("mp_warmuptime 9999") < ran.LastIndexOf("mp_warmuptime 45"));
        Assert.True(ran.LastIndexOf("mp_warmuptime 45") < ran.LastIndexOf("mp_warmup_start"));
    }

    [Fact]
    public void ASwapEndsWarmupOnlyOnceTheLiveCfgsHaveReplacedTheKnifeRoundsMoney()
    {
        var (match, host, _) = AtSideChoice();
        host.CfgFiles["gamemode_competitive.cfg"] = "mp_startmoney 800";
        host.DeferNextFrames = true;

        Assert.True(match.TrySideChoice(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), SideChoiceKind.Swap));
        for (var i = 0; i < 3; i++) host.RunNextFrame();

        var ran = host.Console.Ran;
        Assert.True(ran.LastIndexOf("mp_startmoney 800") < ran.LastIndexOf("mp_swapteams"));
        Assert.True(ran.LastIndexOf("mp_swapteams") < ran.LastIndexOf("mp_warmup_end"));
        Assert.Equal("800", host.Console.Values["mp_startmoney"]);
    }

    [Fact]
    public void PhaseRulesPrecedeADeferredRestartAndUnloadCancelsPendingWork()
    {
        var (match, host, sent) = Build();
        host.DeferNextFrames = true;
        match.GoLive();
        Assert.Contains(Fixture.LiveCfgs, host.CommandNames);
        Assert.DoesNotContain("mp_warmup_end", host.CommandNames);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Empty(sent.OfType<GoingLive>());
        match.Dispose();
        host.RunNextFrame();
        Assert.DoesNotContain("mp_restartgame 1", host.CommandNames);
    }

    [Fact]
    public void SideSelectionCancelsItsTimeoutAndDefersItsOnlyRestart()
    {
        var (match, host, sent) = AtSideChoice();
        host.DeferNextFrames = true;
        host.Commands.Clear();
        Assert.True(match.TrySideChoice(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), SideChoiceKind.Swap));
        Assert.DoesNotContain(host.Timers, timer => !timer.Fired);
        Assert.Equal([Fixture.LiveCfgs], host.CommandNames.ToArray());
        // The backups and the restart wait for the cfgs queued before them, which the fake engine has run by the next frame.
        host.RunNextFrame();
        host.RunNextFrame();
        var commands = host.CommandNames.ToList();
        Assert.Equal(["mp_backup_round_auto 1", "mp_backup_round_file ankor/backups/ankor"], commands.Skip(1).Take(2));
        Assert.True(commands.IndexOf("mp_backup_round_file ankor/backups/ankor") < commands.IndexOf("mp_swapteams"));
        Assert.True(commands.IndexOf("mp_swapteams") < commands.IndexOf("mp_warmup_end"));
        Assert.Single(host.CommandNames, command => command == "mp_swapteams");
        host.Advance(60);
        Assert.Single(sent.OfType<SideChoice>());
    }

    /// <summary>
    /// The plugin silences player_team while this is up: from the moment mp_swapteams is issued
    /// until the swap's own round start, and never for a plain stay.
    /// </summary>
    [Fact]
    public void SwapInProgressCoversExactlyTheEngineSwap()
    {
        var (match, host, _) = AtSideChoice();
        host.DeferNextFrames = true;
        Assert.True(match.TrySideChoice(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), SideChoiceKind.Swap));
        Assert.False(match.SwapInProgress);
        for (var i = 0; i < 3; i++) host.RunNextFrame();
        Assert.Single(host.CommandNames, command => command == "mp_swapteams");
        Assert.True(match.SwapInProgress);
        // A warmup round start while the engine applies the swap leaves it in progress.
        match.OnRoundStart();
        Assert.True(match.SwapInProgress);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.False(match.SwapInProgress);

        var (stay, stayHost, _) = AtSideChoice();
        Assert.True(stay.TrySideChoice(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), SideChoiceKind.Stay));
        stayHost.Advance(5);
        Assert.False(stay.SwapInProgress);
    }

    [Fact]
    public void ASwapGoesLiveWithTheRestartThatMpSwapteamsCausesNotTheRoundThatMpWarmupEndStarts()
    {
        var (match, host, sent) = AtSideChoice();
        var forwarder = new EventForwarder(match, host);
        host.Commands.Clear();

        Assert.True(match.TrySideChoice(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), SideChoiceKind.Swap));

        var commands = host.CommandNames.ToList();
        Assert.True(commands.IndexOf(Fixture.LiveCfgs) < commands.IndexOf("mp_swapteams"));
        Assert.True(commands.IndexOf("mp_swapteams") < commands.IndexOf("mp_warmup_end"));
        Assert.DoesNotContain("mp_restartgame 1", commands);
        Assert.True(match.SwapInProgress);
        Assert.Empty(sent.OfType<GoingLive>());

        host.IsWarmup = false;
        match.OnRoundStart();
        forwarder.OnRoundStart();

        Assert.False(match.IsLive);
        Assert.Empty(sent.OfType<GoingLive>());
        Assert.Empty(sent.OfType<RoundStart>());

        host.RunScheduledRestart();
        match.OnRoundStart();
        forwarder.OnRoundStart();

        Assert.False(match.SwapInProgress);
        Assert.True(match.IsLive);
        Assert.Single(sent.OfType<GoingLive>());
        Assert.Equal(1, Assert.Single(sent.OfType<RoundStart>()).round);
    }

    [Theory]
    [InlineData(Side.T, SideChoiceKind.Swap)]
    [InlineData(Side.Ct, SideChoiceKind.Stay)]
    public void NamingASideIsAStayOrASwapForTheWinners(Side wanted, SideChoiceKind expected)
    {
        // Team 1 (CT) won the knife.
        var (match, host, sent) = AtSideChoice();
        var winner = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);

        Assert.False(match.TrySideChoiceForSide(winner, Side.Spec));
        Assert.True(match.TrySideChoiceForSide(winner, wanted));

        Assert.Equal(expected, Assert.Single(sent.OfType<SideChoice>()).choice);
        Assert.Equal(wanted, match.Team1Side);
        Assert.Equal(expected == SideChoiceKind.Swap, host.CommandNames.Contains("mp_swapteams"));
    }

    [Theory]
    [InlineData(SideChoiceKind.Stay, false)]
    [InlineData(SideChoiceKind.Swap, false)]
    [InlineData(SideChoiceKind.Stay, true)]
    [InlineData(SideChoiceKind.Swap, true)]
    public void SideCommandsWorkOnceTheSideChoiceWarmupRuns(SideChoiceKind choice, bool namedSide)
    {
        var (match, _, sent) = AtSideChoice();
        var captain = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);

        Assert.True(namedSide
            ? match.TrySideChoiceForSide(captain, choice == SideChoiceKind.Stay ? Side.Ct : Side.T)
            : match.TrySideChoice(captain, choice));

        Assert.Equal(choice, Assert.Single(sent.OfType<SideChoice>()).choice);
    }

    [Theory]
    [InlineData(SideChoiceKind.Stay)]
    [InlineData(SideChoiceKind.Swap)]
    public void SideCommandsAreRefusedUntilTheWarmupAfterTheKnifeRoundRuns(SideChoiceKind choice)
    {
        var (match, host, sent) = AtSideChoice();
        var captain = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);
        host.IsWarmup = false;

        Assert.False(match.TrySideChoice(captain, choice));
        Assert.Empty(sent.OfType<SideChoice>());

        host.IsWarmup = true;
        Assert.True(match.TrySideChoice(captain, choice));
    }

    [Fact]
    public void NoAnswerWithoutARunningWarmupStaysAndRestartsIntoTheMatch()
    {
        var (match, host, sent) = AtSideChoice(timeout: 5);
        host.IsWarmup = false;

        host.Advance(4);

        Assert.Equal(SideChoiceKind.Stay, Assert.Single(sent.OfType<SideChoice>()).choice);
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Contains("mp_restartgame 1", host.CommandNames);
        match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, match.Phase);
    }

    [Fact]
    public void NoAnswerWhileTheMatchEndsChoosesNothing()
    {
        var (match, host, sent) = AtSideChoice(timeout: 5);
        match.BeginEnding();

        host.Advance(10);

        Assert.Empty(sent.OfType<SideChoice>());
        Assert.DoesNotContain(host.Chat, line => line.Contains("made no decision"));
    }

    [Fact]
    public void ThePromptGoesOutWithTheSideChoiceWarmupAndNoAnswerStaysASecondBeforeTheClock()
    {
        var (match, host, sent) = AtSideChoice(timeout: 5);
        Assert.Single(host.Chat, line => line.Contains("!stay"));

        host.Advance(3);
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Empty(sent.OfType<SideChoice>());

        host.Advance(1);
        Assert.Equal(SideChoiceKind.Stay, Assert.Single(sent.OfType<SideChoice>()).choice);
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Contains("mp_warmup_end", host.CommandNames);
        Assert.DoesNotContain("mp_restartgame 1", host.CommandNames);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, match.Phase);
    }

    [Theory]
    [InlineData(30)]
    [InlineData(5)]
    public void NoDecisionWithinTheConfiguredTimeoutDefaultsToStay(int timeout)
    {
        var (match, host, sent) = AtSideChoice(timeout: timeout);

        host.Advance(timeout - 2);
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Empty(sent.OfType<SideChoice>());

        host.Advance(1);

        var choice = Assert.Single(sent.OfType<SideChoice>());
        Assert.Equal(SideChoiceKind.Stay, choice.choice);
        Assert.Equal(Side.Ct, choice.team1Side);
        Assert.Equal(Side.Ct, match.Team1Side);
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.DoesNotContain("mp_swapteams", host.CommandNames);
        Assert.Contains(Fixture.Messages.Prefixed("knife.no_decision", match.Roster.TeamName(1)), host.Chat);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, match.Phase);
    }

    [Fact]
    public void AWinnerCanStayBeforeTheTimeout()
    {
        var (match, host, sent) = AtSideChoice();
        var winner = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);

        Assert.True(match.TrySideChoice(winner, SideChoiceKind.Stay));
        Assert.Equal(Side.Ct, match.Team1Side);
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.DoesNotContain("mp_swapteams", host.CommandNames);
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, match.Phase);

        // The timeout must not fire a second choice after the decision.
        host.Advance(60);
        Assert.Single(sent.OfType<SideChoice>());
    }

    [Fact]
    public void AWinnerCanSwapWhichFlipsTeam1Side()
    {
        var (match, host, sent) = AtSideChoice();
        var winner = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);

        Assert.True(match.TrySideChoice(winner, SideChoiceKind.Swap));

        var choice = Assert.Single(sent.OfType<SideChoice>());
        Assert.Equal(SideChoiceKind.Swap, choice.choice);
        Assert.Equal(Side.T, choice.team1Side);
        Assert.Equal(1, choice.team);
        Assert.Equal(Side.T, match.Team1Side);
        Assert.Contains("mp_swapteams", host.CommandNames);
        Assert.DoesNotContain("mp_restartgame 1", host.CommandNames);
        Assert.True(host.CommandNames.ToList().IndexOf(Fixture.LiveCfgs) < host.CommandNames.ToList().IndexOf("mp_swapteams"));
    }

    [Fact]
    public void TheLosingTeamCannotChooseSides()
    {
        var (match, host, sent) = AtSideChoice(knifeWinner: Side.Ct);
        var loser = Fixture.Player(Fixture.Team2Player1, Side.T, 2);

        Assert.False(match.TrySideChoice(loser, SideChoiceKind.Swap));
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Empty(sent.OfType<SideChoice>());
        Assert.Empty(host.DirectChat);
    }

    [Fact]
    public void OnlyTheWinningTeamsCaptainChoosesTheSide()
    {
        // Team 1 won the knife; Team1Player2 is on that roster but not at index 0.
        var (match, host, sent) = AtSideChoice();
        var teammate = Fixture.Player(Fixture.Team1Player2, Side.Ct, 1);

        Assert.False(match.TrySideChoice(teammate, SideChoiceKind.Swap));
        Assert.False(match.TrySideChoiceForSide(teammate, Side.T));

        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Empty(sent.OfType<SideChoice>());
        Assert.Equal(Side.Ct, match.Team1Side);
        // Both refusals name the connected captain who decides instead.
        Assert.Equal(2, host.DirectChat.Count);
        Assert.All(host.DirectChat, line => Assert.Contains("player0", line.Message));

        // The captain is still free to decide afterwards.
        Assert.True(match.TrySideChoice(
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0), SideChoiceKind.Swap));
    }

    [Fact]
    public void TheSideChoicePromptNamesTheCaptainWhoDecides()
    {
        var (_, host, _) = AtSideChoice();

        // ConnectFullRoster seats team 1's captain in slot 0, so the prompt calls them player0.
        Assert.Contains(host.Chat, line => line.Contains("player0")
            && new[] { ChatCommands.Stay, ChatCommands.Swap, ChatCommands.Ct, ChatCommands.T }
                .All(command => line.Contains("!" + command)));
    }

    [Fact]
    public void TheSideChoicePromptStaysGenericWhenTheCaptainIsNotConnected()
    {
        var (match, host, _) = Build();
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        host.IsWarmup = false;
        host.PlayerList.RemoveAll(player => player.SteamIdString == Fixture.Team1Player1);
        host.Chat.Clear();

        match.FinishKnifeRound(new KnifeTally(Side.Ct, 1, 2, 100, 200), Side.Ct);
        host.Advance(0);

        Assert.Contains(Fixture.Messages.Prefixed("knife.side_prompt_no_captain",
            ChatCommands.Stay, ChatCommands.Swap, ChatCommands.Ct, ChatCommands.T, 30), host.Chat);
    }

    [Fact]
    public void AWinnerIsToldOnlyTheCaptainChoosesWhenTheCaptainIsNotConnected()
    {
        var (match, host, sent) = AtSideChoice(knifeWinner: Side.Ct);
        host.PlayerList.RemoveAll(player => player.SteamIdString == Fixture.Team1Player1);
        var caller = host.PlayerList.Single(player => player.SteamIdString == Fixture.Team1Player2);
        host.DirectChat.Clear();

        Assert.False(match.TrySideChoice(caller, SideChoiceKind.Swap));

        Assert.Equal((caller.Slot, Fixture.Messages.Prefixed("knife.not_captain")), Assert.Single(host.DirectChat));
        Assert.Empty(sent.OfType<SideChoice>());
    }

    [Fact]
    public void ABackupLoadBeforeTheSideChoiceWarmupsCfgsRunOpensNoSideChoice()
    {
        var (match, host, _) = Build();
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        host.IsWarmup = false;
        match.FinishKnifeRound(new KnifeTally(Side.Ct, 1, 2, 100, 200), Side.Ct);
        host.DeferNextFrames = true;
        host.Advance(0);
        host.LoadBackup(match);
        host.Commands.Clear();
        host.Chat.Clear();

        host.RunNextFrame();

        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.DoesNotContain("mp_warmup_start", host.CommandNames);
        Assert.Empty(host.Chat);
    }

    [Fact]
    public void ASideChoiceOutsideTheSideChoicePhaseIsIgnored()
    {
        var (match, _, sent) = Build();
        match.EnterWarmup();

        var player = Fixture.Player(Fixture.Team1Player1, Side.Ct, 0);
        Assert.False(match.TrySideChoice(player, SideChoiceKind.Stay));
        Assert.Empty(sent.OfType<SideChoice>());
    }

    [Fact]
    public void AnUnrosteredCallerCannotChooseSidesEvenFromTheWinningSide()
    {
        var (match, host, sent) = Build();
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        match.FinishKnifeRound(new KnifeTally(Side.T, 3, 1, 300, 100), Side.T);
        host.IsWarmup = false;
        match.OnRoundStart();

        // Team 1 holds CT, so the T knife win is team 2's.
        Assert.Equal(2, match.KnifeWinnerTeam);

        var stranger = Fixture.Player(Fixture.Stranger, Side.T, 5);
        Assert.False(match.TrySideChoice(stranger, SideChoiceKind.Swap));
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.Empty(sent.OfType<SideChoice>());
    }

    [Fact]
    public void ATiedKnifeRoundIsBrokenByTheRoundEndWinner()
    {
        var (match, host, _) = Build();
        match.EnterWarmup();
        ConnectFullRoster(host);
        match.MaybeStart();
        host.Advance(14);
        Assert.Equal(MatchPhase.Knife, match.Phase);

        match.FinishKnifeRound(new KnifeTally(null, 2, 2, 150, 150), Side.T);

        Assert.Equal(Side.T, match.KnifeWinnerSide);

        // Team 1 holds CT, so the T knife win belongs to team 2.
        Assert.Equal(2, match.KnifeWinnerTeam);
    }

    [Fact]
    public void FinishingAKnifeRoundOutsideTheKnifePhaseIsIgnored()
    {
        var (match, _, sent) = Build();
        match.EnterWarmup();

        match.FinishKnifeRound(new KnifeTally(Side.T, 3, 1, 300, 100), Side.T);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Null(match.KnifeWinnerSide);
        Assert.Empty(sent.OfType<KnifeEnd>());
    }

    // Going live

    [Theory]
    [InlineData(true, "mp_warmup_end")]
    [InlineData(false, "mp_restartgame 1")]
    public void GoingLiveAppliesConfigAndUsesOneEngineRoundStart(bool warmup, string restart)
    {
        var (match, host, sent) = Build();
        host.IsWarmup = warmup;
        host.ConVars["mp_maxrounds"] = 24;
        host.ConVars["mp_overtime_enable"] = 1;

        match.GoLive();
        Assert.Contains(Fixture.LiveCfgs, host.CommandNames);
        Assert.Empty(sent.OfType<GoingLive>());

        host.Advance(3);
        host.IsWarmup = false;
        match.OnRoundStart();

        match.OnRoundStart();
        var live = Assert.Single(sent.OfType<GoingLive>());
        Assert.Equal(24, live.maxRounds);
        Assert.True(live.overtime);
        Assert.Equal(Side.Ct, live.team1Side);
        Assert.Single(host.CommandNames, c => c == "mp_restartgame 1" || c == "mp_warmup_end");
        Assert.Contains(restart, host.CommandNames);
        Assert.True(host.CommandNames.ToList().IndexOf(Fixture.LiveCfgs) < host.CommandNames.ToList().IndexOf(restart));
    }

    // On a node, the game mode's cfg set mp_warmuptime 120 under a running warmup clock, which then
    // ran two more minutes instead of ending.
    [Fact]
    public void GoingLiveEndsWarmupItselfOnceTheGameModesCfgHasSetItsOwnWarmupTime()
    {
        var (match, host, sent) = Build();
        host.ConVars["mp_maxrounds"] = 24;
        host.CfgFiles["gamemode_competitive.cfg"] = "mp_warmuptime 120";

        match.GoLive();

        var ran = host.Console.Ran;
        Assert.True(ran.IndexOf("mp_warmuptime 120") < ran.IndexOf("mp_warmup_end"));
        host.IsWarmup = false;
        match.OnRoundStart();
        Assert.Single(sent.OfType<GoingLive>());
        Assert.True(match.IsLive);
    }

    // Rounds and score

    [Theory]
    [InlineData(0, 0, "gamemode_casual")]
    [InlineData(0, 1, "gamemode_competitive")]
    [InlineData(0, 2, "gamemode_competitive2v2")]
    public void GoingLiveRunsTheGameModesCfgFirst(int gameType, int gameMode, string modeCfg)
    {
        var (match, host, sent) = Build(Fixture.Config(SidesMode.Team1Ct));
        host.ConVars["game_type"] = gameType;
        host.ConVars["game_mode"] = gameMode;

        match.GoLive();

        Assert.Contains(CfgNames.Exec(CfgNames.Reset, modeCfg, CfgNames.Base, CfgNames.Launch), host.CommandNames);
        Assert.Empty(sent.OfType<PluginLog>());
    }

    /// <summary>The reset runs ahead of the mode's cfg, so a value that the mode sets survives going live.</summary>
    [Fact]
    public void GoingLiveKeepsWhatTheModesCfgSetsOverTheReset()
    {
        var (match, host, _) = Build(Fixture.Config(SidesMode.Team1Ct));
        host.ConVars["game_type"] = 0;
        host.ConVars["game_mode"] = 0;
        host.CfgFiles["gamemode_casual.cfg"] = "mp_round_restart_delay 10";

        match.GoLive();

        Assert.Equal("10", host.Console.Values["mp_round_restart_delay"]);
        Assert.Equal("1", host.Console.Values["mp_give_player_c4"]);
    }

    [Fact]
    public void AGameModeWithoutAKnownCfgGoesLiveOnAnkorsAndTheLaunchsCfgsAndIsLogged()
    {
        var (match, host, sent) = Build(Fixture.Config(SidesMode.Team1Ct));
        host.ConVars["game_type"] = 9;
        host.ConVars["game_mode"] = 9;

        match.GoLive();

        Assert.Contains(CfgNames.Exec(CfgNames.Reset, CfgNames.Base, CfgNames.Launch), host.CommandNames);
        var log = Assert.Single(sent.OfType<PluginLog>());
        Assert.Equal(LogLevel.Error, log.level);
        Assert.Contains("game_type 9 and game_mode 9", log.message);
    }

    [Fact]
    public void TheRoundNumberComesFromTheGamesOwnCounter()
    {
        var (match, host, _) = Build();
        Assert.Equal(1, match.Round);

        host.RoundsPlayed = 12;
        Assert.Equal(13, match.Round);
    }

    [Fact]
    public void RoundTimeIsMeasuredFromFreezeEnd()
    {
        var (match, host, _) = Build();
        match.GoLive();

        host.FreezePeriod = true;
        host.RoundStartTime = host.CurrentTime + 15;
        match.OnRoundStart();
        host.Advance(15);
        Assert.Equal(0, match.RoundTime);

        host.RoundStartTime = host.CurrentTime;
        host.FreezePeriod = false;
        host.Advance(42.5);
        Assert.Equal(42.5, match.RoundTime, precision: 3);

        host.FreezePeriod = true;
        match.OnRoundStart();
        Assert.Equal(0, match.RoundTime);
    }

    [Fact]
    public void RoundClockUsesExistingEngineStateWithoutAFreezeEndCallback()
    {
        var (match, host, _) = Build();
        host.Clock = 142.125;
        host.RoundStartTime = 100;
        host.FreezePeriod = false;
        Assert.Equal(42.125, match.RoundTime, precision: 3);
        host.NowMs -= 60_000;
        Assert.Equal(42.125, match.RoundTime, precision: 3);
        host.Clock += 0.125;
        Assert.Equal(42.250, match.RoundTime, precision: 3);
    }

    private static (MatchState Match, FakeGameHost Host, List<MatchReport> Reports, List<PluginMessage> Sent) LiveWithReports()
    {
        var host = new FakeGameHost();
        var reports = new List<MatchReport>();
        var sent = new List<PluginMessage>();
        var match = new MatchState(host, Fixture.Config(SidesMode.Team1Ct), new FakeAgentChannel(sent, reports), Fixture.Messages, () => { });
        match.EnterWarmup();
        match.GoLive();
        host.Advance(3);
        host.IsWarmup = false;
        match.OnRoundStart();
        reports.Clear();
        sent.Clear();
        return (match, host, reports, sent);
    }

    [Fact]
    public void ARoundIsScoredOffTheScoreboardWhenTheEngineHasCountedItAlready()
    {
        var (match, host, reports, sent) = LiveWithReports();
        host.TeamScores[Side.Ct] = 1;

        Assert.Equal((1, 0), match.ScoreRound(Side.Ct));
        var score = Assert.IsType<PluginScore>(Assert.Single(reports));
        Assert.Equal((1, 0), (score.team1Score, score.team2Score));
        Assert.Empty(sent.OfType<PluginLog>());
    }

    [Fact]
    public void AScoreboardThatDisagreesWithTheCountStandsAndIsLogged()
    {
        var (match, host, _, sent) = LiveWithReports();
        // Set at the console: the engine's score moved without the plugin seeing the rounds.
        host.TeamScores[Side.Ct] = 5;
        host.TeamScores[Side.T] = 3;

        Assert.Equal((5, 3), match.ScoreRound(Side.Ct));
        Assert.Contains(sent, message => message is PluginLog { level: LogLevel.Warn } log && log.message.Contains("the scoreboard reads 5-3, the plugin counted 1-0"));
    }

    [Fact]
    public void AScoreboardThatCannotBeReadLeavesThePluginsCount()
    {
        var (match, host, reports, sent) = LiveWithReports();
        host.TeamScores.Remove(Side.T);

        Assert.Equal((1, 0), match.ScoreRound(Side.Ct));
        Assert.Single(reports);
        Assert.Contains(sent, message => message is PluginLog log && log.message.Contains("the scoreboard could not be read"));
    }

    [Fact]
    public void ScoresAfterHalftimeAreReadOffTeamOnesNewSide()
    {
        var (match, host, _, _) = LiveWithReports();
        match.RecordRoundResult(Side.Ct);
        match.SwapSides();
        // Team 1 now plays T, and the engine's T team holds its score.
        host.TeamScores[Side.T] = 2;
        host.TeamScores[Side.Ct] = 0;

        Assert.Equal((2, 0), match.ScoreRound(Side.T));
    }

    [Fact]
    public void PhaseChangesAreReportedOnceEach()
    {
        var (match, _, reports, _) = LiveWithReports();

        match.EnterPostLive();
        match.EnterPostLive();

        Assert.Equal(MatchPhase.PostLive, Assert.IsType<PluginPhaseChange>(Assert.Single(reports)).phase);
    }

    [Fact]
    public void ScoresAreCountedByLobbyTeamAndOnlyWhileLive()
    {
        var (match, host, _) = Build();

        match.EnterWarmup();
        Assert.Equal((0, 0), match.RecordRoundResult(Side.Ct));

        match.GoLive();
        host.Advance(3);
        host.IsWarmup = false;
        match.OnRoundStart();

        Assert.Equal((1, 0), match.RecordRoundResult(Side.Ct));
        Assert.Equal((1, 1), match.RecordRoundResult(Side.T));
        Assert.Equal((2, 1), match.RecordRoundResult(Side.Ct));
        Assert.Equal(1, match.WinnerTeam);
    }

    [Fact]
    public void AfterSwappingSidesTheScoreFollowsTheLobbyTeam()
    {
        var (match, host, _) = Build();
        match.GoLive();
        host.Advance(3);
        host.IsWarmup = false;
        match.OnRoundStart();

        match.RecordRoundResult(Side.Ct);
        Assert.Equal(1, match.Team1Score);

        match.SwapSides();
        Assert.Equal(Side.T, match.Team1Side);

        match.RecordRoundResult(Side.T);
        Assert.Equal(2, match.Team1Score);
        Assert.Equal(0, match.Team2Score);
    }

    [Fact]
    public void AnEvenScoreHasNoWinner()
    {
        var (match, host, _) = Build();
        match.GoLive();
        host.Advance(3);
        host.IsWarmup = false;
        match.OnRoundStart();

        match.RecordRoundResult(Side.Ct);
        match.RecordRoundResult(Side.T);
        Assert.Null(match.WinnerTeam);
    }

    [Fact]
    public void EndingTheMatchCancelsAPendingSideChoice()
    {
        var (match, host, sent) = AtSideChoice();
        match.EnterPostLive();

        Assert.Equal(MatchPhase.PostLive, match.Phase);
        host.Advance(60);
        Assert.Empty(sent.OfType<SideChoice>());
    }

    [Theory]
    [InlineData("warmup")]
    [InlineData("knife")]
    [InlineData("live")]
    [InlineData("end")]
    [InlineData("dispose")]
    public void LeavingSideChoiceCancelsTheTimerAndFencesItsCallback(string transition)
    {
        var (match, host, sent) = AtSideChoice();
        var timer = Assert.Single(host.Timers, timer => !timer.Fired && !timer.Cancelled);
        switch (transition)
        {
            case "warmup": match.EnterWarmup(); break;
            case "knife": match.StartKnifeRound(); break;
            case "live": match.GoLive(); break;
            case "end": match.EnterPostLive(); break;
            case "dispose": match.Dispose(); break;
        }
        Assert.True(timer.Cancelled);
        var phase = match.Phase;
        var commands = host.CommandNames.ToArray();
        var chat = host.Chat.ToArray();
        timer.Callback(); // Exercise delivery of work already queued before cancellation.
        Assert.Equal(phase, match.Phase);
        Assert.Equal(commands, host.CommandNames.ToArray());
        Assert.Equal(chat, host.Chat.ToArray());
        Assert.Empty(sent.OfType<SideChoice>());
    }

    [Fact]
    public void CancelledTimeoutCannotChooseForALaterKnifeRound()
    {
        var (match, host, sent) = AtSideChoice();
        var oldTimer = Assert.Single(host.Timers, timer => !timer.Fired && !timer.Cancelled);
        match.StartKnifeRound();
        match.FinishKnifeRound(new KnifeTally(Side.Ct, 1, 2, 100, 200), Side.Ct);
        match.OnRoundStart();
        var currentTimer = Assert.Single(host.Timers, timer => !timer.Fired && !timer.Cancelled);
        Assert.True(oldTimer.Cancelled);
        Assert.NotSame(oldTimer, currentTimer);
        var chat = host.Chat.ToArray();

        oldTimer.Callback();
        Assert.Equal(MatchPhase.SideChoice, match.Phase);
        Assert.False(currentTimer.Cancelled);
        Assert.Equal(chat, host.Chat.ToArray());
        Assert.Empty(sent.OfType<SideChoice>());
        host.Advance(match.Config.knifeDecisionSeconds);
        Assert.Single(sent.OfType<SideChoice>());
    }

    [Fact]
    public void EveryEmittedEventCarriesTheMatchContext()
    {
        var (match, host, sent) = Build();
        host.RoundsPlayed = 7;
        match.GoLive();
        host.RoundStartTime = host.CurrentTime;
        host.FreezePeriod = false;
        host.Advance(11.25);

        match.Emit(new BombExploded());

        var bomb = Assert.IsType<BombExploded>(sent.Last());
        Assert.Equal(8, bomb.round);
        Assert.Equal(11.25, bomb.roundTime, precision: 3);
        Assert.True(bomb.timestamp > 0);
    }

    [Fact]
    public void PlayerRefsResolveTheLobbyTeam()
    {
        var (match, _, _) = Build();

        var rostered = match.RefFor(Fixture.Player(Fixture.Team2Player1, Side.T, 3, name: "bravo1"));
        Assert.Equal(Fixture.Team2Player1, rostered.steamId);
        Assert.Equal("bravo1", rostered.name);
        Assert.Equal(Side.T, rostered.side);
        Assert.Equal(2, rostered.team);

        var stranger = match.RefFor(Fixture.Player(Fixture.Stranger, Side.Spec, 9));
        Assert.Null(stranger.team);
    }

    [Fact]
    public void EmptyLiveMatchEndsButWarmupAndRemainingPlayersDoNot()
    {
        var host = new FakeGameHost();
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(), Fixture.Messages, () => { });
        Assert.False(match.ResolvePlayerDeparture());
        match.EnterWarmup();
        Assert.False(match.ResolvePlayerDeparture());
        match.GoLive();
        host.IsWarmup = false;
        match.OnRoundStart();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct));
        Assert.False(match.ResolvePlayerDeparture());
        host.PlayerList.Clear();
        host.PlayerList.Add(Fixture.Bot(Side.T));
        Assert.True(match.ResolvePlayerDeparture());
        match.EnterPostLive();
        Assert.False(match.ResolvePlayerDeparture());
    }

    [Fact]
    public void EarlyMapEndUsesCompletedRoundScoresAndBo1MapWin()
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(sent), Fixture.Messages, () => { });
        match.GoLive();
        host.IsWarmup = false;
        match.OnRoundStart();
        match.RecordRoundResult(Side.Ct);
        match.RecordRoundResult(Side.Ct);
        match.RecordRoundResult(Side.T);
        new EventForwarder(match, host).OnMapEnd();
        match.EnterPostLive();
        var map = Assert.Single(sent.OfType<MapEnd>());
        Assert.Equal(2, map.team1Score);
        Assert.Equal(1, map.team2Score);
        Assert.Equal(1, map.winnerTeam);
    }

    [Theory]
    [InlineData(12, 10, false)]
    [InlineData(13, 10, true)]
    [InlineData(12, 12, false)]
    [InlineData(15, 14, false)]
    [InlineData(16, 14, true)]
    [InlineData(15, 15, false)]
    [InlineData(19, 17, true)]
    public void RegulationAndSuccessiveOvertimes(int first, int second, bool final)
    {
        var host = new FakeGameHost();
        host.ConVars["mp_maxrounds"] = 24;
        host.ConVars["mp_overtime_enable"] = 1;
        host.ConVars["mp_overtime_maxrounds"] = 6;
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(), Fixture.Messages, () => { });
        match.GoLive();
        host.IsWarmup = false;
        match.OnRoundStart();
        for (var i = 0; i < first; i++) match.RecordRoundResult(Side.Ct);
        for (var i = 0; i < second; i++) match.RecordRoundResult(Side.T);
        Assert.Equal(final, match.IsFinalRound());
    }

    /// <summary>
    /// Where the engine switches sides, by rounds played: regulation's halftime and the halftime
    /// of every overtime. Never at the end of a period (regulation into the first overtime, or
    /// one overtime into the next): teams start each overtime on the side they ended the last on.
    /// </summary>
    [Theory]
    [InlineData(11, false)]
    [InlineData(12, true)]
    [InlineData(13, false)]
    [InlineData(23, false)]
    [InlineData(24, false)]
    [InlineData(25, false)]
    [InlineData(26, false)]
    [InlineData(27, true)]
    [InlineData(28, false)]
    [InlineData(29, false)]
    [InlineData(30, false)]
    [InlineData(31, false)]
    [InlineData(33, true)]
    [InlineData(36, false)]
    [InlineData(37, false)]
    [InlineData(39, true)]
    [InlineData(42, false)]
    public void SideSwitchesOnlyAtTheHalftimeOfRegulationAndOfEveryOvertime(int played, bool boundary)
    {
        var host = new FakeGameHost();
        host.ConVars["mp_halftime"] = 1;
        host.ConVars["mp_maxrounds"] = 24;
        host.ConVars["mp_overtime_enable"] = 1;
        host.ConVars["mp_overtime_maxrounds"] = 6;
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(), Fixture.Messages, () => { });
        match.GoLive();
        host.IsWarmup = false;
        match.OnRoundStart();
        // Alternate winners so the score never ends the match before the round under test.
        for (var i = 0; i < played; i++) match.RecordRoundResult(i % 2 == 0 ? Side.Ct : Side.T);
        Assert.Equal(boundary, match.IsHalftimeBoundary());
    }

    private static MatchState LiveAt(FakeGameHost host, int team1, int team2)
    {
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(), Fixture.Messages, () => { });
        match.GoLive();
        host.IsWarmup = false;
        match.OnRoundStart();
        for (var i = 0; i < team1; i++) match.RecordRoundResult(Side.Ct);
        for (var i = 0; i < team2; i++) match.RecordRoundResult(Side.T);
        return match;
    }

    [Theory]
    [InlineData(5, 0, 0, 5, 0, true)] // the win limit alone ends the match
    [InlineData(0, 0, 6, 4, 0, false)] // no round limit: overtime never starts
    [InlineData(0, 24, 0, 13, 12, false)] // past regulation without overtime rounds
    public void WinLimitAndUnsetRoundLimitsDecideTheFinalRound(
        int winLimit, int maxRounds, int overtimeMaxRounds, int team1, int team2, bool final)
    {
        var host = new FakeGameHost();
        host.ConVars["mp_winlimit"] = winLimit;
        host.ConVars["mp_maxrounds"] = maxRounds;
        host.ConVars["mp_overtime_maxrounds"] = overtimeMaxRounds;
        var match = LiveAt(host, team1, team2);

        Assert.Equal(final, match.IsFinalRound());
    }

    [Theory]
    [InlineData(0, 24, 6, 12, false)] // mp_halftime off
    [InlineData(1, 0, 6, 0, false)] // no round limit has no halfway point
    [InlineData(1, 24, 1, 25, false)] // overtimes too short to have a half
    public void NoSideSwitchWithoutHalftimeOrHalvesToSwitchBetween(
        int halftime, int maxRounds, int overtimeMaxRounds, int played, bool boundary)
    {
        var host = new FakeGameHost();
        host.ConVars["mp_halftime"] = halftime;
        host.ConVars["mp_maxrounds"] = maxRounds;
        host.ConVars["mp_overtime_enable"] = 1;
        host.ConVars["mp_overtime_maxrounds"] = overtimeMaxRounds;
        var match = LiveAt(host, (played + 1) / 2, played / 2);

        Assert.Equal(boundary, match.IsHalftimeBoundary());
    }

    [Fact]
    public void PostLiveFreezesEventStreamAndCannotRestartWarmup()
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(sent), Fixture.Messages, () => { });
        match.EnterPostLive();
        match.Emit(new RoundStart());
        match.EnterWarmup();
        match.OnRoundStart();
        Assert.Equal(MatchPhase.PostLive, match.Phase);
        Assert.False(match.IsLive);
        Assert.Empty(sent);
    }

    private static (MatchState Match, FakeGameHost Host, List<PluginMessage> Sent) ExternalWarmup()
    {
        var (match, host, sent) = Build(Fixture.Config(readyMode: ReadyMode.External));
        match.EnterWarmup();
        return (match, host, sent);
    }

    [Fact]
    public void AnExternalMatchWaitsInWarmupWithTheFullRosterConnected()
    {
        var (match, host, _) = ExternalWarmup();
        ConnectFullRoster(host);

        match.MaybeStart();
        host.Advance(60);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
    }

    [Fact]
    public void PlayersHaveNoReadyCommandsInAnExternalMatch()
    {
        var (match, host, _) = ExternalWarmup();
        ConnectFullRoster(host);

        Assert.False(match.TrySetReady(host.PlayerList[1], ready: true));
        Assert.False(match.TrySetReady(host.PlayerList[1], ready: false));
        Assert.False(match.TryForceReady(host.PlayerList[0]));
        host.Advance(60);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Equal(3, host.DirectChat.Count(chat => chat.Message.Contains("organiser starts the match")));
    }

    [Fact]
    public void StartMatchStartsAnExternalMatchWithOnePlayerOnEachTeam()
    {
        var (match, host, _) = ExternalWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 0),
            Fixture.Player(Fixture.Team2Player2, Side.T, 1),
        ]);

        Assert.Null(match.StartFromController());
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Theory]
    [InlineData(ReadyMode.Auto)]
    [InlineData(ReadyMode.Manual)]
    [InlineData(ReadyMode.External)]
    public void StartMatchIsRefusedWithAnEmptyTeam(ReadyMode mode)
    {
        var (match, host, _) = Build(Fixture.Config(readyMode: mode));
        match.EnterWarmup();
        host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.Ct, 0));

        Assert.Equal(CommandResult.TeamEmpty, match.StartFromController());
        host.Advance(60);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
    }

    /// <summary>An admin overrides the players' ready-up in every mode, unready spectators included.</summary>
    [Theory]
    [InlineData(ReadyMode.Auto)]
    [InlineData(ReadyMode.Manual)]
    public void StartMatchStartsAMatchWhateverItsReadyMode(ReadyMode mode)
    {
        var (match, host, _) = Build(Fixture.Config(readyMode: mode));
        match.EnterWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0),
            Fixture.Player(Fixture.Team2Player1, Side.T, 1),
        ]);

        Assert.Null(match.StartFromController());
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void StartMatchIsRefusedOutsideWarmup()
    {
        var (match, host, _) = ExternalWarmup();
        host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, 0),
            Fixture.Player(Fixture.Team2Player1, Side.T, 1),
        ]);
        Assert.Null(match.StartFromController());
        host.Advance(14);

        Assert.Equal(CommandResult.NotInWarmup, match.StartFromController());
        Assert.Equal(MatchPhase.Knife, match.Phase);
    }
}
