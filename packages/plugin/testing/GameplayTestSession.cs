using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Ankor.Diagnostics;

namespace Ankor.Tests;

public sealed class GameplayTestSession
{
    public readonly FakeGameHost Engine = new();
    public readonly List<PluginMessage> Sent = [];
    /// <summary>The phases, scores and backups the plugin reports beside its events.</summary>
    public readonly List<MatchReport> Progress = [];
    private readonly List<ReplayObservation> _tape = [];
    private string? _recordingFailure;
    public List<ReplayObservation> Tape => _recordingFailure is null ? _tape
        : throw new InvalidDataException(_recordingFailure);
    public readonly GameplayReplayHost? Replay;
    /// <summary>The match as the plugin wires it, through the same <see cref="MatchSession"/>.</summary>
    public readonly MatchSession Session;
    public MatchState Match => Session.Match;
    public EventForwarder Forwarder => Session.Forwarder;
    public GameplayCallbacks Callbacks => Session.Callbacks;
    public MatchShutdown Shutdown => Session.Shutdown;
    public RoundBackups Backups => Session.Backups;
    /// <summary>The server's game directory, in temporary storage, where the engine writes its round
    /// backups and the match reads them.</summary>
    public readonly string GameDir;

    private static readonly List<string> GameDirs = [];

    /// <summary>A fresh game directory, which goes when the test process ends.</summary>
    private static string NewGameDir()
    {
        var dir = Directory.CreateTempSubdirectory("ankor-game-").FullName;
        lock (GameDirs)
        {
            if (GameDirs.Count == 0)
                AppDomain.CurrentDomain.ProcessExit += (_, _) =>
                {
                    lock (GameDirs)
                        foreach (var each in GameDirs)
                            try { Directory.Delete(each, recursive: true); }
                            catch (IOException) { }
                };
            GameDirs.Add(dir);
        }
        return dir;
    }
    /// <summary>Set, the engine writes each round's backup as the round starts, as
    /// <c>mp_backup_round_auto</c> has it do; unset, a test writes them.</summary>
    public bool EngineWritesBackups = true;
    /// <summary>How many times a round start had the adapter forget its grenades.</summary>
    public int Cleared;
    private readonly bool _record;

