using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>Convert plain game snapshots to wire events. Round-scoped stats require IsLive.</summary>
public sealed class EventForwarder : IDisposable
{
    /// <summary>Old-round clock value at which the closed round's effects are cut; set by round_end.</summary>
    private double? _roundClosesAt;
    private bool _grenadesClosed;
    private bool _roundCompleted;
    private int? _endedRound;
    private double? _endedRoundLength;
    private Side _endedTeam1Side;
    private RoundCarryover? _carryover;

    private readonly MatchState _match;
    private readonly IGameHost _host;
    private readonly BlindTracker _blinds = new();
    private readonly ShotTracker _shots = new();
    private readonly GrenadeTracker _grenades;

    /// <summary>Players who left in this round: the engine kills a living player right after the disconnect.</summary>
    private readonly HashSet<string> _disconnected = [];

    private readonly Dictionary<ulong, PlayerBuy> _buys = new();
    private readonly HashSet<ulong> _buyZoneExits = [];
    private readonly HashSet<ulong> _finishedBuys = [];
    private readonly Dictionary<ulong, Stack<(PlayerBuy Buy, bool InBuyZone)>> _damageBuys = new();
    /// <summary>
    /// Buying opens at round start and stays open through freeze time; mp_buytime counts from
    /// freeze end, so the closing timer starts there, after any pause that extends freeze time.
    /// </summary>
    public bool BuyWindowOpen { get; private set; }
    private readonly OwnedTimer _buyWindow;
    private bool _disposed;

    public EventForwarder(MatchState match, IGameHost host)
    {
        _match = match;
        _host = host;
        _buyWindow = new OwnedTimer(host);
        _grenades = new GrenadeTracker(message => _match.Emit(message));
    }


    /// <summary>
    /// Joins and departures before the match leaves warmup say nothing the match
    /// keeps, and <c>get_server_status</c> lists who is on the server.
    /// </summary>
    private bool BeforeMatch => _match.Phase is MatchPhase.Loading or MatchPhase.Warmup;

    public void OnPlayerConnect(HostPlayer player, bool rejected)
    {
        _disconnected.Remove(player.SteamIdString);
        if (BeforeMatch) return;
        _match.Emit(new PlayerConnect { player = _match.RefFor(player), rejected = rejected });
    }

    public void OnPlayerDisconnect(HostPlayer player, string reason)
    {
        _disconnected.Add(player.SteamIdString);
        FinishBuy(player);
        if (BeforeMatch) return;
        _match.Emit(new PlayerDisconnect { player = _match.RefFor(player), reason = WireText.Clean(reason, WireText.MaxDisconnectReason) });
    }


    /// <summary>
    /// Capture survivors' equipment after exit kills/pickups and before respawns.
    /// A final round has no next buy to improve.
    /// </summary>
    public void OnRoundPrestart()
    {
        CaptureCarryover();
        CompleteRound();
    }

    private void CaptureCarryover()
    {
        if (!_match.IsLive || !_endedRound.HasValue || _carryover is not null || ResetsEquipment()) return;
        var round = _endedRound.Value;
        _carryover = new RoundCarryover
        {
            fromRound = round,
            sampledTick = _host.SimulationTick,
            players = _host.Players.Where(IsPlaying).Select(player =>
            {
                var current = RoundPlayerOf(player);
                var snapshot = new CarryoverPlayer
                {
                    player = current.player, alive = current.alive, equipmentValue = current.equipmentValue,
                    cash = current.cash, startCash = current.startCash, cashSpent = current.cashSpent,
                    inventory = player.IsAlive ? _host.ReadInventory(player) : new PlayerInventory(),
                };
                snapshot.player.side = snapshot.player.team switch
                {
                    1 => _endedTeam1Side,
                    2 => _endedTeam1Side.Opposite(),
                    _ => snapshot.player.side,
                };
                return snapshot;
            }).ToList(),
        };
    }

    /// <summary>Equipment cannot be saved across a side swap or the start of an overtime.</summary>
    private bool ResetsEquipment()
    {
        if (_endedTeam1Side != _match.Team1Side) return true;
        return _endedRound is { } round && RoundFormat.Read(_host).StartsOvertimeAfter(round);
    }

