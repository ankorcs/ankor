using System.Text.Json.Serialization;
using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>`ServerStatus.match.phase` in plugin-socket.ts.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<MatchPhase>))]
public enum MatchPhase
{
    [JsonStringEnumMemberName("loading")] Loading,
    [JsonStringEnumMemberName("warmup")] Warmup,
    /// <summary>Between warmup and live on a launch with a restore: the engine is loading the backup.</summary>
    [JsonStringEnumMemberName("pending_restore")] PendingRestore,
    [JsonStringEnumMemberName("knife")] Knife,
    [JsonStringEnumMemberName("side_choice")] SideChoice,
    [JsonStringEnumMemberName("live")] Live,
    [JsonStringEnumMemberName("post_live")] PostLive,
}

/// <summary>Drive warmup, optional knife/side choice, live and post-live phases.
/// Start waits for each team's roster, and in manual mode for its !ready, which a captain's !forceready stands in for.
/// A launch with a restore skips knife and side choice and loads the backup instead of going live.
/// </summary>
public sealed class MatchState : IDisposable
{
    /// <summary>The start countdown: the engine's warmup clock, the chat announcement and the timer.</summary>
    public const int CountdownSeconds = 15;

    private readonly IGameHost _host;
    private readonly IMatchOutput _output;
    private readonly IMessages _messages;
    private readonly Action _startRecording;

    private readonly OwnedTimer _sideChoiceTimer;
    private readonly OwnedTimer _startTimer;
    private readonly OwnedTimer _reminderTimer;
    private readonly OwnedTimer _deadlineTimer;
    private readonly OwnedTimer _restoreTimer;
    private readonly ReadyUp _ready;
    /// <summary>
    /// When the ready deadline passes, in the engine's game time (<see cref="IGameHost.CurrentTime"/>),
    /// the clock the warmup clock runs on.
    /// </summary>
    private double _readyDeadlineAt;

    /// <summary>The whole seconds left that the last deadline warning announced, so each goes out once.</summary>
    private int _readyWarnedAt;

    /// <summary>
    /// Seconds the warmup clock runs longer than the ready deadline, so the plugin decides whether the
    /// match starts or is canceled before the engine can end warmup on its own. The clock shows 0:01
    /// when the deadline passes.
    /// </summary>
    internal const int ReadyDeadlineMarginSeconds = 1;

    /// <summary>
    /// Seconds after the ready deadline at which the match ends without its round: a warmup with
    /// nobody on a team may never end into one.
    /// </summary>
    internal const int ReadyExpiryFallbackSeconds = 15;

    /// <summary>Seconds left from which the deadline warnings say only how long is left to ready up.</summary>
    internal const int ReadyTimeoutWarningSeconds = 60;

    /// <summary>The ready deadline passed: the match is cancelled, and nobody readies up any more.</summary>
    private bool _readyExpired;
    /// <summary><see cref="ReadyExpired"/> was raised, so it is raised once.</summary>
    private bool _readyExpiryRaised;
    private int _round;
    private bool _liveAnnounced;
    private bool _knifeAnnounced;
    private bool _liveRestartIssued;
    // Live only once the engine starts the first round after the plugin's restart, so nothing reports a
    // live phase while the server still plays out warmup.
    private bool _goingLive;
    private bool _swapRestartPending;
    private int _restartRevision;
    /// <summary>In <see cref="MatchPhase.PendingRestore"/>: the load was issued, and the next round start is the restored round.</summary>
    /// <summary>The engine is applying a backup: its loading flag was seen up, and the round it restarts into is next.</summary>
    private bool _engineLoadingBackup;
    /// <summary>The side team 1 had in the round a <c>restore_round</c> asked for, applied when the engine starts loading.</summary>
    private Side? _armedRestoreSide;
    private bool _backupsConfigured;

    public MatchState(
        IGameHost host,
        MatchConfig config,
        IMatchOutput output,
        IMessages messages,
        Action startRecording)
    {
        CollectEvents = config.sendStatistics;
        _host = host;
        _startRecording = startRecording;
        _sideChoiceTimer = new OwnedTimer(host);
        _startTimer = new OwnedTimer(host);
        _reminderTimer = new OwnedTimer(host);
        _deadlineTimer = new OwnedTimer(host);
        _restoreTimer = new OwnedTimer(host);
        _output = output;
        _messages = messages;
        Config = config;
        Roster = new RosterEnforcer(config.roster);
        _ready = new ReadyUp(Roster, config.readyMode, config.spectatorsReadyUp);
        Team1Side = config.restore?.team1Side ?? (config.sides == SidesMode.Team1T ? Side.T : Side.Ct);
        _firstHalfTeam1Side = Team1Side;
    }

    /// <summary>Team 1's side in the first half: the team on CT then owns <c>mp_teamname_1</c> for the whole map.</summary>
    private Side _firstHalfTeam1Side;

    public bool CollectEvents { get; }
    public MatchConfig Config { get; private set; }
    public RosterEnforcer Roster { get; private set; }
    public MatchPhase Phase
    {
        get => _phase;
        // Any change of phase replaces a going-live that has not reached its round start.
        private set
        {
            var changed = _phase != value;
            _phase = value;
            _goingLive = false;
            if (changed) _output.ReportMatch(new PluginPhaseChange { phase = value });
        }
    }

    private MatchPhase _phase = MatchPhase.Loading;
    public bool Ending { get; private set; }

    /// <summary>The ready deadline passed: the match will not start, and joining players are kicked.</summary>
    public bool Canceled => _readyExpired;

    /// <summary>Raised when the ready deadline passes; who was there and ready is in <see cref="ReadyAtDeadline"/>.</summary>
    public event Action? ReadyExpired;

    public Side Team1Side { get; private set; }

    /// <summary>
    /// Halftime has swapped <see cref="Team1Side"/>, but the engine moves players to their new
    /// sides only when it resets the round, after the break.
    /// </summary>
    public bool SidesSwapPending { get; private set; }

    /// <summary>Team 1's side as the engine has players now: through the halftime break, the old one.</summary>
    public Side Team1PlacedSide => SidesSwapPending ? Team1Side.Opposite() : Team1Side;

    /// <summary>Latch the one-based round at round_start, before the engine advances its counter at round_end.
    /// Before the first observed start, fall back to the current engine count.
    /// </summary>
    public int Round => _round > 0 ? _round : Math.Max(1, _host.RoundsPlayed + 1);

    public int Team1Score { get; private set; }
    public int Team2Score { get; private set; }

    public Side? KnifeWinnerSide { get; private set; }

    public int? KnifeWinnerTeam =>
        KnifeWinnerSide is { } side ? RosterEnforcer.TeamForSide(side, Team1Side) : null;

    /// <summary>
    /// Live play, and not while the engine loads a backup: its kills, moves and round end then
    /// seat players for the file and are nobody's play. Read from the engine's flag, so it holds
    /// from the load's first callback, before the match notices the load and goes pending.
    /// </summary>
    public bool IsLive => Phase == MatchPhase.Live && _liveAnnounced && !_host.LoadingRoundBackup;

