using Ankor.Core;
using Ankor.Core.Wire;

namespace Ankor.Tests;

/// <summary>
/// In-memory <see cref="IGameHost"/>. The clock and the timers are manual so tests can drive the
/// side-choice timeout and effect lifetimes without waiting.
/// </summary>
public sealed class FakeGameHost : IGameHost
{
    public sealed record ExecutedCommand(string Command, double At);

    public sealed class FakeTimer(FakeGameHost host, double dueAt, Action callback, bool stopOnMapChange, double? period = null) : IHostTimer
    {
        public double DueAt { get; internal set; } = dueAt;
        /// <summary>Seconds between firings of a repeating timer; null for a one-shot.</summary>
        public double? Period { get; } = period;
        public bool StopOnMapChange { get; } = stopOnMapChange;
        public Action Callback { get; } = callback;
        public bool Cancelled { get; private set; }
        public bool IsCancellationRequested => Cancelled;
        public bool Fired { get; set; }

        public void Cancel()
        {
            Cancelled = true;
            host.Timers.Remove(this);
        }
    }

    public List<ExecutedCommand> Commands { get; } = [];
    public List<string> Chat { get; } = [];
    public List<(int Slot, string Message)> DirectChat { get; } = [];
    public List<(HostPlayer Player, Side Side)> SwitchedTeams { get; } = [];
    public List<(HostPlayer Player, Side Side)> ChangedTeams { get; } = [];
    public List<(HostPlayer Player, string Reason)> Kicked { get; } = [];
    public List<FakeTimer> Timers { get; } = [];
    public List<(LogLevel Level, string Message)> Logs { get; } = [];
    public Dictionary<string, string> StringConVars { get; } = new();
    public void SetConVarString(string name, string value) => StringConVars[name] = value;
    public string? GetConVarString(string name) => StringConVars.GetValueOrDefault(name);

    /// <summary>A server always has these set. Round rules stay off unless a test turns them on.</summary>
    public Dictionary<string, int> ConVars { get; } = new()
    {
        ["mp_maxrounds"] = 0,
        ["mp_winlimit"] = 0,
        ["mp_halftime"] = 0,
        ["mp_match_can_clinch"] = 1,
        ["mp_overtime_enable"] = 0,
        ["mp_overtime_maxrounds"] = 0,
        ["mp_buytime"] = BuyTimeSeconds,
        ["mp_round_restart_delay"] = 0,
        ["tv_delay"] = 0,
        // Competitive: the game mode whose cfg the plugin runs going live.
        ["game_type"] = 0,
        ["game_mode"] = 1,
    };

    public const int BuyTimeSeconds = 20;
    public List<HostPlayer> PlayerList { get; } = [];

    public double Clock { get; set; }
    public long NowMs { get; set; } = 1_700_000_000_000;
    public int TickCount { get; set; }
    public double SubtickFraction { get; set; }
    public double SimulationTick => TickCount + SubtickFraction;
    public bool FreezePeriod { get; set; } = true;
    public double RoundStartTime { get; set; }
    public double RoundElapsedTime => FreezePeriod ? 0 : Math.Max(0, Clock - RoundStartTime);
    public double CurrentTime => Clock;
    public string MapName { get; set; } = "de_mirage";
    public bool SteamLoggedOn { get; set; }
    public string? ServerSteamId { get; set; }
    public string ServerVersion { get; set; } = "1.40.7.7";
    public string FrameworkVersion { get; set; } = "SwiftlyS2 1.4.10";