    private RoundPlayer RoundPlayerOf(HostPlayer player)
    {
        var buy = BuyOf(player);
        return new RoundPlayer
        {
            player = buy.player, alive = player.IsAlive,
            equipmentValue = player.IsAlive ? buy.equipmentValue : 0,
            cash = buy.cash, startCash = buy.startCash, cashSpent = buy.cashSpent,
        };
    }

    public void OnRoundStart()
    {
        _disconnected.Clear();
        _roundCompleted = false;
        _grenadesClosed = false;
        _blinds.Reset();
        ResetBuyTracking();
        if (!_match.IsLive)
        {
            _grenades.Flush(_roundClosesAt ?? WireSeconds.Of(_match.RoundTime));
            _shots.Clear();
            _roundClosesAt = null;
            _endedRound = null;
            _carryover = null;
            return;
        }
        // Ordinary rounds allow exit play until restart. Halftime has already flushed its effects.
        _grenades.Flush(_roundClosesAt ?? WireSeconds.Of(_match.RoundTime));
        _roundClosesAt = null;
        // A round that starts again with no round_end, as the engine restarts one a team left, is
        // played from the new start: the aborted attempt has no round_shots.
        if (_endedRound is null) _shots.Clear();
        else FlushShots();
        _shots.Begin(_match.Round);
        var players = _host.Players.Where(IsPlaying).ToList();
        _match.Emit(new RoundStart
        {
            team1Side = _match.Team1Side,
            players = players.Select(RoundPlayerOf).ToList(),
            carryover = _carryover?.fromRound == _match.Round - 1 && !ResetsEquipment() ? _carryover : null,
        });
        _carryover = null;
        _endedRound = null;
        foreach (var player in players.Where(player => player.IsAlive)) _buys[player.SteamId] = BuyOf(player);
        BuyWindowOpen = !_disposed;
    }

    public void OnRoundFreezeEnd()
    {
        if (!_match.IsLive) return;
        if (BuyWindowOpen && !_buyWindow.IsPending)
            _buyWindow.Schedule(_host.GetConVarNumber("mp_buytime")!.Value, EmitRoundBuyEnd);
        _match.Emit(new RoundFreezeEnd
        {
            team1Side = _match.Team1Side,
            players = _host.Players.Where(IsPlaying).Select(RoundPlayerOf).ToList(),
        });
    }

    private static bool IsPlaying(HostPlayer player) =>
        player.IsReportable && player.Side is Side.T or Side.Ct;

    private PlayerBuy BuyOf(HostPlayer player) => new()
    {
        player = _match.RefFor(player),
        equipmentValue = Math.Max(0, player.EquipmentValue),
        cash = Math.Max(0, player.Cash),
        startCash = Math.Max(0, player.StartCash),
        cashSpent = Math.Max(0, player.CashSpent),
        sampledTick = _host.SimulationTick,
    };

    /// <summary>
    /// Damage may end the round before player_death runs. Keep the pre-damage money and equipment
    /// available to both callbacks, before a death drops equipment or round rewards change cash.
    /// Nested native damage calls must restore the enclosing snapshot when they return.
    /// Every hit in the buy window is sampled: the pre-hook's damage is not the engine's final
    /// figure (armor, hitgroup, rounding), so it cannot tell a lethal hit, and a missed one corrupts that buy. This stack
    /// follows the buy window and DamageTracker's follows native calls; they reset at different times.
    /// </summary>
    public bool OnPlayerDamageStarting(HostPlayer player)
    {
        if (!BuyWindowOpen || !IsPlaying(player)) return false;
        if (!_damageBuys.TryGetValue(player.SteamId, out var stack))
            _damageBuys[player.SteamId] = stack = new();
        stack.Push((BuyOf(player), player.InBuyZone));
        return true;
    }

    public void OnPlayerDamageFinished(ulong steamId)
    {
        if (!_damageBuys.TryGetValue(steamId, out var stack)) return;
        stack.Pop();
        if (stack.Count == 0) _damageBuys.Remove(steamId);
    }

    private PlayerBuy CurrentBuy(HostPlayer player) =>
        _damageBuys.TryGetValue(player.SteamId, out var stack) ? stack.Peek().Buy : BuyOf(player);

