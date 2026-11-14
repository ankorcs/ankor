using System.Text.Json.Serialization;
using Ankor.Core;

namespace Ankor.Core.Wire;

/// <summary>Mirror packages/protocol/src/game-events.ts. Preserve wire property names verbatim.
/// Nullable fields are emitted as null; optional fields use WhenWritingNull.
/// </summary>
public abstract class PluginMessage(string type)
{
    /// <summary>Declare type once so its JsonPropertyOrder applies to every subclass.</summary>
    [JsonPropertyOrder(-100)]
    [JsonPropertyName("type")]
    public string type { get; } = type;

    [JsonPropertyOrder(-88)] public long timestamp { get; set; }
    [JsonPropertyOrder(-87)] public double tick { get; set; }
}

public abstract class RoundScopedMessage(string type) : PluginMessage(type)
{
    [JsonPropertyOrder(-80)] public int round { get; set; }
    [JsonPropertyOrder(-79)] public double roundTime { get; set; }
}

/// <summary>`WorldPos` in game-events.ts.</summary>
public sealed class WirePosition
{
    public float x { get; set; }
    public float y { get; set; }
    public float z { get; set; }

    public static WirePosition From(WorldPosition position) => new() { x = position.X, y = position.Y, z = position.Z };

    public static WirePosition? From(WorldPosition? position) => position is { } p ? From(p) : null;
}

/// <summary>`PlayerPosition` in game-events.ts.</summary>
public sealed class WirePlayerPosition
{
    public float x { get; set; }
    public float y { get; set; }
    public float z { get; set; }
    public float pitch { get; set; }
    public float yaw { get; set; }

    public static WirePlayerPosition? From(HostPlayer? player) =>
        player is { Position: { } p, Angles: { } a }
            ? new() { x = p.X, y = p.Y, z = p.Z, pitch = Math.Clamp(a.Pitch, -90f, 90f), yaw = Yaw(a.Yaw) }
            : null;

    /// <summary>The engine can leave yaw unwrapped; the wire carries it within (-180, 180].</summary>
    private static float Yaw(float degrees)
    {
        var wrapped = degrees % 360f;
        return wrapped > 180f ? wrapped - 360f : wrapped <= -180f ? wrapped + 360f : wrapped;
    }
}

public class PlayerRef
{
    public string steamId { get; set; } = "";
    public string name { get; set; } = "";
    public Side side { get; set; }

    public int? team { get; set; }
}

/// <summary>`PlayerAt` in game-events.ts.</summary>
public sealed class PlayerAt : PlayerRef
{
    [JsonPropertyOrder(1)] public WirePlayerPosition? position { get; set; }
}

/// <summary>The match's map has loaded on this server; sent once, the first event of its history.</summary>
public sealed class MapStart() : PluginMessage("map_start")
{
    public string pluginVersion { get; set; } = "";
    public string frameworkVersion { get; set; } = "";
    public string serverVersion { get; set; } = "";
    public string mapName { get; set; } = "";
}

public sealed class PlayerConnect() : PluginMessage("player_connect")
{
    public PlayerRef player { get; set; } = new();
    public bool rejected { get; set; }
}

public sealed class PlayerDisconnect() : PluginMessage("player_disconnect")
{
    public PlayerRef player { get; set; } = new();
    public string reason { get; set; } = "";
}

public sealed class KnifeStart() : PluginMessage("knife_start");

public sealed class KnifeEnd() : PluginMessage("knife_end")
{
    public int winnerTeam { get; set; }
    public Side winnerSide { get; set; }
    public int team1Alive { get; set; }
    public int team2Alive { get; set; }
    public int team1Health { get; set; }
    public int team2Health { get; set; }
}

public sealed class SideChoice() : PluginMessage("side_choice")
{
    public int team { get; set; }
    public SideChoiceKind choice { get; set; }
    public Side team1Side { get; set; }
}

public sealed class GoingLive() : PluginMessage("going_live")
{
    public Side team1Side { get; set; }
    public int maxRounds { get; set; }
    public bool overtime { get; set; }
    public int overtimeMaxRounds { get; set; }
    public bool friendlyFire { get; set; }
}

