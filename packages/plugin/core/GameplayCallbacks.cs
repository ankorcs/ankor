using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// Shared live/replay callback coordination. The adapter copies native inputs; this class owns
/// map load, player lifecycle and round transitions, and waits for the outermost damage operation
/// before closing a half or map.
/// </summary>
public sealed class GameplayCallbacks(MatchState match, IGameHost host, EventForwarder forwarder,
    DamageTracker damage, TeamLock teams, Action clearNativeTracking, Action startShutdown, Action stopRecording,
    Action roundStarted)
    : IDisposable
{
    private readonly OwnedTimer _mapLoadTimer = new(host);
    private int _revision;

    public void Reset() => ++_revision;

    public void Dispose()
    {
        match.BeginEnding();
        Reset();
        _mapLoadTimer.Dispose();
    }

    public void Handle(GameplayInput input)
    {
        // The forwarder takes plain values, not these records, so it stays independent of the capture format.
        switch (input)
        {
            case RoundFreezeEndInput: forwarder.OnRoundFreezeEnd(); break;
            case RoundPrestartInput:
                match.NoticeBackupLoading();
                forwarder.OnRoundPrestart();
                break;
            case RoundMvpInput e: forwarder.OnRoundMvp(e.player, e.reason); break;
            case WeaponFireInput e: forwarder.OnWeaponFire(e.shooter, e.weapon); break;
            case GrenadeSpawnedInput e: forwarder.OnGrenadeSpawned(e.thrower, e.kind, e.entityId); break;
            case GrenadeDetonateInput e: forwarder.OnGrenadeDetonate(e.entityId, e.position); break;
            case GrenadeFireInput e: forwarder.OnGrenadeFire(e.projectileId, e.fireId); break;
            case GrenadeRemovedInput e: forwarder.OnGrenadeRemoved(e.entityId, e.smothered, e.origin); break;
            case FireExtinguishedInput e: forwarder.OnFireExtinguished(e.fireId, e.position); break;
            case FireExpiredInput e: forwarder.OnFireExpired(e.fireId); break;
            case PlayerBlindInput e: forwarder.OnPlayerBlind(e.victim, e.attacker, e.duration, e.entityId); break;
            case ExitBuyZoneInput e: forwarder.OnExitBuyZone(e.player); break;
            case BombBeginPlantInput e: forwarder.OnBombBeginPlant(e.player, e.site); break;
            case BombAbortPlantInput e: forwarder.OnBombAbortPlant(e.player, e.site); break;
            case BombPlantedInput e: forwarder.OnBombPlanted(e.player, e.site); break;
            case BombBeginDefuseInput e: forwarder.OnBombBeginDefuse(e.player, e.hasKit); break;
            case BombAbortDefuseInput e: forwarder.OnBombAbortDefuse(e.player); break;
            case BombDefusedInput e: forwarder.OnBombDefused(e.player, e.timeRemaining, e.hasKit); break;
            case BombExplodedInput e: forwarder.OnBombExploded(); break;
            case PlayerHurtInput e: forwarder.OnPlayerHurt(e.victim, e.attacker, e.weapon, e.dmgHealth, e.dmgArmor, e.hitgroup, e.healthBefore, e.damageEntityId); break;
            case PlayerDeathInput e: forwarder.OnPlayerDeath(e.victim, e.attacker, e.assister, e.weapon, e.headshot, e.penetrated, e.thruSmoke, e.noScope, e.attackerBlind, e.nativeFlashAssist, e.attackerInAir, e.victimEquipmentValue, e.victimGrenadesHeld, e.damageEntityId); break;
            case RoundStartInput:
                if (match.Ending) break;
                clearNativeTracking();
                damage.Reset();
                var restores = match.Restores;
                match.OnRoundStart();
                // The engine seated everyone through the load; from here placement is the roster's
                // again. The match_restored is followed by the round_start alone.
                if (match.Restores != restores)
                {
                    teams.ReapplyAll();
                    forwarder.DropCutOffRound();
                }
                forwarder.OnRoundStart();
                roundStarted();
                break;
            case RoundEndInput e:
                // Noticed before anything is scored: the round a load cuts off ends without a winner.
                match.NoticeBackupLoading();
                match.NoticeRoundEnded();
                RoundEnd(e.winnerSide, e.reason);
                break;
            case DamageEnterInput e: damage.Enter((nint)e.entity); break;
            case DamageSnapshotInput e: damage.Capture(e.snapshot, e.buyPlayer); break;
            case DamageExitInput: damage.Exit(); break;
            case DamageReportedInput e: damage.Hurt((nint)e.entity, e.health); break;
            case MapEndInput: MapEnd(allowBeforeLive: false); break;
            case MapLoadInput: MapLoad(); break;
            case PlayerConnectInput e: PlayerConnect(e.player); break;
            case PlayerDisconnectInput e: PlayerDisconnect(e.player, e.reason); break;
            case PlayerTeamInput e:
                // Noticed before the lock sees the move: the engine is seating players by the file.
                match.NoticeBackupLoading();
                teams.OnPlayerTeam(e.player, e.to);
                match.MaybeStart();
                break;
            case ClientAuthorizedInput e: teams.OnClientAuthorized(e.player); break;
            case SideChoiceInput e: match.TrySideChoice(e.player, e.choice); break;
            case SideChoiceForSideInput e: match.TrySideChoiceForSide(e.player, e.side); break;
            case ForceReadyInput e: match.TryForceReady(e.player); break;
            case ReadyInput e: match.TrySetReady(e.player, e.ready); break;
            default: throw new ArgumentOutOfRangeException(nameof(input), input, "Unknown gameplay input");
        }
    }

    private void MapLoad()
    {
        if (match.Ending) return;
        match.OnMapLoad();
        forwarder.Reset();
        damage.Reset();
        clearNativeTracking();
        Reset();
        // Game rules are unavailable on the map-load tick; the host cancels this on map change.
        _mapLoadTimer.Schedule(1, () =>
        {
            if (match.Ending) return;
            match.EnterWarmup();
            teams.ReapplyAll();
            match.MaybeStart();
        });
    }

    /// <summary>End a match that never went live, such as on a passed ready deadline.</summary>
    public void EndUnplayed() => MapEnd(allowBeforeLive: true);

    private void PlayerConnect(HostPlayer player)
    {
        var rejected = teams.Connect(player);
        if (player.IsReportable) forwarder.OnPlayerConnect(player, rejected);
        if (!rejected) match.MaybeStart();
    }

    private void PlayerDisconnect(HostPlayer player, string reason)
    {
        if (player.IsReportable) forwarder.OnPlayerDisconnect(player, reason);
        teams.Forget(player.Slot);
        if (player.IsBot) return;
        // The departing controller may still exist. Recheck after cleanup; a reconnect wins.
        host.NextFrame(() =>
        {
            if (match.ResolvePlayerDeparture()) MapEnd(allowBeforeLive: true);
        });
    }

    private void RoundEnd(Side winnerSide, int engineReason)
    {
        if (host.IsWarmup) return;
        var revision = _revision;
        if (match.Phase == MatchPhase.Knife)
        {
            host.NextFrame(() =>
            {
                if (revision != _revision || match.Ending || match.Phase != MatchPhase.Knife) return;
                match.FinishKnifeRound(KnifeRound.Decide(host.Players), winnerSide);
            });
            return;
        }
        if (!match.IsLive || winnerSide == Side.Spec) return;
        var reason = RoundEndReasons.FromEngine(engineReason);
        forwarder.OnRoundEnd(winnerSide, reason);
        if (match.IsFinalRound() || RoundEndReasons.IsSurrender(reason)) MapEnd(false);
        else if (match.IsHalftimeBoundary())
        {
            var round = match.Round;
            var length = match.RoundTime;
            damage.RunAfterDamage(() =>
            {
                if (revision != _revision || !match.IsLive) return;
                forwarder.OnHalftime(length);
                clearNativeTracking();
                match.SwapSides();
            });
            // MVP and remaining callbacks in the ending tick still belong to this round.
            host.NextFrame(() => damage.RunAfterDamage(() =>
            {
                if (revision == _revision && !match.Ending && match.IsLive && match.Round == round)
                    forwarder.CompleteRound(length);
            }));
        }
    }

    /// <summary>
    /// CS2's round_end event does not guarantee that all damage callbacks have finished. Leave
    /// capture open through this tick, then finish any active damage before flushing the map. Every end
    /// request shares this boundary so a win-panel callback cannot close it early.
    /// </summary>
    private void MapEnd(bool allowBeforeLive)
    {
        if (match.Ending || (!allowBeforeLive && !match.IsLive)) return;
        match.BeginEnding();
        var revision = _revision;
        bool IsCurrent() => revision == _revision && match.Phase != MatchPhase.PostLive
            && (allowBeforeLive || match.IsLive);
        host.NextFrame(() =>
        {
            if (!IsCurrent()) return;
            damage.RunAfterDamage(() =>
            {
                if (!IsCurrent()) return;
                forwarder.OnMapEnd();
                stopRecording();
                clearNativeTracking();
                match.EnterPostLive();
                startShutdown();
            });
        });
    }
}
