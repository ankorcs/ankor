using System.Text.Json;
using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>Current server facts and optional loaded match details; never a durable event.</summary>
public static class ServerStatus
{
    public static string Serialize(IGameHost host, bool ready, MatchState? match = null, string? requestId = null, string? configError = null) => JsonSerializer.Serialize(new
    {
        type = "server_status",
        requestId,
        mapName = WireText.Clean(host.MapName, WireText.MaxMapName),
        match = match is null ? null : new
        {
            matchId = match.Config.matchId,
            phase = match.Phase,
            team1Side = match.Team1Side,
            team2Side = match.Team1Side.Opposite(),
            team1Score = match.Team1Score,
            team2Score = match.Team2Score,
        },
        players = host.Players.Where(player => player.IsReportable).Take(Roster.MaxPlayers).Select(player => new
        {
            steamId = player.SteamIdString,
            name = WireText.Clean(player.Name, WireText.MaxPlayerName, controls: false),
            side = player.Side,
        }).ToArray(),
        configError = configError is null ? null : WireText.Clean(configError, WireText.MaxConfigError),
        ready,
        serverSteamId = ready ? host.ServerSteamId : null,
    }, WireJson.Options);
}
