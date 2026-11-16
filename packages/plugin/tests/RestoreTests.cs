using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

/// <summary>A live match playing on from one of its earlier rounds: the controller's <c>restore_round</c>, or a load typed at the console.</summary>
public sealed class RestoreTests
{
    /// <summary>A match three rounds in, team 1 (CT) up 2–1, in the halftime break of its six rounds: round 4 ends the break.</summary>
    private static GameplayTestSession ThreeRoundsIn()
    {
        var session = new GameplayTestSession(maxRounds: 6, record: false);
        for (var round = 1; round <= 3; round++)
        {
            session.StartRound(round);
            session.EndRound(round == 2 ? Side.T : Side.Ct, 8);
            session.Engine.RunNextFrame();
        }
        session.Sent.Clear();
        return session;
    }

    /// <summary>Past the break and into round 4, with team 1 on T.</summary>
    private static GameplayTestSession InRoundFour()
    {
        var session = ThreeRoundsIn();
        session.StartRound(4);
        session.Sent.Clear();
        return session;
    }

    private static IEnumerable<string> LoadCommands(GameplayTestSession session) =>
        session.Engine.CommandNames.Where(command => command.StartsWith("mp_backup_restore"));

    /// <summary>
    /// The names go in by the first half's sides when the match goes live, and the engine shows them
    /// by the half of each round, a loaded one too. A restore into either half leaves them alone:
    /// rewriting them by the restored round's sides inverted the scoreboard in the second half.
    /// </summary>
    [Fact]
    public void ARestoreRoundIntoEitherHalfLeavesTheTeamNamesAlone()
    {
        var session = InRoundFour();
        var match = session.Match;
        string Names() => $"{session.Engine.StringConVars["mp_teamname_1"]} / {session.Engine.StringConVars["mp_teamname_2"]}";
        var named = Names();
        Assert.Equal($"{match.Roster.Team1Name} / {match.Roster.Team2Name}", named);

        Assert.Null(match.RestoreRound(4, "ankor/backups/ankor_round03.txt"));
        session.RestoreAt(round: 4, team1Score: 2, team2Score: 1);
        Assert.Equal(Side.T, match.Team1Side);
        Assert.Equal(named, Names());

        Assert.Null(match.RestoreRound(2, "ankor/backups/ankor_round01.txt"));
        session.RestoreAt(round: 2, team1Score: 1, team2Score: 1);
        Assert.Equal(Side.Ct, match.Team1Side);
        Assert.Equal(named, Names());
    }

    [Fact]
    public void ARestoreRoundLeavesTheMatchLiveUntilTheEngineStartsLoadingAndReportsTheRoundBeforeItsStart()
    {
        var session = InRoundFour();
        var match = session.Match;
        Assert.Equal((2, 1), (match.Team1Score, match.Team2Score));

        Assert.Null(match.RestoreRound(2, "ankor/backups/ankor_round01.txt"));

        // Only the command went out: nothing about the match changes until the engine acts.
        Assert.Equal(["mp_backup_restore_load_file ankor/backups/ankor_round01.txt"], LoadCommands(session));
        Assert.DoesNotContain(session.Engine.CommandNames, command => command.Contains("autopause") || command.Contains("mp_backup_round_auto 0"));
        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.True(match.IsLive);
        Assert.Equal(Side.T, match.Team1Side);

        // The engine applies the file: it ends the round without a winner, flag up, and restarts into round 2.
        session.RestoreAt(round: 2, team1Score: 1, team2Score: 1);

        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.Equal((1, 1), (match.Team1Score, match.Team2Score));
        Assert.Equal(1, match.Restores);
        Assert.Equal(["match_restored", "round_start", "round_freeze_end"], session.Sent.Select(message => message.type).Take(3));
        var restored = Assert.IsType<MatchRestored>(session.Sent[0]);
        Assert.Equal((2, Side.Ct, 1, 1), (restored.round, restored.team1Side, restored.team1Score, restored.team2Score));
        Assert.Equal(2, Assert.IsType<RoundStart>(session.Sent[1]).round);
        // The engine's autopause holds the restored round for a second after the confirm.
        Assert.DoesNotContain("mp_unpause_match", session.Engine.CommandNames);
        session.Engine.Advance(1);
        Assert.Contains("mp_unpause_match", session.Engine.CommandNames);

        // Play on: round 2 again, then 3.
        session.EndRound(Side.Ct, 8);
        session.Engine.RunNextFrame();
        Assert.Equal((2, 1), (match.Team1Score, match.Team2Score));
    }

