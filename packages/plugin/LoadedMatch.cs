using Ankor.Diagnostics;
using Microsoft.Extensions.Logging;
using Ankor.Core;
using Ankor.Core.Wire;
using Ankor.GameHost;
using SwiftlyS2.Shared;
using SwiftlyS2.Shared.Commands;
using SwiftlyS2.Shared.GameEventDefinitions;
using SwiftlyS2.Shared.GameHooks;
using SwiftlyS2.Shared.Misc;
using SwiftlyS2.Shared.Players;
using SwiftlyS2.Shared.Plugins;
using SwiftlyS2.Shared.SchemaDefinitions;
using CoreSide = Ankor.Core.Wire.Side;
using CoreLogLevel = Ankor.Core.Wire.LogLevel;

namespace Ankor;

/// <summary>Owns the loaded match configuration, native hooks and match-only resources.</summary>
internal sealed class LoadedMatch : PluginRuntime.IMatch
{
    private const string Version = AnkorPlugin.Version;
    private ISwiftlyCore Core { get; }
    private ILogger Logger => Core.Logger;
    public MatchState State => _match;
    public EngineCapture Capture => _capture;
    public IDisposable? BeginCapture(string name) => _capture.Begin(name);

    private readonly SwiftlyGameHost _host;
    private readonly MatchSession _session;
    private readonly MatchState _match;
    private readonly EventForwarder _forwarder;
    private readonly TeamLock _teamLock;
    private readonly IMessages _messages;
    private readonly GameplayCallbacks _callbacks;
    private readonly MatchShutdown _shutdown;
    private readonly DemoRecorder _recorder;
    private readonly RoundBackups _backups;

    private readonly List<Guid> _gameEventHooks = [];
    private readonly List<Guid> _commands = [];
    private Guid? _clientCommandHook;

    private readonly GrenadeAttribution _grenadeAttribution = new();
    private readonly DamageTracker _damage;
    private EngineCapture _capture = null!;

    public LoadedMatch(ISwiftlyCore core, SwiftlyGameHost host, MatchConfig config,
        IAgentChannel agent, PluginSettings settings)
    {
        Core = core;
        _host = host;
        _messages = LoadMessages(settings.Chat);
        var collectEvents = config.sendStatistics;
        var captures = collectEvents && config.debugCapture;

        // No proxy overhead when diagnostics are disabled; native reads remain in the adapter.
        var coreHost = !captures || string.IsNullOrWhiteSpace(Environment.GetEnvironmentVariable(AnkorEnv.DebugCaptureDir))
            ? (IGameHost)_host
            : GameplayRecording.Wrap(_host, (name, value) => _capture?.Observe(name, value),
                reason => _capture?.Stop(reason));
        // The debug capture starts where the match's wiring reaches the shutdown, and its plugin.load
        // scope lasts until this constructor ends, as it did before the wiring moved to core.
        IDisposable? capture = null;
        try
        {
            _session = new MatchSession(_host, coreHost, config, MatchFiles.Server, new CapturedOutput(agent, this), _messages, ClearGrenadeTracking,
                beforeShutdown: match =>
                {
                    _capture = new EngineCapture(Core, new { pluginVersion = Version, frameworkVersion = _host.FrameworkVersion,
                        config = match.Config }, enabled: captures);
                    _host.Capture = _capture;
                    capture = _capture.Begin("plugin.load");
                });
            _match = _session.Match;
            _forwarder = _session.Forwarder;
            _damage = _session.Damage;
            _teamLock = _session.TeamLock;
            _backups = _session.Backups;
            _shutdown = _session.Shutdown;
            _callbacks = _session.Callbacks;
            _recorder = _session.Recorder;
            RegisterHooks();
            if (collectEvents)
            {
                Core.GameHooks.Entities.TakeDamage.Pre += OnDamagePre;
                Core.GameHooks.Entities.TakeDamage.Post += OnDamagePost;
                Core.Event.OnEntitySpawned += OnGrenadeEntitySpawned;
                Core.Event.OnEntityDeleted += OnGrenadeEntityDeleted;
            }

            Logger.LogInformation(
                "Ankor {Version} loaded on {Framework}: match={MatchId} roster={Rostered} players",
                Version, _host.FrameworkVersion, config.matchId, _match.Roster.PlayerCount);
        }
        finally
        {
            capture?.Dispose();
        }
    }