    private bool InBuyZoneBeforeDamage(HostPlayer player) =>
        _damageBuys.TryGetValue(player.SteamId, out var stack) ? stack.Peek().InBuyZone : player.InBuyZone;

    /// <summary>Remember what a player carries out of a buy zone while buy time is open.</summary>
    public void OnExitBuyZone(HostPlayer player)
    {
        if (!BuyWindowOpen || !IsPlaying(player) || !player.IsAlive || _finishedBuys.Contains(player.SteamId)) return;
        _buys[player.SteamId] = CurrentBuy(player);
        _buyZoneExits.Add(player.SteamId);
    }

    private void FinishBuy(HostPlayer player)
    {
        if (!BuyWindowOpen || !IsPlaying(player) || !_finishedBuys.Add(player.SteamId)) return;
        if (InBuyZoneBeforeDamage(player) || !_buyZoneExits.Contains(player.SteamId))
            _buys[player.SteamId] = CurrentBuy(player);
    }

    /// <summary>
    /// Last buy-zone exit wins outside the zone; inside it, later purchases must be included.
    /// Death and disconnect freeze that choice, retaining players no longer in the host list.
    /// The totals are sums of the emitted samples, never a separate read of the engine.
    /// </summary>
    private void EmitRoundBuyEnd()
    {
        CloseBuyWindow();
        if (!_match.IsLive) return;
        foreach (var player in _host.Players.Where(IsPlaying))
        {
            if (_finishedBuys.Contains(player.SteamId)) continue;
            if (!player.IsAlive && !_damageBuys.ContainsKey(player.SteamId)) continue;
            if (InBuyZoneBeforeDamage(player) || !_buyZoneExits.Contains(player.SteamId))
                _buys[player.SteamId] = CurrentBuy(player);
        }
        var players = _buys.Values.OrderBy(buy => buy.player.steamId, StringComparer.Ordinal).ToList();
        _match.Emit(new RoundBuyEnd
        {
            players = players,
            team1EquipmentValue = players.Where(buy => buy.player.team == 1).Sum(buy => buy.equipmentValue),
            team2EquipmentValue = players.Where(buy => buy.player.team == 2).Sum(buy => buy.equipmentValue),
        });
    }

    /// <summary>Halftime stops play; no exit damage or shots can belong to the restart delay.</summary>
    public void OnHalftime(double roundLength)
    {
        if (!_match.IsLive) return;
        _grenadesClosed = true;
        _roundClosesAt = WireSeconds.Of(roundLength);
        _grenades.Flush(_roundClosesAt.Value);
        FlushShots();
        _carryover = null;
    }

    /// <summary>
    /// The delivery boundary for a scored round: every grenade end and shot report precedes
    /// this event. Ordinary rounds close at prestart; halftime and map end close after their final
    /// callbacks, while the match still belongs to the ending round.
    /// An administratively interrupted round without a result is never declared complete.
    /// </summary>
    public void CompleteRound(double? closesAt = null)
    {
        if (!_match.IsLive || _roundCompleted || !_endedRound.HasValue) return;
        var time = closesAt ?? _roundClosesAt ?? WireSeconds.Of(_match.RoundTime);
        _grenadesClosed = true;
        _grenades.Flush(time);
        FlushShots();
        // A round never completes before it closes, even on a round clock that game over froze at 0.
        var now = WireSeconds.Of(_match.RoundTime);
        _match.Emit(new RoundComplete(), now < time ? time : null);
        _roundCompleted = true;
    }

    public void OnRoundEnd(Side winnerSide, RoundEndReason reason)
    {
        if (!_match.IsLive) return;
        if (BuyWindowOpen)
        {
            // A round shorter than buy time still reports its buy, ahead of its own end.
            EmitRoundBuyEnd();
        }
        _endedRound = _match.Round;
        _endedTeam1Side = _match.Team1Side;
        var length = WireSeconds.Of(_match.RoundTime);
        _endedRoundLength = length;
        // The round's clock keeps running through the restart delay and stops when the engine
        // resets for the next round, clearing every effect still alive.
        _roundClosesAt = length + _host.GetConVarNumber("mp_round_restart_delay")!.Value;
        var (team1, team2) = _match.ScoreRound(winnerSide);
        var winnerTeam = RosterEnforcer.TeamForSide(winnerSide, _match.Team1Side)!.Value;

        _match.Emit(new RoundEnd
        {
            winnerSide = winnerSide,
            winnerTeam = winnerTeam,
            reason = reason,
            team1Score = team1,
            team2Score = team2,
            roundLength = length,
        });
    }

