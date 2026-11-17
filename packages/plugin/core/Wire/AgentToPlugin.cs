using System.Text.Json;
using System.Text.Json.Serialization;

namespace Ankor.Core.Wire;

public sealed class RosterTeam
{
    public required string name { get; set; }
    public required List<string> players { get; set; }
}

public sealed class Roster
{
    /// <summary><c>MAX_SPECTATORS</c> in <c>packages/protocol/src/game-events.ts</c>.</summary>
    public const int MaxSpectators = 2;

    /// <summary><c>MAX_SERVER_PLAYERS</c> in <c>packages/protocol/src/game-events.ts</c>.</summary>
    public const int MaxPlayers = 64;

    /// <summary><c>MAX_TEAM_NAME_LENGTH</c> in <c>packages/protocol/src/game-events.ts</c>.</summary>
    public const int MaxTeamNameLength = 30;

    public required RosterTeam team1 { get; set; }
    public required RosterTeam team2 { get; set; }
    public required List<string> spectators { get; set; }
}

/// <summary>
/// `RestoreFile` in game-events.ts: a live round to play on from. The engine's file holds the round
/// and the scores; the agent writes it to <see cref="Backups.RestoreFile"/> and renders
/// <see cref="rules"/> into the launch cfg, so the plugin reads only <see cref="team1Side"/> and
/// the friendly fire it reports here.
/// </summary>
public sealed class RestoreFile
{
    public required Side team1Side { get; set; }
    public required MatchRules rules { get; set; }
    public required string file { get; set; }
}

/// <summary>The match a server is launched with, from its match file.</summary>
public sealed class MatchConfig
{
    public required string? matchId { get; set; }
    /// <summary>Send the gameplay events that the agent uploads. The phase and score are reported either way.</summary>
    public bool sendStatistics { get; set; }
    public required Roster roster { get; set; }
    public required SidesMode sides { get; set; }
    public required int knifeDecisionSeconds { get; set; }
    public required ReadyMode readyMode { get; set; }
    /// <summary>Under manual ready-up, wait for the rostered spectators' !ready too.</summary>
    public bool spectatorsReadyUp { get; set; }
    /// <summary>Under manual ready-up, let captains and rostered spectators !forceready.</summary>
    public bool allowForceReady { get; set; }
    /// <summary>Seconds both teams have to be ready before the map ends as <c>ready_timeout</c>; null waits indefinitely.</summary>
    public required int? readyTimeoutSeconds { get; set; }
    public required bool recordDemo { get; set; }
    /// <summary>Record a development capture of the callbacks; only a match that sends statistics records one.</summary>
    public required bool debugCapture { get; set; }
    /// <summary>The agent renders them into the launch cfg; a restore's replace them.</summary>
    public required MatchRules rules { get; set; }
    /// <summary>The round to play on from; null plays the map from its start.</summary>
    public RestoreFile? restore { get; set; }

    /// <summary>The rules the match is played under: a restore's, or the launch's.</summary>
    public MatchRules PlayedRules => restore?.rules ?? rules;
}

/// <summary>Every frame the relay sends the plugin, told apart by <c>type</c>.</summary>
[JsonPolymorphic(TypeDiscriminatorPropertyName = "type")]
[JsonDerivedType(typeof(GetServerStatus), "get_server_status")]
[JsonDerivedType(typeof(Exec), "exec")]
[JsonDerivedType(typeof(RestoreRound), "restore_round")]
[JsonDerivedType(typeof(StartMatch), "start_match")]
[JsonDerivedType(typeof(Ack), "ack")]
public abstract class AgentMessage
{
    public sealed class GetServerStatus : AgentMessage
    {
        public string? requestId { get; init; }
    }

    public sealed class Exec : AgentMessage
    {
        public string command { get; init; } = "";
        public required string requestId { get; init; }
    }

    /// <summary>Play on from one of this server's own rounds; the agent has seen the round's backup on disk.</summary>
    public sealed class RestoreRound : AgentMessage
    {
        public required int round { get; init; }
        public required string requestId { get; init; }
    }

    public sealed class StartMatch : AgentMessage
    {
        public required string requestId { get; init; }
    }

    /// <summary>Cumulative: every event of this connection's server through <see cref="toSeq"/> is stored.</summary>
    public sealed class Ack : AgentMessage
    {
        public required long toSeq { get; init; }
    }
}

