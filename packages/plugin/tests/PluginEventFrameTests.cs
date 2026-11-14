using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class PluginEventFrameTests
{
    [Fact]
    public void AnEventFrameSerializesThePolymorphicEventInFull()
    {
        var frame = new PluginEventFrame
        {
            seq = 7,
            @event = new KnifeEnd
            {
                timestamp = 1_700_000_000_000,
                tick = 640,
                winnerTeam = 2,
                winnerSide = Side.T,
            },
        };

        Assert.Equal(
            """
            {"type":"event","seq":7,"event":{"type":"knife_end","timestamp":1700000000000,"tick":640,"winner_team":2,"winner_side":"t","team1_alive":0,"team2_alive":0,"team1_health":0,"team2_health":0}}
            """,
            frame.ToJson());
    }

    [Fact]
    public void AckFramesAreParsedOnTheFastPath()
    {
        var ack = Assert.IsType<AgentMessage.Ack>(WireJson.ParseInbound("""{"type":"ack","to_seq":42}"""));

        Assert.Equal(42, ack.toSeq);
    }

    [Theory]
    [InlineData("""{"type":"get_server_status","request_id":null}""")]
    [InlineData("""{"type":"ack"}""")]
    [InlineData("not json")]
    public void NonAckFramesDoNotParseAsAcks(string frame) => Assert.IsNotType<AgentMessage.Ack>(WireJson.ParseInbound(frame));

    [Theory]
    [InlineData("0199a0b4-5c00-7000-8000-00000000b001", true)]
    [InlineData("0199A0B4-5C00-7000-8000-00000000B001", false)]
    [InlineData("0199a0b45c007000800000000000b001", false)]
    [InlineData("0199a0b4-5c00-7000-8000-00000000b001\n", false)]
    [InlineData("", false)]
    [InlineData(null, false)]
    public void OnlyALowercaseUuidIsAnAllocationId(string? raw, bool accepted) =>
        Assert.Equal(accepted ? raw : null, AnkorEnv.ParseAllocationId(raw));

}