    /// <summary>
    /// Player-facing text comes from SwiftlyS2's translation service, which reads
    /// <c>resources/translations/*.jsonc</c> out of this plugin's own directory (the csproj copies
    /// the file into the build output, the Dockerfile copies that into the overlay); the chat tag
    /// comes from the plugin's config file. A missing translation file is not fatal — the localizer
    /// then echoes the keys — so say so loudly instead.
    /// </summary>
    private IMessages LoadMessages(ChatTag chat)
    {
        try
        {
            var messages = new SwiftlyMessages(Core, chat);
            if (messages.CatalogLooksLoaded) return messages;

            Logger.LogError(
                "no translation for '{Key}': resources/translations/en.jsonc is missing from the " +
                "plugin directory, so chat will show message keys",
                SwiftlyMessages.ProbeKey);
            return messages;
        }
        catch (Exception ex)
        {
            // Never let unreadable chat text stop the match logic from loading.
            Logger.LogError(ex, "the translation service is unavailable; chat will show message keys");
            return new KeyEchoMessages();
        }
    }

    public void Dispose()
    {
        var capture = _capture.Begin("plugin.unload");
        _session.Dispose();
        Core.GameHooks.Entities.TakeDamage.Pre -= OnDamagePre;
        Core.GameHooks.Entities.TakeDamage.Post -= OnDamagePost;
        _damage.Reset();
        Core.Event.OnEntitySpawned -= OnGrenadeEntitySpawned;
        Core.Event.OnEntityDeleted -= OnGrenadeEntityDeleted;
        ClearGrenadeTracking();
        foreach (var guid in _gameEventHooks) Core.GameEvent.Unhook(guid);
        _gameEventHooks.Clear();

        foreach (var guid in _commands) Core.Command.UnregisterCommand(guid);
        _commands.Clear();

        if (_clientCommandHook is { } hook) Core.Command.UnhookClientCommand(hook);
        _clientCommandHook = null;

        Core.Event.OnClientSteamAuthorize -= OnClientSteamAuthorize;
        Core.Event.OnClientDisconnected -= OnClientDisconnected;

        capture?.Dispose();
        _capture.Dispose();
        _host.Capture = null;
    }



    /// <summary>The agent channel, with each gameplay event also observed by the debug capture.</summary>
    private sealed class CapturedOutput(IAgentChannel agent, LoadedMatch owner) : IAgentChannel
    {
        public void Send(PluginMessage message)
        {
            owner._capture?.Observe("plugin.output", message);
            agent.Send(message);
        }

        public void ReportMatch(MatchReport report) => agent.ReportMatch(report);
        public Task SendQuitting() => agent.SendQuitting();
        public long PendingCount => agent.PendingCount;
        public double? OldestPendingAgeSeconds => agent.OldestPendingAgeSeconds;
    }

