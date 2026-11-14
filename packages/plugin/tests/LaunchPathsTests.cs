using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

/// <summary>The agent lays these paths out, so regrouping the constants must not move one.</summary>
public class LaunchPathsTests
{
    [Fact]
    public void AbsolutePathsSitUnderTheGameDirectory()
    {
        Assert.Equal("/home/steam/cs2-dedicated", ServerPaths.InstallDir);
        Assert.Equal("/home/steam/cs2-dedicated/game/csgo", ServerPaths.GameDir);
        Assert.Equal("/home/steam/cs2-dedicated/game/csgo/ankor", ServerPaths.AnkorDir);
        Assert.Equal("/home/steam/cs2-dedicated/game/csgo/ankor/match.json", MatchConfigFile.Path);
        Assert.Equal("/home/steam/cs2-dedicated/game/csgo/ankor/demos", Demos.Path);
        Assert.Equal("/home/steam/cs2-dedicated/game/csgo/ankor/backups", Backups.Path);
        Assert.Equal("/home/steam/cs2-dedicated/game/csgo/ankor/restore.txt", Backups.RestorePath);
    }

    [Fact]
    public void EngineCommandsGetPathsRelativeToTheGameDirectory()
    {
        Assert.Equal("ankor/demos", Demos.RecordDir);
        Assert.Equal("ankor/backups", Backups.Dir);
        Assert.Equal("ankor/backups/ankor", Backups.RoundFile);
        Assert.Equal("ankor/restore.txt", Backups.RestoreFile);
    }

    [Fact]
    public void TheAgentReadsDemosRelativeToTheInstallDirectory()
    {
        Assert.Equal("game/csgo/ankor/demos", Demos.Dir);
    }

    [Theory]
    [InlineData(1, "ankor_round00.txt")]
    [InlineData(5, "ankor_round04.txt")]
    [InlineData(101, "ankor_round100.txt")]
    public void ARoundsBackupIsNumberedByTheRoundsPlayedBeforeIt(int round, string file)
    {
        Assert.Equal(file, Backups.FileNameForRound(round));
    }
}