public static class WireJson
{
    /// <summary>Members are camelCase in C# and snake_case on the wire. Emit null unless WhenWritingNull explicitly omits it.</summary>
    public static readonly JsonSerializerOptions Options = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        DefaultIgnoreCondition = JsonIgnoreCondition.Never,
        NumberHandling = JsonNumberHandling.Strict,
        PropertyNameCaseInsensitive = false,
        // The relay writes `type` wherever its object literal puts it.
        AllowOutOfOrderMetadataProperties = true,
        WriteIndented = false,
    };

    public static string Serialize(PluginMessage message) =>
        JsonSerializer.Serialize(message, message.GetType(), Options);

    /// <summary>The <c>type</c> of every frame the relay sends that this plugin knows.</summary>
    private static readonly HashSet<string> KnownTypes = typeof(AgentMessage)
        .GetCustomAttributes(typeof(JsonDerivedTypeAttribute), false)
        .Cast<JsonDerivedTypeAttribute>()
        .Select(attribute => (string)attribute.TypeDiscriminator!)
        .ToHashSet(StringComparer.Ordinal);

    /// <summary>
    /// A frame whose <c>type</c> this plugin does not know, which a newer agent may send: its type,
    /// and its <c>request_id</c> when it is a command that waits for an answer. Null for a known type
    /// or a frame with no string <c>type</c>.
    /// </summary>
    public static (string Type, string? RequestId)? UnknownMessage(string json)
    {
        try
        {
            using var document = JsonDocument.Parse(json);
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object
                || !root.TryGetProperty("type", out var type) || type.ValueKind != JsonValueKind.String
                || KnownTypes.Contains(type.GetString()!))
                return null;
            var requestId = root.TryGetProperty("request_id", out var id) && id.ValueKind == JsonValueKind.String ? id.GetString() : null;
            return (type.GetString()!, requestId);
        }
        catch (JsonException)
        {
            return null;
        }
    }

    /// <summary>One inbound frame, or null for anything that is not one of ours.</summary>
    public static AgentMessage? ParseInbound(string json)
    {
        try
        {
            return JsonSerializer.Deserialize<AgentMessage>(json, Options);
        }
        catch (Exception ex) when (ex is JsonException or NotSupportedException)
        {
            return null;
        }
    }

    /// <summary>Decode launch configuration, which the agent has already validated. False leaves the
    /// caller with the same choice a missing value gives it; <paramref name="error"/> says what was wrong.
    /// </summary>
    public static bool TryDecodeMatchConfig(string? value, out MatchConfig? config, out string? error)
    {
        config = null;
        if (string.IsNullOrWhiteSpace(value))
        {
            error = "no value";
            return false;
        }

        try
        {
            config = JsonSerializer.Deserialize<MatchConfig>(value, Options);
        }
        catch (JsonException ex)
        {
            // Names the missing or malformed field.
            error = ex.Message;
            return false;
        }

        if (config is null)
        {
            error = "the config is null";
            return false;
        }

        if (config.roster.team1.players.Count == 0 || config.roster.team2.players.Count == 0)
        {
            error = "a team has no players";
            config = null;
            return false;
        }

        if (config.roster.spectators.Count > Roster.MaxSpectators)
        {
            error = $"the roster has more than {Roster.MaxSpectators} spectators";
            config = null;
            return false;
        }

        if (config.roster.team1.players.Count + config.roster.team2.players.Count + config.roster.spectators.Count > Roster.MaxPlayers)
        {
            error = $"the roster has more than {Roster.MaxPlayers} players and spectators";
            config = null;
            return false;
        }

        // The contract counts Unicode code points, including a surrogate pair as one character.
        if (config.roster.team1.name.EnumerateRunes().Count() > Roster.MaxTeamNameLength ||
            config.roster.team2.name.EnumerateRunes().Count() > Roster.MaxTeamNameLength)
        {
            error = $"a team name is longer than {Roster.MaxTeamNameLength} characters";
            config = null;
            return false;
        }

        if (config.restore is { } restore && RestoreProblem(restore) is { } problem)
        {
            error = $"the restore {problem}";
            config = null;
            return false;
        }

        error = null;
        return true;
    }

    /// <summary>What the agent's schema already refuses; a file the engine cannot load is caught when it is loaded.</summary>
    private static string? RestoreProblem(RestoreFile restore) => restore switch
    {
        { team1Side: Side.Spec } => "puts team 1 on no side",
        { rules.maxRounds: < 1 } => "has no rounds",
        { file.Length: 0 } => "has an empty file",
        _ => null,
    };
}
