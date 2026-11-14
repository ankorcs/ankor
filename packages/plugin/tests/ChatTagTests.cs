using System.Text.Json;
using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public class ChatTagTests
{
    [Fact]
    public void TheDefaultTagRendersInBlueByteForByte()
    {
        Assert.Equal(" \x0B[Ankor]\x01", CatalogMessages.Colored(ChatTag.Default.Markup));
    }

    // A copy of CHAT_COLORS in the protocol's launch.ts, because a package reads nothing outside
    // itself. Change both together.
    private static readonly string[] ContractColors =
    [
        "white", "darkred", "lightpurple", "green", "olive", "lime", "red", "gray", "lightyellow",
        "yellow", "silver", "bluegrey", "lightblue", "blue", "darkblue", "purple", "magenta", "lightred",
        "gold", "orange",
    ];

    /// <summary>
    /// The colours the agent's contract accepts are exactly tags SwiftlyS2 colours, so a valid config
    /// never leaves a literal <c>[name]</c> in chat.
    /// </summary>
    [Fact]
    public void EveryColourTheContractAcceptsIsASwiftlyTag()
    {
        var swiftly = CatalogMessages.ColorCodes.Select(c => c.Tag.Trim('[', ']')).ToHashSet();

        foreach (var name in ContractColors)
        {
            Assert.Contains(name, swiftly);
            Assert.DoesNotContain($"[{name}]", CatalogMessages.Colored(new ChatTag("x", name).Markup));
        }
    }

    [Fact]
    public void TheShippedConfigIsTheDefault()
    {
        using var document = JsonDocument.Parse(File.ReadAllText(Fixture.ConfigPath), new JsonDocumentOptions
        {
            CommentHandling = JsonCommentHandling.Skip,
            AllowTrailingCommas = true,
        });
        var chat = document.RootElement.GetProperty("Chat");

        Assert.Equal(ChatTag.Default, new ChatTag(chat.GetProperty("Tag").GetString()!, chat.GetProperty("TagColor").GetString()!));
    }
}