    public void OnRoundMvp(HostPlayer player, int reason)
    {
        if (!_match.IsLive) return;
        _match.Emit(new RoundMvp { player = _match.RefFor(player), reason = reason });
    }


    public void OnPlayerDeath(
        HostPlayer victim,
        HostPlayer? attacker,
        HostPlayer? assister,
        WeaponReference weapon,
        bool headshot,
        int penetrated,
        bool thruSmoke,
        bool noScope,
        bool attackerBlind,
        bool nativeFlashAssist,
        bool attackerInAir,
        int victimEquipmentValue,
        IReadOnlyList<InventoryItem>? victimGrenadesHeld,
        int damageEntityId = 0)
    {
        if (!_match.IsLive) return;
        FinishBuy(victim with { EquipmentValue = victimEquipmentValue });
        var now = _host.CurrentTime;
        var victimId = victim.SteamIdString;
        var credited = GrenadeAttacker(damageEntityId, attacker);

        var blind = _blinds.Lookup(victimId, now);
        _blinds.Remove(victimId);
        if (Placed(victim, "player_death") is not { } victimAt) return;
        var assisted = assister is { IsReportable: true } ? _match.RefFor(assister) : nativeFlashAssist ? blind?.Blinder : null;
        // An assist goes to a third player: a flash the attacker threw is no assist.
        if (assisted?.steamId == victimId || assisted?.steamId == credited?.steamId) assisted = null;

        _match.Emit(new PlayerDeath
        {
            victim = victimAt,
            attacker = credited is null ? null : MatchState.Place(credited, attacker),
            assister = assisted,
            weapon = Weapon.Of(weapon),
            cause = DamageCauses.Of(victimId, credited?.steamId, weapon, _disconnected.Contains(victimId)),
            headshot = headshot,
            penetrated = penetrated,
            thruSmoke = thruSmoke,
            noScope = noScope,
            attackerBlind = attackerBlind,
            attackerInAir = attackerInAir,
            flashAssist = nativeFlashAssist,
            friendlyFire = IsFriendlyFire(credited, _match.RefFor(victim)),
            victimBlindBy = blind?.Blinder,
            victimBlindRemaining = blind?.Remaining ?? 0,
            victimEquipmentValue = Math.Max(0, victimEquipmentValue),
            victimGrenadesHeld = victimGrenadesHeld?.ToArray(),
        });
    }

    public void OnPlayerHurt(
        HostPlayer victim,
        HostPlayer? attacker,
        WeaponReference weapon,
        int dmgHealth,
        int dmgArmor,
        int hitgroup,
        int? healthBefore = null,
        int damageEntityId = 0)
    {
        if (!_match.IsLive) return;
        if (Placed(victim, "player_hurt") is not { } victimAt) return;
        var credited = GrenadeAttacker(damageEntityId, attacker);
        var friendly = IsFriendlyFire(credited, _match.RefFor(victim));
        // Fall damage arrives with no attacker, no inflictor item and an empty weapon name, while
        // the death the same fall causes says "worldent". Name the hits the same way.
        if (credited is null && weapon is { Id: null, Fallback: null }) weapon = new WeaponReference(null, "worldent");
        _match.Emit(new PlayerHurt
        {
            victim = victimAt,
            attacker = credited is null ? null : MatchState.Place(credited, attacker),
            weapon = Weapon.Of(weapon),
            cause = DamageCauses.Of(victim.SteamIdString, credited?.steamId, weapon, _disconnected.Contains(victim.SteamIdString)),
            // Map kill triggers deal far more; past any health a player can have it is only overkill.
            dmgHealth = Math.Clamp(dmgHealth, 0, PlayerHurt.MaxDamage),
            // A pre-hit sample below the health after it is not this hit's.
            healthBefore = healthBefore >= Math.Max(0, victim.Health) ? healthBefore : null,
            healthAfter = Math.Max(0, victim.Health),
            dmgArmor = Math.Clamp(dmgArmor, 0, PlayerHurt.MaxDamage),
            // HitGroup_t's invalid group is -1; anything outside the named groups goes out as generic.
            hitgroup = hitgroup is >= 0 and <= 15 ? hitgroup : 0,
            friendlyFire = friendly,
            projectileImpact = _grenades.IsProjectileImpact(damageEntityId),
            grenadeId = _grenades.Reference(damageEntityId),
        });
    }