    public GameplayTestSession(bool replay = false, int maxRounds = 6, bool record = true, bool collectEvents = true,
        SidesMode sides = SidesMode.Team1Ct, RestoreFile? restore = null, int gameType = 0, int gameMode = 1,
        string? gameDir = null)
    {
        // A replay reads the files its live run's engine wrote, which the capture does not record.
        GameDir = gameDir ?? NewGameDir();
        _record = record;
        // Set on the command line in CS2, so before anything reads it.
        Engine.ConVars["game_type"] = gameType;
        Engine.ConVars["game_mode"] = gameMode;
        Engine.ConVars["mp_maxrounds"] = maxRounds;
        Engine.ConVars["mp_halftime"] = 1;
        Engine.ConVars["mp_overtime_maxrounds"] = 6;
        Engine.ConVars["mp_round_restart_delay"] = 7;
        Engine.ConVars["mp_match_can_clinch"] = 1;
        IGameHost host;
        if (replay) (host, Replay) = GameplayReplayHost.Create(Engine);
        else host = record ? GameplayRecording.Wrap(Engine, Observe, reason => _recordingFailure = reason) : Engine;
        var config = Fixture.Config(sides, restore: restore);
        config.sendStatistics = collectEvents;
        Session = new MatchSession(Engine, host, config, MatchFiles.At(GameDir), new FakeAgentChannel(Sent, Progress), Fixture.Messages,
            () => Cleared++, _ => { });
        // Explicit bootstrap; fixtures cover gameplay from the first live round, not Steam or WS startup.
        Match.EnterWarmup();
        Engine.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct, position: new(1, 1, 1), angles: new(0, 0)),
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 1),
            Fixture.Player(Fixture.Team2Player1, Side.T, 2, position: new(2, 2, 2), angles: new(0, 0)),
            Fixture.Player(Fixture.Team2Player2, Side.T, 3),
        ]);
        Match.MaybeStart();
        Engine.Advance(14);
        Engine.PlayerList.RemoveAll(p => p.Position is null);
        Engine.IsWarmup = false;
        Engine.FreezePeriod = false;
        Tape.Clear();
        Sent.Clear();
        Progress.Clear();
        Match.Emit(new MapStart { pluginVersion = "test", frameworkVersion = "test",
            serverVersion = "test", mapName = Engine.MapName });
    }

    private void Observe(string name, object value) => Tape.Add(new(name,
        JsonSerializer.SerializeToElement(value, value.GetType(), GameplayRecording.Json)));

    public void Send(GameplayInput input, double? tick = null)
    {
        if (tick is { } value)
        {
            Engine.TickCount = (int)value;
            Engine.SubtickFraction = value - Engine.TickCount;
        }
        if (_record) Observe(CaptureNames.Gameplay.Input, new GameplayRecord( input));
        if (input is RoundEndInput { winnerSide: Side.Ct or Side.T } end && Match.IsLive && !Engine.IsWarmup && !Engine.LoadingRoundBackup)
            Engine.ScoreRound(Match, end.winnerSide);
        Callbacks.Handle(input);
    }

    public void StartRound(int number)
    {
        Send(new RoundPrestartInput(), number * 100000 - 1);
        Engine.RoundsPlayed = number - 1;
        Engine.RoundStartTime = Engine.Clock;
        if (EngineWritesBackups) WriteBackup(Ankor.Core.Wire.Backups.FileNameForRound(number), Fixture.EmptyBackup, Engine.NowMs);
        Engine.PlayerList[0] = Engine.PlayerList[0] with { Side = Match.Team1Side, Health = 100, IsAlive = true };
        Engine.PlayerList[1] = Engine.PlayerList[1] with { Side = Match.Team1Side.Opposite(), Health = 100, IsAlive = true };
        Send(new RoundStartInput(), number * 100000);
        Send(new RoundFreezeEndInput(), number * 100000);
    }

    /// <summary>
    /// The engine has loaded a backup: with its loading flag up it ends the current round without a
    /// winner, then restarts into this round with the file's scores, team 1 seated on
    /// <see cref="MatchState.Team1Side"/>.
    /// </summary>
    public void RestoreAt(int round, int team1Score, int team2Score)
    {
        Engine.LoadingRoundBackup = true;
        Send(new RoundEndInput(Side.Spec, 10));
        Engine.LoadingRoundBackup = false;
        Engine.TeamScores[Match.Team1Side] = team1Score;
        Engine.TeamScores[Match.Team1Side.Opposite()] = team2Score;
        StartRound(round);
    }

    /// <summary>Writes a round backup into the game directory, last written at <paramref name="writtenMs"/>.</summary>
    public void WriteBackup(string name, string contents, long writtenMs) =>
        WriteFile(Path.Combine(Ankor.Core.Wire.Backups.Dir, name), contents, writtenMs);

    /// <summary>Writes a file at <paramref name="path"/> under the game directory.</summary>
    public void WriteFile(string path, string contents = "", long? writtenMs = null)
    {
        var full = Path.Combine(GameDir, path);
        Directory.CreateDirectory(Path.GetDirectoryName(full)!);
        File.WriteAllText(full, contents);
        if (writtenMs is { } ms) File.SetLastWriteTimeUtc(full, DateTimeOffset.FromUnixTimeMilliseconds(ms).UtcDateTime);
    }

    public HostPlayer Ct => Engine.PlayerList.First(p => p.Side == Side.Ct && p.Position is not null);
    public HostPlayer T => Engine.PlayerList.First(p => p.Side == Side.T && p.Position is not null);

    public void Kill(HostPlayer victim, HostPlayer? attacker, int weapon = 7)
    {
        Send(new DamageEnterInput(victim.Slot + 100));
        Send(new DamageSnapshotInput(new(100, 0, new(weapon, null), 200, []), victim));
        var dead = victim with { Health = 0, IsAlive = false };
        Engine.PlayerList[Engine.PlayerList.FindIndex(p => p.SteamId == victim.SteamId)] = dead;
        Send(new PlayerHurtInput(dead, attacker, new(weapon, null), 100, 0, 1, 100));
        Send(new PlayerDeathInput(dead, attacker, null, new(weapon, null), false, 0, false, false,
            false, false, false, 200, []));
    }

    public void EndRound(Side side, int reason)
    {
        Engine.RoundsPlayed++;
        Send(new RoundMvpInput(side == Side.Ct ? Ct : T, reason));
        Send(new RoundEndInput(side, reason));
    }
}