    /// <summary>A backup is loading: the engine's flag is up, or the match waits for the restored round.</summary>
    private bool LoadingBackup => Phase == MatchPhase.PendingRestore || _host.LoadingRoundBackup;

    /// <summary>How many loads have been confirmed, so a caller can tell a round start that restored the match from one that did not.</summary>
    public int Restores { get; private set; }

    /// <summary>
    /// True from the knife swap's <c>mp_swapteams</c> until the next round start, which moves the players.
    /// The engine's halftime swap is silent, this command's is not, so the plugin silences its
    /// player_team events to match.
    /// </summary>
    public bool SwapInProgress => _swapRestartPending;

    // Phase stays Warmup until the live round starts, a few frames after going live began; no ready-up
    // command may start a second countdown in between.
    private bool ReadyingUp => Phase == MatchPhase.Warmup && !_goingLive && !_readyExpired;

    // The engine holds a scheduled restart in mp_restartgame until it runs it.
    private bool RestartScheduled => (_host.GetConVarNumber("mp_restartgame") ?? 0) > 0;

    /// <summary>Seconds since freeze time ended. Goes on the wire as <c>round_time</c>.</summary>
    public double RoundTime => _host.RoundElapsedTime;


    /// <summary>
    /// Stamps the message with the current time, tick and round. <paramref name="roundTime"/>
    /// replaces the engine's round clock where that clock is known to be wrong.
    /// </summary>
    public T Stamp<T>(T message, double? roundTime = null) where T : PluginMessage
    {
        message.timestamp = _host.NowMs;
        message.tick = _host.SimulationTick;

        if (message is RoundScopedMessage scoped)
        {
            scoped.round = Round;
            scoped.roundTime = roundTime ?? WireSeconds.Of(RoundTime);
        }

        return message;
    }

    public PlayerRef RefFor(HostPlayer player) => new()
    {
        steamId = player.SteamIdString,
        name = WireText.Clean(player.Name, WireText.MaxPlayerName, controls: false),
        side = player.Side,
        team = Roster.LobbyTeam(player.SteamIdString),
    };

    public PlayerAt AtFor(HostPlayer player) => Place(RefFor(player), player);

    /// <summary>Positions come only from the same player's live snapshot, never another occupant of the slot.</summary>
    public static PlayerAt Place(PlayerRef reference, HostPlayer? live) => new()
    {
        steamId = reference.steamId,
        name = reference.name,
        side = reference.side,
        team = reference.team,
        position = live?.SteamIdString == reference.steamId ? WirePlayerPosition.From(live) : null,
    };

    public void Emit(PluginMessage message, double? roundTime = null)
    {
        if (Phase == MatchPhase.PostLive || !CollectEvents) return;
        // Nothing the engine does while it loads a backup is play; the plugin's own word still goes out.
        if (LoadingBackup && message is not (PluginLog or MatchRestored)) return;
        _output.Send(Stamp(message, roundTime));
    }

    /// <summary>A live round's backup goes with the events when the match sends them, and to the agent alone otherwise.</summary>
    public void SendBackup(RoundBackup backup)
    {
        if (!IsLive) return;
        if (CollectEvents) Emit(backup);
        else _output.ReportMatch(new PluginBackup { backup = Stamp(backup) });
    }

    public void LogEvent(LogLevel level, string message) =>
        Emit(new PluginLog { level = level, message = WireText.Clean(message, WireText.MaxLogMessage) });


    public void EnterWarmup()
    {
        // mp_match_end_restart brings the map back after the match; that must not start a second one.
        if (Ending)
        {
            return;
        }

        _startTimer.Cancel();
        ++_restartRevision;
        _liveRestartIssued = false;
        _swapRestartPending = false;
        Phase = MatchPhase.Warmup;
        _round = 0;
        KnifeWinnerSide = null;
        SetScore(0, 0);
        _liveAnnounced = false;
        EndReason = MapEndReasons.Played;
        ReadyAtDeadline = null;
        _engineLoadingBackup = false;
        _armedRestoreSide = null;
        _beforeRestore = null;
        _backupsConfigured = false;
        _restoreTimer.Cancel();
        CloseSideChoice();
        _ready.Clear();

        // The launch's own settings over Ankor's, and warmup's over both.
        Exec(CfgNames.Base, CfgNames.Launch, CfgNames.Warmup);
        var revision = _restartRevision;
        AfterConfig(() =>
        {
            if (revision != _restartRevision || Phase != MatchPhase.Warmup) return;
            ApplyTeamNames();
            ShowReadyDeadlineOnTheWarmupClock();
            _host.ExecuteCommand("mp_warmup_start");
        });

        AnnounceWaiting();
        StartReadyUp();
    }

    public void MaybeStart()
    {
        if (Ending || !ReadyingUp) return;
        ShowReadyTags();
        if (_ready.External) return;
        if (!_ready.TeamReady(1, _host.Players) || !_ready.TeamReady(2, _host.Players)) return;
        if (!_ready.SpectatorsReady(_host.Players)) return;
        StartCountdown();
    }

    public bool TrySetReady(HostPlayer caller, bool ready)
    {
        if (Ending || !ReadyingUp) return false;

        if (_ready.External)
        {
            _host.PrintToChat(caller.Slot, _messages.Prefixed("ready.external"));
            return false;
        }

        if (!_ready.Manual)
        {
            _host.PrintToChat(caller.Slot, _messages.Prefixed("ready.automatic"));
            return false;
        }

        if (!_ready.Counted(caller.SteamIdString))
        {
            // Not practically possible.
            return false;
        }

        if (_startTimer.IsPending)
        {
            return false;
        }

        if (!ready && _ready.Forced(caller.SteamIdString))
        {
            _host.PrintToChat(caller.Slot, _messages.Prefixed("ready.forced"));
            return false;
        }

        var changed = ready ? _ready.Mark(caller.SteamIdString) : _ready.Unmark(caller.SteamIdString);
        if (!changed)
        {
            _host.PrintToChat(caller.Slot, ready
                ? _messages.Prefixed("ready.already_ready")
                : _messages.Prefixed("ready.already_unready", ChatCommands.Ready));
            return false;
        }

        _host.PrintToChatAll(ready
            ? _messages.Prefixed("ready.player_ready", caller.Name, _ready.ReadyCount(_host.Players), _ready.ReadyTotal)
            : _messages.Prefixed("ready.player_unready", caller.Name));
        MaybeStart();
        return true;
    }

