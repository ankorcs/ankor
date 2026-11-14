namespace Ankor.Core;

/// <summary>
/// The bracketed tag in front of every chat line, e.g. <c>[Ankor]</c>, from the plugin's SwiftlyS2
/// config file (<c>configs/plugins/Ankor/config.jsonc</c>, section <c>Chat</c>). The agent
/// renders that file from a tag its contract validated (<c>ChatTag</c> in <c>launch.ts</c>), so the
/// plugin takes it as written. <c>GameHost/SwiftlyMessages.cs</c> turns <see cref="Markup"/> into
/// chat bytes with the framework's colour tags.
/// </summary>
public sealed record ChatTag(string Text, string Color)
{
    /// <summary>What the image ships, and what an unreadable config falls back to.</summary>
    public static ChatTag Default { get; } = new("Ankor", "blue");

    /// <summary>The tag in SwiftlyS2 colour markup, <c>[blue][Ankor][default]</c>.</summary>
    public string Markup => $"[{Color}][{Text}][default]";
}