    public IReadOnlyList<HostPlayer> Players => PlayerList;
    public HashSet<int> UnreadableSlots { get; } = [];
    public HostPlayer? ReadPlayerOrKick(int slot, string reason)
    {
        var player = PlayerList.FirstOrDefault(player => player.Slot == slot);
        if (player is null || !UnreadableSlots.Contains(slot)) return player;
        Kick(player, reason);
        return null;
    }
    public IReadOnlyList<HostPlayer> ReadPlayersOrKick(string reason) => PlayerList.ToArray()
        .Select(player => ReadPlayerOrKick(player.Slot, reason)).OfType<HostPlayer>().ToArray();
    public Dictionary<ulong, PlayerInventory> Inventories { get; } = new();
    public int InventoryReads { get; private set; }
    public PlayerInventory? ReadInventory(HostPlayer player)
    {
        InventoryReads++;
        return Inventories.GetValueOrDefault(player.SteamId);
    }

    /// <summary>The commands run, in order, for readable assertions.</summary>
    public IEnumerable<string> CommandNames => Commands.Select(c => c.Command);

    /// <summary>The engine counts a round on its scoreboard before it fires round_end.</summary>
    public void ScoreRound(MatchState match, Side winner)
    {
        var team1Won = winner == match.Team1Side;
        TeamScores[match.Team1Side] = match.Team1Score + (team1Won ? 1 : 0);
        TeamScores[match.Team1Side.Opposite()] = match.Team2Score + (team1Won ? 0 : 1);
    }

    /// <summary>The engine applying a backup: its loading flag is up as it ends the round it cuts off.</summary>
    public void LoadBackup(MatchState match)
    {
        LoadingRoundBackup = true;
        match.NoticeBackupLoading();
        LoadingRoundBackup = false;
    }

    public FakeGameHost() => Console = new FakeConsole(CfgText, () =>
    {
        if (_afterCommands.TryDequeue(out var action)) action();
    });

    /// <summary>What the engine ran of the commands, and the convar values it set.</summary>
    public FakeConsole Console { get; }

    /// <summary>
    /// The server's cfg/ directory by path, as exec finds files there; the image's ankor/ is read from
    /// packages/plugin/cfg. A launch cfg in the agent's format is there from the start.
    /// </summary>
    public Dictionary<string, string> CfgFiles { get; } = new(StringComparer.OrdinalIgnoreCase)
    {
        [CfgNames.Launch + ".cfg"] = File.ReadAllText(Path.Combine(Fixture.PluginRoot(), "tests", "fixtures", "ankor_launch.cfg")),
    };

    private string? CfgText(string name)
    {
        var path = name.EndsWith(".cfg", StringComparison.OrdinalIgnoreCase) ? name : name + ".cfg";
        if (CfgFiles.TryGetValue(path, out var text)) return text;
        if (!path.StartsWith(CfgNames.Dir + "/", StringComparison.Ordinal)) return null;
        var image = Path.Combine(Fixture.PluginRoot(), "cfg", path[(CfgNames.Dir.Length + 1)..]);
        return File.Exists(image) ? File.ReadAllText(image) : null;
    }

    public void ExecuteCommand(string command)
    {
        Commands.Add(new ExecutedCommand(command, Clock));
        // As the engine does: mp_swapteams swaps the teams by scheduling a restart.
        if (command == "mp_swapteams") ConVars["mp_restartgame"] = 1;
        Console.Queue(command);
        if (!DeferNextFrames) Console.Run();
    }

    /// <summary>The engine runs the restart that mp_restartgame scheduled; call OnRoundStart for the round it starts.</summary>
    public void RunScheduledRestart() => ConVars["mp_restartgame"] = 0;

    /// <summary>Unset, the engine runs each command as it is queued; set, it runs them on the next frame.</summary>
    public bool DeferNextFrames { get; set; }
    private readonly List<Action> _nextFrame = [];
    private readonly Queue<Action> _afterCommands = new();
    public void NextFrame(Action action)
    {
        if (DeferNextFrames) _nextFrame.Add(action);
        else action();
    }

    /// <summary>The way the real host does it: the marker cfg queued behind the commands, settling the oldest wait.</summary>
    public void AfterCommands(Action action)
    {
        _afterCommands.Enqueue(action);
        Console.Queue($"exec {CfgNames.Applied}");
        if (!DeferNextFrames) Console.Run();
    }

