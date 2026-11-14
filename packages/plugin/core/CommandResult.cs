using System.Text.Json;
using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary><c>CommandResult</c> in plugin-socket.ts; the error codes are its <c>CommandRefusal</c>.</summary>
public static class CommandResult
{
    /// <summary>The server runs without a match.</summary>
    public const string NoMatch = "no_match";
    /// <summary>The match is past its warmup.</summary>
    public const string NotInWarmup = "not_in_warmup";
    /// <summary>A team has no player connected.</summary>
    public const string TeamEmpty = "team_empty";
    /// <summary>The server has no backup of the round that the restore named.</summary>
    public const string NoBackup = "no_backup";
    /// <summary>The match is not live, such as in warmup, during the knife round or after it ended.</summary>
    public const string NotLive = "not_live";
    /// <summary>The match is in its halftime break, while the sides are about to swap.</summary>
    public const string AtHalftime = "at_halftime";
    /// <summary>An exec named a command of <see cref="AgentControl.MatchBlockedCommands"/> while a match is loaded.</summary>
    public const string CommandBlocked = "command_blocked";
    /// <summary>The command is one a newer agent sends that this plugin does not have yet.</summary>
    public const string UnsupportedRequest = "unsupported_request";

    public static string Serialize(string requestId, string? error) => JsonSerializer.Serialize(new
    {
        type = "command_result",
        requestId,
        error,
    }, WireJson.Options);
}
