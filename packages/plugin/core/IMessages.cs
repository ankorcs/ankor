namespace Ankor.Core;

/// <summary>
/// Every player-facing string, by key. The strings themselves live in
/// <c>resources/translations/en.jsonc</c>; the plugin resolves them through SwiftlyS2's translation
/// service (<c>GameHost/SwiftlyMessages.cs</c>) and the tests through the same file, so the core
/// never holds English text, colour bytes or a chat prefix.
/// </summary>
/// <remarks>
/// The keys used here are the contract: <c>tests/MessagesTests.cs</c> scans the source for
/// <c>Get("…")</c> and <c>Prefixed("…")</c> and fails when a key is missing from the catalog, unused
/// in the code, or given the wrong number of arguments. Keys must therefore be literals at the call
/// site — pick the key with a conditional, never build one.
/// </remarks>
public interface IMessages
{
    /// <summary>The bare string, with <c>{0}</c>-style placeholders filled in.</summary>
    string Get(string key, params object[] args);

    /// <summary>The same behind the chat tag. Every chat line the plugin prints uses this.</summary>
    string Prefixed(string key, params object[] args);
}