/// <summary>
/// `MatchRules` in game-events.ts: what a launch asks for, and what <see cref="RoundBackup"/>
/// reports back. <see cref="GoingLive"/> carries the same keys flat.
/// </summary>
public sealed class MatchRules
{
    public required int maxRounds { get; set; }
    public required bool overtime { get; set; }
    public required int overtimeMaxRounds { get; set; }
    public required bool friendlyFire { get; set; }
}

public sealed class RoundBackup() : PluginMessage("round_backup")
{
    /// <summary><c>MAX_BACKUP_FILE_CHARS</c> in game-events.ts.</summary>
    public const int MaxFileChars = 512 * 1024;

    /// <summary><c>file</c>'s rule in game-events.ts: no control character but tab, LF and CR.</summary>
    public static bool IsWireText(string contents) =>
        !contents.Any(c => char.IsControl(c) && c is not ('\t' or '\n' or '\r'));

    public int round { get; set; }
    public Side team1Side { get; set; }
    public int team1Score { get; set; }
    public int team2Score { get; set; }
    public MatchRules rules { get; set; } = null!;
    public string file { get; set; } = "";
    public Roster roster { get; set; } = null!;
}

/// <summary>A backup was loaded; the <c>round_start</c> that follows is <see cref="round"/> played again.</summary>
public sealed class MatchRestored() : PluginMessage("match_restored")
{
    public int round { get; set; }
    public Side team1Side { get; set; }
    public int team1Score { get; set; }
    public int team2Score { get; set; }
}

public sealed class InventoryItem
{
    public int weapon { get; init; }
    public int quantity { get; init; }
    public int unitValue { get; init; }
}

public sealed class PlayerInventory
{
    public IReadOnlyList<InventoryItem> items { get; init; } = [];
    public int armor { get; init; }
    public bool helmet { get; init; }
    public bool defuseKit { get; init; }
}

public sealed class RoundCarryover
{
    public int fromRound { get; init; }
    public double sampledTick { get; init; }
    public List<CarryoverPlayer> players { get; init; } = [];
}

public class RoundPlayer
{
    public PlayerRef player { get; set; } = new();
    public bool alive { get; set; }
    public int equipmentValue { get; set; }
    public int cash { get; set; }
    public int startCash { get; set; }
    public int cashSpent { get; set; }
}

public sealed class CarryoverPlayer : RoundPlayer
{
    public PlayerInventory? inventory { get; set; }
}

public sealed class PlayerBuy
{
    public PlayerRef player { get; set; } = new();
    public int equipmentValue { get; set; }
    public int cash { get; set; }
    public int startCash { get; set; }
    public int cashSpent { get; set; }
    public double sampledTick { get; set; }
}

public sealed class RoundStart() : RoundScopedMessage("round_start")
{
    public Side team1Side { get; set; }
    public List<RoundPlayer> players { get; set; } = [];
    public RoundCarryover? carryover { get; set; }
}

public sealed class RoundFreezeEnd() : RoundScopedMessage("round_freeze_end")
{
    public Side team1Side { get; set; }
    public List<RoundPlayer> players { get; set; } = [];
}

/// <summary>Individual buy samples and their team sums; sampling rules live in EventForwarder.</summary>
public sealed class RoundBuyEnd() : RoundScopedMessage("round_buy_end")
{
    public List<PlayerBuy> players { get; set; } = [];
    public int team1EquipmentValue { get; set; }
    public int team2EquipmentValue { get; set; }
}

public sealed class RoundEnd() : RoundScopedMessage("round_end")
{
    public Side winnerSide { get; set; }
    public int winnerTeam { get; set; }
    public RoundEndReason reason { get; set; }
    public int team1Score { get; set; }
    public int team2Score { get; set; }
    public double roundLength { get; set; }
}

/// <summary>All gameplay reports for this scored round have been emitted.</summary>
public sealed class RoundComplete() : RoundScopedMessage("round_complete");

public sealed class PlayerDeath() : RoundScopedMessage("player_death")
{
    public PlayerAt victim { get; set; } = new();
    public PlayerAt? attacker { get; set; }
    public PlayerRef? assister { get; set; }
    public Weapon weapon { get; set; } = new();
    public DamageCause cause { get; set; }
    public bool headshot { get; set; }
    public int penetrated { get; set; }
    public bool thruSmoke { get; set; }
    public bool noScope { get; set; }
    public bool attackerBlind { get; set; }
    public bool attackerInAir { get; set; }
    public bool flashAssist { get; set; }
    public bool friendlyFire { get; set; }

