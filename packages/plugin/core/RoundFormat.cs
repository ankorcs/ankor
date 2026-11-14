namespace Ankor.Core;

/// <summary>
/// The round layout the server runs, from its cvars: <see cref="MaxRounds"/> of regulation, then
/// overtimes of <see cref="OvertimeRounds"/> each. Read it when it is needed, because a cfg can
/// change the cvars.
/// </summary>
public sealed record RoundFormat(
    int MaxRounds, bool OvertimeEnabled, int OvertimeRounds, bool Halftime, int WinLimit, bool CanClinch)
{
    public static RoundFormat Read(IGameHost host) => new(
        host.GetConVarInt("mp_maxrounds")!.Value,
        host.GetConVarBool("mp_overtime_enable")!.Value,
        host.GetConVarInt("mp_overtime_maxrounds")!.Value,
        host.GetConVarBool("mp_halftime")!.Value,
        host.GetConVarInt("mp_winlimit")!.Value,
        host.GetConVarBool("mp_match_can_clinch")!.Value);

    /// <summary>Whether an overtime starts after <paramref name="round"/>, so equipment cannot carry over.</summary>
    public bool StartsOvertimeAfter(int round) =>
        OvertimeEnabled && OvertimeRounds > 0 && InOvertime(round + 1) && RoundInOvertime(round + 1) == 0;

    /// <summary>Whether the round that just ended, already counted in the scores, ends the match.</summary>
    public bool IsFinalRound(int team1Score, int team2Score)
    {
        var high = Math.Max(team1Score, team2Score);
        var played = team1Score + team2Score;
        if (WinLimit > 0 && high >= WinLimit) return true;
        if (MaxRounds <= 0) return false;
        if (!InOvertime(played))
        {
            if (CanClinch && high > MaxRounds / 2) return true;
            return played == MaxRounds && (team1Score != team2Score || !OvertimeEnabled);
        }
        if (OvertimeRounds <= 0) return false;
        // Winning an overtime takes more than half of its rounds on top of the score it started from.
        return high > MaxRounds / 2 + (Overtime(played) + 1) * (OvertimeRounds / 2);
    }

    /// <summary>
    /// Whether the round that just ended, already counted in the scores, is the last one before the
    /// engine switches sides. Regulation swaps at its halfway point. Overtime keeps the sides of
    /// regulation's second half for its first half, and swaps only at each overtime's halftime.
    /// </summary>
    public bool IsHalftimeBoundary(int played)
    {
        if (!Halftime || MaxRounds <= 0) return false;
        if (played == MaxRounds / 2) return true;
        if (!InOvertime(played) || OvertimeRounds / 2 <= 0) return false;
        return RoundInOvertime(played) == OvertimeRounds / 2 - 1;
    }

    private bool InOvertime(int round) => MaxRounds > 0 && round > MaxRounds;

    /// <summary>Which overtime, from 0, an overtime round falls in.</summary>
    private int Overtime(int round) => (round - MaxRounds - 1) / OvertimeRounds;

    /// <summary>The position, from 0, of an overtime round within its overtime.</summary>
    private int RoundInOvertime(int round) => (round - MaxRounds - 1) % OvertimeRounds;
}
