using System.Reflection;
using Microsoft.Extensions.Logging;
using Ankor.Core;
using Ankor.Core.Wire;
using SwiftlyS2.Shared;
using SwiftlyS2.Shared.Commands;
using SwiftlyS2.Shared.Players;
using SwiftlyS2.Shared.ProtobufDefinitions;
using SwiftlyS2.Shared.SchemaDefinitions;
using SwiftlyS2.Shared.SteamAPI;
using CoreSide = Ankor.Core.Wire.Side;
using CoreLogLevel = Ankor.Core.Wire.LogLevel;
using MsLogLevel = Microsoft.Extensions.Logging.LogLevel;
using SwiftlyTeam = SwiftlyS2.Shared.Players.Team;

namespace Ankor.GameHost;

/// <summary>SwiftlyS2 adapter. Access engine state on the game thread; NextFrame marshals external callbacks.</summary>
public sealed class SwiftlyGameHost(ISwiftlyCore core) : IGameHost
{
    public EngineCapture? Capture { get; set; }
    public bool DiagnosticsEnabled { get; set; } = true;

    /// <summary>Cancel the framework timer when the core disposes its handle.</summary>
    private sealed class SwiftlyTimer(CancellationTokenSource cts) : IHostTimer
    {
        public bool IsCancellationRequested => cts.IsCancellationRequested;
        public void Cancel() => cts.Cancel();
    }

    public long NowMs => DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

    /// <summary>Map tick with its sub-tick fraction, or zero while engine globals are unavailable, as on the map-load tick.</summary>
    public double SimulationTick
    {
        get
        {
            try
            {
                ref var globals = ref core.Engine.GlobalVars;
                return (double)globals.TickCount + globals.SubtickFraction;
            }
            catch (Exception)
            {
                return 0;
            }
        }
    }
    public double RoundElapsedTime
    {
        get
        {
            var rules = GameRules;
            if (rules is null || rules.FreezePeriod) return 0;
            // CS2 sets this reference to freeze end (a future time during freeze).
            return Math.Max(0, CurrentTime - rules.RoundStartTime.Value);
        }
    }

    /// <summary>Map time in seconds, or zero while engine globals are unavailable.</summary>
    public double CurrentTime
    {
        get
        {
            try
            {
                return core.Engine.GlobalVars.CurrentTime;
            }
            catch (Exception)
            {
                return 0;
            }
        }
    }

    public string MapName
    {
        get
        {
            try
            {
                return core.Engine.GlobalVars.MapName.Value;
            }
            catch (Exception)
            {
                return "";
            }
        }
    }

    public string? ServerSteamId
    {
        get
        {
            try
            {
                var id = SteamGameServer.GetSteamID();
                // An anonymous logon has no token, so its SteamID names nothing a controller can check.
                return id.BPersistentGameServerAccount() ? id.m_SteamID.ToString(System.Globalization.CultureInfo.InvariantCulture) : null;
            }
            catch (Exception)
            {
                return null;
            }
        }
    }

    public bool SteamLoggedOn
    {
        get
        {
            try
            {
                return SteamGameServer.BLoggedOn();
            }
            catch (Exception)
            {
                return false;
            }
        }
    }

    /// <summary>Read the version convar, falling back to unknown.</summary>
    public string ServerVersion =>
        ConVarString("sv_version")
        ?? ConVarString("host_version")
        ?? "unknown";

    /// <summary>Report the loaded SDK assembly’s informational version.</summary>
    public string FrameworkVersion { get; } = $"SwiftlyS2 {SwiftlyAssemblyVersion()}";

    private static string SwiftlyAssemblyVersion()
    {
        var assembly = typeof(ISwiftlyCore).Assembly;
        var informational = assembly
            .GetCustomAttribute<AssemblyInformationalVersionAttribute>()?.InformationalVersion;

        if (!string.IsNullOrWhiteSpace(informational))
        {
            var plus = informational.IndexOf('+');
            return plus > 0 ? informational[..plus] : informational;
        }

        return assembly.GetName().Version?.ToString() ?? "unknown";
    }

