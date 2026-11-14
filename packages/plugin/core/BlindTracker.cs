using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>The engine credits a flash assist only to a team-mate, so a player who blinds an enemy
/// and kills them himself is credited with nothing. Remember who blinded whom and for how long, so
/// every death can name the blind that was still running when it happened.
/// </summary>
public sealed class BlindTracker
{
    private readonly record struct Blind(PlayerRef Blinder, double EndsAt);

    private readonly Dictionary<string, Blind> _blinds = [];

    public void Reset() => _blinds.Clear();
    public void Remove(string victimId) => _blinds.Remove(victimId);

    /// <summary>Keep the blind that ends latest; a self-blind is nobody's assist. At is server seconds.</summary>
    public void Record(string victimId, PlayerRef blinder, double duration, double now)
    {
        if (duration <= 0 || blinder.steamId == victimId) return;

        var endsAt = now + duration;
        if (_blinds.TryGetValue(victimId, out var current) && current.EndsAt >= endsAt) return;
        _blinds[victimId] = new Blind(blinder, endsAt);
    }

    /// <summary>The blind still running on the victim and its remaining seconds; null once it expired.</summary>
    public (PlayerRef Blinder, double Remaining)? Lookup(string victimId, double now)
    {
        if (!_blinds.TryGetValue(victimId, out var blind)) return null;

        var remaining = blind.EndsAt - now;
        if (remaining > 0) return (blind.Blinder, WireSeconds.Of(remaining));
        _blinds.Remove(victimId);
        return null;
    }
}
