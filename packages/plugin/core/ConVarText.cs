using System.Globalization;

namespace Ankor.Core;

public static class ConVarText
{
    public static int? Int(string? text)
    {
        var raw = text?.Trim();
        if (string.IsNullOrEmpty(raw)) return null;

        if (int.TryParse(raw, NumberStyles.Integer, CultureInfo.InvariantCulture, out var parsed))
        {
            return parsed;
        }

        // Float-valued convars (mp_freezetime and friends are declared float in CS2) and the
        // "true"/"false" spelling of a bool.
        if (double.TryParse(raw, NumberStyles.Float, CultureInfo.InvariantCulture, out var real))
        {
            return (int)real;
        }

        return raw.ToLowerInvariant() switch
        {
            "true" => 1,
            "false" => 0,
            _ => null,
        };
    }

    public static double? Number(string? text)
    {
        var raw = text?.Trim();
        if (string.IsNullOrEmpty(raw)) return null;
        return double.TryParse(raw, NumberStyles.Float, CultureInfo.InvariantCulture, out var real) ? real : null;
    }

    public static bool? Bool(string? text) => Int(text) switch
    {
        null => null,
        0 => false,
        _ => true,
    };
}
