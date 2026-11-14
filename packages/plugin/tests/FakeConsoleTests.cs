using Xunit;

namespace Ankor.Tests;

// The fake engine has to order commands as CS2 does, or tests of the plugin's waits prove nothing.
public class FakeConsoleTests
{
    private static (FakeConsole Console, List<string> Markers) Build(Dictionary<string, string> files)
    {
        var markers = new List<string>();
        FakeConsole? console = null;
        console = new FakeConsole(name => files.GetValueOrDefault(name), () => markers.Add($"marker after {console!.Ran.Count}"));
        return (console, markers);
    }

    // The order the exec order probe logged on a CS2 node.
    [Fact]
    public void ALinesExecsRunAfterTheLineAndExecsInsideAFileRunInPlace()
    {
        var (console, _) = Build(new()
        {
            ["a"] = "echo a1\ntv_delay 11\nexec b\necho a2",
            ["b"] = "echo b1\ntv_delay 22\nexec d\necho b2",
            ["c"] = "echo c1\ntv_delay 33\necho c2",
            ["d"] = "echo d1\ntv_delay 44\necho d2",
        });

        console.Queue("exec a; echo line; exec c");
        console.Run();

        Assert.Equal(["exec a", "echo line", "exec c",
            "echo a1", "tv_delay 11", "exec b", "echo b1", "tv_delay 22", "exec d", "echo d1", "tv_delay 44", "echo d2",
            "echo b2", "echo a2", "echo c1", "tv_delay 33", "echo c2"], console.Ran);
        Assert.Equal("33", console.Values["tv_delay"]);
    }

    [Fact]
    public void TheMarkerCfgRunsAfterEveryLineOfTheCfgsBeforeIt()
    {
        var (console, markers) = Build(new()
        {
            ["base"] = "tv_delay 105\nexec node/x",
            ["node/x"] = "tv_delay 30",
            ["ankor/applied"] = "ankor_cfg_applied",
        });

        console.Queue("exec base");
        console.Queue("tv_record demo");
        console.Queue("exec ankor/applied");
        console.Run();

        Assert.True(console.Ran.IndexOf("tv_record demo") < console.Ran.IndexOf("tv_delay 105"));
        Assert.Equal([$"marker after {console.Ran.IndexOf("ankor_cfg_applied") + 1}"], markers);
        Assert.True(console.Ran.IndexOf("tv_delay 30") < console.Ran.IndexOf("ankor_cfg_applied"));
    }

    [Fact]
    public void SemicolonsAndNewlinesSeparateCommandsOutsideQuotes()
    {
        var (console, _) = Build([]);

        console.Queue("hostname \"Ankor | a;b\"; sv_cheats 0\nsay \"x // not a comment\"");
        console.Run();

        Assert.Equal(["hostname \"Ankor | a;b\"", "sv_cheats 0", "say \"x // not a comment\""], console.Ran);
        Assert.Equal("Ankor | a;b", console.Values["hostname"]);
    }

    [Fact]
    public void ACommentEndsItsLineAndTheNextLineStillRuns()
    {
        var (console, _) = Build([]);

        console.Queue("mp_maxrounds 24 // the format\nmp_overtime_enable 1");
        console.Run();

        Assert.Equal(["mp_maxrounds 24", "mp_overtime_enable 1"], console.Ran);
    }

    [Fact]
    public void ANewlineEndsAnUnclosedQuote()
    {
        var (console, _) = Build([]);

        console.Queue("say \"unclosed; still quoted\necho");
        console.Run();

        Assert.Equal(["say \"unclosed; still quoted", "echo"], console.Ran);
    }

    [Fact]
    public void ACommandThatActsSetsNoValue()
    {
        var (console, _) = Build([]);

        console.Queue("mp_restartgame 1; mp_warmuptime 45; tv_record ankor/demos/x");
        console.Run();

        Assert.Equal(["mp_warmuptime"], console.Values.Keys);
    }
}