    private void RegisterHooks()
    {
        HookPost<EventPlayerConnectFull>(OnPlayerConnectFull);
        HookPost<EventPlayerDisconnect>(OnPlayerDisconnect);
        HookPost<EventPlayerTeam>(OnPlayerTeam);
        Core.Event.OnClientSteamAuthorize += OnClientSteamAuthorize;
        Core.Event.OnClientDisconnected += OnClientDisconnected;

        _clientCommandHook = Core.Command.HookClientCommand((playerId, commandLine) =>
        {
            using var capture = _capture.Begin("client.command");
            if (!TeamLock.IsTeamChangeCommand(commandLine)) return HookResult.Continue;
            var player = _teamLock.ReadPlayer(playerId);
            return player is null || _teamLock.BlockTeamChange(player)
                ? HookResult.Stop : HookResult.Continue;
        });

        // Register chat aliases through the framework’s command-prefix handling.
        Command(ChatCommands.Stay, "Keep the current sides after winning the knife round", OnStayCommand);
        Command(ChatCommands.Swap, "Swap sides after winning the knife round", OnSwapCommand);
        Command(ChatCommands.Switch, "Swap sides after winning the knife round", OnSwapCommand);
        Command(ChatCommands.Ct, "Take the CT side after winning the knife round", OnCtCommand);
        Command(ChatCommands.T, "Take the T side after winning the knife round", OnTCommand);
        // Captains only: starts the match before the whole roster has connected. Without
        // allow_force_ready the command is not registered, so !forceready is plain chat.
        if (_match.Config.allowForceReady)
            Command(ChatCommands.ForceReady, "Start the match without the full roster (captains only)", OnForceReadyCommand);
        Command(ChatCommands.Ready, "Mark yourself ready in warmup", OnReadyCommand);
        Command(ChatCommands.ReadyShort, "Mark yourself ready in warmup", OnReadyCommand);
        Command(ChatCommands.Unready, "Withdraw your ready in warmup", OnUnreadyCommand);
        Command(ChatCommands.NotReady, "Withdraw your ready in warmup", OnUnreadyCommand);
        Command(ChatCommands.UnreadyShort, "Withdraw your ready in warmup", OnUnreadyCommand);

        HookPre<EventPlayerTeam>(OnPlayerTeamPre);

        HookPre<EventRoundPrestart>(OnRoundPrestart);
        HookPost<EventRoundStart>(OnRoundStart);
        HookPost<EventRoundFreezeEnd>(OnRoundFreezeEnd);

        // Use a pre-hook to tally players before round-end cleanup.
        HookPre<EventRoundEnd>(OnRoundEnd);
        HookPost<EventCsWinPanelMatch>(OnWinPanelMatch);
        if (!_match.CollectEvents) return;

        HookPost<EventExitBuyzone>(OnExitBuyzone);
        HookPost<EventPlayerDeath>(OnPlayerDeath);
        HookPost<EventPlayerHurt>(OnPlayerHurt);
        HookPost<EventPlayerBlind>(OnPlayerBlind);
        HookPost<EventWeaponFire>(OnWeaponFire);

        HookPost<EventFlashbangDetonate>(OnFlashbangDetonate);
        HookPost<EventHegrenadeDetonate>(OnHeGrenadeDetonate);
        HookPost<EventSmokegrenadeDetonate>(OnSmokeDetonate);
        HookPost<EventDecoyDetonate>(OnDecoyDetonate);
        HookPost<EventMolotovDetonate>(OnMolotovDetonate);
        HookPost<EventInfernoExtinguish>(OnInfernoExtinguish);
        HookPost<EventInfernoExpire>(OnInfernoExpire);

        HookPost<EventBombBeginplant>(OnBombBeginPlant);
        HookPost<EventBombAbortplant>(OnBombAbortPlant);
        HookPost<EventBombPlanted>(OnBombPlanted);
        HookPost<EventBombBegindefuse>(OnBombBeginDefuse);
        HookPost<EventBombAbortdefuse>(OnBombAbortDefuse);
        HookPost<EventBombDefused>(OnBombDefused);
        HookPost<EventBombExploded>(OnBombExploded);

        HookPost<EventRoundMvp>(OnRoundMvp);
    }

    private void HookPre<T>(SwiftlyS2.Shared.GameEvents.IGameEventService.GameEventHandler<T> handler)
        where T : SwiftlyS2.Shared.GameEvents.IGameEvent<T> =>
        _gameEventHooks.Add(Core.GameEvent.HookPre<T>(value => _capture.GameEvent("pre", value, handler)));

    private void HookPost<T>(SwiftlyS2.Shared.GameEvents.IGameEventService.GameEventHandler<T> handler)
        where T : SwiftlyS2.Shared.GameEvents.IGameEvent<T> =>
        _gameEventHooks.Add(Core.GameEvent.HookPost<T>(value => _capture.GameEvent("post", value, handler)));

    private void Command(string name, string helpText, ICommandService.CommandListener handler) =>
        _commands.Add(Core.Command.RegisterCommand(name, context =>
            {
                using var capture = _capture.Begin("command." + name);
                handler(context);
            }, registerRaw: false,
            permission: "", helpText: helpText));


    public string? RestoreRound(int round)
    {
        using var capture = _capture.Begin("control.restore_round");
        return _backups.Restore(round);
    }

    public string? StartMatch()
    {
        using var capture = _capture.Begin("control.start_match");
        return _match.StartFromController();
    }

    public bool OnBackupLoad(string file)
    {
        using var capture = _capture.Begin("command.backup_load");
        return _backups.BeforeLoad(file);
    }