    /// <summary>
    /// Over warmup.cfg's paused clock: with a ready deadline, the warmup clock counts it down in
    /// game. The clock runs <see cref="ReadyDeadlineMarginSeconds"/> longer than the deadline, so the
    /// plugin decides whether the match starts or is canceled before the engine can end warmup on its own.
    /// </summary>
    private void ShowReadyDeadlineOnTheWarmupClock()
    {
        if (Config.readyTimeoutSeconds is not { } timeout) return;
        _host.ExecuteCommand($"mp_warmuptime {timeout + ReadyDeadlineMarginSeconds}");
        _host.ExecuteCommand("mp_warmup_pausetimer 0");
    }

    private void StartReadyUp()
    {
        _host.SetConVarString("sv_clantags_enabled", _ready.Manual ? "1" : "0");
        if (_ready.Manual) _reminderTimer.ScheduleRepeating(ReminderSeconds, ReminderSeconds, RemindUnready);
        if (Config.readyTimeoutSeconds is { } timeout)
        {
            // The ticks pace the warnings; the time left is read from the engine's clock against a
            // fixed end, so a late or skipped tick cannot move the deadline.
            _readyDeadlineAt = _host.CurrentTime + timeout;
            _readyWarnedAt = timeout;
            _deadlineTimer.ScheduleRepeating(1, 1, TickReadyDeadline);
        }
    }

    private void EndReadyUp()
    {
        _reminderTimer.Cancel();
        _deadlineTimer.Cancel();
        if (!_ready.Manual) return;
        foreach (var player in RosteredPlayers()) _host.SetClanTag(player, "");
        _host.SetConVarString("sv_clantags_enabled", "0");
    }

    private void ShowReadyTags()
    {
        if (!_ready.Manual || _startTimer.IsPending) return;
        foreach (var player in RosteredPlayers())
        {
            _host.SetClanTag(player, _ready.IsReady(player.SteamIdString)
                ? _messages.Get("ready.tag_ready")
                : _messages.Get("ready.tag_not_ready"));
        }
    }

    private IEnumerable<HostPlayer> RosteredPlayers() =>
        _host.Players.Where(player => !player.IsBot && _ready.Counted(player.SteamIdString));

    private const int ReminderSeconds = 30;

    private void RemindUnready()
    {
        // The deadline's tick can come later in the same frame: the reminder runs on the same
        // 30-second steps, and its lines would follow the deadline's cancellation.
        var deadlinePassed = Config.readyTimeoutSeconds is not null && ReadySecondsLeft() <= 0;
        if (Ending || Phase != MatchPhase.Warmup || _startTimer.IsPending || deadlinePassed)
        {
            _reminderTimer.Cancel();
            return;
        }
        var unready = string.Join(", ", _ready.Unready(_host.Players).Select(p => p.Name));
        if (unready.Length > 0) _host.PrintToChatAll(_messages.Prefixed("ready.reminder", ChatCommands.Ready, unready));
        AnnounceWaiting();
    }

    private void TickReadyDeadline()
    {
        if (Ending || Phase != MatchPhase.Warmup || _startTimer.IsPending)
        {
            _deadlineTimer.Cancel();
            return;
        }
        var team1Ready = _ready.TeamReady(1, _host.Players);
        var team2Ready = _ready.TeamReady(2, _host.Players);
        var awaitingSpectators = team1Ready && team2Ready && !_ready.SpectatorsReady(_host.Players);
        var left = ReadySecondsLeft();
        if (left > 0)
        {
            if (left != _readyWarnedAt && ReadyUp.ShouldWarn(left))
            {
                _readyWarnedAt = left;
                var time = ReadyUp.FormatTime(left);
                _host.PrintToChatAll(left <= ReadyTimeoutWarningSeconds
                    ? _messages.Prefixed("ready.timeout_warning", time)
                    : awaitingSpectators
                    ? _messages.Prefixed("ready.deadline_spectators", time)
                    : team1Ready == team2Ready
                        ? _messages.Prefixed("ready.deadline_both", time)
                        : _messages.Prefixed("ready.deadline_team", Roster.TeamName(team1Ready ? 2 : 1), time));
            }
            return;
        }

        // Stops the reminders of who is unready and how many players are missing, and clears the tags.
        EndReadyUp();
        _readyExpired = true;
        _host.PrintToChatAll(_messages.Prefixed("ready.canceled"));

        // The plugin reports who was there and ready; the controller decides what that means.
        EndReason = MapEndReasons.ReadyTimeout;
        ReadyAtDeadline = ReadyTeamsNow(team1Ready, team2Ready);
        LogEvent(LogLevel.Warn, $"ready deadline passed: team 1 {Describe(ReadyAtDeadline.team1)}, team 2 {Describe(ReadyAtDeadline.team2)}"
            + (ReadyAtDeadline.spectators is { } watching ? $", spectators {Describe(watching)}" : ""));
        // The client shows the end screen only for a match that left warmup, under the live cfgs, so
        // round 1 starts and ends the match. Its team intro would freeze the players through the screen.
        ExecLive();
        _host.AfterCommands(() =>
        {
            if (Ending) return;
            _host.SetConVarString("mp_team_intro_time", "0");
            _host.ExecuteCommand("mp_warmup_end");
        });
        // The fallback covers a warmup that never ends into a round.
        _deadlineTimer.Schedule(ReadyExpiryFallbackSeconds, RaiseReadyExpired);
    }

    private void RaiseReadyExpired()
    {
        if (_readyExpiryRaised) return;
        _readyExpiryRaised = true;
        _deadlineTimer.Cancel();
        ReadyExpired?.Invoke();
    }

    /// <summary>
    /// Whole seconds until the ready deadline, to the nearest: the ticks land on whole seconds from
    /// its start, so the figure is what the warmup clock shows, less its margin.
    /// </summary>
    private int ReadySecondsLeft() => Math.Max(0, (int)Math.Round(_readyDeadlineAt - _host.CurrentTime));

    private ReadyTeams ReadyTeamsNow(bool team1Ready, bool team2Ready)
    {
        var connected = _host.Players.Where(player => player.IsReportable).ToArray();
        int Count(Func<string, bool> rostered) => connected.Count(player => rostered(player.SteamIdString));
        return new ReadyTeams
        {
            team1 = new ReadyTeam { connected = Count(id => Roster.LobbyTeam(id) == 1), ready = team1Ready },
            team2 = new ReadyTeam { connected = Count(id => Roster.LobbyTeam(id) == 2), ready = team2Ready },
            spectators = _ready.WaitsForSpectators
                ? new ReadyTeam { connected = Count(Roster.IsSpectator), ready = _ready.SpectatorsReady(_host.Players) }
                : null,
        };
    }

    private static string Describe(ReadyTeam team) => $"{team.connected} connected, {(team.ready ? "ready" : "not ready")}";

