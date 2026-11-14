using Ankor.Core.Wire;

namespace Ankor.Core;

public static class RoundEndReasons
{
    /// <summary>The engine's reason code as SwiftlyS2's <c>RoundEndReason</c> numbers it; a code the protocol does not name is <c>unknown</c>.</summary>
    public static RoundEndReason FromEngine(int code) => code switch
    {
        1 => RoundEndReason.TargetBombed,
        4 => RoundEndReason.TerroristsEscaped,
        5 => RoundEndReason.CtsPreventEscape,
        6 => RoundEndReason.EscapingTerroristsNeutralized,
        7 => RoundEndReason.BombDefused,
        8 => RoundEndReason.CtsWin,
        9 => RoundEndReason.TerroristsWin,
        10 => RoundEndReason.RoundDraw,
        11 => RoundEndReason.AllHostagesRescued,
        12 => RoundEndReason.TargetSaved,
        13 => RoundEndReason.HostagesNotRescued,
        14 => RoundEndReason.TerroristsNotEscaped,
        16 => RoundEndReason.GameCommencing,
        17 => RoundEndReason.TerroristsSurrender,
        18 => RoundEndReason.CtsSurrender,
        19 => RoundEndReason.TerroristsPlanted,
        20 => RoundEndReason.CtsReachedHostage,
        21 => RoundEndReason.SurvivalWin,
        22 => RoundEndReason.SurvivalDraw,
        _ => RoundEndReason.Unknown,
    };

    public static bool IsSurrender(RoundEndReason reason) =>
        reason is RoundEndReason.TerroristsSurrender or RoundEndReason.CtsSurrender;
}