    [Fact]
    public void TheEnginesFirstMoveTakesTheMatchPendingAndTheLockStandsBack()
    {
        var session = InRoundFour();
        var match = session.Match;
        var straggler = Fixture.Player(Fixture.Team2Player2, Side.Ct, 7);
        session.Engine.PlayerList.Add(straggler);
        match.RestoreRound(2, "ankor/backups/ankor_round01.txt");

        // The engine unseats everyone with its flag up; the first move is the sighting.
        session.Engine.LoadingRoundBackup = true;
        session.Send(new PlayerTeamInput(session.Engine.PlayerList[0], Side.T, Side.Spec));
        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.False(match.IsLive);
        Assert.Equal(Side.Ct, match.Team1Side);
        session.Send(new PlayerTeamInput(straggler, Side.Ct, Side.Spec));
        Assert.Empty(session.Engine.SwitchedTeams);
        session.Engine.LoadingRoundBackup = false;

        session.RestoreAt(round: 2, team1Score: 1, team2Score: 1);

        // The restored round reseats whoever the engine left on the wrong side.
        Assert.Equal(Side.Ct, match.Team1Side);
        var moved = Assert.Single(session.Engine.SwitchedTeams);
        Assert.Equal((Fixture.Team2Player2, Side.T), (moved.Player.SteamIdString, moved.Side));

        // A reconnect after the restore is placed by the restored sides.
        session.Engine.SwitchedTeams.Clear();
        session.Send(new PlayerConnectInput(Fixture.Player(Fixture.Team1Player2, Side.T, 8)));
        Assert.Equal((Fixture.Team1Player2, Side.Ct), (Assert.Single(session.Engine.SwitchedTeams).Player.SteamIdString, session.Engine.SwitchedTeams[0].Side));
    }

    /// <summary>Loading a backup, the engine kills each player before it moves them; neither is the players' doing.</summary>
    [Fact]
    public void SendsNoDeathDamageOrTeamMoveOfTheEnginesSeatingForABackup()
    {
        var session = InRoundFour();
        var player = session.Engine.PlayerList[0];
        session.Match.RestoreRound(2, "ankor/backups/ankor_round01.txt");
        var before = session.Sent.Count;

        session.Engine.LoadingRoundBackup = true;
        session.Kill(player, player, weapon: 0);
        session.Send(new PlayerTeamInput(player, player.Side, Side.Spec));
        session.Send(new PlayerTeamInput(player, Side.Spec, player.Side));

        Assert.Equal(MatchPhase.PendingRestore, session.Match.Phase);
        Assert.DoesNotContain(session.Sent.Skip(before), m => m is PlayerHurt or PlayerDeath);
    }

    /// <summary>Events no live check guards are held back too, while the plugin's own log still goes out.</summary>
    [Fact]
    public void SendsNothingButItsOwnLogWhileTheEngineLoadsABackup()
    {
        var session = InRoundFour();
        var before = session.Sent.Count;

        session.Engine.LoadingRoundBackup = true;
        session.Send(new PlayerDisconnectInput(session.Engine.PlayerList[0], "disconnect_by_user"));
        session.Match.LogEvent(LogLevel.Warn, "still reported");

        Assert.False(session.Match.IsLive);
        Assert.Equal(["log"], session.Sent.Skip(before).Select(m => m.type));
    }

    /// <summary>From the request to the restored round, the stream holds the restore and the round's start, and nothing between them.</summary>
    [Fact]
    public void SendsTheRestoreAndThenTheRoundStartAndNothingElseOfTheLoad()
    {
        var session = InRoundFour();
        var player = session.Engine.PlayerList[0];
        session.Match.RestoreRound(2, "ankor/backups/ankor_round01.txt");
        var before = session.Sent.Count;

        session.Engine.LoadingRoundBackup = true;
        session.Kill(player, player, weapon: 0);
        session.Send(new PlayerTeamInput(player, player.Side, Side.Spec));
        session.Send(new PlayerTeamInput(player, Side.Spec, player.Side));
        session.RestoreAt(round: 2, team1Score: 1, team2Score: 1);

        Assert.Equal(["match_restored", "round_start"], session.Sent.Skip(before).Select(m => m.type).Take(2));
        Assert.DoesNotContain(session.Sent.Skip(before), m => m is PlayerHurt or PlayerDeath or RoundEnd);
    }