    /// <summary>
    /// Let a captain ready their own team with whoever of it is here, without the rest of its roster
    /// or its !ready; the match starts once the other team is ready too.
    /// </summary>
    public bool TryForceReady(HostPlayer caller)
    {
        if (Ending || !ReadyingUp) return false;

        if (_ready.External)
        {
            _host.PrintToChat(caller.Slot, _messages.Prefixed("ready.external"));
            return false;
        }

        // Auto starts once everyone is connected, so there is no ready-up to cut short.
        if (!_ready.Manual)
        {
            _host.PrintToChat(caller.Slot, _messages.Prefixed("ready.automatic"));
            return false;
        }

        // A match without force-ready treats !forceready as an unknown command, and answers nothing.
        if (!Config.allowForceReady) return false;

        if (_startTimer.IsPending)
        {
            return false;
        }

        if (Roster.IsSpectator(caller.SteamIdString) && _ready.WaitsForSpectators) return TryForceReadySpectators(caller);

        if (Roster.CaptainTeam(caller.SteamIdString) is not { } team)
        {
            _host.PrintToChat(caller.Slot,
                Roster.LobbyTeam(caller.SteamIdString) is { } own && CaptainDisplayName(own) is { } captain
                    ? _messages.Prefixed("forceready.not_captain_ask", captain)
                    : _messages.Prefixed("forceready.not_captain"));
            return false;
        }

        if (!_ready.Force(team))
        {
            _host.PrintToChat(caller.Slot, _messages.Prefixed("forceready.already"));
            return false;
        }

        _host.PrintToChatAll(_messages.Prefixed("forceready.done", caller.Name, Roster.TeamName(team)));
        LogEvent(LogLevel.Warn, $"force ready of team {team} by {caller.SteamIdString} ({caller.Name})");
        MaybeStart();
        return true;
    }

    /// <summary>Any rostered spectator readies the spectators, so one who does not show up blocks nobody.</summary>
    private bool TryForceReadySpectators(HostPlayer caller)
    {
        if (!_ready.ForceSpectators())
        {
            _host.PrintToChat(caller.Slot, _messages.Prefixed("forceready.already"));
            return false;
        }

        _host.PrintToChatAll(_messages.Prefixed("forceready.done_spectators", caller.Name));
        LogEvent(LogLevel.Warn, $"force ready of the spectators by {caller.SteamIdString} ({caller.Name})");
        MaybeStart();
        return true;
    }

    /// <summary>Null once the countdown runs, otherwise the <see cref="CommandResult"/> refusal.</summary>
    public string? StartFromController()
    {
        if (Ending || !ReadyingUp) return CommandResult.NotInWarmup;
        if (!Roster.BothTeamsPresent(_host.Players)) return CommandResult.TeamEmpty;

        LogEvent(LogLevel.Info, "start_match from the controller");
        StartCountdown();
        return null;
    }

    private void AnnounceWaiting()
    {
        var missing = Roster.MissingPlayers(_host.Players).Count;
        if (missing <= 0) return;
        _host.PrintToChatAll(missing == 1
            ? _messages.Prefixed("warmup.waiting_one", missing)
            : _messages.Prefixed("warmup.waiting_many", missing));
    }

    /// <summary>Once running, only a reset stops it: a player leaving meanwhile is their own problem.</summary>
    private void StartCountdown()
    {
        if (_startTimer.IsPending) return;
        EndReadyUp();
        _host.PrintToChatAll(Config.restore is not null
            ? _messages.Prefixed("warmup.countdown_restore", CountdownSeconds)
            : Config.sides == SidesMode.Knife ? _messages.Prefixed("warmup.countdown_knife", CountdownSeconds)
            : _messages.Prefixed("warmup.countdown_live", CountdownSeconds));
        _host.ExecuteCommand($"mp_warmuptime {CountdownSeconds}");
        _host.ExecuteCommand("mp_warmup_pausetimer 0");
        // A second before the engine's warmup clock would end the warmup on its own.
        _startTimer.Schedule(CountdownSeconds - 1, BeginMatch);
    }

    /// <summary>
    /// Resolve departures after the engine removes the disconnected players. True when the whole
    /// roster has left a match that began, which ends the map as abandoned. Leaving warmup ends nothing.
    /// </summary>
    public bool ResolvePlayerDeparture()
    {
        if (Ending) return false;
        if (Phase == MatchPhase.Warmup) _ready.Retain(_host.Players);
        var players = _host.Players.Where(player => player.IsReportable
            && Roster.LobbyTeam(player.SteamIdString) is not null).ToArray();
        // Side choice is still part of knife: both teams leaving there ends the map too.
        if (Phase is not (MatchPhase.Live or MatchPhase.Knife or MatchPhase.SideChoice)) return false;
        if (players.Length == 0)
        {
            EndReason = MapEndReasons.Abandoned;
            return true;
        }
        if (Phase != MatchPhase.Knife) return false;
        var team1Present = players.Any(player => Roster.LobbyTeam(player.SteamIdString) == 1);
        var team2Present = players.Any(player => Roster.LobbyTeam(player.SteamIdString) == 2);
        if (team1Present && team2Present) return false;
        var winner = RosterEnforcer.SideForTeam(team1Present ? 1 : 2, Team1Side);
        FinishKnifeRound(KnifeRound.Decide(players) with { Winner = winner }, winner);
        return false;
    }

    private void BeginMatch()
    {
        if (Config.restore is not null)
        {
            StartLaunchRestore();
        }
        else if (Config.sides == SidesMode.Knife)
        {
            StartKnifeRound();
        }
        else
        {
            GoLive();
        }
    }

    /// <summary>
    /// Go live on the launch's rules and let the engine's warmup end start round 1; the load goes
    /// out from inside that round, where the engine expects it, and the restart it causes is
    /// confirmed like any other load. Nothing is reported until then.
    /// </summary>
    private void StartLaunchRestore()
    {
        if (Ending) return;
        _startTimer.Cancel();
        Phase = MatchPhase.PendingRestore;
        _liveAnnounced = false;
        _liveRestartIssued = false;
        _swapRestartPending = false;
        _engineLoadingBackup = false;
        _armedRestoreSide = null;
        _beforeRestore = null;
        _round = 0;
        SidesSwapPending = false;
        Team1Side = Config.restore!.team1Side;
        CloseSideChoice();

        // The engine's round 1 before the load writes round 1's file at 0–0: the point a restore_round 1
        // on this server resets to.
        ExecLiveThenRecord();
        // The game mode's cfg sets its own mp_warmuptime, so the plugin ends warmup into round 1 itself.
        // Not RestartIntoLive, which goes live: a restore loads its backup from inside round 1 instead.
        AfterConfig(() =>
        {
            if (Phase == MatchPhase.PendingRestore && _host.IsWarmup) _host.ExecuteCommand("mp_warmup_end");
        });
    }

    /// <summary>
    /// The backup file and the recording follow the live cfgs, whose lines run after anything sent
    /// directly: a cfg could otherwise redirect the backups and change the GOTV delay mid-recording.
    /// </summary>
    private void ExecLiveThenRecord()
    {
        ExecLive();
        var phase = Phase;
        // Not AfterConfig: the restart that follows bumps the revision it would wait under.
        _host.AfterCommands(() =>
        {
            if (Ending || Phase != phase) return;
            ConfigureBackups();
            _startRecording();
        });
    }

