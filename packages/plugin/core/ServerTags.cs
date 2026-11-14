namespace Ankor.Core;

/// <summary>
/// The allocation's tag in <c>sv_tags</c>. Steam's server list shows the tags, so a controller can
/// find the server it launched there by its allocation, with the <c>server_steam_id</c> the server
/// status reports.
/// </summary>
public static class ServerTags
{
    /// <summary>
    /// Characters the tags may take: Steamworks' <c>k_cbMaxGameServerTags</c> of 128 less its
    /// terminating NUL, and less the <c>,empty,insecure</c> that CS2 appends when it builds the list.
    /// </summary>
    public const int MaxLength = 127 - 15;

    /// <summary>The allocation id without its dashes, as the tag carries it.</summary>
    public static string Tag(string allocationId) => allocationId.Replace("-", "", StringComparison.Ordinal);

    /// <summary>
    /// <paramref name="current"/> with the allocation's tag added after the tags already set, or
    /// null when it already has the tag. Tags past <see cref="MaxLength"/> are dropped from the
    /// end, so the allocation's tag always fits.
    /// </summary>
    public static string? With(string? current, string allocationId)
    {
        var tag = Tag(allocationId);
        var tags = (current ?? "").Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries);
        if (tags.Contains(tag, StringComparer.Ordinal)) return null;

        var kept = new List<string>();
        var length = tag.Length;
        foreach (var other in tags)
        {
            if (length + other.Length + 1 > MaxLength) break;
            kept.Add(other);
            length += other.Length + 1;
        }
        kept.Add(tag);
        return string.Join(',', kept);
    }
}
