namespace Ankor.Tests;

/// <summary>
/// The engine's command buffer, ordered as the exec order probe measured it on CS2: an exec on a
/// console line runs its file after everything already queued, and an exec inside a file runs that
/// file in place. Every line runs in the pass that drains the buffer, as the engine runs them in one tick.
/// </summary>
public sealed class FakeConsole(Func<string, string?> file, Action marker)
{
    private sealed record Line(string Text, bool InFile);

    // Commands that act rather than set a value, so their argument is not a convar's.
    private static readonly HashSet<string> Actions = new(StringComparer.OrdinalIgnoreCase)
    {
        "exec", "ankor_cfg_applied", "mp_restartgame", "mp_warmup_start", "mp_warmup_end",
        "mp_swapteams", "mp_pause_match", "mp_unpause_match", "tv_record", "tv_stoprecord", "bot_kick",
        "mp_backup_restore_load_file", "changelevel", "map", "host_workshop_map", "quit", "say", "echo", "kickid",
    };

    private readonly LinkedList<Line> _buffer = new();
    private readonly Dictionary<string, string> _values = new(StringComparer.OrdinalIgnoreCase);
    private bool _running;

    /// <summary>Every command the engine ran, in the order it ran them.</summary>
    public List<string> Ran { get; } = [];

    /// <summary>The convars the engine set, by name, as each last set them.</summary>
    public IReadOnlyDictionary<string, string> Values => _values;

    public void Queue(string line)
    {
        foreach (var command in Split(line)) _buffer.AddLast(new Line(command, InFile: false));
    }

    public void Run()
    {
        // A command run here can queue more; the pass already running drains it.
        if (_running) return;
        _running = true;
        try
        {
            while (_buffer.First is { } next)
            {
                _buffer.RemoveFirst();
                Execute(next.Value);
            }
        }
        finally
        {
            _running = false;
        }
    }

    private void Execute(Line line)
    {
        Ran.Add(line.Text);
        var space = line.Text.IndexOfAny([' ', '\t']);
        var name = space < 0 ? line.Text : line.Text[..space];
        var argument = space < 0 ? "" : line.Text[(space + 1)..].Trim();

        if (name.Equals("exec", StringComparison.OrdinalIgnoreCase))
        {
            var lines = Split(file(argument) ?? "").Select(text => new Line(text, InFile: true));
            if (line.InFile)
                foreach (var nested in lines) Execute(nested);
            else
                foreach (var queued in lines) _buffer.AddLast(queued);
            return;
        }

        if (name.Equals("ankor_cfg_applied", StringComparison.OrdinalIgnoreCase))
        {
            marker();
            return;
        }

        if (argument.Length > 0 && !Actions.Contains(name)) _values[name] = argument.Trim('"');
    }

    private static IEnumerable<string> Split(string text)
    {
        foreach (var rawLine in text.Split('\n'))
        {
            var command = new System.Text.StringBuilder();
            var quoted = false;
            for (var i = 0; i < rawLine.Length; i++)
            {
                var c = rawLine[i];
                if (c == '"') quoted = !quoted;
                if (!quoted && c == '/' && i + 1 < rawLine.Length && rawLine[i + 1] == '/') break;
                if (!quoted && c == ';')
                {
                    if (command.ToString().Trim() is { Length: > 0 } done) yield return done;
                    command.Clear();
                    continue;
                }
                command.Append(c);
            }
            if (command.ToString().Trim() is { Length: > 0 } last) yield return last;
        }
    }
}