    /// <summary>A grenade still alive when the load starts belongs to the round the restore cut off: it gets no grenade_end.</summary>
    [Fact]
    public void SendsNoEndOfAGrenadeStillAliveWhenTheRestoreCutItsRoundOff()
    {
        var session = InRoundFour();
        var thrower = session.Engine.PlayerList[0];
        session.Send(new GrenadeSpawnedInput(thrower, GrenadeKind.Molotov, 50));
        Assert.Single(session.Sent.OfType<GrenadeThrow>());
        session.Match.RestoreRound(2, "ankor/backups/ankor_round01.txt");
        var before = session.Sent.Count;

        session.Engine.LoadingRoundBackup = true;
        session.Send(new PlayerTeamInput(thrower, thrower.Side, Side.Spec));
        session.RestoreAt(round: 2, team1Score: 1, team2Score: 1);

        Assert.Equal(["match_restored", "round_start"], session.Sent.Skip(before).Select(m => m.type).Take(2));
        Assert.DoesNotContain(session.Sent.Skip(before), m => m is GrenadeEnd or GrenadeDetonate);
    }

    /// <summary>The lock seats a player the engine left on the wrong side before the round starts.</summary>
    [Fact]
    public void SeatsAStragglerBeforeTheRestoredRoundStarts()
    {
        var session = InRoundFour();
        var straggler = Fixture.Player(Fixture.Team2Player2, Side.Ct, 7);
        session.Engine.PlayerList.Add(straggler);
        session.Match.RestoreRound(2, "ankor/backups/ankor_round01.txt");
        var before = session.Sent.Count;

        session.Engine.LoadingRoundBackup = true;
        session.Send(new PlayerTeamInput(straggler, Side.Ct, Side.Spec));
        session.Engine.LoadingRoundBackup = false;
        session.RestoreAt(round: 2, team1Score: 1, team2Score: 1);

        var moved = Assert.Single(session.Engine.SwitchedTeams);
        Assert.Equal(Side.T, moved.Side);
        // The engine's word of the move comes after the round start and asks for no other.
        session.Send(new PlayerTeamInput(straggler, Side.Ct, Side.T));
        Assert.Single(session.Engine.SwitchedTeams);
        Assert.Equal(["match_restored", "round_start"], session.Sent.Skip(before).Select(m => m.type).Take(2));
    }

    [Fact]
    public void ARestoreToTheOtherHalfSeatsTeam1OnTheSideThatRoundHad()
    {
        var session = ThreeRoundsIn();
        var match = session.Match;
        // Halftime after round 3: team 1 goes to T for round 4.
        Assert.Equal(Side.T, match.Team1Side);
        session.StartRound(4);
        session.EndRound(Side.T, 8);
        session.Engine.RunNextFrame();
        Assert.Equal((3, 1), (match.Team1Score, match.Team2Score));
        // A restore goes in once the next round has started; between rounds it is refused.
        session.StartRound(5);

        Assert.Null(match.RestoreRound(2, "ankor/backups/ankor_round01.txt"));
        session.RestoreAt(round: 2, team1Score: 1, team2Score: 1);

        Assert.Equal(Side.Ct, match.Team1Side);
        Assert.Equal((1, 1), (match.Team1Score, match.Team2Score));
    }

    /// <summary>The console hook arms the plugin the same way the request does; the engine's flag does the rest.</summary>
    [Fact]
    public void ALoadTypedAtTheConsoleIsFollowedLikeARequest()
    {
        var session = InRoundFour();
        var match = session.Match;

        Assert.True(match.BeforeBackupLoad());

        Assert.Equal(MatchPhase.Live, match.Phase);
        session.RestoreAt(round: 3, team1Score: 1, team2Score: 1);
        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.Equal((1, 1), (match.Team1Score, match.Team2Score));
        Assert.Equal(1, match.Restores);
        Assert.Equal("match_restored", session.Sent[0].type);
    }

    /// <summary>A load nobody armed, as from a path the hook does not cover, is followed from the engine's flag alone.</summary>
    [Fact]
    public void ALoadNobodyArmedIsFollowedFromTheEnginesFlag()
    {
        var session = InRoundFour();
        var match = session.Match;

        session.RestoreAt(round: 3, team1Score: 1, team2Score: 1);

        Assert.Equal((1, 1), (match.Team1Score, match.Team2Score));
        Assert.Equal(1, match.Restores);
        Assert.Equal("match_restored", session.Sent[0].type);
    }

