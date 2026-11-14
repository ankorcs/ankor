using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// Copied inputs to the shared gameplay handlers. Native damage addresses are opaque identity
/// tokens; replay never dereferences them. These inputs contain no native objects.
///
/// Capture audit: player snapshots, damage snapshots, weapon resolution, bomb site/kit/time and
/// grenade entity/kind/position are copied here. GameplayCallbacks owns round/halftime/map decisions.
/// Native validity checks, projectile-to-inferno matching, weapon-source lookup
/// and SDK reads remain adapter responsibilities, not claims made by core replay tests.
/// Their diagnostic observations remain in the same capture, alongside these replayable inputs.
/// </summary>
public abstract record GameplayInput;

public sealed record RoundPrestartInput() : GameplayInput;
public sealed record RoundStartInput() : GameplayInput;
public sealed record RoundFreezeEndInput() : GameplayInput;
public sealed record RoundEndInput(Side winnerSide, int reason) : GameplayInput;
public sealed record RoundMvpInput(HostPlayer player, int reason) : GameplayInput;
public sealed record WeaponFireInput(HostPlayer shooter, WeaponReference weapon) : GameplayInput;
public sealed record GrenadeSpawnedInput(HostPlayer thrower, GrenadeKind kind, int entityId) : GameplayInput;
public sealed record GrenadeDetonateInput(int entityId, WorldPosition position) : GameplayInput;
public sealed record GrenadeFireInput(int projectileId, int fireId) : GameplayInput;
public sealed record GrenadeRemovedInput(int entityId, bool smothered = false, WorldPosition? origin = null) : GameplayInput;
public sealed record FireExtinguishedInput(int fireId, WorldPosition position) : GameplayInput;
public sealed record FireExpiredInput(int fireId) : GameplayInput;
public sealed record PlayerBlindInput(HostPlayer victim, HostPlayer? attacker, double duration, int entityId) : GameplayInput;
public sealed record ExitBuyZoneInput(HostPlayer player) : GameplayInput;
public sealed record BombBeginPlantInput(HostPlayer player, BombSite? site) : GameplayInput;
public sealed record BombAbortPlantInput(HostPlayer player, BombSite? site) : GameplayInput;
public sealed record BombPlantedInput(HostPlayer player, BombSite? site) : GameplayInput;
public sealed record BombBeginDefuseInput(HostPlayer player, bool hasKit) : GameplayInput;
public sealed record BombAbortDefuseInput(HostPlayer player) : GameplayInput;
public sealed record BombDefusedInput(HostPlayer player, double timeRemaining, bool hasKit) : GameplayInput;
public sealed record BombExplodedInput() : GameplayInput;
public sealed record PlayerHurtInput(HostPlayer victim, HostPlayer? attacker, WeaponReference weapon, int dmgHealth, int dmgArmor, int hitgroup, int? healthBefore = null, int damageEntityId = 0) : GameplayInput;
public sealed record PlayerDeathInput(HostPlayer victim, HostPlayer? attacker, HostPlayer? assister, WeaponReference weapon, bool headshot, int penetrated, bool thruSmoke, bool noScope, bool attackerBlind, bool nativeFlashAssist, bool attackerInAir, int victimEquipmentValue, IReadOnlyList<InventoryItem>? victimGrenadesHeld, int damageEntityId = 0) : GameplayInput;
public sealed record DamageEnterInput(long entity) : GameplayInput;
public sealed record DamageSnapshotInput(DamageSnapshot snapshot, HostPlayer? buyPlayer) : GameplayInput;
public sealed record DamageExitInput() : GameplayInput;
public sealed record DamageReportedInput(long entity, int health) : GameplayInput;
public sealed record MapEndInput : GameplayInput;
public sealed record MapLoadInput() : GameplayInput;
public sealed record PlayerConnectInput(HostPlayer player) : GameplayInput;
public sealed record PlayerDisconnectInput(HostPlayer player, string reason) : GameplayInput;
public sealed record PlayerTeamInput(HostPlayer player, Side from, Side to) : GameplayInput;
public sealed record ClientAuthorizedInput(HostPlayer player) : GameplayInput;
public sealed record SideChoiceInput(HostPlayer player, SideChoiceKind choice) : GameplayInput;
public sealed record SideChoiceForSideInput(HostPlayer player, Side side) : GameplayInput;
public sealed record ForceReadyInput(HostPlayer player) : GameplayInput;
public sealed record ReadyInput(HostPlayer player, bool ready) : GameplayInput;
