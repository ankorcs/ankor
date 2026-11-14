using System.Text.RegularExpressions;
using Xunit;

namespace Ankor.Tests;

/// <summary>
/// A one-shot timer that schedules its own method again fires a frame late each time, and the
/// lateness adds up: the ready deadline once ran seconds behind the warmup clock that way. Work that
/// repeats uses OwnedTimer.ScheduleRepeating.
/// </summary>
public sealed class TimerConventionTests
{
    private static readonly Regex Method = new(@"(?:private|public|internal)\s+(?:static\s+)?void\s+(\w+)\s*\([^)]*\)\s*\{", RegexOptions.Compiled);

    [Fact]
    public void NoMethodSchedulesAOneShotTimerBackToItself()
    {
        var root = Fixture.PluginRoot();
        var sources = new[] { "core", "GameHost" }
            .SelectMany(dir => Directory.GetFiles(Path.Combine(root, dir), "*.cs", SearchOption.AllDirectories))
            .Concat(Directory.GetFiles(root, "*.cs"));
        var chains = new List<string>();

        foreach (var file in sources)
        {
            var text = File.ReadAllText(file);
            foreach (Match match in Method.Matches(text))
            {
                var name = match.Groups[1].Value;
                var body = Body(text, match.Index + match.Length);
                if (Regex.IsMatch(body, $@"\.Schedule\([^;]*\b{name}\b"))
                    chains.Add($"{Path.GetRelativePath(root, file)}: {name}");
            }
        }

        Assert.True(chains.Count == 0, $"one-shot timers scheduled from their own method, which drift: {string.Join(", ", chains)}");
    }

    /// <summary>The method body after its opening brace, up to the brace that closes it.</summary>
    private static string Body(string text, int start)
    {
        var depth = 1;
        var end = start;
        while (depth > 0 && end < text.Length)
        {
            depth += text[end] switch { '{' => 1, '}' => -1, _ => 0 };
            end++;
        }
        return text[start..end];
    }
}
