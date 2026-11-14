using System.Runtime.CompilerServices;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

/// <summary>
/// Guards the cfg files the image ships in <c>cfg/</c>. Going live, the plugin runs the game mode's
/// own cfg, then base, then the launch's cfg, and nothing else puts back what warmup and the knife
/// round change. They are plain text the game execs, so the only thing a test can do is assert the
/// invariants the state machine depends on.
/// </summary>
public class CfgTests
{
    private static string CfgDir([CallerFilePath] string thisFile = "") =>
        Path.Combine(Path.GetDirectoryName(thisFile)!, "..", "cfg");

    /// <summary>
    /// What warmup and the knife round set that the plugin leaves to Valve's mode cfg to set again
    /// going live: every one of them is in <c>gamemode_competitive.cfg</c> and
    /// <c>gamemode_competitive2v2.cfg</c>. A cvar not here and not in base.cfg would keep its warmup or
    /// knife value through the match.
    /// </summary>
    private static readonly HashSet<string> SetByTheModeCfg = new(StringComparer.OrdinalIgnoreCase)
    {
        "mp_afterroundmoney", "mp_buy_anywhere", "mp_buytime", "mp_ct_default_melee", "mp_ct_default_primary",
        "mp_ct_default_secondary", "mp_death_drop_defuser", "mp_death_drop_grenade", "mp_death_drop_gun",
        "mp_defuser_allocation", "mp_free_armor", "mp_freezetime", "mp_friendlyfire", "mp_halftime",
        "mp_match_can_clinch", "mp_maxmoney", "mp_maxrounds", "mp_playercashawards", "mp_randomspawn",
        "mp_respawn_immunitytime", "mp_respawn_on_death_ct", "mp_respawn_on_death_t", "mp_roundtime",
        "mp_roundtime_defuse", "mp_roundtime_hostage", "mp_solid_teammates", "mp_startmoney",
        "mp_t_default_melee", "mp_t_default_primary", "mp_t_default_secondary", "mp_teamcashawards",
        "mp_timelimit", "mp_warmuptime", "mp_weapons_allow_map_placed", "mp_weapons_allow_zeus",
        "sv_allow_votes", "sv_deadtalk", "sv_infinite_ammo", "sv_talk_enemy_dead", "sv_talk_enemy_living",
    };

    private static string[] Lines(string name) =>
        File.ReadAllLines(Path.Combine(CfgDir(), name))
            .Select(l => l.Split("//")[0].Trim())
            .Where(l => l.Length > 0)
            .ToArray();

    private static IEnumerable<string> Cvars(string name) => Lines(name).Select(l => l.Split(' ', '\t')[0]);

    [Fact]
    public void TheImageShipsResetBaseWarmupKnifeAndTheMarkerAlone()
    {
        Assert.Equal(["applied.cfg", "base.cfg", "knife.cfg", "reset.cfg", "warmup.cfg"],
            Directory.GetFiles(CfgDir(), "*.cfg").Select(Path.GetFileName).Order(StringComparer.Ordinal));
    }

    [Fact]
    public void TheMarkerCfgRunsTheMarkerAlone()
    {
        Assert.Equal([CfgNames.AppliedCommand], Lines("applied.cfg"));
        Assert.Equal($"{CfgNames.Dir}/applied", CfgNames.Applied);
    }

    [Theory]
    [InlineData("base.cfg")]
    [InlineData("warmup.cfg")]
    [InlineData("knife.cfg")]
    public void TeamSelectionRemainsDisabledAcrossPhaseChanges(string file)
    {
        Assert.Contains("sv_disable_teamselect_menu 1", Lines(file));
    }

    /// <summary>Going live puts back every setting warmup and the knife round change, from the reset, the mode's cfg or base.</summary>
    [Theory]
    [InlineData("warmup.cfg")]
    [InlineData("knife.cfg")]
    public void EverythingAPhaseChangesIsSetAgainGoingLive(string file)
    {
        var setAgain = Cvars("reset.cfg").Concat(Cvars("base.cfg")).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var leftOver = Cvars(file).Where(cvar => !setAgain.Contains(cvar) && !SetByTheModeCfg.Contains(cvar)).ToArray();

        Assert.True(leftOver.Length == 0, $"{file} sets {string.Join(", ", leftOver)}, which nothing sets again going live");
    }

    /// <summary>GOTV is the launch's: the shipped cfgs never set it, so no phase change touches it.</summary>
    [Theory]
    [InlineData("reset.cfg")]
    [InlineData("base.cfg")]
    [InlineData("warmup.cfg")]
    [InlineData("knife.cfg")]
    public void NoShippedCfgSetsGotv(string file)
    {
        Assert.DoesNotContain(Cvars(file), cvar => cvar.StartsWith("tv_", StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>The format is the mode's and the launch's: base sets no round rules of its own.</summary>
    [Theory]
    [InlineData("mp_maxrounds")]
    [InlineData("mp_overtime_enable")]
    [InlineData("mp_overtime_maxrounds")]
    [InlineData("ff_damage_reduction_bullets")]
    [InlineData("ff_damage_reduction_other")]
    public void BaseLeavesTheRulesToTheModeAndTheLaunch(string cvar)
    {
        Assert.DoesNotContain(Cvars("base.cfg"), name => name.Equals(cvar, StringComparison.OrdinalIgnoreCase));
    }

    [Fact]
    public void KnifeOverridesWarmupRoundLimitSoItsFirstWinCannotEndTheMatch()
    {
        Assert.Contains("mp_maxrounds 0", Lines("warmup.cfg"));
        Assert.Contains("mp_maxrounds 24", Lines("knife.cfg"));
        Assert.Contains("mp_winlimit 0", Lines("knife.cfg"));
    }

    /// <summary>CS2 plays the team intro going live only after a warmup and knife round with halftime off.</summary>
    [Theory]
    [InlineData("warmup.cfg")]
    [InlineData("knife.cfg")]
    public void HalftimeIsOffBeforeGoingLiveSoTheTeamIntroPlays(string file)
    {
        Assert.Contains("mp_halftime 0", Lines(file));
    }

    [Fact]
    public void TheKnifeCfgKeepsTheGameOutOfTheWayForOneRound()
    {
        var lines = Lines("knife.cfg");

        Assert.Contains("mp_match_can_clinch 0", lines);
        Assert.Contains("mp_startmoney 0", lines);
        Assert.Contains("mp_ct_default_melee weapon_knife", lines);
        Assert.Contains("mp_t_default_melee weapon_knife", lines);
    }

    [Fact]
    public void WarmupGivesTwoSecondsOfRespawnImmunity()
    {
        Assert.Contains("mp_respawn_immunitytime 2", Lines("warmup.cfg"));
    }
}
