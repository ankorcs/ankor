using System.Text.Json;
using Ankor.Core;

namespace Ankor.Tests;

/// <summary>
/// <see cref="IMessages"/> reading the shipped <c>resources/translations/en.jsonc</c>, so the tests
/// assert on the same strings players see. It stands in for SwiftlyS2's translation service, which
/// the test project deliberately cannot reference, and reproduces the three things that service
/// does to a value: resolve the <c>[colour]</c> tags once at load, prepend a space to a value that
/// starts with a tag, and <c>string.Format</c> the arguments in at lookup.
/// </summary>
public sealed class CatalogMessages : IMessages
{
    /// <summary>The chat colour bytes SwiftlyS2 substitutes for its tags.</summary>
    public static readonly (string Tag, string Code)[] ColorCodes =
    [
        ("[default]", "\x01"), ("[/]", "\x01"), ("[white]", "\x01"), ("[darkred]", "\x02"),
        ("[lightpurple]", "\x03"), ("[green]", "\x04"), ("[olive]", "\x05"), ("[lime]", "\x06"),
        ("[red]", "\x07"), ("[gray]", "\x08"), ("[grey]", "\x08"), ("[lightyellow]", "\x09"),
        ("[yellow]", "\x09"), ("[silver]", "\x0A"), ("[bluegrey]", "\x0A"), ("[lightblue]", "\x0B"),
        ("[blue]", "\x0B"), ("[darkblue]", "\x0C"), ("[purple]", "\x0E"), ("[magenta]", "\x0E"),
        ("[lightred]", "\x0F"), ("[gold]", "\x10"), ("[orange]", "\x10"),
    ];

    private readonly Dictionary<string, string> _colored;
    private readonly string _prefix;

    /// <param name="tag">The chat tag; the plugin reads it from its config file, tests use the default.</param>
    public CatalogMessages(IReadOnlyDictionary<string, string> catalog, ChatTag? tag = null)
    {
        _colored = catalog.ToDictionary(entry => entry.Key, entry => Colored(entry.Value));
        _prefix = Colored((tag ?? ChatTag.Default).Markup);
    }

    public static CatalogMessages Load(string path) => new(Parse(File.ReadAllText(path)));

    /// <summary>The catalog file is JSONC: a flat object of string values, comments allowed.</summary>
    public static IReadOnlyDictionary<string, string> Parse(string jsonc)
    {
        using var document = JsonDocument.Parse(jsonc, new JsonDocumentOptions
        {
            CommentHandling = JsonCommentHandling.Skip,
            AllowTrailingCommas = true,
        });

        return document.RootElement.EnumerateObject()
            .ToDictionary(property => property.Name, property => property.Value.GetString()!);
    }

    /// <summary>SwiftlyS2's Helper.Colored: leading space for a value that opens with a tag.</summary>
    public static string Colored(string value)
    {
        var text = value.StartsWith('[') ? $" {value}" : value;
        foreach (var (tag, code) in ColorCodes) text = text.Replace(tag, code);
        return text;
    }

    /// <summary>A missing key comes back as the localizer's <c>&lt;language&gt;.&lt;key&gt;</c>.</summary>
    public string Get(string key, params object[] args)
    {
        if (!_colored.TryGetValue(key, out var value)) return $"en.{key}";
        return args.Length == 0 ? value : string.Format(value, args);
    }

    public string Prefixed(string key, params object[] args) =>
        $"{_prefix} {Get(key, args).TrimStart()}";
}
