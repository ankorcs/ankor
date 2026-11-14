using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public class ConVarTextTests
{
    [Theory]
    [InlineData("15", 15)]
    [InlineData(" 15 ", 15)]
    [InlineData("-1", -1)]
    [InlineData("15.9", 15)]
    [InlineData("1e2", 100)]
    [InlineData("true", 1)]
    [InlineData("FALSE", 0)]
    [InlineData("", null)]
    [InlineData("   ", null)]
    [InlineData(null, null)]
    [InlineData("de_dust2", null)]
    public void ReadsIntegersFromIntegerFloatAndBoolText(string? text, int? expected)
    {
        Assert.Equal(expected, ConVarText.Int(text));
    }

    [Theory]
    [InlineData("105", 105.0)]
    [InlineData(" 0.5 ", 0.5)]
    [InlineData("true", null)]
    [InlineData("", null)]
    [InlineData(null, null)]
    public void ReadsNumbersWithoutTheBoolSpelling(string? text, double? expected)
    {
        Assert.Equal(expected, ConVarText.Number(text));
    }

    [Theory]
    [InlineData("0", false)]
    [InlineData("0.4", false)]
    [InlineData("1", true)]
    [InlineData("2", true)]
    [InlineData("true", true)]
    [InlineData("false", false)]
    [InlineData("yes", null)]
    [InlineData(null, null)]
    public void ReadsBoolsAsNonZeroIntegers(string? text, bool? expected)
    {
        Assert.Equal(expected, ConVarText.Bool(text));
    }
}
