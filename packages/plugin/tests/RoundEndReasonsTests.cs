using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class RoundEndReasonsTests
{
    [Theory]
    [InlineData(0, RoundEndReason.Unknown)]
    [InlineData(1, RoundEndReason.TargetBombed)]
    [InlineData(4, RoundEndReason.TerroristsEscaped)]
    [InlineData(5, RoundEndReason.CtsPreventEscape)]
    [InlineData(6, RoundEndReason.EscapingTerroristsNeutralized)]
    [InlineData(7, RoundEndReason.BombDefused)]
    [InlineData(8, RoundEndReason.CtsWin)]
    [InlineData(9, RoundEndReason.TerroristsWin)]
    [InlineData(10, RoundEndReason.RoundDraw)]
    [InlineData(11, RoundEndReason.AllHostagesRescued)]
    [InlineData(12, RoundEndReason.TargetSaved)]
    [InlineData(13, RoundEndReason.HostagesNotRescued)]
    [InlineData(14, RoundEndReason.TerroristsNotEscaped)]
    [InlineData(16, RoundEndReason.GameCommencing)]
    [InlineData(17, RoundEndReason.TerroristsSurrender)]
    [InlineData(18, RoundEndReason.CtsSurrender)]
    [InlineData(19, RoundEndReason.TerroristsPlanted)]
    [InlineData(20, RoundEndReason.CtsReachedHostage)]
    [InlineData(21, RoundEndReason.SurvivalWin)]
    [InlineData(22, RoundEndReason.SurvivalDraw)]
    // Codes the engine leaves unused, and any it adds later, travel as unknown.
    [InlineData(2, RoundEndReason.Unknown)]
    [InlineData(15, RoundEndReason.Unknown)]
    [InlineData(23, RoundEndReason.Unknown)]
    [InlineData(-1, RoundEndReason.Unknown)]
    public void NamesTheEngineCodeOrFallsBackToUnknown(int code, RoundEndReason expected)
    {
        Assert.Equal(expected, RoundEndReasons.FromEngine(code));
    }

    /// <summary>A reason the contract adds needs an engine code here, or it always travels as unknown.</summary>
    [Fact]
    public void EveryReasonTheContractNamesHasAnEngineCode()
    {
        var mapped = Enumerable.Range(-1, 64).Select(RoundEndReasons.FromEngine).ToHashSet();

        Assert.Equal(Enum.GetValues<RoundEndReason>().ToHashSet(), mapped);
    }
}
