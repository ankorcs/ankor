using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.Logging;
using Ankor.Core;
using Ankor.Core.Wire;
using SwiftlyS2.Shared;

namespace Ankor.GameHost;

public sealed record PluginSettings(ChatTag Chat);

/// <summary>
/// The plugin's SwiftlyS2 configuration file: <c>configs/plugins/Ankor/config.jsonc</c> under
/// the framework's managed root (<c>addons/swiftlys2/</c>). The source is
/// <c>packages/plugin/configs/config.jsonc</c>; the Dockerfile puts it in the overlay, which is
/// read-only at run time, so the file has to exist there — SwiftlyS2 only reads an existing file
/// and would otherwise try to create the directory.
/// </summary>
public static class PluginConfig
{
    public const string FileName = "config.jsonc";

    /// <summary>Read once per plugin load.</summary>
    public static PluginSettings Load(ISwiftlyCore core, ILogger logger)
    {
        try
        {
            core.Configuration
                .InitializeWithTemplate(FileName, FileName)
                .Configure(builder => builder.AddJsonFile(core.Configuration.GetConfigPath(FileName), optional: false, reloadOnChange: false));
            var section = core.Configuration.Manager.GetSection(PluginConfigKeys.ChatSection);

            var chat = new ChatTag(
                section[PluginConfigKeys.ChatTag] ?? throw new InvalidOperationException("no Chat.Tag"),
                section[PluginConfigKeys.ChatTagColor] ?? throw new InvalidOperationException("no Chat.TagColor"));
            return new PluginSettings(chat);
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "cannot read {File}; using defaults", FileName);
            return new PluginSettings(ChatTag.Default);
        }
    }
}
