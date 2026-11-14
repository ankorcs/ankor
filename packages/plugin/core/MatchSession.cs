using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>The files a match reads and marks on the server: round backups and demos.</summary>
/// <param name="ReadBackup">A backup file by name, or null when it is not there.</param>
/// <param name="Exists">Whether a backup file is there, by its path.</param>
/// <param name="DemoDir">The directory the demos are recorded to.</param>
public sealed record MatchFiles(Func<string, BackupFile?> ReadBackup, Func<string, bool> Exists, string DemoDir)
{
    /// <summary>The server's own files, under the paths the image lays out.</summary>
    public static readonly MatchFiles Server = new(ReadServerBackup, File.Exists, Demos.Path);

    private static BackupFile? ReadServerBackup(string name) => ReadBackupFile(Path.Combine(Wire.Backups.Path, name));

    /// <summary>
    /// The same files laid out under <paramref name="gameDir"/> in place of the server's game
    /// directory, for a host without the engine. A path the match asks about under the server's
    /// game directory is looked for there.
    /// </summary>
    public static MatchFiles At(string gameDir)
    {
        string Local(string path) => path.StartsWith(ServerPaths.GameDir + "/", StringComparison.Ordinal)
            ? Path.Combine(gameDir, path[(ServerPaths.GameDir.Length + 1)..])
            : path;
        return new(name => ReadBackupFile(Path.Combine(gameDir, Wire.Backups.Dir, name)), path => File.Exists(Local(path)),
            Path.Combine(gameDir, Demos.RecordDir));
    }

    private static BackupFile? ReadBackupFile(string path)
    {
        if (!File.Exists(path)) return null;
        var written = new DateTimeOffset(File.GetLastWriteTimeUtc(path)).ToUnixTimeMilliseconds();
        return new BackupFile(File.ReadAllText(path), written);
    }
}

/// <summary>
/// A loaded match's logic, wired together: its state, the demo, the events, the team lock, the
/// round backups, the shutdown and the gameplay callbacks. The SwiftlyS2 adapter builds one and
/// feeds it the engine's callbacks; a host without the engine can build one the same way.
/// </summary>
public sealed class MatchSession : IDisposable
{
    private readonly IGameHost _host;
    private readonly MatchFiles _files;

    public MatchState Match { get; }
    public DemoRecorder Recorder { get; }
    public EventForwarder Forwarder { get; }
    public DamageTracker Damage { get; }
    public TeamLock TeamLock { get; }
    public RoundBackups Backups { get; }
    public MatchShutdown Shutdown { get; }
    public GameplayCallbacks Callbacks { get; }
    public BacklogMonitor? Backlog { get; }

    /// <param name="host">The engine.</param>
    /// <param name="coreHost">The engine as the match logic reads it, which a debug capture may record.</param>
    /// <param name="agent">The match's connection to its agent.</param>
    /// <param name="clearGrenades">Forgets the grenades the adapter tracks, as each round starts.</param>
    /// <param name="beforeShutdown">Runs after the backups and before the shutdown are built, with the
    /// match, so the adapter can start its debug capture at the point it always has.</param>
    public MatchSession(IGameHost host, IGameHost coreHost, MatchConfig config, MatchFiles files, IAgentChannel agent,
        IMessages messages, Action clearGrenades, Action<MatchState> beforeShutdown)
    {
        _host = host;
        _files = files;
        var collectEvents = config.sendStatistics;

        Recorder = new DemoRecorder(coreHost, config, LogBoth, MarkDemoReady);
        Match = new MatchState(coreHost, config, agent, messages, Recorder.Start);
        Forwarder = new EventForwarder(Match, coreHost);
        Damage = new DamageTracker(Forwarder);
        TeamLock = new TeamLock(Match, coreHost, messages);
        Backups = new RoundBackups(host, Match, files, LogBoth);

        beforeShutdown(Match);

        Shutdown = new MatchShutdown(coreHost, agent, () => Recorder.Flushed, messages);
        Callbacks = new GameplayCallbacks(Match, coreHost, Forwarder, Damage, TeamLock, clearGrenades,
            () => Shutdown.Start(unplayed: Match.EndReason == MapEndReasons.ReadyTimeout), Recorder.ScheduleStop, Backups.OnRoundStart);
        Match.ReadyExpired += Callbacks.EndUnplayed;
        if (collectEvents)
        {
            Backlog = new BacklogMonitor(host, agent, LogBoth);
            Backlog.Start();
        }
    }

    /// <summary>
    /// A map loaded: the match reports its start, unless it has ended. True when the map load then
    /// goes to the gameplay callbacks, as a <see cref="MapLoadInput"/>. The server's version is read
    /// only for the report.
    /// </summary>
    public bool OnMapLoad(string mapName, string pluginVersion, string frameworkVersion, Func<string> serverVersion)
    {
        if (Match.Phase == MatchPhase.PostLive) return false;
        Match.Emit(new MapStart
        {
            pluginVersion = WireText.Clean(pluginVersion, WireText.MaxVersion),
            frameworkVersion = WireText.Clean(frameworkVersion, WireText.MaxVersion),
            serverVersion = WireText.Clean(serverVersion(), WireText.MaxVersion),
            mapName = mapName,
        });
        return true;
    }

    /// <summary>Logs on the server and into the match's record.</summary>
    public void LogBoth(LogLevel level, string message)
    {
        _host.Log(level, message);
        Match.LogEvent(level, message);
    }

    private void MarkDemoReady(string name) => DemoRecorder.MarkReady(_files.DemoDir, name, LogBoth);

    /// <summary>Stops the match's logic, in the order its parts depend on each other.</summary>
    public void Dispose()
    {
        Callbacks.Dispose();
        TeamLock.Dispose();
        Backlog?.Dispose();
        Shutdown.Dispose();
        Recorder.Dispose();
        Backups.Dispose();
        Forwarder.Dispose();
        Match.Dispose();
    }
}
