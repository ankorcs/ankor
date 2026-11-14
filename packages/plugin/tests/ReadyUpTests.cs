using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class ReadyUpTests
{
    /// <summary>The fixture's roster has one spectator, whom manual mode waits for only when asked to.</summary>
    private static (MatchState Match, FakeGameHost Host, List<PluginMessage> Sent) Build(
        ReadyMode mode = ReadyMode.Manual, int? timeout = null, bool spectatorsReadyUp = false, bool allowForceReady = true)
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        var config = Fixture.Config(readyMode: mode, readyTimeoutSeconds: timeout, allowForceReady: allowForceReady);
        config.spectatorsReadyUp = spectatorsReadyUp;
        var match = new MatchState(host, config, new FakeAgentChannel(sent), Fixture.Messages, () => { });
        match.EnterWarmup();
        return (match, host, sent);
    }

    [Fact]
    public void TheWarmupClockRunsOneSecondPastTheReadyDeadline()
    {
        var (_, host, _) = Build(timeout: 300);

        // Over warmup.cfg's paused 9999 seconds, which ran first.
        Assert.Equal($"{300 + MatchState.ReadyDeadlineMarginSeconds}", host.Console.Values["mp_warmuptime"]);
        Assert.Equal(1, MatchState.ReadyDeadlineMarginSeconds);
        Assert.Equal("0", host.Console.Values["mp_warmup_pausetimer"]);
    }

    [Fact]
    public void WithoutAReadyDeadlineTheWarmupClockStaysPaused()
    {
        var (_, host, _) = Build();

        Assert.Equal("9999", host.Console.Values["mp_warmuptime"]);
        Assert.Equal("1", host.Console.Values["mp_warmup_pausetimer"]);
    }

    [Fact]
    public void AnExpiredReadyDeadlineLetsTheWarmupClockRunOutAndEndsTheMatchWhenTheRoundStarts()
    {
        var (match, host, _) = Build(timeout: 5);
        var expired = 0;
        match.ReadyExpired += () => expired++;

        host.Advance(5);
        Assert.Equal("0", host.Console.Values["mp_warmup_pausetimer"]);
        Assert.Equal(0, expired);

        EndWarmupIntoRound(host, match);
        Assert.Equal(1, expired);
        EndWarmupIntoRound(host, match);
        host.Advance(MatchState.ReadyExpiryFallbackSeconds);
        Assert.Equal(1, expired);
    }

    [Fact]
    public void AWarmupThatNeverEndsAfterTheDeadlineStillEndsTheMatch()
    {
        var (match, host, _) = Build(timeout: 5);
        var expired = 0;
        match.ReadyExpired += () => expired++;

        host.Advance(5);
        host.Advance(MatchState.ReadyExpiryFallbackSeconds - 0.1);
        Assert.Equal(0, expired);
        host.Advance(0.1);
        Assert.Equal(1, expired);
    }

    /// <summary>The engine's warmup clock ran out: warmup ends, and the first round starts.</summary>
    private static void EndWarmupIntoRound(FakeGameHost host, MatchState match)
    {
        host.IsWarmup = false;
        match.OnRoundStart();
    }

    private static void Connect(FakeGameHost host, MatchState match, params string[] steamIds)
    {
        foreach (var id in steamIds)
        {
            var team = id is Fixture.Team1Player1 or Fixture.Team1Player2 ? Side.Ct
                : id == Fixture.Spectator ? Side.Spec : Side.T;
            host.PlayerList.Add(Fixture.Player(id, team, host.PlayerList.Count, name: $"p{id[^2..]}"));
        }
        match.MaybeStart();
    }

    private static readonly string[] Everyone =
        [Fixture.Team1Player1, Fixture.Team1Player2, Fixture.Team2Player1, Fixture.Team2Player2];

    private static HostPlayer P(FakeGameHost host, string steamId) =>
        host.PlayerList.First(player => player.SteamIdString == steamId);

    private static void ReadyAll(FakeGameHost host, MatchState match, params string[] steamIds)
    {
        foreach (var id in steamIds) Assert.True(match.TrySetReady(P(host, id), true));
    }

    [Fact]
    public void AFullRosterWaitsForEveryReadyThenCountsDown()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone);
        host.Advance(20);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        ReadyAll(host, match, Everyone[..3]);
        Assert.Contains(host.Chat, line => line.Contains("is ready (3/4)"));
        host.Advance(20);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        ReadyAll(host, match, Everyone[3]);
        Assert.Contains(Fixture.Messages.Prefixed("warmup.countdown_knife", MatchState.CountdownSeconds), host.Chat);
        host.Advance(14);
        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void ReadyingBeforeTheRosterIsCompleteStartsOnceTheLastPlayerConnectsAndReadies()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone[..3]);
        ReadyAll(host, match, Everyone[..3]);
        Connect(host, match, Everyone[3]);
        host.Advance(15);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        ReadyAll(host, match, Everyone[3]);
        host.Advance(14);
        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void UnreadyWithdrawsUntilTheCountdownRuns()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone);
        ReadyAll(host, match, Everyone[..3]);

        Assert.True(match.TrySetReady(P(host, Everyone[0]), false));
        Assert.Contains(host.Chat, line => line.Contains("is no longer ready"));
        ReadyAll(host, match, Everyone[3]);
        host.Advance(15);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        ReadyAll(host, match, Everyone[0]);
        host.DirectChat.Clear();
        Assert.False(match.TrySetReady(P(host, Everyone[0]), false));
        Assert.Empty(host.DirectChat);
        host.Advance(14);
        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void RepeatingAReadyStateIsRefusedPrivately()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone[0]);

        Assert.False(match.TrySetReady(P(host, Everyone[0]), false));
        ReadyAll(host, match, Everyone[0]);
        Assert.False(match.TrySetReady(P(host, Everyone[0]), true));

        Assert.Equal(2, host.DirectChat.Count);
        Assert.Contains("!" + ChatCommands.Ready, host.DirectChat[0].Message);
        Assert.Contains("already ready", host.DirectChat[1].Message);
    }

    [Fact]
    public void OnlyRosteredPeopleReady()
    {
        var (match, host, _) = Build(spectatorsReadyUp: true);

        Assert.False(match.TrySetReady(Fixture.Player(Fixture.Stranger, Side.Spec, 7), true));

        Assert.Empty(host.DirectChat);
    }

    [Fact]
    public void ManualModeWaitsForTheRosteredSpectatorsReady()
    {
        var (match, host, _) = Build(spectatorsReadyUp: true);
        Connect(host, match, Everyone);
        ReadyAll(host, match, Everyone);
        host.Advance(20);
        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Contains(host.Chat, line => line.Contains("is ready (4/5)"));

        Connect(host, match, Fixture.Spectator);
        Assert.Equal("NOT READY", host.ClanTags[Fixture.Spectator]);
        ReadyAll(host, match, Fixture.Spectator);
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void ManualModeWaitsForNoSpectatorUnlessTheMatchAsks()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone);
        ReadyAll(host, match, Everyone);
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
        Assert.Contains(host.Chat, line => line.Contains("is ready (4/4)"));
    }

    [Fact]
    public void ASpectatorsForceReadyReadiesTheSpectatorsOnly()
    {
        var (match, host, _) = Build(spectatorsReadyUp: true);
        Connect(host, match, Fixture.Spectator, Fixture.Team1Player1);

        Assert.True(match.TryForceReady(P(host, Fixture.Spectator)));
        Assert.Contains(host.Chat, line => line.Contains("force-readied the spectators"));
        Assert.False(match.TryForceReady(P(host, Fixture.Spectator)));
        host.Advance(20);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        Connect(host, match, Everyone[1..]);
        ReadyAll(host, match, Everyone);
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void AnAbsentSpectatorBlocksNobodyOnceAnotherForceReadies()
    {
        var host = new FakeGameHost();
        var config = Fixture.Config(readyMode: ReadyMode.Manual);
        config.roster.spectators = [Fixture.Spectator, Fixture.Stranger];
        config.spectatorsReadyUp = true;
        var match = new MatchState(host, config, new FakeAgentChannel(), Fixture.Messages, () => { });
        match.EnterWarmup();
        Connect(host, match, [.. Everyone, Fixture.Spectator]);
        ReadyAll(host, match, Everyone);

        Assert.True(match.TryForceReady(P(host, Fixture.Spectator)));
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void AutoModeDoesNotWaitForSpectators()
    {
        var (match, host, _) = Build(ReadyMode.Auto, spectatorsReadyUp: true);
        Connect(host, match, Everyone);
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void TheDeadlineReportsReadyTeamsAndUnreadySpectators()
    {
        var (match, host, _) = Build(timeout: 30, spectatorsReadyUp: true);
        Connect(host, match, Everyone);
        ReadyAll(host, match, Everyone);

        host.Advance(30);

        Assert.Null(match.WinnerTeam);
        var teams = match.ReadyAtDeadline!;
        Assert.Equal((4, true), (teams.team1.connected + teams.team2.connected, teams.team1.ready && teams.team2.ready));
        Assert.Equal((0, false), (teams.spectators!.connected, teams.spectators.ready));
    }

    [Fact]
    public void AutoModeAnswersReadyWithoutChangingAnything()
    {
        var (match, host, _) = Build(ReadyMode.Auto);
        Connect(host, match, Everyone[0]);

        Assert.False(match.TrySetReady(P(host, Everyone[0]), true));

        Assert.Contains("starts on its own", Assert.Single(host.DirectChat).Message);
    }

    [Fact]
    public void LeavingDropsTheReady()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone);
        ReadyAll(host, match, Everyone[..3]);

        host.PlayerList.RemoveAll(player => player.SteamIdString == Everyone[0]);
        match.ResolvePlayerDeparture();
        Connect(host, match, Everyone[0]);
        ReadyAll(host, match, Everyone[3]);
        host.Advance(15);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.Contains(host.Chat, line => line.Contains("is ready (3/4)"));
    }

    [Fact]
    public void RestartedWarmupClearsReadies()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone);
        ReadyAll(host, match, Everyone[..3]);

        match.EnterWarmup();
        ReadyAll(host, match, Everyone[3]);
        host.Advance(15);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
    }

    [Fact]
    public void AForceReadiedTeamNeedsNoReadiesWhileTheOtherTeamStillDoes()
    {
        var (match, host, _) = Build();
        Connect(host, match, Fixture.Team1Player1, Fixture.Team2Player1, Fixture.Team2Player2);

        Assert.True(match.TryForceReady(P(host, Fixture.Team1Player1)));
        Assert.Contains(host.Chat, line => line.Contains("force-readied") && line.Contains("Alpha"));
        Assert.Equal("READY", host.ClanTags[Fixture.Team1Player1]);
        host.Advance(20);
        Assert.Equal(MatchPhase.Warmup, match.Phase);

        ReadyAll(host, match, Fixture.Team2Player1, Fixture.Team2Player2);
        host.Advance(14);

        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void WithoutAllowForceReadyNeitherACaptainNorASpectatorCanForceReady()
    {
        var (match, host, sent) = Build(spectatorsReadyUp: true, allowForceReady: false);
        Connect(host, match, [.. Everyone, Fixture.Spectator]);
        host.DirectChat.Clear();
        host.Chat.Clear();

        Assert.False(match.TryForceReady(P(host, Fixture.Team1Player1)));
        Assert.False(match.TryForceReady(P(host, Fixture.Spectator)));

        Assert.Empty(host.DirectChat);
        Assert.Empty(host.Chat);
        Assert.Equal("NOT READY", host.ClanTags[Fixture.Team1Player1]);
        Assert.DoesNotContain(sent.OfType<PluginLog>(), log => log.message.Contains("force ready"));
        host.Advance(20);
        Assert.Equal(MatchPhase.Warmup, match.Phase);
    }

    [Fact]
    public void UnreadyCannotWithdrawAForceReady()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone);
        Assert.True(match.TryForceReady(P(host, Fixture.Team1Player1)));
        host.DirectChat.Clear();

        Assert.False(match.TrySetReady(P(host, Fixture.Team1Player2), false));

        Assert.Contains("force-readied", Assert.Single(host.DirectChat).Message);
        Assert.Equal("READY", host.ClanTags[Fixture.Team1Player2]);
    }

    [Fact]
    public void AForceReadiedTeamIsLeftOutOfTheReminder()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone);
        Assert.True(match.TryForceReady(P(host, Fixture.Team1Player1)));
        host.Chat.Clear();

        host.Advance(30);

        Assert.Contains(host.Chat, line => line.Contains("Not ready: p11, p12."));
    }

    [Fact]
    public void AForceReadiedTeamCountsAsReadyAtTheDeadline()
    {
        var (match, host, _) = Build(timeout: 60);
        Connect(host, match, Fixture.Team1Player1, Fixture.Team2Player1);
        Assert.True(match.TryForceReady(P(host, Fixture.Team1Player1)));

        host.Advance(60);

        Assert.Null(match.WinnerTeam);
        Assert.Equal((1, true), (match.ReadyAtDeadline!.team1.connected, match.ReadyAtDeadline.team1.ready));
        Assert.Equal((1, false), (match.ReadyAtDeadline.team2.connected, match.ReadyAtDeadline.team2.ready));
    }

    [Fact]
    public void OnlyRosteredPeopleMakeATeamPresent()
    {
        var (match, host, _) = Build(ReadyMode.Auto);
        Connect(host, match, Fixture.Team1Player1, Fixture.Team1Player2);
        host.PlayerList.AddRange([Fixture.Bot(Side.T, 8), Fixture.Player(Fixture.Stranger, Side.T, 9)]);
        match.MaybeStart();

        host.Advance(20);

        Assert.Equal(MatchPhase.Warmup, match.Phase);
    }

    [Fact]
    public void ClanTagsShowReadinessUntilTheCountdown()
    {
        var (match, host, _) = Build();
        Assert.Equal("1", host.StringConVars["sv_clantags_enabled"]);
        Connect(host, match, Everyone);
        Assert.Equal("NOT READY", host.ClanTags[Everyone[0]]);

        ReadyAll(host, match, Everyone[0]);
        Assert.Equal("READY", host.ClanTags[Everyone[0]]);
        Assert.Equal("NOT READY", host.ClanTags[Everyone[1]]);

        ReadyAll(host, match, Everyone[1..]);
        Assert.All(Everyone, id => Assert.Equal("", host.ClanTags[id]));
        Assert.Equal("0", host.StringConVars["sv_clantags_enabled"]);
    }

    [Fact]
    public void AutoModeHidesClanTags()
    {
        var (match, host, _) = Build(ReadyMode.Auto);
        Connect(host, match, Everyone);

        Assert.Equal("0", host.StringConVars["sv_clantags_enabled"]);
        Assert.Empty(host.ClanTags);
    }

    [Fact]
    public void TheReminderNamesUnreadyPlayersEveryThirtySeconds()
    {
        var (match, host, _) = Build();
        Connect(host, match, Everyone);
        ReadyAll(host, match, Everyone[1..]);
        host.Chat.Clear();

        host.Advance(30);
        Assert.Contains(host.Chat, line => line.Contains("Not ready: p01.") && line.Contains("!" + ChatCommands.Ready));

        ReadyAll(host, match, Everyone[0]);
        host.Chat.Clear();
        host.Advance(30);
        Assert.DoesNotContain(host.Chat, line => line.Contains("Not ready"));
    }

    [Fact]
    public void TheDeadlineWarnsTheUnreadyTeamAndReportsWhoWasReady()
    {
        var (match, host, _) = Build(timeout: 120);
        var expired = 0;
        match.ReadyExpired += () => expired++;
        Connect(host, match, Everyone);
        ReadyAll(host, match, Fixture.Team2Player1, Fixture.Team2Player2, Fixture.Team1Player1);

        host.Advance(30);
        Assert.Contains(host.Chat, line => line.Contains("Alpha") && line.Contains("must be ready within 1:30"));
        Assert.Equal(0, expired);

        host.Advance(90);
        EndWarmupIntoRound(host, match);
        Assert.Equal(1, expired);
        Assert.Equal(MapEndReasons.ReadyTimeout, match.EndReason);
        Assert.Null(match.WinnerTeam);
        Assert.Equal((2, false), (match.ReadyAtDeadline!.team1.connected, match.ReadyAtDeadline.team1.ready));
        Assert.Equal((2, true), (match.ReadyAtDeadline.team2.connected, match.ReadyAtDeadline.team2.ready));
        Assert.Null(match.ReadyAtDeadline.spectators);
    }

    // The deadline ticks on one repeating timer: a chain of one-shot timers, each scheduled from
    // the last, fired a frame late per second and ran seconds behind the warmup clock.
    [Fact]
    public void TheDeadlineTicksOnOneRepeatingTimer()
    {
        var (_, host, _) = Build(timeout: 60);

        var tick = Assert.Single(host.Timers, timer => timer.Period == 1);
        host.Advance(10);
        Assert.Same(tick, Assert.Single(host.Timers, timer => timer.Period == 1));
    }

    [Fact]
    public void TheLastMinuteWarnsAtSixtyAndThirtySecondsAndTheDeadlineCancelsTheMatch()
    {
        var (match, host, _) = Build(timeout: 90);
        var expired = 0;
        match.ReadyExpired += () => expired++;

        host.Advance(29.5);
        Assert.DoesNotContain(host.Chat, line => line.Contains("left to ready up"));
        host.Advance(0.5);
        Assert.Single(host.Chat, line => line.Contains("1:00 left to ready up"));
        host.Advance(30);
        Assert.Single(host.Chat, line => line.Contains("0:30 left to ready up"));
        Assert.DoesNotContain(host.Chat, line => line.Contains("must be ready within"));

        host.Advance(29.9);
        Assert.DoesNotContain(Fixture.Messages.Prefixed("ready.canceled"), host.Chat);
        host.Advance(0.1);
        Assert.Equal(0, expired);
        EndWarmupIntoRound(host, match);
        Assert.Equal(1, expired);
        Assert.Equal(2, host.Chat.Count(line => line.Contains("left to ready up")));
        Assert.Single(host.Chat, line => line == Fixture.Messages.Prefixed("ready.canceled"));
    }

    [Theory]
    [InlineData(ReadyMode.Manual)]
    [InlineData(ReadyMode.Auto)]
    public void AfterTheDeadlineNobodyReadiesUpAndNoCountdownStarts(ReadyMode mode)
    {
        var (match, host, _) = Build(mode, timeout: 30);
        Connect(host, match, Everyone[..3]);

        host.Advance(30);
        host.DirectChat.Clear();
        Connect(host, match, Everyone[3]);

        if (mode == ReadyMode.Manual)
        {
            Assert.False(match.TrySetReady(P(host, Everyone[0]), true));
            Assert.False(match.TryForceReady(P(host, Everyone[0])));
            Assert.Empty(host.DirectChat);
        }
        host.Advance(20);
        Assert.Equal(MatchPhase.Warmup, match.Phase);
        Assert.DoesNotContain(host.Chat, line => line.Contains("starts in"));
    }

    [Fact]
    public void AfterTheDeadlineAJoiningPlayerIsKickedAndNobodyIsRemindedToReadyUp()
    {
        var (match, host, _) = Build(timeout: 30);
        var teams = new TeamLock(match, host, Fixture.Messages);
        Connect(host, match, Everyone[..3]);

        host.Advance(30);
        host.Chat.Clear();
        var late = Fixture.Player(Everyone[3], Side.T, host.PlayerList.Count);

        Assert.True(match.Canceled);
        Assert.True(teams.Apply(late));
        Assert.Equal((late, "Ready-up time ran out"), Assert.Single(host.Kicked));
        host.Advance(60);
        Assert.DoesNotContain(host.Chat, line => line.Contains("Not ready") || line.Contains("Waiting for"));
    }

    [Fact]
    public void TheReminderDueOnTheDeadlineTickIsNotSent()
    {
        var (match, host, _) = Build(timeout: 60);
        Connect(host, match, Everyone[..1]);
        host.Advance(59.5);
        host.Chat.Clear();

        host.Advance(0.5);

        Assert.Contains(Fixture.Messages.Prefixed("ready.canceled"), host.Chat);
        Assert.DoesNotContain(host.Chat, line => line.Contains("Not ready") || line.Contains("Waiting for"));
    }

    [Fact]
    public void ADeadlineOnAnEmptyServerReportsNobodyConnected()
    {
        var (match, host, _) = Build(timeout: 30);
        var expired = 0;
        match.ReadyExpired += () => expired++;

        host.Advance(30 + MatchState.ReadyExpiryFallbackSeconds);

        Assert.Equal(1, expired);
        Assert.Null(match.WinnerTeam);
        var teams = match.ReadyAtDeadline!;
        Assert.Equal((0, false, 0, false), (teams.team1.connected, teams.team1.ready, teams.team2.connected, teams.team2.ready));
    }

    [Fact]
    public void AutoModeDeadlineCountsAMissingPlayerAsUnready()
    {
        var (match, host, _) = Build(ReadyMode.Auto, timeout: 30);
        Connect(host, match, Everyone[..3]);

        host.Advance(30);

        Assert.Null(match.WinnerTeam);
        Assert.True(match.ReadyAtDeadline!.team1.ready);
        Assert.Equal((1, false), (match.ReadyAtDeadline.team2.connected, match.ReadyAtDeadline.team2.ready));
    }

    [Fact]
    public void TheCountdownStopsTheDeadline()
    {
        var (match, host, _) = Build(timeout: 20);
        var expired = 0;
        match.ReadyExpired += () => expired++;
        Connect(host, match, Everyone);

        host.Advance(10);
        ReadyAll(host, match, Everyone);
        host.Advance(14);

        Assert.Equal(0, expired);
        Assert.Equal(MatchPhase.Knife, match.Phase);
    }

    [Fact]
    public void TheDeadlineEndsTheUnplayedMatch()
    {
        var host = new FakeGameHost();
        var sent = new List<PluginMessage>();
        var shutdowns = 0;
        var match = new MatchState(host, Fixture.Config(readyMode: ReadyMode.Manual, readyTimeoutSeconds: 30), new FakeAgentChannel(sent),
            Fixture.Messages, () => { });
        var forwarder = new EventForwarder(match, host);
        var callbacks = new GameplayCallbacks(match, host, forwarder, new DamageTracker(forwarder),
            new TeamLock(match, host, Fixture.Messages), () => { }, () => shutdowns++, () => { }, () => { });
        match.ReadyExpired += callbacks.EndUnplayed;
        match.EnterWarmup();
        Connect(host, match, Fixture.Team1Player1, Fixture.Team1Player2);
        ReadyAll(host, match, Fixture.Team1Player1, Fixture.Team1Player2);

        host.Advance(30);
        Assert.Empty(sent.OfType<MapEnd>());
        host.IsWarmup = false;
        callbacks.Handle(new RoundStartInput());
        host.Advance(0.1);

        var end = Assert.Single(sent.OfType<MapEnd>());
        Assert.Equal(MapEndReasons.ReadyTimeout, end.reason);
        Assert.Null(end.winnerTeam);
        Assert.Equal((0, 0), (end.team1Score, end.team2Score));
        Assert.Equal((2, true, 0, false), (end.teams!.team1.connected, end.teams.team1.ready, end.teams.team2.connected, end.teams.team2.ready));
        Assert.Equal(MatchPhase.PostLive, match.Phase);
        Assert.Equal(1, shutdowns);
    }

    [Theory]
    [InlineData(600, true)]
    [InlineData(330, false)]
    [InlineData(300, true)]
    [InlineData(90, true)]
    [InlineData(45, false)]
    [InlineData(10, false)]
    [InlineData(5, false)]
    [InlineData(0, false)]
    public void WarningsComeEachMinuteThenEveryThirtySeconds(int secondsLeft, bool warn) =>
        Assert.Equal(warn, ReadyUp.ShouldWarn(secondsLeft));
}