    /// <summary>Projectile id links player_blind to flashbang_detonate; 0 means absent.</summary>
    public void OnPlayerBlind(HostPlayer victim, HostPlayer? attacker, double duration, int entityId = 0)
    {
        if (!_match.IsLive) return;
        var credited = GrenadeAttacker(entityId, attacker);
        if (credited is null || Placed(victim, "player_blind") is not { } victimAt) return;
        var friendly = IsFriendlyFire(credited, _match.RefFor(victim));
        _blinds.Record(victim.SteamIdString, credited, duration, _host.CurrentTime);
        _match.Emit(new PlayerBlind
        {
            victim = victimAt,
            attacker = MatchState.Place(credited, attacker),
            duration = duration,
            friendlyFire = friendly,
            grenadeId = _grenades.Reference(entityId),
        });
    }

    public void OnWeaponFire(HostPlayer shooter, WeaponReference weapon)
    {
        if (!_match.IsLive) return;
        _shots.Record(_match.RefFor(shooter), weapon, _match.Round, _host.SimulationTick);
    }

    /// <summary>
    /// A restore cut the round off: its shots and the grenades still alive in it are dropped, as
    /// the round has no round_end, so the match_restored is followed by the round_start alone.
    /// </summary>
    public void DropCutOffRound()
    {
        _grenades.Drop();
        _shots.Clear();
    }

    private void FlushShots()
    {
        foreach (var shots in _shots.Flush()) _match.Emit(shots);
    }

    /// <summary>
    /// A projectile owns its thrower's identity, independently of a live controller or reused
    /// player slot. Only current positions may come from the live player; never invent them.
    /// </summary>
    private PlayerRef? GrenadeAttacker(int entityId, HostPlayer? attacker) =>
        _grenades.Owner(entityId) ?? (attacker is { IsReportable: true } ? _match.RefFor(attacker) : null);

    /// <summary>
    /// Events whose player acts in the world require a position. The engine raises them for a
    /// live pawn, so a snapshot without one is reported and the event dropped, never sent unplaced.
    /// </summary>
    private PlayerAt? Placed(HostPlayer player, string type)
    {
        var at = _match.AtFor(player);
        if (at.position is not null) return at;
        _match.LogEvent(LogLevel.Warn, $"{type} without a pawn position for {player.SteamIdString}; event dropped");
        return null;
    }

    public bool IsTrackedGrenade(int entityId) => _grenades.Owner(entityId) is not null;

    /// <summary>Compare throw-time lobby teams, falling back to sides when team identity is unavailable.</summary>
    private static bool IsFriendlyFire(PlayerRef? attacker, PlayerRef victim)
    {
        if (attacker is null || attacker.steamId == victim.steamId) return false;
        if (attacker.team is not null && victim.team is not null) return attacker.team == victim.team;
        return attacker.side == victim.side && attacker.side != Side.Spec;
    }


    public void OnGrenadeSpawned(HostPlayer thrower, GrenadeKind kind, int entityId)
    {
        // A throw's weapon_fire can precede the final kill while its projectile spawns afterward.
        // Halftime/map end have already flushed effects; ordinary round ends still allow exit play.
        if (!_match.IsLive || _grenadesClosed) return;
        if (Placed(thrower, "grenade_throw") is not { } throwerAt) return;
        _grenades.Throw(entityId, _match.Round, WireSeconds.Of(_match.RoundTime), new GrenadeThrow {
            player = throwerAt,
            grenade = kind,
        });
    }