    /// <summary>Include bots in match snapshots; IsReportable filters them at the wire boundary.</summary>
    public IReadOnlyList<HostPlayer> Players
    {
        get
        {
            var players = new List<HostPlayer>();
            foreach (var player in core.PlayerManager.GetAllPlayers())
            {
                var snapshot = Snapshot(player);
                if (snapshot is not null) players.Add(snapshot);
            }

            return players;
        }
    }

    public HostPlayer? ReadPlayerOrKick(int slot, string reason) =>
        ReadPlayerOrKick(core.PlayerManager.GetPlayer(slot), reason);

    public IReadOnlyList<HostPlayer> ReadPlayersOrKick(string reason) =>
        core.PlayerManager.GetAllPlayers().ToArray()
            .Select(player => ReadPlayerOrKick(player, reason))
            .OfType<HostPlayer>().ToArray();

    private HostPlayer? ReadPlayerOrKick(IPlayer? client, string reason)
    {
        if (client is null) return null;
        try
        {
            // GOTV is deliberately absent from snapshots, not a failed roster read.
            if (client.Controller is { IsValid: true, IsHLTV: true }) return null;
            var snapshot = Snapshot(client);
            if (snapshot is not null) return snapshot;
        }
        catch (Exception)
        {
            // Identity reads may fail too; disconnect the same native connection below.
        }
        Log(CoreLogLevel.Warn, $"Cannot read player in slot {client.Slot} for roster enforcement; disconnecting");
        client.Kick(reason, ENetworkDisconnectionReason.NETWORK_DISCONNECT_KICKED);
        return null;
    }

    /// <summary>
    /// Full inventories are read at round boundaries. Grenade quantities live in ammo slots (two
    /// flashbangs share one weapon entity); gun ammunition is not a count of weapons. An
    /// unreadable item invalidates the snapshot so partial gear cannot look like a poor buy.
    /// </summary>
    public Ankor.Core.Wire.PlayerInventory? ReadInventory(HostPlayer player)
    {
        try
        {
            var live = core.PlayerManager.GetPlayer(player.Slot);
            if (live is null || live.UnauthorizedSteamID != player.SteamId || live.PlayerPawn is not { IsValid: true } pawn
                || pawn.ItemServices is not { } itemServices)
                return null;
            var items = ReadInventoryItems(pawn);
            if (items is null) return null;
            return new()
            {
                items = items,
                armor = Math.Max(0, pawn.ArmorValue),
                helmet = itemServices.HasHelmet,
                defuseKit = itemServices.HasDefuser,
            };
        }
        catch (Exception ex)
        {
            Log(CoreLogLevel.Warn, $"inventory snapshot unavailable: {ex.Message}");
            return null;
        }
    }

