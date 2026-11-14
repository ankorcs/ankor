using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class ServerStatusTests
{
    [Fact]
    public void ReportsCurrentHumanPlayersWithoutMatchStateOrLobbyTeams()
    {
        var host = new FakeGameHost();
        host.PlayerList.Add(new HostPlayer(1, 76561198000000001, "A", Side.Ct, 100, true, IsBot: false));
        host.PlayerList.Add(new HostPlayer(2, 0, "Bot", Side.T, 100, true, IsBot: true));
        using var first = JsonDocument.Parse(ServerStatus.Serialize(host, false));
        var root = first.RootElement;
        Assert.Equal("server_status", root.GetProperty("type").GetString());
        Assert.Equal(JsonValueKind.Null, root.GetProperty("match").ValueKind);
        Assert.False(root.GetProperty("ready").GetBoolean());
        Assert.False(root.TryGetProperty("match_id", out _));
        var player = Assert.Single(root.GetProperty("players").EnumerateArray());
        Assert.Equal("76561198000000001", player.GetProperty("steam_id").GetString());
        Assert.Equal("ct", player.GetProperty("side").GetString());
        Assert.False(player.TryGetProperty("team", out _));
        host.PlayerList.Clear();
        using var next = JsonDocument.Parse(ServerStatus.Serialize(host, false));
        Assert.Empty(next.RootElement.GetProperty("players").EnumerateArray());
        Assert.Empty(host.Logs);
        Assert.IsType<AgentMessage.GetServerStatus>(WireJson.ParseInbound("{\"type\":\"get_server_status\"}"));
    }
    [Fact]
    public void ReportsTheServersSteamIdOnceItIsReady()
    {
        var host = new FakeGameHost { ServerSteamId = "85568392936840254" };

        using var starting = JsonDocument.Parse(ServerStatus.Serialize(host, false));
        using var ready = JsonDocument.Parse(ServerStatus.Serialize(host, true));
        host.ServerSteamId = null;
        using var anonymous = JsonDocument.Parse(ServerStatus.Serialize(host, true));

        Assert.Equal(JsonValueKind.Null, starting.RootElement.GetProperty("server_steam_id").ValueKind);
        Assert.Equal("85568392936840254", ready.RootElement.GetProperty("server_steam_id").GetString());
        Assert.Equal(JsonValueKind.Null, anonymous.RootElement.GetProperty("server_steam_id").ValueKind);
    }

    [Fact]
    public void ReportsLoadedConfigurationAndCurrentTeamsWithoutChangingTheMatch()
    {
        var host = new FakeGameHost();
        var config = Fixture.Config();
        config.sides = SidesMode.Team1T;
        using var match = new MatchState(host, config, new FakeAgentChannel(), Fixture.Messages, () => { });
        var request = Assert.IsType<AgentMessage.GetServerStatus>(WireJson.ParseInbound(
            "{\"type\":\"get_server_status\",\"request_id\":\"inspect-1\"}"));
        using var json = JsonDocument.Parse(ServerStatus.Serialize(host, true, match, request.requestId));
        Assert.Equal("inspect-1", json.RootElement.GetProperty("request_id").GetString());
        Assert.True(json.RootElement.GetProperty("ready").GetBoolean());
        Assert.Equal(JsonValueKind.Null, json.RootElement.GetProperty("config_error").ValueKind);
        var details = json.RootElement.GetProperty("match");
        Assert.Equal(config.matchId, details.GetProperty("match_id").GetString());
        Assert.False(details.TryGetProperty("config", out _));
        Assert.Equal(["match_id", "phase", "team1_side", "team2_side", "team1_score", "team2_score"],
            details.EnumerateObject().Select(property => property.Name));
        Assert.Equal("t", details.GetProperty("team1_side").GetString());
        Assert.Equal("ct", details.GetProperty("team2_side").GetString());
        Assert.Equal("loading", details.GetProperty("phase").GetString());
        Assert.Equal(0, details.GetProperty("team1_score").GetInt32());
        Assert.Equal(MatchPhase.Loading, match.Phase);
        Assert.Empty(host.Commands);
        Assert.Empty(host.Logs);
        match.SwapSides();
        using var swapped = JsonDocument.Parse(ServerStatus.Serialize(host, true, match));
        Assert.Equal("ct", swapped.RootElement.GetProperty("match").GetProperty("team1_side").GetString());
    }

    [Fact]
    public void AMatchWithoutAnIdReportsANullId()
    {
        var host = new FakeGameHost();
        var config = Fixture.Config();
        config.matchId = null;
        using var match = new MatchState(host, config, new FakeAgentChannel(), Fixture.Messages, () => { });
        using var json = JsonDocument.Parse(ServerStatus.Serialize(host, true, match));
        Assert.Equal(JsonValueKind.Null, json.RootElement.GetProperty("match").GetProperty("match_id").ValueKind);
    }

    [Fact]
    public void ARefusedConfigIsReportedWithoutAMatch()
    {
        using var json = JsonDocument.Parse(ServerStatus.Serialize(new FakeGameHost(), false, null, null, "map_name is empty"));
        Assert.Equal(JsonValueKind.Null, json.RootElement.GetProperty("match").ValueKind);
        Assert.Equal("map_name is empty", json.RootElement.GetProperty("config_error").GetString());
    }

    [Fact]
    public void CutsTextAndPlayersToTheContractLimits()
    {
        var host = new FakeGameHost { MapName = new string('m', 300) };
        for (var i = 0; i < Roster.MaxPlayers + 2; i++)
            host.PlayerList.Add(new HostPlayer(i, 76561198000000001UL + (ulong)i, "N\u0007" + new string('x', 300), Side.Ct, 100, true, IsBot: false));
        using var json = JsonDocument.Parse(ServerStatus.Serialize(host, false, null, null, new string('e', 1000)));
        var root = json.RootElement;
        Assert.Equal(WireText.MaxMapName, root.GetProperty("map_name").GetString()!.Length);
        Assert.Equal(WireText.MaxConfigError, root.GetProperty("config_error").GetString()!.Length);
        var players = root.GetProperty("players").EnumerateArray().ToArray();
        Assert.Equal(Roster.MaxPlayers, players.Length);
        var name = players[0].GetProperty("name").GetString()!;
        Assert.Equal(WireText.MaxPlayerName, name.Length);
        Assert.StartsWith("Nx", name);
    }
}