    public void OnGrenadeDetonate(int entityId, WorldPosition position) => _grenades.Detonate(entityId, position);
    public void OnGrenadeFire(int projectileId, int fireId) => _grenades.AttachFire(projectileId, fireId);
    /// <summary>An inferno going while flagged as created in smoke never burned; the smoke it lit in is credited.</summary>
    public void OnGrenadeRemoved(int entityId, bool smothered = false, WorldPosition? origin = null)
    {
        if (smothered) _grenades.Smother(entityId, WireSeconds.Of(_match.RoundTime), origin);
        _grenades.RemoveEntity(entityId, WireSeconds.Of(_match.RoundTime), _roundClosesAt);
    }

    public void OnFireExtinguished(int fireId, WorldPosition position)
    {
        if (_grenades.Extinguish(fireId, WireSeconds.Of(_match.RoundTime), position) is false)
            _match.LogEvent(LogLevel.Warn, $"inferno {fireId} extinguished with no live smoke within {GrenadeTracker.SmokeExtinguishRadius} units");
    }

    public void OnFireExpired(int fireId) => _grenades.Expire(fireId, WireSeconds.Of(_match.RoundTime));

    public void OnBombBeginPlant(HostPlayer player, BombSite? site)
    {
        if (!_match.IsLive || Placed(player, "bomb_begin_plant") is not { } at) return;
        _match.Emit(new BombBeginPlant { player = at, site = site });
    }

    public void OnBombAbortPlant(HostPlayer player, BombSite? site)
    {
        if (!_match.IsLive || Placed(player, "bomb_abort_plant") is not { } at) return;
        _match.Emit(new BombAbortPlant { player = at, site = site });
    }

    public void OnBombPlanted(HostPlayer player, BombSite? site)
    {
        if (!_match.IsLive || Placed(player, "bomb_planted") is not { } at) return;
        _match.Emit(new BombPlanted { player = at, site = site });
    }

    public void OnBombBeginDefuse(HostPlayer player, bool hasKit)
    {
        if (!_match.IsLive || Placed(player, "bomb_begin_defuse") is not { } at) return;
        _match.Emit(new BombBeginDefuse { player = at, hasKit = hasKit });
    }

    public void OnBombAbortDefuse(HostPlayer player)
    {
        if (!_match.IsLive || Placed(player, "bomb_abort_defuse") is not { } at) return;
        _match.Emit(new BombAbortDefuse { player = at });
    }

    public void OnBombDefused(HostPlayer player, double timeRemaining, bool hasKit)
    {
        if (!_match.IsLive || Placed(player, "bomb_defused") is not { } at) return;
        _match.Emit(new BombDefused
        {
            player = at,
            timeRemaining = Math.Max(0, WireSeconds.Of(timeRemaining)),
            hasKit = hasKit,
        });
    }

    public void OnBombExploded()
    {
        if (!_match.IsLive) return;
        _match.Emit(new BombExploded());
    }


    private void CloseBuyWindow()
    {
        BuyWindowOpen = false;
        _buyWindow.Cancel();
    }

    private void ResetBuyTracking()
    {
        CloseBuyWindow();
        _buys.Clear();
        _buyZoneExits.Clear();
        _finishedBuys.Clear();
        _damageBuys.Clear();
    }

    public void Reset()
    {
        _roundCompleted = false;
        _grenadesClosed = true;
        ResetBuyTracking();
        _carryover = null;
        _endedRound = null;
        _grenades.Flush(_roundClosesAt ?? WireSeconds.Of(_match.RoundTime));
        _roundClosesAt = null;
        _blinds.Reset();
        _shots.Clear();
    }

    public void Dispose()
    {
        _disposed = true;
        Reset();
        _buyWindow.Dispose();
    }

    public void OnMapEnd()
    {
        // At game over the engine can freeze the round clock at 0, so the round closes no earlier
        // than its end.
        var now = WireSeconds.Of(_match.RoundTime);
        var closesAt = _endedRound.HasValue && _endedRoundLength is { } ended && now < ended ? ended : now;
        CompleteRound(closesAt);
        _grenadesClosed = true;
        _grenades.Flush(closesAt);
        FlushShots();
        _match.Emit(new MapEnd
        {
            reason = _match.EndReason,
            winnerTeam = _match.WinnerTeam,
            team1Score = _match.Team1Score,
            team2Score = _match.Team2Score,
            teams = _match.ReadyAtDeadline,
        });
        Reset();
    }
}
