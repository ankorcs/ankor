using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

/// <summary>A newer agent may send what this plugin does not have yet: unknown keys are ignored,
/// and a frame of an unknown type is named with its request id, which the plugin answers.</summary>
public sealed class UnknownMessageTests
{
    [Fact]
    public void Names_an_unknown_command_with_its_request_id_and_an_unknown_notice_without_one()
    {
        Assert.Equal(("pause_match", "r1"), WireJson.UnknownMessage("{\"type\":\"pause_match\",\"request_id\":\"r1\"}"));
        Assert.Equal(("weather", (string?)null), WireJson.UnknownMessage("{\"type\":\"weather\",\"sky\":\"clear\"}"));
    }

    [Fact]
    public void Leaves_a_known_type_or_a_frame_without_a_string_type_to_the_parser()
    {
        Assert.Null(WireJson.UnknownMessage("{\"type\":\"exec\",\"request_id\":\"r1\",\"command\":\"say hi\"}"));
        Assert.Null(WireJson.UnknownMessage("{\"type\":7}"));
        Assert.Null(WireJson.UnknownMessage("[1]"));
        Assert.Null(WireJson.UnknownMessage("{"));
    }

    [Fact]
    public void Ignores_keys_it_does_not_know()
    {
        var request = Assert.IsType<AgentMessage.GetServerStatus>(WireJson.ParseInbound(
            "{\"type\":\"get_server_status\",\"request_id\":\"r1\",\"added_later\":{\"x\":1}}"));
        Assert.Equal("r1", request.requestId);
    }
}
