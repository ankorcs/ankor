using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public class DisconnectReasonsTests
{
    [Theory]
    [InlineData(0, "invalid")]
    [InlineData(1, "shutdown")]
    [InlineData(2, "disconnect_by_user")]
    [InlineData(3, "disconnect_by_server")]
    [InlineData(4, "lost")]
    [InlineData(5, "overflow")]
    [InlineData(6, "steam_banned")]
    [InlineData(7, "steam_inuse")]
    [InlineData(8, "steam_ticket")]
    [InlineData(9, "steam_logon")]
    [InlineData(10, "steam_authcancelled")]
    [InlineData(29, "timedout")]
    [InlineData(39, "kicked")]
    [InlineData(40, "banadded")]
    [InlineData(41, "kickbanadded")]
    [InlineData(157, "kicked_votedoff")]
    [InlineData(158, "kicked_idle")]
    public void NamesKnownCodes(int code, string expected)
    {
        Assert.Equal(expected, DisconnectReasons.Name(code));
    }

    [Theory]
    [InlineData(11, "11")]
    [InlineData(-1, "-1")]
    public void KeepsOtherCodesAsNumbers(int code, string expected)
    {
        Assert.Equal(expected, DisconnectReasons.Name(code));
    }
}
