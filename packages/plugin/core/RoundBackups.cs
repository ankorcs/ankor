using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>A backup file's text and when the engine last wrote it.</summary>
public sealed record BackupFile(string Contents, long WrittenMs);

/// <summary>
/// Sends the engine's backup of each live round with the match state the file lacks. The engine
/// writes it as the round starts, so a file that is missing or older than the round is an error.
/// A restored round has none: the engine writes no file for it, and the controller keeps the backup sent when
/// the round first started.
/// </summary>
public sealed class RoundBackups(IGameHost host, MatchState match, MatchFiles files, Action<LogLevel, string> log) : IDisposable
{
    private const long WriteSlackMs = 2_000;

    private bool _disposed;
    private int _restoresSeen;

    /// <summary>
    /// Sent when going live: until then the engine's default file is in the read-only game
    /// directory, so the warmup and knife rounds leave nothing in <see cref="Backups.Path"/>. Queued
    /// after the live cfg, so the cfg cannot redirect them.
    /// </summary>
    public static void Configure(IGameHost host)
    {
        // Reconnecting players get their money and equipment back from the round's backup.
        host.ExecuteCommand("mp_backup_round_auto 1");
        host.ExecuteCommand($"mp_backup_round_file {Backups.RoundFile}");
    }

    public void OnRoundStart()
    {
        var restored = match.Restores != _restoresSeen;
        _restoresSeen = match.Restores;
        if (restored || match.Ending || !match.IsLive) return;

        var backup = new RoundBackup
        {
            round = match.Round,
            team1Side = match.Team1Side,
            team1Score = match.Team1Score,
            team2Score = match.Team2Score,
            rules = match.ReadMatchRules(),
            roster = match.Config.roster,
        };
        var startedMs = host.NowMs;
        host.NextFrame(() => Send(backup, startedMs));
    }

    private void Send(RoundBackup backup, long startedMs)
    {
        if (_disposed || match.Ending) return;

        var name = Backups.FileNameForRound(backup.round);
        BackupFile? file;
        try
        {
            file = files.ReadBackup(name);
        }
        catch (Exception error)
        {
            log(LogLevel.Error, $"round {backup.round} has no backup: could not read {name}: {error.Message}");
            return;
        }

        var problem = file switch
        {
            null => $"{name} is missing",
            _ when file.WrittenMs < startedMs - WriteSlackMs => $"{name} is older than the round",
            { Contents.Length: 0 } => $"{name} is empty",
            { Contents.Length: > RoundBackup.MaxFileChars } => $"{name} has {file.Contents.Length} characters, over the limit",
            _ when !RoundBackup.IsWireText(file.Contents) => $"{name} holds a control character other than a tab or line break",
            _ => null,
        };
        if (problem is not null)
        {
            log(LogLevel.Error, $"round {backup.round} has no backup: {problem}");
            return;
        }

        backup.file = file!.Contents;
        match.SendBackup(backup);
    }

    // The engine only logs a file it cannot find and plays on, so a missing one is refused here.
    public string? Restore(int round)
    {
        var name = Backups.FileNameForRound(round);
        if (!files.Exists(Path.Combine(Backups.Path, name))) return CommandResult.NoBackup;
        return match.RestoreRound(round, $"{Backups.Dir}/{name}");
    }

    // A missing file is refused here because the engine would only log it and play on.
    public bool BeforeLoad(string file)
    {
        if (file.Length == 0 || !files.Exists(Path.Combine(ServerPaths.GameDir, file)))
        {
            log(LogLevel.Warn, $"a backup load was refused: {(file.Length == 0 ? "no file named" : file + " is not in the game directory")}");
            return false;
        }

        return match.BeforeBackupLoad();
    }

    public void Dispose() => _disposed = true;
}
