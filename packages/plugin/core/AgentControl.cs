namespace Ankor.Core;

public static class AgentControl
{
    private const int ExcerptChars = 200;

    // CS2 holds back a last line without a newline, such as status_json's closing brace, until the
    // next one. A newline rather than ; also survives an unclosed quote or a // comment.
    public static string ExecLine(string command) => $"{command}\necho";

    /// <summary>
    /// <c>MATCH_BLOCKED_COMMANDS</c> in agent-socket.ts: commands that change a match outside the
    /// plugin's control. The plugin runs them itself where the match needs them.
    /// </summary>
    public static readonly string[] MatchBlockedCommands =
    [
        "mp_restartgame", "mp_warmup_start", "mp_warmup_end", "mp_warmup_pausetimer", "mp_warmuptime",
        "mp_swapteams", "mp_switchteams", "mp_scrambleteams",
        "mp_backup_restore_load_file", "mp_backup_round_file", "mp_backup_round_auto",
        "restart", "changelevel", "map", "host_workshop_map", "ds_workshop_changelevel",
        "tv_record", "tv_stoprecord", "alias",
    ];

    private static readonly HashSet<string> Blocked = new(MatchBlockedCommands, StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// The first command of <paramref name="line"/> that <see cref="MatchBlockedCommands"/> names, or
    /// null. The line splits on every <c>;</c> and newline, quoted or not, so a split the engine
    /// would not make can only block more. The engine matches command names in any case.
    /// </summary>
    public static string? BlockedCommand(string line)
    {
        foreach (var command in line.Split([';', '\n', '\r']))
        {
            var name = command.TrimStart(' ', '\t', '"').Split([' ', '\t', '"'], 2)[0];
            if (Blocked.Contains(name)) return name;
        }
        return null;
    }

    public static string Excerpt(string frame) =>
        frame.Length > ExcerptChars ? frame[..ExcerptChars] + "…" : frame;
}