    public void RunNextFrame()
    {
        Console.Run();
        var work = _nextFrame.ToArray();
        _nextFrame.Clear();
        foreach (var action in work) action();
    }

    public void PrintToChatAll(string message) => Chat.Add(message);

    public void PrintToChat(int slot, string message) => DirectChat.Add((slot, message));

    public bool IsWarmup { get; set; } = true;
    public int RoundsPlayed { get; set; }

    /// <summary>The scoreboard by side; a side without an entry reads as the engine having no team for it.</summary>
    public Dictionary<Side, int> TeamScores { get; } = new() { [Side.Ct] = 0, [Side.T] = 0 };
    public int? ReadTeamScore(Side side) => TeamScores.TryGetValue(side, out var score) ? score : null;

    public bool LoadingRoundBackup { get; set; }

    public void SwitchTeam(HostPlayer player, Side side) => SwitchedTeams.Add((player, side));

    public void ChangeTeam(HostPlayer player, Side side) => ChangedTeams.Add((player, side));

    public void Kick(HostPlayer player, string reason) => Kicked.Add((player, reason));
    public int Aborted { get; private set; }
    public void AbortMatch() => Aborted++;

    public Dictionary<string, string> ClanTags { get; } = [];

    public void SetClanTag(HostPlayer player, string tag) => ClanTags[player.SteamIdString] = tag;

    public IHostTimer AddTimer(double seconds, Action callback, bool stopOnMapChange = true)
    {
        var timer = new FakeTimer(this, Clock + seconds, callback, stopOnMapChange);
        Timers.Add(timer);
        return timer;
    }

    public IHostTimer AddRepeatingTimer(double delaySeconds, double periodSeconds, Action callback, bool stopOnMapChange = true)
    {
        var timer = new FakeTimer(this, Clock + delaySeconds, callback, stopOnMapChange, periodSeconds);
        Timers.Add(timer);
        return timer;
    }

    /// <summary>Reset map time while preserving the remaining delay of timers that survive the change.</summary>
    public void ChangeMap(string mapName)
    {
        foreach (var timer in Timers.ToArray())
        {
            if (timer.StopOnMapChange || timer.Fired) timer.Cancel();
            else timer.DueAt -= Clock;
        }
        MapName = mapName;
        Clock = 0;
        RoundStartTime = 0;
        TickCount = 0;
        SubtickFraction = 0;
        RoundsPlayed = 0;
        IsWarmup = true;
        FreezePeriod = true;
    }

    public int? GetConVarInt(string name) => ConVars.TryGetValue(name, out var v) ? v : null;
    public double? GetConVarNumber(string name) => ConVars.TryGetValue(name, out var v) ? v : null;

    public bool? GetConVarBool(string name) => ConVars.TryGetValue(name, out var v) ? v != 0 : null;

    public void Log(LogLevel level, string message) => Logs.Add((level, message));

    /// <summary>Advances the clock and fires every timer that came due, oldest first.</summary>
    public void Advance(double seconds)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(seconds);
        var startedAt = Clock;
        var startedMs = NowMs;
        var target = Clock + seconds;
        while (Timers.Where(t => !t.Fired && !t.Cancelled && t.DueAt <= target)
                   .OrderBy(t => t.DueAt).FirstOrDefault() is { } timer)
        {
            Clock = timer.DueAt;
            NowMs = startedMs + (long)Math.Round((Clock - startedAt) * 1000);
            timer.Fired = true;
            timer.Callback();
            // A repeating timer keeps its cadence: the next firing is a period after this one was due.
            if (timer.Period is { } period && !timer.Cancelled)
            {
                timer.DueAt += period;
                timer.Fired = false;
            }
        }
        Clock = target;
        NowMs = startedMs + (long)Math.Round(seconds * 1000);
    }
}