    /// <summary>A round the restart cuts off ends with the flag up: nothing is scored, swapped or ended from it.</summary>
    [Fact]
    public void ARoundEndWithTheEnginesFlagUpHasNoResult()
    {
        var session = InRoundFour();
        var match = session.Match;
        Assert.Equal(Side.T, match.Team1Side);

        session.Engine.LoadingRoundBackup = true;
        session.EndRound(Side.T, 8);
        session.Engine.RunNextFrame();

        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.Equal((2, 1), (match.Team1Score, match.Team2Score));
        Assert.Empty(session.Sent.OfType<RoundEnd>());

        // The engine seats team 1 back on CT for round 2; the lock leaves it to the engine.
        var moved = session.Engine.PlayerList[0] with { Side = Side.Ct };
        session.Send(new PlayerTeamInput(moved, Side.T, Side.Ct));
        Assert.Empty(session.Engine.SwitchedTeams);
        session.Engine.PlayerList[0] = moved;
        session.Engine.PlayerList[1] = session.Engine.PlayerList[1] with { Side = Side.T };
        session.Engine.TeamScores[Side.Ct] = 1;
        session.Engine.TeamScores[Side.T] = 1;
        session.Send(new RoundPrestartInput());
        session.Engine.LoadingRoundBackup = false;
        session.Engine.RoundsPlayed = 1;
        session.Send(new RoundStartInput());

        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.Equal(Side.Ct, match.Team1Side);
        Assert.Equal((1, 1), (match.Team1Score, match.Team2Score));
    }

    /// <summary>The engine says nothing when it cannot read a file; the match plays on as if nothing was asked, and the log says so.</summary>
    [Fact]
    public void ALoadTheEngineIgnoresChangesNothingAndIsLogged()
    {
        var session = InRoundFour();
        var match = session.Match;
        match.RestoreRound(2, "ankor/backups/ankor_round01.txt");

        // Round 4 ends as a round, flag down: no load happened.
        session.EndRound(Side.T, 8);
        session.Engine.RunNextFrame();
        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.Equal((3, 1), (match.Team1Score, match.Team2Score));
        Assert.Equal(4, Assert.Single(session.Sent.OfType<RoundEnd>()).round);
        Assert.Equal(Side.T, match.Team1Side);
        Assert.Empty(session.Sent.OfType<PluginLog>());

        session.Engine.Advance(5);

        var log = Assert.Single(session.Sent.OfType<PluginLog>());
        Assert.Equal(LogLevel.Warn, log.level);
        Assert.Contains("did not load", log.message);
        Assert.Contains(Fixture.Messages.Prefixed("restore.failed"), session.Engine.Chat);
        Assert.Empty(session.Sent.OfType<MatchRestored>());
        Assert.Equal(0, match.Restores);

        // Round 5 is round 5.
        session.Sent.Clear();
        session.StartRound(5);
        Assert.Equal(5, Assert.Single(session.Sent.OfType<RoundStart>()).round);
    }

    /// <summary>The engine started loading and then never restarted the round: the match goes back to how the load found it.</summary>
    [Fact]
    public void ALoadThatNeverRestartsTheRoundFailsBackToTheLiveMatch()
    {
        var session = InRoundFour();
        var match = session.Match;
        match.RestoreRound(2, "ankor/backups/ankor_round01.txt");
        session.Engine.LoadingRoundBackup = true;
        session.Send(new PlayerTeamInput(session.Engine.PlayerList[0], Side.T, Side.Spec));
        session.Engine.LoadingRoundBackup = false;
        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.Equal(Side.Ct, match.Team1Side);

        session.Engine.Advance(10);

        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.True(match.IsLive);
        Assert.Equal(Side.T, match.Team1Side);
        Assert.Equal((2, 1), (match.Team1Score, match.Team2Score));
        var log = Assert.Single(session.Sent.OfType<PluginLog>());
        Assert.Equal(LogLevel.Error, log.level);
        Assert.Contains(Fixture.Messages.Prefixed("restore.failed"), session.Engine.Chat);
        Assert.Equal(0, match.Restores);

        // A second load needs the engine's flag of its own.
        session.Sent.Clear();
        match.RestoreRound(2, "ankor/backups/ankor_round01.txt");
        session.StartRound(5);
        Assert.Equal(5, Assert.Single(session.Sent.OfType<RoundStart>()).round);
        Assert.Empty(session.Sent.OfType<MatchRestored>());
    }

