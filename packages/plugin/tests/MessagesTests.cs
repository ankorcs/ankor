using System.Text;
using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

/// <summary>The source scan requires literal keys to check translations and argument counts.</summary>
public class MessagesTests
{
    private static readonly IReadOnlyDictionary<string, string> Catalog =
        CatalogMessages.Parse(File.ReadAllText(Fixture.CatalogPath));

    private sealed record CallSite(string File, int Line, string Method, string Key, int ArgCount);

    [Fact]
    public void EveryKeyUsedInCodeIsTranslated()
    {
        var missing = CallSites()
            .Where(call => !Catalog.ContainsKey(call.Key))
            .Select(call => $"{call.File}:{call.Line} uses '{call.Key}'")
            .ToArray();

        Assert.Empty(missing);
    }

    /// <summary>The adapter's load-time probe must be a real key, or it would always report a missing catalog.</summary>
    [Fact]
    public void TheAdapterProbeKeyIsTranslated()
    {
        var adapter = File.ReadAllText(Path.Combine(Fixture.PluginRoot(), "GameHost", "SwiftlyMessages.cs"));
        var probe = System.Text.RegularExpressions.Regex.Match(adapter, "ProbeKey = \"([^\"]+)\"").Groups[1].Value;

        Assert.True(Catalog.ContainsKey(probe), $"probe key '{probe}' is not in the catalog");
    }

    /// <summary>
    /// Chat names commands through <see cref="ChatCommands"/>, so the adapter must register those
    /// same names: every constant once, and no command by a literal.
    /// </summary>
    [Fact]
    public void TheAdapterRegistersEveryChatCommandByItsConstant()
    {
        var adapter = File.ReadAllText(Path.Combine(Fixture.PluginRoot(), "LoadedMatch.cs"));
        var registered = System.Text.RegularExpressions.Regex
            .Matches(adapter, @"(?<!void )\bCommand\(\s*([^,]+),")
            .Select(match => match.Groups[1].Value.Trim())
            .ToArray();
        var constants = typeof(ChatCommands)
            .GetFields(System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Static)
            .Where(field => field.IsLiteral)
            .Select(field => $"{nameof(ChatCommands)}.{field.Name}")
            .ToArray();

        Assert.NotEmpty(registered);
        Assert.Equal(constants.Order(), registered.Order());
    }

    [Fact]
    public void CallSitesPassTheArgumentsTheirTranslationNeeds()
    {
        var wrong = CallSites()
            .Where(call => Catalog.ContainsKey(call.Key))
            .Where(call => call.ArgCount != Placeholders(Catalog[call.Key]).Count)
            .Select(call =>
                $"{call.File}:{call.Line} passes {call.ArgCount} argument(s) to '{call.Key}', " +
                $"which has {Placeholders(Catalog[call.Key]).Count} placeholder(s)")
            .ToArray();

        Assert.Empty(wrong);
    }

    [Fact]
    public void EveryTranslationRendersWithDummyArguments()
    {
        var messages = new CatalogMessages(Catalog);
        foreach (var (key, value) in Catalog)
        {
            var args = Enumerable.Range(0, Placeholders(value).Count)
                .Select(object (i) => $"arg{i}")
                .ToArray();

            var rendered = messages.Prefixed(key, args);
            Assert.False(string.IsNullOrWhiteSpace(rendered));
            // An unresolved colour tag would reach players as literal "[green]" text.
            Assert.DoesNotMatch(@"\[[a-z/]+\]", rendered);
            Assert.DoesNotContain("{", rendered, StringComparison.Ordinal);
        }
    }

    /// <summary>A key built at runtime would slip past the scan, so every key must be a literal.</summary>
    [Fact]
    public void KeysAreLiterals()
    {
        var dynamic = new List<string>();
        foreach (var (path, text) in Sources())
        {
            foreach (var (index, method) in Calls(text, ["Get", "Prefixed"]))
            {
                var open = text.IndexOf('(', index);
                if (text[SkipSpace(text, open + 1)] != '"')
                {
                    dynamic.Add($"{path}:{LineOf(text, index)} calls {method} with a computed key");
                }
            }
        }

        Assert.Empty(dynamic);
    }

    [Fact]
    public void ChatGoesThroughTheCatalog()
    {
        var inline = new List<string>();
        foreach (var (path, text) in Sources())
        {
            foreach (var (index, method) in Calls(text, ["PrintToChat", "PrintToChatAll", "Kick"]))
            {
                var arguments = Arguments(text, text.IndexOf('(', index));
                // Strip the catalog lookups; a quote left over is a hand-written string.
                var remainder = System.Text.RegularExpressions.Regex.Replace(
                    arguments, "(?:Get|Prefixed)\\(\\s*\"[^\"]*\"", "");
                if (remainder.Contains('"'))
                {
                    inline.Add($"{path}:{LineOf(text, index)} passes a literal to {method}");
                }
            }
        }

        Assert.Empty(inline);
    }