/// <summary>Builders for the fixtures every test needs.</summary>
public static class Fixture
{
    /// <summary>The line the plugin queues going live on <see cref="FakeGameHost"/>'s competitive mode.</summary>
    public static readonly string LiveCfgs = CfgNames.Exec(CfgNames.Reset, "gamemode_competitive", CfgNames.Base, CfgNames.Launch);

    /// <summary>The line the plugin queues entering warmup.</summary>
    public static readonly string WarmupCfgs = CfgNames.Exec(CfgNames.Base, CfgNames.Launch, CfgNames.Warmup);

    /// <summary>packages/plugin, found from this source file so no build step has to copy it.</summary>
    public static string PluginRoot([System.Runtime.CompilerServices.CallerFilePath] string thisFile = "") =>
        Path.GetFullPath(Path.Combine(Path.GetDirectoryName(thisFile)!, ".."));

    /// <summary>The translation file the image ships, read straight out of the source tree.</summary>
    public static string CatalogPath =>
        Path.Combine(PluginRoot(), "resources", "translations", "en.jsonc");

    /// <summary>The plugin config file the image ships, read straight out of the source tree.</summary>
    public static string ConfigPath => Path.Combine(PluginRoot(), "configs", "config.jsonc");

    /// <summary>The real catalog, so every test asserts on the strings players actually see.</summary>
    public static CatalogMessages Messages { get; } = CatalogMessages.Load(CatalogPath);

    /// <summary>A round backup as the engine writes it, with nothing in it.</summary>
    public const string EmptyBackup = "\"SaveFile\"\n{\n}\n";

    public const string Team1Player1 = "76561198000000001";
    public const string Team1Player2 = "76561198000000002";
    public const string Team2Player1 = "76561198000000011";
    public const string Team2Player2 = "76561198000000012";
    public const string Spectator = "76561198000000031";
    public const string Stranger = "76561198000000099";

    public static MatchConfig Config(
        SidesMode sides = SidesMode.Knife,
        int knifeDecisionSeconds = 30,
        bool recordDemo = true,
        ReadyMode readyMode = ReadyMode.Auto,
        int? readyTimeoutSeconds = null,
        RestoreFile? restore = null,
        bool allowForceReady = true) => new()
    {
        allowForceReady = allowForceReady,
        matchId = "0199a0b4-5c00-7000-8000-00000000abc1",
        sendStatistics = true,
        restore = restore,
        sides = sides,
        knifeDecisionSeconds = knifeDecisionSeconds,
        readyMode = readyMode,
        readyTimeoutSeconds = readyTimeoutSeconds,
        recordDemo = recordDemo,
        debugCapture = false,
        rules = new MatchRules { maxRounds = 24, overtime = true, overtimeMaxRounds = 6, friendlyFire = false },
        roster = new Roster
        {
            // Index 0 is the lobby captain on each team.
            team1 = new RosterTeam
            {
                name = "Alpha",
                players = [Team1Player1, Team1Player2],
            },
            team2 = new RosterTeam
            {
                name = "Bravo",
                players = [Team2Player1, Team2Player2],
            },
            spectators = [Spectator],
        },
    };

    public static HostPlayer Player(
        string steamId,
        Side side,
        int slot = 0,
        int health = 100,
        bool alive = true,
        string? name = null,
        WorldPosition? position = null,
        ViewAngles? angles = null) =>
        new(slot, ulong.Parse(steamId), name ?? $"player{slot}", side, health, alive, IsBot: false)
        {
            Authorized = true,
            Position = position,
            Angles = angles,
        };

    /// <summary>A live round to play on from, team 1 on T, under an MR12 with overtime.</summary>
    public static RestoreFile Restore(Side team1Side = Side.T) => new()
    {
        team1Side = team1Side,
        rules = new MatchRules { maxRounds = 24, overtime = true, overtimeMaxRounds = 6, friendlyFire = false },
        file = EmptyBackup,
    };

    public static HostPlayer Bot(Side side, int slot = 9, int health = 100) =>
        new(slot, 0, "Bot Sam", side, health, IsAlive: true, IsBot: true);
}
