using System.Text.Json;
using System.Text.Json.Serialization;

namespace Ankor.Core.Wire;

/// <summary>
/// An event and its position among the server's events from zero: <c>PluginEventFrame</c> in
/// plugin-socket.ts. It names no server, because the relay key the connection opened with does.
/// </summary>
public sealed class PluginEventFrame
{
    /// <summary>Tells the frame apart from status and `quitting` on the relay socket.</summary>
    [JsonPropertyOrder(-1)]
    public string type { get; } = "event";

    public long seq { get; set; }

    /// <summary>Use object so System.Text.Json serializes the concrete event type.</summary>
    [JsonPropertyName("event")]
    public object @event { get; set; } = null!;

    public string ToJson() => JsonSerializer.Serialize(this, WireJson.Options);
}

/// <summary>
/// What the match reports beside its events, whether or not it sends statistics:
/// <c>PluginPhaseChange</c>, <c>PluginScore</c> and <c>PluginBackup</c> in plugin-socket.ts.
/// </summary>
public abstract class MatchReport(string type)
{
    [JsonPropertyOrder(-1)]
    public string type { get; } = type;

    public string ToJson() => JsonSerializer.Serialize<object>(this, WireJson.Options);
}

public sealed class PluginPhaseChange() : MatchReport("phase_change")
{
    public MatchPhase phase { get; init; }
}

public sealed class PluginScore() : MatchReport("score")
{
    public int team1Score { get; init; }
    public int team2Score { get; init; }
}

/// <summary>
/// A round backup of a match without statistics, which would otherwise carry it in its events:
/// <c>PluginBackup</c> in plugin-socket.ts.
/// </summary>
public sealed class PluginBackup() : MatchReport("backup")
{
    public RoundBackup backup { get; init; } = null!;
}
