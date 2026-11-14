using System.Text.Json.Serialization;

namespace Ankor.Core.Wire;

[JsonConverter(typeof(JsonStringEnumConverter<Side>))]
public enum Side
{
    [JsonStringEnumMemberName("t")] T,
    [JsonStringEnumMemberName("ct")] Ct,
    [JsonStringEnumMemberName("spec")] Spec,
}

[JsonConverter(typeof(JsonStringEnumConverter<SideChoiceKind>))]
public enum SideChoiceKind
{
    [JsonStringEnumMemberName("stay")] Stay,
    [JsonStringEnumMemberName("swap")] Swap,
}

/// <summary><c>DamageCause</c> in game-events.ts: what caused damage or a death.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<DamageCause>))]
public enum DamageCause
{
    [JsonStringEnumMemberName("player")] Player,
    [JsonStringEnumMemberName("bomb")] Bomb,
    [JsonStringEnumMemberName("fall")] Fall,
    [JsonStringEnumMemberName("self")] Self,
    [JsonStringEnumMemberName("disconnect")] Disconnect,
    [JsonStringEnumMemberName("other")] Other,
}

[JsonConverter(typeof(JsonStringEnumConverter<GrenadeKind>))]
public enum GrenadeKind
{
    [JsonStringEnumMemberName("flashbang")] Flashbang,
    [JsonStringEnumMemberName("hegrenade")] HeGrenade,
    [JsonStringEnumMemberName("smokegrenade")] SmokeGrenade,
    [JsonStringEnumMemberName("molotov")] Molotov,
    [JsonStringEnumMemberName("incgrenade")] IncGrenade,
    [JsonStringEnumMemberName("decoy")] Decoy,
}

/// <summary>`RoundEndReason` in game-events.ts.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<RoundEndReason>))]
public enum RoundEndReason
{
    [JsonStringEnumMemberName("unknown")] Unknown,
    [JsonStringEnumMemberName("target_bombed")] TargetBombed,
    [JsonStringEnumMemberName("terrorists_escaped")] TerroristsEscaped,
    [JsonStringEnumMemberName("cts_prevent_escape")] CtsPreventEscape,
    [JsonStringEnumMemberName("escaping_terrorists_neutralized")] EscapingTerroristsNeutralized,
    [JsonStringEnumMemberName("bomb_defused")] BombDefused,
    [JsonStringEnumMemberName("cts_win")] CtsWin,
    [JsonStringEnumMemberName("terrorists_win")] TerroristsWin,
    [JsonStringEnumMemberName("round_draw")] RoundDraw,
    [JsonStringEnumMemberName("all_hostages_rescued")] AllHostagesRescued,
    [JsonStringEnumMemberName("target_saved")] TargetSaved,
    [JsonStringEnumMemberName("hostages_not_rescued")] HostagesNotRescued,
    [JsonStringEnumMemberName("terrorists_not_escaped")] TerroristsNotEscaped,
    [JsonStringEnumMemberName("game_commencing")] GameCommencing,
    [JsonStringEnumMemberName("terrorists_surrender")] TerroristsSurrender,
    [JsonStringEnumMemberName("cts_surrender")] CtsSurrender,
    [JsonStringEnumMemberName("terrorists_planted")] TerroristsPlanted,
    [JsonStringEnumMemberName("cts_reached_hostage")] CtsReachedHostage,
    [JsonStringEnumMemberName("survival_win")] SurvivalWin,
    [JsonStringEnumMemberName("survival_draw")] SurvivalDraw,
}

/// <summary>`FireEnd` in game-events.ts.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<FireEnd>))]
public enum FireEnd
{
    [JsonStringEnumMemberName("expired")] Expired,
    [JsonStringEnumMemberName("extinguished")] Extinguished,
    [JsonStringEnumMemberName("smothered")] Smothered,
}

[JsonConverter(typeof(JsonStringEnumConverter<LogLevel>))]
public enum LogLevel
{
    [JsonStringEnumMemberName("debug")] Debug,
    [JsonStringEnumMemberName("info")] Info,
    [JsonStringEnumMemberName("warn")] Warn,
    [JsonStringEnumMemberName("error")] Error,
}

[JsonConverter(typeof(JsonStringEnumConverter<SidesMode>))]
public enum SidesMode
{
    [JsonStringEnumMemberName("knife")] Knife,
    [JsonStringEnumMemberName("team1_ct")] Team1Ct,
    [JsonStringEnumMemberName("team1_t")] Team1T,
}

[JsonConverter(typeof(JsonStringEnumConverter<ReadyMode>))]
public enum ReadyMode
{
    [JsonStringEnumMemberName("auto")] Auto,
    [JsonStringEnumMemberName("manual")] Manual,
    [JsonStringEnumMemberName("external")] External,
}

[JsonConverter(typeof(JsonStringEnumConverter<BombSite>))]
public enum BombSite
{
    [JsonStringEnumMemberName("a")] A,
    [JsonStringEnumMemberName("b")] B,
}

public static class SideExtensions
{
    public static Side Opposite(this Side side) => side switch
    {
        Side.T => Side.Ct,
        Side.Ct => Side.T,
        _ => Side.Spec,
    };

    public static Side FromTeamNum(int teamNum) => teamNum switch
    {
        2 => Side.T,
        3 => Side.Ct,
        _ => Side.Spec,
    };
}

/// <summary>Seconds on the wire carry millisecond precision; the controller stores them as integer milliseconds.</summary>
public static class WireSeconds
{
    public static double Of(double seconds) => Math.Round(seconds, 3);
}
