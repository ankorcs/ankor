using Ankor.Core;
using SwiftlyS2.Shared;

namespace Ankor.GameHost;

/// <summary>
/// <see cref="IMessages"/> over SwiftlyS2's translation service. The strings come from
/// <c>resources/translations/&lt;language&gt;.jsonc</c> beside the plugin assembly, which the
/// framework reads at load: it resolves the <c>[green]</c>-style colour tags to chat bytes once,
/// then <c>localizer[key, args]</c> is a lookup plus <c>string.Format</c>. A key that is not in the
/// catalog comes back as <c>&lt;language&gt;.&lt;key&gt;</c> rather than throwing, which is what
/// <see cref="CatalogLooksLoaded"/> checks for at load. The tag in front of every line comes from
/// the config file (<see cref="PluginConfig"/>), rendered through the same colour tags.
/// </summary>
public sealed class SwiftlyMessages(ISwiftlyCore core, ChatTag tag) : IMessages
{
    /// <summary>A key every build has; if it does not resolve, the catalog was not read at all.</summary>
    public const string ProbeKey = "match.live";

    private readonly string _prefix = tag.Markup.Colored();

    public string Get(string key, params object[] args) =>
        args.Length == 0 ? core.Localizer[key] : core.Localizer[key, args];

    /// <remarks>
    /// <c>Colored</c> prepends a space to a value that starts with a colour tag, so the prefix
    /// carries its own leading space and the message's is dropped here to keep lines single-spaced.
    /// </remarks>
    public string Prefixed(string key, params object[] args) =>
        $"{_prefix} {Get(key, args).TrimStart()}";

    /// <summary>
    /// False when the translation file was not found, in which case every key renders as itself and
    /// chat is unreadable. Cheap to check and worth an error in the log.
    /// </summary>
    public bool CatalogLooksLoaded => !core.Localizer[ProbeKey].EndsWith(ProbeKey, StringComparison.Ordinal);
}

/// <summary>
/// Last resort when the translation service has nothing to read: every line becomes its key. Chat
/// is then useless, but team lock, the match phases and the event stream keep working.
/// </summary>
public sealed class KeyEchoMessages : IMessages
{
    public string Get(string key, params object[] args) => key;

    public string Prefixed(string key, params object[] args) => key;
}