    // Who last blinded the victim while the blind was still running, and how many seconds it had
    // left: null and 0 when the victim could see.
    public PlayerRef? victimBlindBy { get; set; }
    public double victimBlindRemaining { get; set; }

    // What the victim carried when the killing blow landed, in dollars, as the engine counts it.
    public int victimEquipmentValue { get; set; }

    // Grenades left in the victim's inventory at that same moment; the C4 and the knife are not
    // grenades, and two flashbangs count as two.
    // Null means unreadable; an empty array means the victim carried no grenades.
    public IReadOnlyList<InventoryItem>? victimGrenadesHeld { get; set; }
}

public sealed class PlayerHurt() : RoundScopedMessage("player_hurt")
{
    /// <summary><c>MAX_DAMAGE</c> in game-events.ts.</summary>
    public const int MaxDamage = 1000;

    public PlayerAt victim { get; set; } = new();
    public PlayerAt? attacker { get; set; }
    public Weapon weapon { get; set; } = new();
    public DamageCause cause { get; set; }
    public int dmgHealth { get; set; }
    // Raw damage can exceed remaining HP. The controller uses this to calculate effective damage.
    public int? healthBefore { get; set; }
    public int healthAfter { get; set; }
    public int dmgArmor { get; set; }
    public int hitgroup { get; set; }
    public bool friendlyFire { get; set; }
    // The grenade itself struck the player before detonating: damage, but not utility damage.
    public bool projectileImpact { get; set; }
    public long? grenadeId { get; set; }
}

public sealed class PlayerBlind() : RoundScopedMessage("player_blind")
{
    public PlayerAt victim { get; set; } = new();
    public PlayerAt attacker { get; set; } = new();
    public double duration { get; set; }
    public bool friendlyFire { get; set; }
    public long? grenadeId { get; set; }
}

public sealed class GrenadeThrow() : RoundScopedMessage("grenade_throw")
{
    public long grenadeId { get; set; }
    // Placed where the player stood when the projectile spawned.
    public PlayerAt player { get; set; } = new();
    public GrenadeKind grenade { get; set; }
}

public sealed class GrenadeDetonate() : RoundScopedMessage("grenade_detonate")
{
    public long grenadeId { get; set; }
    public PlayerRef player { get; set; } = new();
    public WirePosition? detonatePos { get; set; }
}

/// <summary>Not round-scoped: it can be sent after the round clock reset, so it keeps the throw's round.</summary>
public sealed class GrenadeEnd() : PluginMessage("grenade_end")
{
    public int round { get; set; }
    public long grenadeId { get; set; }
    public PlayerRef player { get; set; } = new();
    public double endRoundTime { get; set; }
    public FireEnd? fireEnd { get; set; }
    public long? extinguishedBy { get; set; }
}

/// <summary>The shots every player fired over one round, posted once when the round closes.</summary>
public sealed class RoundShots() : PluginMessage("round_shots")
{
    /// <summary><c>MAX_ROUND_SHOTS</c> in game-events.ts: shots one player fires in a round, over all weapons.</summary>
    public const int MaxShots = 4096;

    public int round { get; set; }
    /// <summary>Each player who fired; empty when nobody did.</summary>
    public IReadOnlyList<PlayerShots> players { get; set; } = [];
}

/// <summary><c>PlayerShots</c> in game-events.ts.</summary>
public sealed class PlayerShots
{
    public PlayerRef player { get; set; } = new();
    public IReadOnlyList<WeaponShots> weapons { get; set; } = [];
}

/// <summary><c>WeaponShots</c> in game-events.ts.</summary>
public sealed class WeaponShots
{
    public Weapon weapon { get; set; } = new();
    /// <summary>The number of <see cref="ticks"/>.</summary>
    public int shots { get; set; }
    public IReadOnlyList<double> ticks { get; set; } = [];
}

/// <summary><c>Weapon</c> in game-events.ts: a weapon by its item index or, without one, its engine class name.</summary>
public sealed class Weapon
{
    public int? id { get; set; }
    public string? fallback { get; set; }

    public static Weapon Of(WeaponReference weapon) => new() { id = weapon.Id, fallback = weapon.Fallback };
}

public sealed class BombBeginPlant() : RoundScopedMessage("bomb_begin_plant")
{
    public PlayerAt player { get; set; } = new();
    public BombSite? site { get; set; }
}