    public void OnMapLoad(string mapName)
    {
        using var capture = _capture.Begin("map.load");
        if (_session.OnMapLoad(mapName, Version, _host.FrameworkVersion, () => _host.ServerVersion)) Handle(new MapLoadInput());
    }


    /// <summary>Check the roster after Steam authorization supplies a trusted id.</summary>
    private void OnClientSteamAuthorize(SwiftlyS2.Shared.Events.IOnClientSteamAuthorizeEvent @event)
    {
        using var capture = _capture.Begin("client.authorized");
        var player = _teamLock.ReadPlayer(@event.PlayerId);
        if (player is not null) Handle(new ClientAuthorizedInput(player));
    }

    private void OnClientDisconnected(SwiftlyS2.Shared.Events.IOnClientDisconnectedEvent @event)
    {
        using var capture = _capture.Begin("client.disconnected");
        _teamLock.Forget(@event.PlayerId);
    }

    private HookResult OnPlayerConnectFull(EventPlayerConnectFull @event)
    {
        var player = @event.UserIdPlayer is { } client ? _teamLock.ReadPlayer(client.Slot) : null;
        if (player is null) return HookResult.Continue;

        Handle(new PlayerConnectInput(player));
        return HookResult.Continue;
    }

    private HookResult OnPlayerDisconnect(EventPlayerDisconnect @event)
    {
        // Fall back to event fields if the player object is already gone.
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer)
                     ?? HostPlayer.Departed(@event.PlayerID, @event.XuID, @event.Name);