    /// <summary>Through the break the engine has the new sides but not yet the players: a load now would land on sides about to flip.</summary>
    [Fact]
    public void ALoadDuringHalftimeIsRefusedFromEveryPath()
    {
        var session = ThreeRoundsIn();
        var match = session.Match;
        Assert.True(match.AtHalftime);

        Assert.Equal(CommandResult.AtHalftime, match.RestoreRound(2, "ankor/backups/ankor_round01.txt"));
        Assert.False(match.BeforeBackupLoad());

        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.True(match.IsLive);
        Assert.Empty(LoadCommands(session));
        // The console load has no one to answer, so it is the one that logs.
        Assert.Single(session.Sent.OfType<PluginLog>(), log => log.level == LogLevel.Warn && log.message.Contains("halftime"));

        // The next round start ends the break, and the same load goes through.
        session.StartRound(4);
        Assert.False(match.AtHalftime);
        match.RestoreRound(2, "ankor/backups/ankor_round01.txt");
        Assert.Single(LoadCommands(session));
    }

    /// <summary>
    /// From a round's end to the next round's start, the engine applies a loaded backup's scores but
    /// never restarts into its round, and stays loading: the load is refused from every path until the
    /// next round has started.
    /// </summary>
    [Fact]
    public void ALoadBetweenRoundsIsRefusedFromEveryPath()
    {
        var session = InRoundFour();
        var match = session.Match;
        session.EndRound(Side.T, 8);
        session.Engine.RunNextFrame();
        session.Sent.Clear();
        Assert.True(match.BetweenRounds);
        Assert.False(match.AtHalftime);

        Assert.Equal(CommandResult.BetweenRounds, match.RestoreRound(2, "ankor/backups/ankor_round01.txt"));
        Assert.False(match.BeforeBackupLoad());

        Assert.Equal(MatchPhase.Live, match.Phase);
        Assert.True(match.IsLive);
        Assert.Empty(LoadCommands(session));
        // The console load has no one to answer, so it is the one that logs.
        Assert.Single(session.Sent.OfType<PluginLog>(), log => log.level == LogLevel.Warn && log.message.Contains("the round has ended"));

        // The next round start opens the window again, and the same load goes through.
        session.StartRound(5);
        Assert.False(match.BetweenRounds);
        Assert.Null(match.RestoreRound(2, "ankor/backups/ankor_round01.txt"));
        Assert.Single(LoadCommands(session));
    }

    /// <summary>A load already running at halftime cannot be refused, so it is followed.</summary>
    [Fact]
    public void ALoadRunningAtHalftimeIsFollowed()
    {
        var session = ThreeRoundsIn();
        var match = session.Match;
        Assert.True(match.AtHalftime);

        session.Engine.LoadingRoundBackup = true;
        session.Send(new PlayerTeamInput(session.Engine.PlayerList[0], Side.T, Side.Spec));

        Assert.Equal(MatchPhase.PendingRestore, match.Phase);
        Assert.Empty(session.Engine.SwitchedTeams);
    }

    /// <summary>Nothing plays on past a map_end: the hook cancels a load, and a restore_round is ignored.</summary>
    [Fact]
    public void ALoadAfterTheMatchHasEndedIsRefused()
    {
        var session = ThreeRoundsIn();
        var match = session.Match;
        match.EnterPostLive();
        session.Sent.Clear();
        session.Engine.Logs.Clear();

        Assert.False(match.BeforeBackupLoad());
        Assert.Equal(CommandResult.NotLive, match.RestoreRound(2, "ankor/backups/ankor_round01.txt"));

        Assert.Equal(MatchPhase.PostLive, match.Phase);
        Assert.Single(session.Engine.Logs, log => log.Level == LogLevel.Warn && log.Message.Contains("match has ended"));
        Assert.Empty(LoadCommands(session));
    }

    [Fact]
    public void ARestoreRoundNeedsALiveMatch()
    {
        var session = new GameplayTestSession(record: false);
        session.Match.EnterWarmup();
        session.Sent.Clear();

        Assert.Equal(CommandResult.NotLive, session.Match.RestoreRound(2, "ankor/backups/ankor_round01.txt"));

        Assert.Equal(MatchPhase.Warmup, session.Match.Phase);
        Assert.Empty(LoadCommands(session));
    }
}