    /// <summary>Once per map: the engine's auto-backup into the slot.</summary>
    private void ConfigureBackups()
    {
        if (_backupsConfigured) return;
        _backupsConfigured = true;
        RoundBackups.Configure(_host);
    }

    /// <summary>Seconds after a load command without the engine starting on it before that is logged. The engine starts within a tick.</summary>
    private const int BackupLoadReportSeconds = 5;

    /// <summary>Seconds the engine gets to restart into the restored round once it has started loading, before the restore counts as failed.</summary>
    private const int RestoreTimeoutSeconds = 10;

    /// <summary>Seconds the restored round stays in the engine's autopause after it is confirmed, so the HUD settles before play.</summary>
    private const int RestoreUnpauseSeconds = 1;

    /// <summary>Team 1's side as each live round started: the side a restore to it puts team 1 on, and falls back to when the engine seats nobody on a playing side.</summary>
    private readonly Dictionary<int, Side> _restoreSideByRound = [];

    /// <summary>Where the engine's load found a live match, to go back to when it never restarts the round.</summary>
    private (Side Team1Side, bool Announced)? _beforeRestore;

    /// <summary>
    /// Through the halftime break the engine has the teams' new sides but not yet the players; a
    /// backup loaded then lands on sides that are about to flip. The next round start is seconds away.
    /// </summary>
    public bool AtHalftime => SidesSwapPending;

    /// <summary>
    /// From a round's end to the next round's start. A backup loaded then has its scores applied, but
    /// the engine never restarts into the restored round: the round end already under way absorbs the
    /// restart, and the server is left loading for good. The next round start is seconds away.
    /// </summary>
    public bool BetweenRounds { get; private set; }

    /// <summary>A round ended, live or not; the next round start clears it.</summary>
    public void NoticeRoundEnded() => BetweenRounds = true;

    /// <summary>
    /// Play on from one of this server's earlier rounds: the controller's <c>restore_round</c>, with the
    /// file the agent found. Null once the engine was asked to load it, otherwise the <see cref="CommandResult"/> refusal.
    /// </summary>
    public string? RestoreRound(int round, string file)
    {
        if (Ending || !IsLive) return CommandResult.NotLive;
        if (AtHalftime) return CommandResult.AtHalftime;
        if (BetweenRounds) return CommandResult.BetweenRounds;

        WatchBackupLoad();
        _armedRestoreSide = _restoreSideByRound.TryGetValue(round, out var side) ? side : null;
        // The engine's autopause stays on: the restored round opens paused, and the confirm lifts it.
        _host.ExecuteCommand($"{Backups.LoadCommand} {file}");
        return null;
    }

    /// <summary>
    /// A backup is about to be loaded, by this plugin or by whoever reached the console. Nothing
    /// changes here: the engine runs the command on a later tick and, if it takes the file, shows it
    /// through <see cref="NoticeBackupLoading"/>, which is where the match goes pending. A load the
    /// engine does nothing with leaves the match as it is, and is logged after a few seconds. False
    /// refuses the load: the caller keeps the command from the engine.
    /// </summary>
    public bool BeforeBackupLoad()
    {
        if (Ending || Phase == MatchPhase.PostLive)
        {
            // The match is over and its result reported; a load now would play on past the map_end.
            _host.Log(LogLevel.Warn, "a backup load was refused: the match has ended");
            return false;
        }

        if (AtHalftime)
        {
            LogEvent(LogLevel.Warn, "a backup load was refused: the match is at halftime; load it once the next round has started");
            return false;
        }

        if (BetweenRounds)
        {
            LogEvent(LogLevel.Warn, "a backup load was refused: the round has ended; load it once the next round has started");
            return false;
        }

        // A backup belongs to live play; loaded in the knife round or the side choice it would skip the
        // live cfgs and the demo, and a failed one would leave no phase to go back to.
        if (Phase is MatchPhase.Knife or MatchPhase.SideChoice)
        {
            LogEvent(LogLevel.Warn, "a backup load was refused: load one in warmup or during live play");
            return false;
        }

        WatchBackupLoad();
        return true;
    }

    private void WatchBackupLoad()
    {
        if (_engineLoadingBackup) return;
        _restoreTimer.Schedule(BackupLoadReportSeconds, () =>
        {
            LogEvent(LogLevel.Warn, "the engine did not load the backup it was asked to");
            _host.PrintToChatAll(_messages.Prefixed("restore.failed"));
        });
    }

    /// <summary>
    /// The engine's own word that it is applying a backup, read where a load shows: the moves that
    /// unseat players, the round end it forces and the prestart that follows. It is down again by
    /// the round start. The first sighting takes the match pending, whoever asked for the load.
    /// </summary>
    public void NoticeBackupLoading()
    {
        if (_engineLoadingBackup || !_host.LoadingRoundBackup) return;
        if (Ending || Phase is MatchPhase.PostLive or MatchPhase.Loading) return;

        if (Phase != MatchPhase.PendingRestore)
        {
            _beforeRestore = (Team1Side, _liveAnnounced);
            _startTimer.Cancel();
            CloseSideChoice();
            if (Phase == MatchPhase.Warmup) EndReadyUp();
            Phase = MatchPhase.PendingRestore;
            _liveAnnounced = false;
            _liveRestartIssued = false;
            _swapRestartPending = false;
        }

        if (_armedRestoreSide is { } side) Team1Side = side;
        _armedRestoreSide = null;
        _engineLoadingBackup = true;
        _restoreTimer.Schedule(RestoreTimeoutSeconds, () => RestoreFailed("the engine did not restart into the restored round"));
    }

    private void OnRestoreRoundStart()
    {
        if (_engineLoadingBackup)
        {
            ConfirmRestore();
            return;
        }

        // The launch's file, from inside the engine's round 1; not in its callback, the load restarts the round.
        _host.NextFrame(() =>
        {
            if (Ending || Phase != MatchPhase.PendingRestore || _engineLoadingBackup || !BeforeBackupLoad()) return;
            _host.ExecuteCommand($"{Backups.LoadCommand} {Backups.RestoreFile}");
        });
    }