public sealed class BombAbortPlant() : RoundScopedMessage("bomb_abort_plant")
{
    public PlayerAt player { get; set; } = new();
    public BombSite? site { get; set; }
}

public sealed class BombPlanted() : RoundScopedMessage("bomb_planted")
{
    public PlayerAt player { get; set; } = new();
    public BombSite? site { get; set; }
}

public sealed class BombBeginDefuse() : RoundScopedMessage("bomb_begin_defuse")
{
    public PlayerAt player { get; set; } = new();
    public bool hasKit { get; set; }
}

public sealed class BombAbortDefuse() : RoundScopedMessage("bomb_abort_defuse")
{
    public PlayerAt player { get; set; } = new();
}

public sealed class BombDefused() : RoundScopedMessage("bomb_defused")
{
    public PlayerAt player { get; set; } = new();
    public double timeRemaining { get; set; }
    /// <summary>Read off the pawn: the engine's bomb_defused event says nothing about the kit.</summary>
    public bool hasKit { get; set; }
}

public sealed class BombExploded() : RoundScopedMessage("bomb_exploded");

public sealed class RoundMvp() : RoundScopedMessage("round_mvp")
{
    public PlayerRef player { get; set; } = new();
    public int reason { get; set; }
}

/// <summary>Why a map ended, as <c>MapEndReason</c> in game-events.ts names it.</summary>
public static class MapEndReasons
{
    public const string Played = "played";
    public const string ReadyTimeout = "ready_timeout";
    public const string Abandoned = "abandoned";
}

/// <summary>A team's rostered players on the server when the ready deadline passed, and whether it was ready.</summary>
public sealed class ReadyTeam
{
    public int connected { get; set; }
    public bool ready { get; set; }
}

public sealed class ReadyTeams
{
    public ReadyTeam team1 { get; set; } = new();
    public ReadyTeam team2 { get; set; } = new();
    public ReadyTeam? spectators { get; set; }
}

public sealed class MapEnd() : PluginMessage("map_end")
{
    public string reason { get; set; } = MapEndReasons.Played;
    public int? winnerTeam { get; set; }
    public int team1Score { get; set; }
    public int team2Score { get; set; }
    public ReadyTeams? teams { get; set; }
}

public sealed class PluginLog() : PluginMessage("log")
{
    public LogLevel level { get; set; }
    public string message { get; set; } = "";
}

/// <summary>Text as the wire contract takes it: no NUL, no lone surrogate, cut to a length.</summary>
public static class WireText
{
    /// <summary>`MAX_PLAYER_NAME_CHARS` in game-events.ts.</summary>
    public const int MaxPlayerName = 64;

    /// <summary>`MAX_LOG_MESSAGE_CHARS` in game-events.ts.</summary>
    public const int MaxLogMessage = 8192;

    /// <summary>`MAP_NAME_MAX_CHARS` in agent-socket.ts.</summary>
    public const int MaxMapName = 64;

    /// <summary>`CONFIG_ERROR_MAX_CHARS` in agent-socket.ts.</summary>
    public const int MaxConfigError = 512;

    /// <summary>`MAX_VERSION_CHARS` in game-events.ts.</summary>
    public const int MaxVersion = 128;

    /// <summary>`MAX_DISCONNECT_REASON_CHARS` in game-events.ts.</summary>
    public const int MaxDisconnectReason = 64;

    /// <summary>
    /// Drops what the contract refuses, and every control character when <paramref name="controls"/>
    /// is false, then cuts to <paramref name="max"/> code points, which the contract counts, so a
    /// surrogate pair counts as one and is never split.
    /// </summary>
    public static string Clean(string text, int max, bool controls = true)
    {
        var kept = new System.Text.StringBuilder(Math.Min(text.Length, max));
        var count = 0;
        for (var i = 0; i < text.Length && count < max; i++)
        {
            var c = text[i];
            if (char.IsHighSurrogate(c) && i + 1 < text.Length && char.IsLowSurrogate(text[i + 1]))
            {
                kept.Append(c).Append(text[++i]);
                count++;
                continue;
            }
            if (char.IsSurrogate(c) || c == '\0' || (!controls && char.IsControl(c))) continue;
            kept.Append(c);
            count++;
        }
        return kept.ToString();
    }
}