    /// <summary>
    /// Shared by round inventories and pre-damage utility snapshots. Filtering uses the native
    /// gear slot, not an item-id list: C4, knives and tasers do not count as unspent utility.
    /// Null signals an incomplete read, never an empty inventory.
    /// </summary>
    public static IReadOnlyList<Ankor.Core.Wire.InventoryItem>? ReadInventoryItems(CCSPlayerPawn? pawn, bool grenadesOnly = false)
    {
        try
        {
            if (pawn?.WeaponServices is not { } services) return null;
            var items = new List<Ankor.Core.Wire.InventoryItem>();
            var ammo = services.Ammo;
            foreach (var weapon in services.MyValidWeapons)
            {
                if (weapon is not { IsValid: true }) return null;
                var data = weapon.As<CCSWeaponBase>().WeaponBaseVData;
                if (data is null) return null;
                var grenade = data.GearSlot == gear_slot_t.GEAR_SLOT_GRENADES;
                if (grenadesOnly && !grenade) continue;
                var id = (int)weapon.AttributeManager.Item.ItemDefinitionIndex;
                if (id <= 0) return null;
                var quantity = 1;
                if (grenade)
                {
                    int index = data.PrimaryAmmoType.Value;
                    if (index >= 0 && index < ammo.ElementCount) quantity = Math.Max(1, (int)ammo[index]);
                }
                items.Add(new() { weapon = id, quantity = quantity, unitValue = Math.Max(0, data.Price) });
            }
            return items.OrderBy(item => item.weapon).ToArray();
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// <summary>Check the controller, not IPlayer.IsValid: authorization and spectators may have no live pawn.
    /// Exclude GOTV explicitly.
    /// </summary>
    public static HostPlayer? Snapshot(IPlayer? player)
    {
        if (player is null) return null;

        try
        {
            // `Controller` is the nullable accessor (`RequiredController` is the throwing one).
            if (player.Controller is not { IsValid: true } controller || controller.IsHLTV) return null;

            var isBot = player.IsFakeClient;

            // Strict SteamAuth keeps SteamID at 0 until Steam verifies the player. UnauthorizedSteamID
            // is the id the client connected with, which Steam verifies, so a player who loads faster
            // than Steam answers is placed by it; TeamLock kicks the player if Steam has not verified
            // it in time. The slot guards below compare against the same id.
            var authorized = player.IsAuthorized;
            var steamId = isBot ? 0UL : player.UnauthorizedSteamID;

            // Controller PawnHealth/PawnIsAlive are cached and can lag the killing damage.
            var pawn = player.PlayerPawn is { IsValid: true } valid ? valid : null;
            var health = pawn is null ? 0 : Math.Max(0, pawn.Health);
            var alive = pawn is not null && health > 0 && pawn.LifeState == 0;

            WorldPosition? position = null;
            ViewAngles? angles = null;
            if (pawn is not null)
            {
                if (pawn.AbsOrigin is { } origin)
                    position = new WorldPosition(origin.X, origin.Y, origin.Z);
                var eye = pawn.EyeAngles;
                angles = new ViewAngles(eye.Pitch, eye.Yaw);
            }

            var money = controller.InGameMoneyServices;

            return new HostPlayer(
                Slot: player.Slot,
                SteamId: steamId,
                Name: controller.PlayerName ?? "",
                Side: Core.Wire.SideExtensions.FromTeamNum(controller.TeamNum),
                Health: health,
                IsAlive: alive,
                IsBot: isBot)
            {
                Authorized = authorized,
                UserId = player.UserID,
                Position = position,
                Angles = angles,
                EquipmentValue = pawn is null ? 0 : (int)pawn.CurrentEquipmentValue,
                InBuyZone = pawn is not null && pawn.InBuyZone,
                Cash = money?.Account ?? 0,
                StartCash = money?.StartAccount ?? 0,
                CashSpent = money?.CashSpentThisRound ?? 0,
            };
        }
        catch (Exception)
        {
            return null;
        }
    }

    public static SwiftlyTeam TeamOf(CoreSide side) => side switch
    {
        CoreSide.T => SwiftlyTeam.T,
        CoreSide.Ct => SwiftlyTeam.CT,
        _ => SwiftlyTeam.Spectator,
    };

    public void ExecuteCommand(string command)
    {
        // MatchShutdown.Drain sends this "quit". A quit does not unload the plugin, so a running
        // capture is closed first, or the replay reader rejects it for its missing capture_end.
        if (command == "quit" && Capture is { } capture)
            capture.CompleteBefore(() => core.Engine.ExecuteCommand(command));
        else core.Engine.ExecuteCommand(command);
    }

    private AfterCommandsQueue? _afterCommands;
    private Guid? _appliedCommand;

    private AfterCommandsQueue CommandQueue => _afterCommands ??= new(
        command => core.Engine.ExecuteCommand(command),
        (seconds, callback) => AddTimer(seconds, callback),
        id => core.Logger.LogDebug("{Command} {Id}: the cfgs queued before it have run", CfgNames.AppliedCommand, id),
        id => core.Logger.LogWarning("{Command} {Id} did not run within {Seconds} s; going on without it", CfgNames.AppliedCommand, id, AfterCommandsQueue.TimeoutSeconds));

    public void AfterCommands(Action action)
    {
        _appliedCommand ??= core.Command.RegisterCommand(CfgNames.AppliedCommand, OnCommandsApplied,
            registerRaw: true, permission: "", helpText: "Ankor: the commands queued before this one have run");
        CommandQueue.Add(action);
    }

    private void OnCommandsApplied(ICommandContext context)
    {
        // Only the server's own buffer runs it; a player typing it changes nothing.
        if (context.IsSentByPlayer) return;
        CommandQueue.Applied();
    }

    /// <summary>
    /// Drops what waited for the old map's cfgs. A map change can drop a queued marker, and without
    /// this every later wait would settle one marker behind.
    /// </summary>
    public void DropPendingCommands() => _afterCommands?.Clear();

    /// <summary>Removes <see cref="CfgNames.AppliedCommand"/> when the plugin unloads.</summary>
    public void UnregisterCommands()
    {
        if (_appliedCommand is { } command) core.Command.UnregisterCommand(command);
        _appliedCommand = null;
        _afterCommands?.Clear();
    }

    /// <summary>Schedule an Action on the next game tick; do not select the async-lambda overload.</summary>
    public void NextFrame(Action action) => core.Scheduler.NextTick(() =>
    {
        using var capture = Capture?.Begin("scheduler.next_tick");
        action();
    });

    public void PrintToChatAll(string message) => core.PlayerManager.SendChat(message);

    public void PrintToChat(int slot, string message)
    {
        var player = core.PlayerManager.GetPlayer(slot);
        if (player is null) return;

        try
        {
            player.SendChat(message);
        }
        catch (Exception ex)
        {
            Log(CoreLogLevel.Debug, $"could not chat to slot {slot}: {ex.Message}");
        }
    }

    public int? ReadTeamScore(CoreSide side)
    {
        var teamNum = (int)TeamOf(side);
        try
        {
            foreach (var team in core.EntitySystem.GetAllEntitiesByDesignerName<CCSTeam>("cs_team_manager"))
            {
                if (team.TeamNum == teamNum) return team.Score;
            }
        }
        catch (Exception ex)
        {
            Log(CoreLogLevel.Warn, $"could not read the {side} score: {ex.Message}");
        }
        return null;
    }

    public CCSGameRules? GameRules
    {
        get
        {
            // SwiftlyS2 throws instead of returning null while the entity system is down: map load
            // (EventForwarder.Reset), plugin unload (Dispose) and possibly map end. Those callers only
            // need a round time of 0 there. A caller that must tell "no rules yet" from a real value
            // needs its own check rather than this null.
            try
            {
                return core.EntitySystem.GetGameRules();
            }
            catch (InvalidOperationException)
            {
                return null;
            }
        }
    }

    public bool IsWarmup => GameRules?.WarmupPeriod ?? false;

    public int RoundsPlayed => GameRules?.TotalRoundsPlayed ?? 0;

    public bool LoadingRoundBackup => GameRules?.LoadingRoundBackupData ?? false;

    /// <summary>Defer team moves to the next tick.</summary>
    public void SwitchTeam(HostPlayer player, CoreSide side)
    {
        var slot = player.Slot;
        var target = TeamOf(side);

        NextFrame(() =>
        {
            var live = core.PlayerManager.GetPlayer(slot);
            // A disconnected player's slot may belong to someone else by the next tick.
            if (live is null || live.UnauthorizedSteamID != player.SteamId) return;

            var controller = live.Controller;
            if (!controller.IsValid) return;
            if ((SwiftlyTeam)controller.TeamNum == target) return;

            live.SwitchTeam(target);

            // Respawn moved players only during warmup.
            if (IsWarmup && !controller.PawnIsAlive) live.Respawn();
        });
    }

    public void ChangeTeam(HostPlayer player, CoreSide side)
    {
        var slot = player.Slot;
        var target = TeamOf(side);

        NextFrame(() =>
        {
            var live = core.PlayerManager.GetPlayer(slot);
            if (live is null || live.UnauthorizedSteamID != player.SteamId || !live.Controller.IsValid) return;
            live.ChangeTeam(target);
        });
    }

    /// <summary>Use kickid so roster rejection can act during connection.</summary>
    public void Kick(HostPlayer player, string reason)
    {
        var client = core.PlayerManager.GetPlayer(player.Slot);
        if (client is null) return;
        Log(CoreLogLevel.Info, $"kicking slot {player.Slot} ({player.SteamIdString}): {reason}");
        client.Kick(reason, ENetworkDisconnectionReason.NETWORK_DISCONNECT_KICKED);
    }

    /// <summary>Holds the aborted match's intermission past the 10 s the engine gives it, so the plugin quits the server first.</summary>
    private const float IntermissionHoldSeconds = 86400;

    public void AbortMatch()
    {
        core.Game.GoToIntermission(abortedMatch: true);
        // An aborted match restarts after 10 s, whatever mp_match_restart_delay says; the server
        // quits first, as after a played match.
        if (GameRules is { } rules) rules.IntermissionEndTime.Value = (float)CurrentTime + IntermissionHoldSeconds;
        // GoToIntermission ends the match but fires no win panel, which the client draws the end screen from.
        core.GameEvent.Fire<SwiftlyS2.Shared.GameEventDefinitions.EventCsWinPanelMatch>();
    }

    public void SetClanTag(HostPlayer player, string tag)
    {
        var live = core.PlayerManager.GetPlayer(player.Slot);
        if (live is null || live.UnauthorizedSteamID != player.SteamId || live.Controller is not { IsValid: true } controller) return;

        try
        {
            controller.Clan = tag;
            controller.ClanUpdated();
        }
        catch (Exception ex)
        {
            Log(CoreLogLevel.Debug, $"could not set clan tag for slot {player.Slot}: {ex.Message}");
        }
    }

    /// <summary>Apply the timer lifetime requested by the core; Swiftly cancels all timers on unload.</summary>
    /// <summary>
    /// The shortest wait SwiftlyS2 schedules. Its timer steps throw on a wait under one millisecond,
    /// inside the scheduler, which drops the timer without running it; this wait runs on the next tick.
    /// </summary>
    private const double MinimumWaitSeconds = 0.001;

    public IHostTimer AddTimer(double seconds, Action callback, bool stopOnMapChange = true)
    {
        var cts = core.Scheduler.DelayBySeconds((float)Math.Max(seconds, MinimumWaitSeconds), () =>
        {
            using var capture = Capture?.Begin("scheduler.timer");
            callback();
        });
        if (stopOnMapChange) core.Scheduler.StopOnMapChange(cts);
        return new SwiftlyTimer(cts);
    }

    public IHostTimer AddRepeatingTimer(double delaySeconds, double periodSeconds, Action callback, bool stopOnMapChange = true)
    {
        var cts = core.Scheduler.DelayAndRepeatBySeconds((float)Math.Max(delaySeconds, MinimumWaitSeconds),
            (float)Math.Max(periodSeconds, MinimumWaitSeconds), () =>
        {
            using var capture = Capture?.Begin("scheduler.timer");
            callback();
        });
        if (stopOnMapChange) core.Scheduler.StopOnMapChange(cts);
        return new SwiftlyTimer(cts);
    }

    /// <summary>Read as text to avoid generic convar lookup type mismatches.</summary>
    public void SetConVarString(string name, string value)
    {
        var convar = core.ConVar.FindAsString(name);
        if (convar is null)
        {
            Log(CoreLogLevel.Warn, $"cannot set missing convar {name}");
            return;
        }
        convar.ValueAsString = value;
    }

    public string? GetConVarString(string name) => ConVarString(name);

    private string? ConVarString(string name)
    {
        try
        {
            return core.ConVar.FindAsString(name)?.ValueAsString;
        }
        catch (Exception ex)
        {
            Log(CoreLogLevel.Warn, $"could not read convar {name}: {ex.Message}");
            return null;
        }
    }

    public int? GetConVarInt(string name) => ConVarText.Int(ConVarString(name));

    public double? GetConVarNumber(string name) => ConVarText.Number(ConVarString(name));

    public bool? GetConVarBool(string name) => ConVarText.Bool(ConVarString(name));

    public void Log(CoreLogLevel level, string message)
    {
        if (!DiagnosticsEnabled) return;
        core.Logger.Log(level switch
        {
            CoreLogLevel.Debug => MsLogLevel.Debug,
            CoreLogLevel.Info => MsLogLevel.Information,
            CoreLogLevel.Warn => MsLogLevel.Warning,
            _ => MsLogLevel.Error,
        }, "{Message}", message);
    }
}