    /// <summary>The restored round has started: the file is the one record of where the match stands, and the engine has just read it.</summary>
    private void ConfirmRestore()
    {
        var side = PlacedSideOfTeam1() ?? Team1Side;
        var team1 = _host.ReadTeamScore(side);
        var team2 = _host.ReadTeamScore(side.Opposite());
        if (team1 is null || team2 is null)
        {
            RestoreFailed("the scores could not be read");
            return;
        }

        _restoreTimer.Cancel();
        _engineLoadingBackup = false;
        _beforeRestore = null;
        Phase = MatchPhase.Live;
        _liveAnnounced = true;
        Team1Side = side;
        SetScore(team1.Value, team2.Value);
        Restores++;
        _restoreSideByRound[Round] = Team1Side;
        // The engine writes no backup for the restored round, so RoundBackups sends none for it.
        // The rounds after it are backed up as usual.
        ConfigureBackups();
        // A restore keeps the names: the engine shows them by the restored round's half. Only the
        // launch's own restore, the server's first, has none set for its match's first half yet.
        if (Restores == 1 && Config.restore is not null)
        {
            _firstHalfTeam1Side = FirstHalfSide(Round, Team1Side);
            ApplyTeamNames();
        }
        // The engine's autopause holds the restored round in its freeze time. Lifting it a second in,
        // rather than at once, is what brings the HUD's round clock back; an immediate unpause leaves
        // the clock hidden until its last ten seconds.
        _restoreTimer.Schedule(RestoreUnpauseSeconds, () => _host.ExecuteCommand("mp_unpause_match"));
        _host.Log(LogLevel.Info, $"restored: playing on from round {Round} at {Team1Score}–{Team2Score}, team 1 on {Team1Side}");
        _host.PrintToChatAll(_messages.Prefixed("match.restored", Round, Team1Score, Team2Score));
        Emit(new MatchRestored { round = Round, team1Side = Team1Side, team1Score = Team1Score, team2Score = Team2Score });
    }

    /// <summary>The side the engine seated team 1 on: where most of its players are; null with none on a playing side.</summary>
    private Side? PlacedSideOfTeam1()
    {
        var t = 0;
        var ct = 0;
        foreach (var player in _host.Players)
        {
            if (player.IsBot || Roster.LobbyTeam(player.SteamIdString) != 1) continue;
            if (player.Side == Side.T) t++;
            else if (player.Side == Side.Ct) ct++;
        }

        return t == ct ? null : t > ct ? Side.T : Side.Ct;
    }

    /// <summary>
    /// The engine started loading but never restarted the round, or restarted it with scores the
    /// plugin cannot read. A live match goes back to how the load found it. A launch stays pending,
    /// unreported, until a load is tried again or the controller, which sees the phase and the
    /// log, ends the server.
    /// </summary>
    private void RestoreFailed(string reason)
    {
        _restoreTimer.Cancel();
        _engineLoadingBackup = false;
        LogEvent(LogLevel.Error, $"the backup could not be restored: {reason}");
        _host.PrintToChatAll(_messages.Prefixed("restore.failed"));
        var before = _beforeRestore;
        _beforeRestore = null;
        if (before is not { } live) return;
        Phase = MatchPhase.Live;
        Team1Side = live.Team1Side;
        _liveAnnounced = live.Announced;
    }

    /// <summary>The countdown's warmup clock runs out into the knife round, so the engine ends warmup.</summary>
    public void StartKnifeRound()
    {
        if (Ending) return;
        _startTimer.Cancel();
        CloseSideChoice();
        Phase = MatchPhase.Knife;
        _knifeAnnounced = false;
        KnifeWinnerSide = null;

        Exec(CfgNames.Knife);
    }

    /// <summary>Name the team's captain as connected players know them; null when nobody with that id is here.</summary>
    private string? CaptainDisplayName(int team)
    {
        if (Roster.Captain(team) is not { Length: > 0 } captain) return null;
        return _host.Players.FirstOrDefault(player =>
            !player.IsBot && player.SteamIdString == captain)?.Name;
    }

    /// <summary>Use the pre-cleanup alive/HP tally; engine winner breaks exact ties.</summary>
    public void FinishKnifeRound(KnifeTally tally, Side roundEndWinner)
    {
        if (Ending || Phase != MatchPhase.Knife) return;

        var winnerSide = KnifeRound.Resolve(tally, roundEndWinner);
        var winnerTeam = RosterEnforcer.TeamForSide(winnerSide, Team1Side)!.Value;

        KnifeWinnerSide = winnerSide;
        Phase = MatchPhase.SideChoice;

        Emit(new KnifeEnd
        {
            winnerTeam = winnerTeam, winnerSide = winnerSide,
            team1Alive = Team1Side == Side.T ? tally.TAlive : tally.CtAlive,
            team2Alive = Team1Side == Side.T ? tally.CtAlive : tally.TAlive,
            team1Health = Team1Side == Side.T ? tally.THealth : tally.CtHealth,
            team2Health = Team1Side == Side.T ? tally.CtHealth : tally.THealth,
        });

        var name = Roster.TeamName(winnerTeam);
        _host.PrintToChatAll(_messages.Prefixed(
            "knife.winner", name, tally.TAlive, tally.CtAlive, tally.THealth, tally.CtHealth));
        CloseSideChoice();
        // The round's end plays out; warmup starts a second before the engine's own restart would.
        var delay = Math.Max(0, (_host.GetConVarNumber("mp_round_restart_delay") ?? 1) - 1);
        _sideChoiceTimer.Schedule(delay, () =>
        {
            if (Ending || Phase != MatchPhase.SideChoice) return;
            EnterSideChoiceWarmup();
        });
    }

    /// <summary>
    /// Back to warmup while the winners choose, its clock counting down their decision. The plugin's
    /// timer stays on the sides a second before the clock runs out, so the plugin ends warmup itself,
    /// after the live cfgs, rather than the engine ending it on the warmup cfgs.
    /// </summary>
    private void EnterSideChoiceWarmup()
    {
        Exec(OverModeCfg(CfgNames.Base, CfgNames.Launch, CfgNames.Warmup));
        AfterConfig(() =>
        {
            if (Phase != MatchPhase.SideChoice) return;
            _host.ExecuteCommand($"mp_warmuptime {Config.knifeDecisionSeconds}");
            _host.ExecuteCommand("mp_warmup_pausetimer 0");
            _host.ExecuteCommand("mp_warmup_start");
            OpenSideChoice();
        });
    }

    private void OpenSideChoice()
    {
        var captain = CaptainDisplayName(KnifeWinnerTeam!.Value);
        _host.PrintToChatAll(captain is null
            ? _messages.Prefixed("knife.side_prompt_no_captain",
                ChatCommands.Stay, ChatCommands.Swap, ChatCommands.Ct, ChatCommands.T,
                Config.knifeDecisionSeconds)
            : _messages.Prefixed("knife.side_prompt", captain,
                ChatCommands.Stay, ChatCommands.Swap, ChatCommands.Ct, ChatCommands.T,
                Config.knifeDecisionSeconds));

        // A second before the engine's warmup clock would end the warmup on its own.
        _sideChoiceTimer.Schedule(Math.Max(1, Config.knifeDecisionSeconds - 1), () =>
        {
            if (Ending || Phase != MatchPhase.SideChoice) return;
            _host.PrintToChatAll(_messages.Prefixed("knife.no_decision", Roster.TeamName(KnifeWinnerTeam!.Value)));
            ApplySideChoice(SideChoiceKind.Stay);
        });
    }