    private static IReadOnlyList<int> Placeholders(string value)
    {
        var found = new List<int>();
        for (var i = 0; i < value.Length; i++)
        {
            if (value[i] != '{') continue;
            var close = value.IndexOf('}', i + 1);
            if (close < 0) continue;
            var token = value[(i + 1)..close];
            if (token.Length > 0 && token.All(char.IsAsciiDigit)) found.Add(int.Parse(token));
            i = close;
        }

        return found.Distinct().Order().ToArray();
    }

    private static IEnumerable<CallSite> CallSites()
    {
        foreach (var (path, text) in Sources())
        {
            foreach (var (index, method) in Calls(text, ["Get", "Prefixed"]))
            {
                var open = text.IndexOf('(', index);
                var start = SkipSpace(text, open + 1);
                if (text[start] != '"') continue;

                var end = text.IndexOf('"', start + 1);
                var key = text[(start + 1)..end];
                var arguments = Arguments(text, open);
                yield return new CallSite(
                    path, LineOf(text, index), method, key, TopLevelParts(arguments).Count - 1);
            }
        }
    }

    /// <summary>Every .cs file the plugin ships; tests and build output are not part of the scan.</summary>
    private static IEnumerable<(string Path, string Text)> Sources()
    {
        var root = Fixture.PluginRoot();
        var skipped = new[] { "tests", "obj", "bin", "build" };
        foreach (var path in Directory.EnumerateFiles(root, "*.cs", SearchOption.AllDirectories))
        {
            var relative = Path.GetRelativePath(root, path);
            if (relative.Split(Path.DirectorySeparatorChar).Any(part => skipped.Contains(part))) continue;
            yield return (relative, File.ReadAllText(path));
        }
    }

    /// <summary>
    /// Positions of <c>something.Name(</c> for the wanted methods. The receiver is what separates a
    /// call from the interface's own declaration, and comment lines are skipped so that prose about
    /// a key never counts as a use of it.
    /// </summary>
    private static IEnumerable<(int Index, string Method)> Calls(string text, string[] methods)
    {
        foreach (var method in methods)
        {
            for (var i = text.IndexOf(method, StringComparison.Ordinal); i >= 0;
                 i = text.IndexOf(method, i + 1, StringComparison.Ordinal))
            {
                if (i == 0 || text[i - 1] != '.') continue;
                var after = i + method.Length;
                if (after >= text.Length || text[after] != '(') continue;
                if (InComment(text, i)) continue;
                yield return (i, method);
            }
        }
    }

    /// <summary>True when the position sits after a <c>//</c> on its own line.</summary>
    private static bool InComment(string text, int index)
    {
        var lineStart = text.LastIndexOf('\n', index) + 1;
        var before = text[lineStart..index];
        return before.Contains("//", StringComparison.Ordinal)
               || before.TrimStart().StartsWith('*');
    }

    /// <summary>The text between a call's parentheses, quotes and nesting respected.</summary>
    private static string Arguments(string text, int open)
    {
        var depth = 0;
        var quoted = false;
        var body = new StringBuilder();
        for (var i = open; i < text.Length; i++)
        {
            var c = text[i];
            if (quoted)
            {
                body.Append(c);
                if (c == '\\') body.Append(text[++i]);
                else if (c == '"') quoted = false;
                continue;
            }

            if (c == '"')
            {
                quoted = true;
                body.Append(c);
                continue;
            }

            if (c is '(' or '[' or '{')
            {
                if (++depth > 1) body.Append(c);
                continue;
            }

            if (c is ')' or ']' or '}')
            {
                if (--depth == 0) return body.ToString();
                body.Append(c);
                continue;
            }

            body.Append(c);
        }

        throw new FormatException($"unbalanced call at offset {open}");
    }

    /// <summary>Splits an argument list on the commas that are not inside a nested expression.</summary>
    private static IReadOnlyList<string> TopLevelParts(string arguments)
    {
        var parts = new List<string>();
        var depth = 0;
        var quoted = false;
        var current = new StringBuilder();
        for (var i = 0; i < arguments.Length; i++)
        {
            var c = arguments[i];
            if (quoted)
            {
                current.Append(c);
                if (c == '\\') { current.Append(arguments[++i]); continue; }
                if (c == '"') quoted = false;
                continue;
            }

            switch (c)
            {
                case '"': quoted = true; break;
                case '(' or '[' or '{': depth++; break;
                case ')' or ']' or '}': depth--; break;
                case ',' when depth == 0:
                    parts.Add(current.ToString());
                    current.Clear();
                    continue;
            }

            current.Append(c);
        }

        if (current.ToString().Trim().Length > 0) parts.Add(current.ToString());
        return parts;
    }

    private static int SkipSpace(string text, int index)
    {
        while (index < text.Length && char.IsWhiteSpace(text[index])) index++;
        return index;
    }

    private static int LineOf(string text, int index) =>
        text.AsSpan(0, index).Count('\n') + 1;
}