        Handle(new PlayerDisconnectInput(player, DisconnectReasons.Name(@event.Reason)));
        return HookResult.Continue;
    }

    /// <summary>
    /// mp_swapteams moves everyone with a non-silent player_team, so every client prints a
    /// "joined team" line per player. Halftime's own swap is silent; make the knife swap match.
    /// </summary>
    private HookResult OnPlayerTeamPre(EventPlayerTeam @event)
    {
        if (_match.SwapInProgress && !@event.Disconnect) @event.Silent = true;
        return HookResult.Continue;
    }

    private HookResult OnPlayerTeam(EventPlayerTeam @event)
    {
        if (@event.Disconnect) return HookResult.Continue;
        var player = @event.UserIdPlayer is { } client ? _teamLock.ReadPlayer(client.Slot) : null;
        if (player is null) return HookResult.Continue;

        var from = SideExtensions.FromTeamNum(@event.OldTeam);
        var to = SideExtensions.FromTeamNum(@event.Team);

        Handle(new PlayerTeamInput(player, from, to));
        return HookResult.Continue;
    }


    private void ClearGrenadeTracking() => _grenadeAttribution.Clear();

    private void Handle(GameplayInput input)
    {
        // Observe before dispatch: outputs must never become replay inputs.
        _capture.Observe(CaptureNames.Gameplay.Input, new GameplayRecord(input));
        _callbacks.Handle(input);
    }

    private HookResult OnRoundPrestart(EventRoundPrestart @event)
    {
        Handle(new RoundPrestartInput());
        return HookResult.Continue;
    }

    private HookResult OnRoundStart(EventRoundStart @event)
    {
        Handle(new RoundStartInput());
        return HookResult.Continue;
    }

    private HookResult OnRoundFreezeEnd(EventRoundFreezeEnd @event)
    {
        Handle(new RoundFreezeEndInput());
        return HookResult.Continue;
    }

    private HookResult OnRoundEnd(EventRoundEnd @event)
    {
        Handle(new RoundEndInput(SideExtensions.FromTeamNum(@event.Winner), @event.Reason));
        return HookResult.Continue;
    }

    private HookResult OnRoundMvp(EventRoundMvp @event)
    {
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (player is { IsReportable: true }) Handle(new RoundMvpInput(player, @event.Reason));
        return HookResult.Continue;
    }

    private HookResult OnWinPanelMatch(EventCsWinPanelMatch @event)
    {
        Handle(new MapEndInput());
        return HookResult.Continue;
    }


    private HookResult OnPlayerDeath(EventPlayerDeath @event)
    {
        var victim = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (victim is null || !victim.IsReportable) return HookResult.Continue;

        victim = victim with { IsAlive = false, Health = 0 };

        // Use the SDK’s Actual fields rather than obsolete narrow event properties.
        var pawn = @event.UserIdPlayer?.PlayerPawn;
        var damage = pawn is null ? null : _damage.Current(pawn.Address);
        Handle(new PlayerDeathInput(
            victim,
            SwiftlyGameHost.Snapshot(@event.AttackerPlayer),
            SwiftlyGameHost.Snapshot(@event.AssisterPlayer),
            damage?.Weapon ?? ResolveWeapon(@event.Weapon ?? "", @event.AttackerPlayer),
            @event.Headshot,
            @event.Penetrated,
            @event.ThruSmoke,
            @event.NoScope,
            @event.AttackerBlind,
            @event.AssistedFlash,
            @event.AttackerInAir,
            victimEquipmentValue: damage?.EquipmentValue ?? victim.EquipmentValue,
            // Without a pre-damage sample, a post-death read could mistake dropped gear for empty hands.
            victimGrenadesHeld: damage?.GrenadesHeld,
            damageEntityId: damage?.Inflictor ?? 0));

        return HookResult.Continue;
    }

    private void OnDamagePre(ref TakeDamageEntityPreContext context)
    {
        using var capture = _capture.Begin("damage.pre");
        Handle(new DamageEnterInput((long)context.Params.Entity.Address));
        if (!_match.IsLive) return;
        var entity = context.Params.Entity;
        if (entity.DesignerName != "player") return;

        var inflictorIndex = 0;
        WeaponReference? source = null;
        try
        {
            var inflictor = context.Params.Info.Inflictor.Value;
            inflictorIndex = (int)(inflictor?.Index ?? 0);
            source = DamageSource(inflictor);
            if (source is null && context.Params.Info.Ability.Value is { } ability
                && ability.DesignerName.StartsWith(WeaponIdentity.Prefix, StringComparison.Ordinal))
            {
                var item = ability.As<CCSWeaponBase>().AttributeManager.Item.ItemDefinitionIndex;
                source = WeaponReference.FromItem(item, WeaponIdentity.WithoutPrefix(ability.DesignerName));
            }
        }
        catch (Exception ex)
        {
            Logger.LogDebug("damage source unreadable: {Message}", ex.Message);
        }

        // Read before the blow lands: a death drops the gear, the engine's figure and the
        // inventory with it.
        var pawn = entity.As<CCSPlayerPawn>();
        var buyPlayer = _forwarder.BuyWindowOpen
            ? SwiftlyGameHost.Snapshot(Core.PlayerManager.GetPlayerFromPawn(pawn)) : null;
        Handle(new DamageSnapshotInput(new DamageSnapshot(
            Math.Max(0, entity.Health), inflictorIndex, source,
            (int)pawn.CurrentEquipmentValue, SwiftlyGameHost.ReadInventoryItems(pawn, grenadesOnly: true)), buyPlayer));
    }

    private void OnDamagePost(ref TakeDamageEntityPostContext context)
    {
        using var capture = _capture.Begin("damage.post");
        Handle(new DamageExitInput());
    }

    private HookResult OnPlayerHurt(EventPlayerHurt @event)
    {
        var victim = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (victim is null || !victim.IsReportable) return HookResult.Continue;

        var pawn = @event.UserIdPlayer?.PlayerPawn;
        var damage = pawn is null ? null : _damage.Current(pawn.Address);

        Handle(new PlayerHurtInput(
            victim with { Health = @event.ActualHealth },
            SwiftlyGameHost.Snapshot(@event.AttackerPlayer),
            damage?.Weapon ?? ResolveWeapon(@event.Weapon ?? "", @event.AttackerPlayer),
            @event.ActualDmgHealth,
            @event.ActualDmgArmor,
            (int)@event.ActualHitGroup,
            damage?.Health,
            damage?.Inflictor ?? 0));
        if (pawn is not null) Handle(new DamageReportedInput((long)pawn.Address, Math.Max(0, @event.ActualHealth)));

        return HookResult.Continue;
    }

    private WeaponReference? DamageSource(CEntityInstance? entity) =>
        entity is null ? null : _grenadeAttribution.Source((int)entity.Index, entity.DesignerName,
            () => entity.As<CBaseCSGrenadeProjectile>().ItemIndex);

    private WeaponReference ResolveWeapon(string reported, IPlayer? attacker)
    {
        var weapon = attacker?.PlayerPawn?.WeaponServices?.ActiveWeapon.Value;
        int? equippedId = weapon is { IsValid: true } ? weapon.AttributeManager.Item.ItemDefinitionIndex : null;
        var equipped = equippedId is > 0 ? Core.Helpers.GetClassnameByDefinitionIndex(equippedId.Value) ?? weapon!.DesignerName : null;
        return WeaponIdentity.Resolve(reported, equippedId, equipped, classname => Core.Helpers.GetDefinitionIndexByClassname(classname));
    }

    /// <summary>Use the projectile entity id to associate the blind with its flashbang.</summary>
    private HookResult OnPlayerBlind(EventPlayerBlind @event)
    {
        var victim = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        var attacker = SwiftlyGameHost.Snapshot(@event.AttackerPlayer);
        if (victim is null || !victim.IsReportable) return HookResult.Continue;

        Handle(new PlayerBlindInput(victim, attacker, @event.BlindDuration, @event.EntityID));
        return HookResult.Continue;
    }

    private HookResult OnWeaponFire(EventWeaponFire @event)
    {
        var shooter = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (shooter is null || !shooter.IsReportable) return HookResult.Continue;

        // Knives and grenades fire no rounds, so a swing or a throw is not a shot. Grenades are
        // tracked from their projectiles instead.
        var weapon = WeaponIdentity.Fired(@event.Weapon);
        if (WeaponIdentity.IsKnife(weapon) || WeaponIdentity.Grenade(weapon) is not null) return HookResult.Continue;

        Handle(new WeaponFireInput(shooter, ResolveWeapon(weapon, @event.UserIdPlayer)));
        return HookResult.Continue;
    }


    private void OnGrenadeEntitySpawned(SwiftlyS2.Shared.Events.IOnEntitySpawnedEvent @event)
    {
        using var capture = _capture.Begin("entity.spawned");
        var name = @event.Entity.DesignerName;
        if (!GrenadeAttribution.IsTracked(name)) return;
        var entity = @event.Entity.As<CBaseEntity>();
        var owner = entity.OwnerEntity.Value;
        var ownerId = owner is { IsValid: true, DesignerName: "player" } ? (int?)owner.Index : null;
        var entityId = (int)entity.Index;
        var tick = Core.Engine.GlobalVars.TickCount;
        if (name == GrenadeAttribution.Fire)
        {
            if (_grenadeAttribution.FireSpawned(entityId, tick, ownerId, Origin(entity)) is { } projectileId)
                Handle(new GrenadeFireInput(projectileId, entityId));
            else if (_match.IsLive)
                Logger.LogWarning("Cannot identify source of inferno {Entity}", entityId);
            return;
        }
        if (ownerId is null || owner is null) return;
        // IsIncGrenade is set by the time the projectile spawns.
        var kind = GrenadeAttribution.Kind(name, () => entity.As<CMolotovProjectile>().IsIncGrenade);
        if (kind is null) return;
        var thrower = SwiftlyGameHost.Snapshot(Core.PlayerManager.GetPlayerFromPawn(owner.As<CCSPlayerPawn>()));
        if (thrower is not { IsReportable: true }) return;
        DamageSource(@event.Entity);
        Handle(new GrenadeSpawnedInput(thrower, kind.Value, entityId));
    }

    private void OnGrenadeEntityDeleted(SwiftlyS2.Shared.Events.IOnEntityDeletedEvent @event)
    {
        using var capture = _capture.Begin("entity.deleted");
        var id = (int)@event.Entity.Index;
        _grenadeAttribution.Deleted(id);
        if (@event.Entity.DesignerName != GrenadeAttribution.Fire)
        {
            Handle(new GrenadeRemovedInput(id));
            return;
        }
        // m_bWasCreatedInSmoke is set in the inferno's first think, after OnEntitySpawned ran, so
        // only here does it say whether the fire ever burned.
        var smothered = false;
        WorldPosition? origin = null;
        try
        {
            var inferno = @event.Entity.As<CInferno>();
            smothered = inferno.WasCreatedInSmoke;
            origin = Origin(inferno);
        }
        catch (Exception ex)
        {
            Logger.LogDebug("inferno unreadable at deletion: {Message}", ex.Message);
        }
        Handle(new GrenadeRemovedInput(id, smothered, origin));
    }

    // Every *_detonate event carries the detonation origin as x/y/z floats (gameevents viewer,
    // mod.gameevents); molotov_detonate is the one without an entityid.
    private HookResult OnFlashbangDetonate(EventFlashbangDetonate @event) =>
        Detonated(@event.EntityID, new WorldPosition(@event.X, @event.Y, @event.Z));
    private HookResult OnHeGrenadeDetonate(EventHegrenadeDetonate @event) =>
        Detonated(@event.EntityID, new WorldPosition(@event.X, @event.Y, @event.Z));
    private HookResult OnSmokeDetonate(EventSmokegrenadeDetonate @event) =>
        Detonated(@event.EntityID, new WorldPosition(@event.X, @event.Y, @event.Z));
    private HookResult OnDecoyDetonate(EventDecoyDetonate @event) =>
        Detonated(@event.EntityID, new WorldPosition(@event.X, @event.Y, @event.Z));

    private HookResult OnMolotovDetonate(EventMolotovDetonate @event)
    {
        // This native event omits entityid. Its exact position identifies the projectile;
        // inferno spawn follows synchronously before that projectile is deleted.
        // Do not guess by throw order: multiple grenades can be airborne for one player.
        var owner = @event.UserIdPlayer?.PlayerPawn;
        var ownerIndex = owner is { IsValid: true } ? (uint?)owner.Index : null;
        var candidates = Core.EntitySystem.GetAllEntitiesByDesignerName<CMolotovProjectile>(GrenadeAttribution.MolotovProjectile)
            .Where(p => p.IsValid && _forwarder.IsTrackedGrenade((int)p.Index)
                && (ownerIndex is null || p.OwnerEntity.Value?.Index == ownerIndex)
                && p.AbsOrigin is { } position
                && position.X == @event.X && position.Y == @event.Y && position.Z == @event.Z)
            .ToArray();
        if (candidates.Length == 1)
        {
            var id = (int)candidates[0].Index;
            DamageSource(candidates[0]);
            var nativeOwner = candidates[0].OwnerEntity.Value;
            _grenadeAttribution.Detonated(id, Core.Engine.GlobalVars.TickCount,
                nativeOwner is { IsValid: true } ? (int)nativeOwner.Index : null,
                new WorldPosition(@event.X, @event.Y, @event.Z));
            Handle(new GrenadeDetonateInput(id, new WorldPosition(@event.X, @event.Y, @event.Z)));
        }
        else
            Logger.LogWarning("Cannot identify detonating fire projectile: {Candidates} exact candidates", candidates.Length);
        return HookResult.Continue;
    }

    private HookResult Detonated(int entityId, WorldPosition position)
    {
        Handle(new GrenadeDetonateInput(entityId, position));
        return HookResult.Continue;
    }

    /// <summary>inferno_extinguish names the fire entity; the smoke is found by position.</summary>
    private HookResult OnInfernoExtinguish(EventInfernoExtinguish @event)
    {
        Handle(new FireExtinguishedInput(@event.EntityID, new WorldPosition(@event.X, @event.Y, @event.Z)));
        return HookResult.Continue;
    }

    private HookResult OnInfernoExpire(EventInfernoExpire @event)
    {
        Handle(new FireExpiredInput(@event.EntityID));
        return HookResult.Continue;
    }

    private static WorldPosition? Origin(CBaseEntity entity) =>
        entity.AbsOrigin is { } origin ? new WorldPosition(origin.X, origin.Y, origin.Z) : null;


    private HookResult OnExitBuyzone(EventExitBuyzone @event)
    {
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (player is null || !player.IsReportable) return HookResult.Continue;

        Handle(new ExitBuyZoneInput(player));
        return HookResult.Continue;
    }

    private HookResult OnBombBeginPlant(EventBombBeginplant @event)
    {
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (player is null || !player.IsReportable) return HookResult.Continue;

        Handle(new BombBeginPlantInput(player, TargetSite(@event.Site)));
        return HookResult.Continue;
    }

    private HookResult OnBombAbortPlant(EventBombAbortplant @event)
    {
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (player is null || !player.IsReportable) return HookResult.Continue;

        Handle(new BombAbortPlantInput(player, TargetSite(@event.Site)));
        return HookResult.Continue;
    }

    /// <summary>The plant events carry the func_bomb_target's entity index, not a site number.</summary>
    private BombSite? TargetSite(int entityIndex)
    {
        if (entityIndex <= 0) return null;
        var target = Core.EntitySystem.GetEntityByIndex<CBombTarget>((uint)entityIndex);
        if (target is null || !target.IsValid || target.DesignerName != "func_bomb_target") return null;
        return target.IsBombSiteB ? BombSite.B : BombSite.A;
    }

    private HookResult OnBombPlanted(EventBombPlanted @event)
    {
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (player is null || !player.IsReportable) return HookResult.Continue;

        Handle(new BombPlantedInput(player, PlantedSite()));
        return HookResult.Continue;
    }

    // event.site is an entity index, not the site.
    private BombSite? PlantedSite() => Bomb.PlantedSite(PlantedBomb()?.BombSite);

    private CPlantedC4? PlantedBomb() =>
        Core.EntitySystem.GetAllEntitiesByDesignerName<CPlantedC4>("planted_c4")
            .FirstOrDefault(bomb => bomb.IsValid);

    private HookResult OnBombBeginDefuse(EventBombBegindefuse @event)
    {
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (player is null || !player.IsReportable) return HookResult.Continue;

        Handle(new BombBeginDefuseInput(player, @event.HasKit));
        return HookResult.Continue;
    }

    private HookResult OnBombAbortDefuse(EventBombAbortdefuse @event)
    {
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (player is null || !player.IsReportable) return HookResult.Continue;

        Handle(new BombAbortDefuseInput(player));
        return HookResult.Continue;
    }

    private HookResult OnBombDefused(EventBombDefused @event)
    {
        var player = SwiftlyGameHost.Snapshot(@event.UserIdPlayer);
        if (player is null || !player.IsReportable) return HookResult.Continue;

        var hasKit = @event.UserIdPlayer?.PlayerPawn?.ItemServices?.HasDefuser ?? false;
        Handle(new BombDefusedInput(player, DefuseTimeRemaining(), hasKit));
        return HookResult.Continue;
    }

    private double DefuseTimeRemaining() =>
        PlantedBomb() is { } bomb ? Bomb.DefuseTimeRemaining(bomb.C4Blow.Value, _host.CurrentTime) : 0;

    private HookResult OnBombExploded(EventBombExploded @event)
    {
        Handle(new BombExplodedInput());
        return HookResult.Continue;
    }


    private void OnStayCommand(ICommandContext context) =>
        HandleSideChoice(context, SideChoiceKind.Stay);

    private void OnSwapCommand(ICommandContext context) =>
        HandleSideChoice(context, SideChoiceKind.Swap);

    private void OnCtCommand(ICommandContext context) =>
        HandleSideChoiceForSide(context, CoreSide.Ct);

    private void OnTCommand(ICommandContext context) =>
        HandleSideChoiceForSide(context, CoreSide.T);

    private void OnForceReadyCommand(ICommandContext context)
    {
        var player = Caller(context);
        if (player is null) return;

        Handle(new ForceReadyInput(player));
    }

    private void OnReadyCommand(ICommandContext context) => HandleReady(context, ready: true);

    private void OnUnreadyCommand(ICommandContext context) => HandleReady(context, ready: false);

    private void HandleReady(ICommandContext context, bool ready)
    {
        var player = Caller(context);
        if (player is null) return;

        Handle(new ReadyInput(player, ready));
    }

    private void HandleSideChoiceForSide(ICommandContext context, CoreSide wanted)
    {
        var player = Caller(context);
        if (player is null) return;

        Handle(new SideChoiceForSideInput(player, wanted));
    }

    private void HandleSideChoice(ICommandContext context, SideChoiceKind choice)
    {
        var player = Caller(context);
        if (player is null) return;

        Handle(new SideChoiceInput(player, choice));
    }

    /// <summary>Ignore server-console invocations; these commands require a player.</summary>
    private static HostPlayer? Caller(ICommandContext context) =>
        context.IsSentByPlayer ? SwiftlyGameHost.Snapshot(context.Sender) : null;
}
