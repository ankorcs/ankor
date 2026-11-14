using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class RoundBackupsTests
{
    private sealed class Harness
    {
        public readonly GameplayTestSession Session;
        /// <summary>The server's log, where the backups log beside the match's record.</summary>
        public List<(LogLevel Level, string Message)> Logs => Session.Engine.Logs;
        public RoundBackups Backups => Session.Backups;

        public Harness(int gameType = 0, int gameMode = 1)
        {
            Session = new(maxRounds: 24, gameType: gameType, gameMode: gameMode) { EngineWritesBackups = false };
            Session.Engine.ConVars["mp_overtime_enable"] = 1;
            Session.Engine.Logs.Clear();
        }

        public void StartRound(int number) => Session.StartRound(number);

        public void Write(string name, string contents = Fixture.EmptyBackup, double agoSeconds = 0) =>
            Session.WriteBackup(name, contents, Session.Engine.NowMs - (long)(agoSeconds * 1000));

        public RoundBackup[] Sent => Session.Sent.OfType<RoundBackup>().ToArray();
    }

    [Fact]
    public void ALiveRoundSendsTheEngineFileWithTheStateItLacks()
    {
        var harness = new Harness();
        harness.Write("ankor_round00.txt");

        harness.StartRound(1);

        var backup = Assert.Single(harness.Sent);
        Assert.Equal(1, backup.round);
        Assert.Equal(Side.Ct, backup.team1Side);
        Assert.Equal((0, 0), (backup.team1Score, backup.team2Score));
        Assert.Equal((24, true, 6), (backup.rules.maxRounds, backup.rules.overtime, backup.rules.overtimeMaxRounds));
        Assert.Equal(Fixture.EmptyBackup, backup.file);
        Assert.Same(harness.Session.Match.Config.roster, backup.roster);
        Assert.Empty(harness.Logs);
    }

    [Fact]
    public void TheFileIsNamedByTheRoundsPlayedAndTheScoreIsTheOneBeforeTheRound()
    {
        var harness = new Harness();
        harness.Write("ankor_round00.txt");
        harness.StartRound(1);
        harness.Session.EndRound(Side.Ct, 8);
        harness.Session.Engine.RunNextFrame();
        harness.Write("ankor_round01.txt", "round two");

        harness.StartRound(2);

        var backup = harness.Sent[^1];
        Assert.Equal(2, backup.round);
        Assert.Equal("round two", backup.file);
        Assert.Equal((1, 0), (backup.team1Score, backup.team2Score));
    }

    [Theory]
    [InlineData(false, "", 0, "ankor_round00.txt is missing")]
    [InlineData(true, "", 0, "ankor_round00.txt is empty")]
    [InlineData(true, "x", 60, "ankor_round00.txt is older than the round")]
    public void ALiveRoundWithoutAUsableFileIsAnError(bool written, string contents, double agoSeconds, string problem)
    {
        var harness = new Harness();
        if (written) harness.Write("ankor_round00.txt", contents, agoSeconds);

        harness.StartRound(1);

        Assert.Empty(harness.Sent);
        Assert.Equal([(LogLevel.Error, $"round 1 has no backup: {problem}")], harness.Logs);
    }

    [Fact]
    public void AFileTooLongForTheWireIsAnError()
    {
        var harness = new Harness();
        harness.Write("ankor_round00.txt", new string('x', RoundBackup.MaxFileChars + 1));

        harness.StartRound(1);

        Assert.Empty(harness.Sent);
        Assert.Equal(LogLevel.Error, Assert.Single(harness.Logs).Level);
    }

    [Fact]
    public void TabsAndLineBreaksOfEitherKindAreSent()
    {
        var harness = new Harness();
        harness.Write("ankor_round00.txt", "\"SaveFile\"\r\n{\n\t\"name\"\t\t\"a\"\n}");

        harness.StartRound(1);

        Assert.Equal("\"SaveFile\"\r\n{\n\t\"name\"\t\t\"a\"\n}", Assert.Single(harness.Sent).file);
    }

    [Theory]
    [InlineData("\u0001")]
    [InlineData("\u001b")]
    [InlineData("\u007f")]
    public void AFileWithAControlCharacterIsAnError(string control)
    {
        var harness = new Harness();
        harness.Write("ankor_round00.txt", $"\"SaveFile\"\n{{\n\t\"name\"\t\t\"a{control}b\"\r\n}}");

        harness.StartRound(1);

        Assert.Empty(harness.Sent);
        Assert.Equal([(LogLevel.Error, "round 1 has no backup: ankor_round00.txt holds a control character other than a tab or line break")], harness.Logs);
    }

    [Fact]
    public void TheBackupFileIsSetOnceAfterTheLiveCfg()
    {
        var harness = new Harness();
        var commands = harness.Session.Engine.CommandNames.ToList();

        // The session bootstrapped through warmup into live; warmup leaves the engine's default file.
        var live = commands.IndexOf(Fixture.LiveCfgs);
        Assert.Equal([Fixture.LiveCfgs, "mp_backup_round_auto 1", "mp_backup_round_file ankor/backups/ankor"],
            commands.Skip(live).Take(3));
        Assert.Single(commands, command => command.StartsWith("mp_backup_round_file"));
    }

    [Fact]
    public void NothingIsReadBeforeTheMatchIsLive()
    {
        var session = new GameplayTestSession();
        session.WriteBackup("ankor_round00.txt", Fixture.EmptyBackup, session.Engine.NowMs);

        session.Match.EnterWarmup();
        session.Engine.IsWarmup = true;
        session.Send(new RoundStartInput());
        session.Engine.RunNextFrame();

        Assert.Empty(session.Sent.OfType<RoundBackup>());
        Assert.DoesNotContain(session.Engine.Logs, log => log.Level is LogLevel.Warn or LogLevel.Error);
    }

    [Fact]
    public void AMatchWithoutStatisticsSendsItsBackupToTheAgentInsteadOfItsEvents()
    {
        var session = new GameplayTestSession(collectEvents: false);

        session.StartRound(1);
        session.Engine.RunNextFrame();

        Assert.Empty(session.Sent);
        var backup = Assert.Single(session.Progress.OfType<PluginBackup>()).backup;
        Assert.Equal(1, backup.round);
        Assert.Equal(Fixture.EmptyBackup, backup.file);
    }

    [Fact]
    public void ABackupIsDroppedWhenTheMatchLeftLiveBeforeItWasRead()
    {
        var session = new GameplayTestSession(collectEvents: false);

        session.Engine.DeferNextFrames = true;
        session.StartRound(1);
        session.Match.EnterWarmup();
        session.Engine.RunNextFrame();

        Assert.Empty(session.Progress.OfType<PluginBackup>());
    }

    private static IEnumerable<string> LoadCommands(Harness harness) =>
        harness.Session.Engine.CommandNames.Where(command => command.StartsWith(Ankor.Core.Wire.Backups.LoadCommand));

    [Fact]
    public void ARestoreLoadsTheFileTheEngineWroteAsThatRoundStarted()
    {
        var harness = new Harness();
        harness.StartRound(1);
        harness.Session.WriteFile(Path.Combine(Ankor.Core.Wire.Backups.Dir, "ankor_round00.txt"));

        Assert.Null(harness.Backups.Restore(1));

        Assert.Equal(["mp_backup_restore_load_file ankor/backups/ankor_round00.txt"], LoadCommands(harness));
    }

    [Fact]
    public void ARestoreWithoutItsFileIsRefusedAndLoadsNothing()
    {
        var harness = new Harness();
        harness.StartRound(1);

        Assert.Equal(CommandResult.NoBackup, harness.Backups.Restore(1));
        Assert.Empty(LoadCommands(harness));
    }

    [Fact]
    public void TheRestoredRoundSendsNoBackupAndLogsNothing()
    {
        var harness = new Harness();
        for (var round = 1; round <= 3; round++)
        {
            harness.Write(Ankor.Core.Wire.Backups.FileName(round - 1));
            harness.StartRound(round);
            harness.Session.EndRound(Side.Ct, 8);
            harness.Session.Engine.RunNextFrame();
        }
        harness.Session.Sent.Clear();
        harness.Logs.Clear();
        harness.Session.Engine.Advance(60);

        harness.Session.RestoreAt(round: 2, team1Score: 1, team2Score: 0);
        harness.Session.Engine.RunNextFrame();

        Assert.Empty(harness.Sent);
        // The match logs the restore itself; the backups, which log only problems, say nothing.
        Assert.DoesNotContain(harness.Logs, log => log.Level is LogLevel.Warn or LogLevel.Error);
    }

    [Fact]
    public void ARoundPlayedAfterTheRestoredOneIsBackedUpFromItsNewFile()
    {
        var harness = new Harness();
        harness.Write("ankor_round00.txt");
        harness.StartRound(1);
        harness.Session.EndRound(Side.Ct, 8);
        harness.Session.Engine.RunNextFrame();
        harness.Session.RestoreAt(round: 1, team1Score: 0, team2Score: 0);
        harness.Session.Engine.RunNextFrame();
        harness.Session.EndRound(Side.Ct, 8);
        harness.Session.Engine.RunNextFrame();
        harness.Session.Sent.Clear();
        harness.Write("ankor_round01.txt", "round 2");

        harness.StartRound(2);
        harness.Session.Engine.RunNextFrame();

        var backup = Assert.Single(harness.Sent);
        Assert.Equal((2, "round 2"), (backup.round, backup.file));
    }

    [Fact]
    public void ALoadOfAFileInTheGameDirectoryGoesToTheMatch()
    {
        var harness = new Harness();
        harness.StartRound(1);
        harness.Session.WriteFile("ankor/backups/ankor_round00.txt");

        Assert.True(harness.Backups.BeforeLoad("ankor/backups/ankor_round00.txt"));
    }

    [Theory]
    [InlineData("", "a backup load was refused: no file named")]
    [InlineData("ankor/backups/ankor_round00.txt", "a backup load was refused: ankor/backups/ankor_round00.txt is not in the game directory")]
    public void ALoadWithoutAFileIsRefused(string file, string warning)
    {
        var harness = new Harness();
        harness.StartRound(1);
        harness.Logs.Clear();

        Assert.False(harness.Backups.BeforeLoad(file));
        Assert.Equal([(LogLevel.Warn, warning)], harness.Logs);
    }
}