    /// <summary>Accept stay/swap only from the knife-winning team's captain.</summary>
    public bool TrySideChoice(HostPlayer caller, SideChoiceKind choice)
    {
        // Only once the knife round is over and its warmup runs, so the clock players see is the choice's.
        if (Phase != MatchPhase.SideChoice || !_host.IsWarmup || !_sideChoiceTimer.IsPending || KnifeWinnerSide is null) return false;

        var callerTeam = Roster.LobbyTeam(caller.SteamIdString);
        if (callerTeam is null || callerTeam != KnifeWinnerTeam)
        {
            return false;
        }

        if (!Roster.IsCaptain(caller.SteamIdString, callerTeam.Value))
        {
            var captain = CaptainDisplayName(callerTeam.Value);
            _host.PrintToChat(caller.Slot, captain is null
                ? _messages.Prefixed("knife.not_captain")
                : _messages.Prefixed("knife.not_captain_named", captain));
            return false;
        }

        _host.PrintToChatAll(choice == SideChoiceKind.Stay
            ? _messages.Prefixed("knife.chose_stay", caller.Name)
            : _messages.Prefixed("knife.chose_swap", caller.Name));
        ApplySideChoice(choice);
        return true;
    }

    public bool TrySideChoiceForSide(HostPlayer caller, Side wanted)
    {
        if (KnifeWinnerSide is null) return false;
        if (wanted == Side.Spec) return false;
        return TrySideChoice(caller, wanted == KnifeWinnerSide ? SideChoiceKind.Stay : SideChoiceKind.Swap);
    }

    private void ApplySideChoice(SideChoiceKind choice)
    {
        CloseSideChoice();

        var team = KnifeWinnerTeam!.Value;
        if (choice == SideChoiceKind.Swap)
        {
            Team1Side = Team1Side.Opposite();
        }

        Emit(new SideChoice { team = team, choice = choice, team1Side = Team1Side });
        GoLive(swapTeams: choice == SideChoiceKind.Swap);
    }

    public void GoLive(bool swapTeams = false)
    {
        if (Ending) return;
        // The side choice is over: these are the first half's sides.
        _firstHalfTeam1Side = Team1Side;
        _startTimer.Cancel();
        _goingLive = true;
        _liveAnnounced = false;
        _liveRestartIssued = false;
        _swapRestartPending = false;
        _round = 0;
        SidesSwapPending = false;
        SetScore(0, 0);
        CloseSideChoice();

        ExecLiveThenRecord();
        // The plugin ends warmup itself: the game mode's cfg sets its own mp_warmuptime, which would
        // stretch a running warmup clock instead of letting it run out.
        RestartIntoLive(swapTeams);
    }

    private void RestartIntoLive(bool swapTeams)
    {
        var revision = ++_restartRevision;
        var phase = Phase;
        // The phase's cfgs have to have run before the restart starts the phase on them.
        AfterConfig(() =>
        {
            if (revision != _restartRevision || Phase != phase) return;
            _liveRestartIssued = true;
            if (swapTeams)
            {
                _swapRestartPending = true;
                _host.ExecuteCommand("mp_swapteams");
            }
            _host.ExecuteCommand(_host.IsWarmup ? "mp_warmup_end" : "mp_restartgame 1");
        });
    }

    /// <summary>Runs the action once the cfgs queued so far have run, unless the match has moved on to another phase or map meanwhile.</summary>
    private void AfterConfig(Action action)
    {
        var revision = _restartRevision;
        _host.AfterCommands(() =>
        {
            if (revision != _restartRevision) return;
            action();
        });
    }

    private void AnnounceLive()
    {
        if (_liveAnnounced) return;
        _liveAnnounced = true;

        for (var i = 0; i < 3; i++)
        {
            _host.PrintToChatAll(_messages.Prefixed("match.live"));
        }

        var rules = ReadMatchRules();
        Emit(new GoingLive
        {
            team1Side = Team1Side,
            maxRounds = rules.maxRounds,
            overtime = rules.overtime,
            overtimeMaxRounds = rules.overtimeMaxRounds,
            friendlyFire = rules.friendlyFire,
        });
    }

    /// <summary>
    /// The round settings from the cvars, not the launch's rules: they are what the server actually
    /// runs, whatever set them. Friendly fire is the launch's, which sets damage reductions the
    /// engine cannot be asked about as one switch.
    /// </summary>
    public MatchRules ReadMatchRules()
    {
        var format = RoundFormat.Read(_host);
        return new()
        {
            maxRounds = format.MaxRounds,
            overtime = format.OvertimeEnabled,
            overtimeMaxRounds = format.OvertimeRounds,
            friendlyFire = Config.PlayedRules.friendlyFire,
        };
    }


    /// <summary>The engine has moved players to the sides <see cref="SwapSides"/> named.</summary>
    public void SidesSwapped() => SidesSwapPending = false;

    public void OnRoundStart()
    {
        // The round reset that started this round moved every player, whether or not anyone was on.
        SidesSwapPending = false;
        BetweenRounds = false;
        if (Ending) return;
        if (_readyExpired && !_host.IsWarmup)
        {
            RaiseReadyExpired();
            return;
        }
        if (_swapRestartPending && !_host.IsWarmup) _swapRestartPending = false;

        if (!_host.IsWarmup) _startTimer.Cancel();
        _round = Math.Max(1, _host.RoundsPlayed + 1);
        if (Phase == MatchPhase.Knife && !_host.IsWarmup && !_knifeAnnounced)
        {
            _knifeAnnounced = true;
            _host.PrintToChatAll(_messages.Prefixed("knife.start"));
            Emit(new KnifeStart());
        }
        if (Phase == MatchPhase.PendingRestore && !_host.IsWarmup) OnRestoreRoundStart();
        // mp_swapteams schedules mp_restartgame 1, so after a swap out of warmup the round mp_warmup_end
        // starts is restarted a moment later; the round that follows the restart is the live one.
        else if (_goingLive && _liveRestartIssued && !_host.IsWarmup && !RestartScheduled)
        {
            Phase = MatchPhase.Live;
            // mp_swapteams swaps the team names along with the teams, so they are set once its restart has run.
            ApplyTeamNames();
            AnnounceLive();
        }
        if (IsLive && !_host.IsWarmup) _restoreSideByRound[Round] = Team1Side;
    }


    /// <summary>Record a live round result and return the updated score.</summary>
    public (int Team1, int Team2) RecordRoundResult(Side winnerSide)
    {
        var counted = Counted(winnerSide);
        SetScore(counted.Team1, counted.Team2);
        return counted;
    }

    /// <summary>The score with a live round's point added to the winner's team.</summary>
    private (int Team1, int Team2) Counted(Side winnerSide)
    {
        if (Phase != MatchPhase.Live) return (Team1Score, Team2Score);
        var team = RosterEnforcer.TeamForSide(winnerSide, Team1Side);
        return (Team1Score + (team == 1 ? 1 : 0), Team2Score + (team == 2 ? 1 : 0));
    }

