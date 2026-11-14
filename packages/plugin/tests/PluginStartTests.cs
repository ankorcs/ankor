using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class PluginStartTests
{
    private const string Match = """
        {"match_id":"0199a0b4-5c00-7000-8000-000000000a09",
        "roster":{"team1":{"name":"Alpha","players":["76561198000000001"]},
        "team2":{"name":"Bravo","players":["76561198000000011"]},
        "spectators":[]},
        "sides":"knife","knife_decision_seconds":45,"ready_mode":"manual","ready_timeout_seconds":300,"record_demo":true,"debug_capture":false,"rules":{"max_rounds":24,"overtime":true,"overtime_max_rounds":6,"friendly_fire":false},"restore":null}
        """;

    [Fact]
    public void AMatchFileDecodesToItsMatch()
    {
        var config = MatchConfigFile.Decode(Match, null, out var error);

        Assert.Equal("0199a0b4-5c00-7000-8000-000000000a09", config?.matchId);
        Assert.Null(error);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData(" \n")]
    public void AMissingOrBlankFileHoldsNoMatchAndIsNoError(string? json)
    {
        Assert.Null(MatchConfigFile.Decode(json, null, out var error));
        Assert.Null(error);
    }

    [Fact]
    public void AnUnreadableFileKeepsItsReadError()
    {
        Assert.Null(MatchConfigFile.Decode(null, "Permission denied", out var error));
        Assert.Equal("Permission denied", error);
    }

    [Fact]
    public void AFileThatDoesNotDecodeIsAnError()
    {
        Assert.Null(MatchConfigFile.Decode("{}", null, out var error));
        Assert.NotNull(error);
    }

    [Theory]
    [InlineData("mp_backup_restore_load_file", true)]
    [InlineData("MP_BACKUP_RESTORE_LOAD_FILE", true)]
    [InlineData("mp_backup_restore_list_files", false)]
    [InlineData(null, false)]
    public void TheBackupLoadCommandMatchesRegardlessOfCase(string? name, bool load)
    {
        Assert.Equal(load, Backups.IsLoadCommand(name));
    }

    [Theory]
    [InlineData("ws://127.0.0.1:27100/plugin", "ws://127.0.0.1:27100/plugin")]
    [InlineData(" ws://127.0.0.1:27100/plugin\n", "ws://127.0.0.1:27100/plugin")]
    [InlineData("127.0.0.1:27100", null)]
    public void TheRelayUrlMustBeAbsolute(string raw, string? expected)
    {
        Assert.Equal(expected, AnkorEnv.ParseWsUrl(raw)?.ToString());
    }

    [Fact]
    public void AnExecEndsWithALineOfItsOwn()
    {
        Assert.Equal("status_json\necho", AgentControl.ExecLine("status_json"));
    }

    [Fact]
    public void AnUnknownFrameIsLoggedByItsFirst200Characters()
    {
        Assert.Equal("{}", AgentControl.Excerpt("{}"));
        Assert.Equal(new string('x', 200), AgentControl.Excerpt(new string('x', 200)));
        Assert.Equal(new string('x', 200) + "…", AgentControl.Excerpt(new string('x', 201)));
    }
}