    /// <summary>Report the score whenever it changes.</summary>
    private void SetScore(int team1, int team2)
    {
        if ((team1, team2) == (Team1Score, Team2Score)) return;
        (Team1Score, Team2Score) = (team1, team2);
        _output.ReportMatch(new PluginScore { team1Score = team1, team2Score = team2 });
    }

    /// <summary>
    /// Score a live round from the engine's scoreboard, which has counted the round by the time
    /// round_end fires. The plugin's own count stands only when the scoreboard cannot be read.
    /// </summary>
    public (int Team1, int Team2) ScoreRound(Side winnerSide)
    {
        var counted = Counted(winnerSide);
        if (counted == (Team1Score, Team2Score)) return counted;

        if (_host.ReadTeamScore(Team1Side) is not { } team1 || _host.ReadTeamScore(Team1Side.Opposite()) is not { } team2)
        {
            LogEvent(LogLevel.Warn, $"round {Round}: the scoreboard could not be read, so the score is the plugin's count {counted.Team1}-{counted.Team2}");
            SetScore(counted.Team1, counted.Team2);
            return counted;
        }

        if ((team1, team2) != counted)
            LogEvent(LogLevel.Warn, $"round {Round}: the scoreboard reads {team1}-{team2}, the plugin counted {counted.Team1}-{counted.Team2}; the scoreboard stands");

        SetScore(team1, team2);
        return (team1, team2);
    }

    /// <summary>Mirror a halftime side swap.</summary>
    public void SwapSides()
    {
        Team1Side = Team1Side.Opposite();
        SidesSwapPending = true;
        LogEvent(LogLevel.Info, $"halftime: team 1 is now {Team1Side}");
    }

    /// <summary>
    /// <c>mp_teamname_1</c> names the team that plays CT in the first half and <c>mp_teamname_2</c>
    /// the team on T then. The engine shows each name on its team's side in every half, swapping
    /// them at halftime and after a backup load by the loaded round, so they are set once, going
    /// live, and never rewritten.
    /// </summary>
    private void ApplyTeamNames()
    {
        if (Phase == MatchPhase.PostLive) return;
        _host.SetConVarString("mp_teamname_1", _firstHalfTeam1Side == Side.Ct ? Roster.Team1Name : Roster.Team2Name);
        _host.SetConVarString("mp_teamname_2", _firstHalfTeam1Side == Side.T ? Roster.Team1Name : Roster.Team2Name);
    }

    /// <summary>Team 1's side in the first half, from its side in <paramref name="round"/>: one flip per halftime before it.</summary>
    private Side FirstHalfSide(int round, Side sideInRound)
    {
        var format = RoundFormat.Read(_host);
        var switches = Enumerable.Range(1, Math.Max(0, round - 1)).Count(format.IsHalftimeBoundary);
        return switches % 2 == 0 ? sideInRound : sideInRound.Opposite();
    }

    /// <summary>Stop phase work as soon as ending begins; gameplay finalization may wait for damage.</summary>
    public void BeginEnding()
    {
        Ending = true;
        CancelPhaseWork();
    }

    /// <summary>Reset map-scoped work before the deferred warmup initialization.</summary>
    public void OnMapLoad()
    {
        CancelPhaseWork();
        SidesSwapPending = false;
        if (!Ending) Phase = MatchPhase.Loading;
    }

    private void CancelPhaseWork()
    {
        ++_restartRevision;
        _startTimer.Cancel();
        _reminderTimer.Cancel();
        _deadlineTimer.Cancel();
        _restoreTimer.Cancel();
        // A map change or the match's end leaves no load to confirm.
        _engineLoadingBackup = false;
        _armedRestoreSide = null;
        _beforeRestore = null;
        CloseSideChoice();
    }

    public void EnterPostLive()
    {
        if (Phase == MatchPhase.PostLive) return;
        BeginEnding();
        Phase = MatchPhase.PostLive;
    }

    public bool IsFinalRound() => RoundFormat.Read(_host).IsFinalRound(Team1Score, Team2Score);

    /// <summary>
    /// Whether the round that just ended (already counted in the scores) is the last one before
    /// the engine switches sides. The plugin's own count: the engine's TotalRoundsPlayed may or may
    /// not include the ending round inside a round_end pre-hook.
    /// </summary>
    public bool IsHalftimeBoundary() => RoundFormat.Read(_host).IsHalftimeBoundary(Team1Score + Team2Score);

    public void Dispose()
    {
        EnterPostLive();
        _startTimer.Dispose();
        _reminderTimer.Dispose();
        _deadlineTimer.Dispose();
        _restoreTimer.Dispose();
        _sideChoiceTimer.Dispose();
    }

    /// <summary>Why the map ends, as <see cref="MapEndReasons"/> names it: played out unless the ready deadline or departures ended it.</summary>
    public string EndReason { get; private set; } = MapEndReasons.Played;

    /// <summary>Who was on the server and ready when the ready deadline passed; null for any other end.</summary>
    public ReadyTeams? ReadyAtDeadline { get; private set; }

    /// <summary>The score leader of a played map. Any other end has no winner; the controller decides.</summary>
    public int? WinnerTeam => EndReason != MapEndReasons.Played || Team1Score == Team2Score
        ? null
        : Team1Score > Team2Score ? 1 : 2;

    public void CloseSideChoice() => _sideChoiceTimer.Cancel();

    /// <summary>Execs <paramref name="cfgs"/> as one console line, so nothing queued meanwhile runs between them.</summary>
    private void Exec(params string[] cfgs)
    {
        var line = CfgNames.Exec(cfgs);
        _host.Log(LogLevel.Debug, $"cfg: {line}");
        _host.ExecuteCommand(line);
    }

    /// <summary>
    /// The live format, whatever warmup and the knife round changed: the game mode's own cfg, then
    /// Ankor's and the launch's over it. Before recording starts, as the launch's GOTV delay must not
    /// change while a demo records.
    /// </summary>
    private void ExecLive() => Exec(OverModeCfg(CfgNames.Base, CfgNames.Launch));

    /// <summary>
    /// The reset and the game mode's cfg ahead of <paramref name="cfgs"/>. The reset puts what
    /// warmup and the knife round changed back to the engine's defaults; the mode's cfg then sets
    /// its own values over them, such as casual's <c>mp_round_restart_delay 10</c>, and puts
    /// back what only it sets, such as the default weapons.
    /// </summary>
    private string[] OverModeCfg(params string[] cfgs)
    {
        var gameType = _host.GetConVarInt("game_type");
        var gameMode = _host.GetConVarInt("game_mode");
        if (gameType is { } type && gameMode is { } mode && CfgNames.Mode(type, mode) is { } modeCfg)
            return [CfgNames.Reset, modeCfg, .. cfgs];

        LogEvent(LogLevel.Error, $"game_type {gameType} and game_mode {gameMode} name no mode cfg the plugin knows; running Ankor's and the launch's cfgs alone");
        return [CfgNames.Reset, .. cfgs];
    }
}
