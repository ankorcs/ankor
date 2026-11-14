using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

/// <summary>
/// Guards the JSON contract in <c>packages/protocol/src/game-events.ts</c>. The expected property
/// names below are transcribed from that file by hand; if the zod schema changes, these tests are
/// where the plugin finds out.
///
/// Nullable zod fields (<c>.nullable()</c>) require the key to be present with a null value, so
/// "missing" and "null" are asserted separately from optional fields (<c>.optional()</c>).
/// </summary>
public class WireSchemaTests
{
    /// <summary>Shared fields on every plugin event.</summary>
    private static readonly string[] Base = ["type", "timestamp", "tick"];

    /// <summary>`roundScoped` = base plus these.</summary>
    private static readonly string[] RoundScoped = [.. Base, "round", "round_time"];

    /// <summary>`PlayerRef`.</summary>
    private static readonly string[] PlayerRefKeys = ["steam_id", "name", "side", "team"];

    /// <summary>`PlayerAt`.</summary>
    private static readonly string[] PlayerAtKeys = [.. PlayerRefKeys, "position"];

    private static JsonElement Parse(PluginMessage message) =>
        JsonDocument.Parse(WireJson.Serialize(message)).RootElement;

    private static void AssertKeys(PluginMessage message, params string[] extra)
    {
        var actual = Parse(message).EnumerateObject().Select(p => p.Name).ToHashSet(StringComparer.Ordinal);
        var expected = new HashSet<string>(extra, StringComparer.Ordinal);

        Assert.Equal(expected.OrderBy(k => k, StringComparer.Ordinal),
            actual.OrderBy(k => k, StringComparer.Ordinal));
    }

    private static string[] With(string[] head, params string[] tail) => [.. head, .. tail];

    // Discriminators

    /// <summary>Every literal in the <c>PluginToControllerMessage</c> discriminated union.</summary>
    public static TheoryData<string, PluginMessage> AllTypes() => new()
    {
        { "map_start", new MapStart() },
        { "player_connect", new PlayerConnect() },
        { "player_disconnect", new PlayerDisconnect() },
        { "knife_start", new KnifeStart() },
        { "knife_end", new KnifeEnd { winnerTeam = 1 } },
        { "side_choice", new SideChoice { team = 1 } },
        { "going_live", new GoingLive() },
        { "round_backup", new RoundBackup() },
        { "match_restored", new MatchRestored() },
        { "round_start", new RoundStart() },
        { "round_freeze_end", new RoundFreezeEnd() },
        { "round_buy_end", new RoundBuyEnd() },
        { "round_end", new RoundEnd { winnerTeam = 1 } },
        { "round_complete", new RoundComplete { round = 1 } },
        { "player_death", new PlayerDeath() },
        { "player_hurt", new PlayerHurt() },
        { "player_blind", new PlayerBlind() },
        { "grenade_throw", new GrenadeThrow() },
        { "grenade_detonate", new GrenadeDetonate() },
        { "grenade_end", new GrenadeEnd() },
        { "round_shots", new RoundShots { round = 1 } },
        { "bomb_begin_plant", new BombBeginPlant() },
        { "bomb_abort_plant", new BombAbortPlant() },
        { "bomb_planted", new BombPlanted() },
        { "bomb_begin_defuse", new BombBeginDefuse() },
        { "bomb_abort_defuse", new BombAbortDefuse() },
        { "bomb_defused", new BombDefused() },
        { "bomb_exploded", new BombExploded() },
        { "round_mvp", new RoundMvp() },
        { "map_end", new MapEnd() },
        { "log", new PluginLog() },
    };

    [Theory]
    [MemberData(nameof(AllTypes))]
    public void EveryMessageIsStampedWithTheEngineTickAndBaseFields(string expected, PluginMessage message)
    {
        var host = new FakeGameHost { TickCount = 8192, SubtickFraction = 0.171875 };
        var sent = new List<PluginMessage>();
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(sent), Fixture.Messages, () => { });
        match.Emit(message);

        var json = Parse(Assert.Single(sent));
        Assert.Equal(expected, json.GetProperty("type").GetString());
        Assert.Equal(8192.171875, json.GetProperty("tick").GetDouble());
        Assert.False(json.TryGetProperty("match_id", out _));
        Assert.Equal(1_700_000_000_000, json.GetProperty("timestamp").GetInt64());
    }

    // Field-by-field, per schema object

    [Fact]
    public void MapStartMatchesSchema() =>
        AssertKeys(new MapStart(),
            With(Base, "plugin_version", "framework_version", "server_version", "map_name"));

    [Fact]
    public void PlayerConnectMatchesSchema() =>
        AssertKeys(new PlayerConnect(), With(Base, "player", "rejected"));

    [Fact]
    public void PlayerDisconnectMatchesSchema() =>
        AssertKeys(new PlayerDisconnect(), With(Base, "player", "reason"));

    [Fact]
    public void KnifeStartCarriesOnlyTheBaseFields() => AssertKeys(new KnifeStart(), Base);

    [Fact]
    public void KnifeEndMatchesSchema() =>
        AssertKeys(new KnifeEnd(), With(Base, "winner_team", "winner_side", "team1_alive", "team2_alive", "team1_health", "team2_health"));

    [Fact]
    public void SideChoiceMatchesSchema() =>
        AssertKeys(new SideChoice(), With(Base, "team", "choice", "team1_side"));

    [Fact]
    public void GoingLiveMatchesSchema() =>
        AssertKeys(new GoingLive(), With(Base, "team1_side", "max_rounds", "overtime", "overtime_max_rounds", "friendly_fire"));

    [Fact]
    public void RoundBackupMatchesSchema() =>
        AssertKeys(new RoundBackup(), With(Base, "round", "team1_side", "team1_score", "team2_score", "rules", "file", "roster"));

    [Fact]
    public void MatchRestoredMatchesSchema() =>
        AssertKeys(new MatchRestored(), With(Base, "round", "team1_side", "team1_score", "team2_score"));

    [Fact]
    public void ARestoreRoundNamesItsRound()
    {
        var message = WireJson.ParseInbound("""{"type":"restore_round","round":14,"request_id":"r1"}""");
        Assert.Equal(14, Assert.IsType<AgentMessage.RestoreRound>(message).round);
        Assert.Equal("r1", ((AgentMessage.RestoreRound)message).requestId);
        Assert.Null(WireJson.ParseInbound("""{"type":"restore_round","round":14}"""));
    }

    [Fact]
    public void AStartMatchCarriesItsRequestId()
    {
        var message = WireJson.ParseInbound("""{"type":"start_match","request_id":"r1"}""");
        Assert.Equal("r1", Assert.IsType<AgentMessage.StartMatch>(message).requestId);
        Assert.Null(WireJson.ParseInbound("""{"type":"start_match"}"""));
    }

    [Fact]
    public void ACommandResultSpellsItsKeys()
    {
        Assert.Equal("""{"type":"command_result","request_id":"r1","error":null}""", CommandResult.Serialize("r1", null));
        Assert.Equal("""{"type":"command_result","request_id":"r1","error":"team_empty"}""", CommandResult.Serialize("r1", CommandResult.TeamEmpty));
    }

    [Fact]
    public void RoundStartMatchesSchema() =>
        AssertKeys(new RoundStart(), With(RoundScoped, "team1_side", "players", "carryover"));

    [Fact]
    public void RoundFreezeEndMatchesSchema() =>
        AssertKeys(new RoundFreezeEnd(), With(RoundScoped, "team1_side", "players"));

    [Fact]
    public void RoundCompleteMatchesSchema() =>
        AssertKeys(new RoundComplete(), RoundScoped);

    [Fact]
    public void RoundBuyEndMatchesSchema() =>
        AssertKeys(new RoundBuyEnd(), With(RoundScoped, "team1_equipment_value", "team2_equipment_value", "players"));

    [Fact]
    public void RoundSnapshotsEmitRequiredInventoryAndNumericEconomy()
    {
        var start = Parse(new RoundStart { players = [new RoundPlayer()] });
        var player = start.GetProperty("players")[0];
        Assert.False(player.TryGetProperty("inventory", out _));
        Assert.Equal(JsonValueKind.Null, start.GetProperty("carryover").ValueKind);
        var buy = Parse(new RoundBuyEnd { players = [new PlayerBuy()] }).GetProperty("players")[0];
        foreach (var field in new[] { "equipment_value", "cash", "start_cash", "cash_spent" })
        {
            Assert.Equal(0, player.GetProperty(field).GetInt32());
            Assert.Equal(0, buy.GetProperty(field).GetInt32());
        }
    }

    [Fact]
    public void RoundEndMatchesSchema() =>
        AssertKeys(new RoundEnd(),
            With(RoundScoped, "winner_side", "winner_team", "reason", "team1_score", "team2_score",
                "round_length"));

    [Fact]
    public void PlayerDeathMatchesSchema() =>
        AssertKeys(new PlayerDeath(),
            With(RoundScoped, "victim", "attacker", "assister", "weapon", "cause", "headshot", "penetrated",
                "thru_smoke", "no_scope", "attacker_blind", "attacker_in_air", "flash_assist", "friendly_fire",
                "victim_blind_by", "victim_blind_remaining", "victim_equipment_value", "victim_grenades_held"));

    [Fact]
    public void PlayerHurtMatchesSchema() =>
        AssertKeys(new PlayerHurt(),
            With(RoundScoped, "victim", "attacker", "weapon", "cause", "dmg_health", "health_before", "health_after", "dmg_armor", "hitgroup",
                "friendly_fire", "projectile_impact", "grenade_id"));

    /// <summary>`PlayerAt`: `PlayerRef` plus `position`, nullable and always present.</summary>
    [Fact]
    public void PlayerAtPositionIsNullableAndFlat()
    {
        var unplaced = Parse(new PlayerHurt { attacker = new PlayerAt() });
        Assert.Equal(PlayerAtKeys.Order(StringComparer.Ordinal),
            unplaced.GetProperty("victim").EnumerateObject().Select(p => p.Name).Order(StringComparer.Ordinal));
        Assert.Equal(JsonValueKind.Null, unplaced.GetProperty("attacker").GetProperty("position").ValueKind);

        var json = Parse(new PlayerHurt
        {
            victim = new PlayerAt { position = new WirePlayerPosition { x = 1.5f, y = -2, z = 3, pitch = 4, yaw = -90.25f } },
        });
        var position = json.GetProperty("victim").GetProperty("position");
        Assert.Equal(["x", "y", "z", "pitch", "yaw"], position.EnumerateObject().Select(p => p.Name).ToArray());
        Assert.Equal(-90.25, position.GetProperty("yaw").GetDouble());
    }

    [Fact]
    public void PlayerBlindMatchesSchema() =>
        AssertKeys(new PlayerBlind(),
            With(RoundScoped, "victim", "attacker", "duration", "friendly_fire", "grenade_id"));

    [Fact]
    public void GrenadeThrowMatchesSchema() =>
        AssertKeys(new GrenadeThrow(), With(RoundScoped, "grenade_id", "player", "grenade"));

    [Fact]
    public void GrenadeDetonateMatchesSchema() =>
        AssertKeys(new GrenadeDetonate(), With(RoundScoped, "grenade_id", "player", "detonate_pos"));

    [Fact]
    public void GrenadeEndMatchesSchema() =>
        AssertKeys(new GrenadeEnd(), With(Base, "round", "grenade_id", "player", "end_round_time", "fire_end", "extinguished_by"));

    [Fact]
    public void GrenadeFollowUpsCarryTheThrowersIdentityWithoutPosition()
    {
        var thrower = new PlayerAt { steamId = "76561198000000001", position = new() };
        Assert.False(Parse(new GrenadeDetonate { player = thrower }).GetProperty("player").TryGetProperty("position", out _));
        Assert.False(Parse(new GrenadeEnd { player = thrower }).GetProperty("player").TryGetProperty("position", out _));
    }

    [Fact]
    public void GrenadePositionsAreNullableNotOptional()
    {
        // detonate_pos is .nullable(), the key always present.
        Assert.Equal(JsonValueKind.Null, Parse(new GrenadeDetonate()).GetProperty("detonate_pos").ValueKind);
    }

    [Fact]
    public void RoundShotsMatchesSchema() =>
        AssertKeys(new RoundShots(), With(Base, "round", "players"));

    [Fact]
    public void BombPlantedMatchesSchema() =>
        AssertKeys(new BombPlanted(), With(RoundScoped, "player", "site"));

    [Fact]
    public void BombDefusedMatchesSchema() =>
        AssertKeys(new BombDefused(), With(RoundScoped, "player", "time_remaining", "has_kit"));

    [Fact]
    public void BombExplodedCarriesOnlyTheRoundScopedFields() =>
        AssertKeys(new BombExploded(), RoundScoped);

    [Fact]
    public void RoundMvpMatchesSchema() =>
        AssertKeys(new RoundMvp(), With(RoundScoped, "player", "reason"));

    [Fact]
    public void MapEndMatchesSchema() =>
        AssertKeys(new MapEnd(), With(Base, "reason", "winner_team", "team1_score", "team2_score", "teams"));

    [Theory]
    [InlineData("ok\u0000name", 128, true, "okname")]
    [InlineData("tab\there", 128, false, "tabhere")]
    [InlineData("line\nkept", 128, true, "line\nkept")]
    [InlineData("ab\uD83D\uDE00", 3, true, "ab\uD83D\uDE00")]
    [InlineData("ab\uD83D\uDE00c", 3, true, "ab\uD83D\uDE00")]
    [InlineData("\uD83D\uDE00\uD83D\uDE00\uD83D\uDE00", 2, true, "\uD83D\uDE00\uD83D\uDE00")]
    [InlineData("abcdef", 4, true, "abcd")]
    [InlineData("a\uD83D\uDE00b", 128, false, "a\uD83D\uDE00b")]
    public void WireTextKeepsWhatTheContractTakes(string text, int max, bool controls, string expected) =>
        Assert.Equal(expected, WireText.Clean(text, max, controls));

    [Theory]
    [InlineData(-170f, -170f)]
    [InlineData(-180f, 180f)]
    [InlineData(-400f, -40f)]
    [InlineData(540f, 180f)]
    public void YawIsWrappedIntoTheContractsHalfOpenRange(float engine, float wire)
    {
        var player = Fixture.Player(Fixture.Team1Player1, Side.Ct,
            position: new WorldPosition(0, 0, 0), angles: new ViewAngles(0, engine));

        Assert.Equal(wire, WirePlayerPosition.From(player)!.yaw);
    }

    [Fact]
    public void WireTextDropsALoneSurrogate() =>
        // Built at run time: attribute strings cannot hold a lone surrogate.
        Assert.Equal("lonehalf", WireText.Clean("lone" + (char)0xD800 + "half", 128));

    [Fact]
    public void LogMatchesSchema() => AssertKeys(new PluginLog(), With(Base, "level", "message"));

    [Fact]
    public void PlayerRefMatchesSchema()
    {
        var json = Parse(new PlayerConnect
        {
            player = new PlayerRef { steamId = Fixture.Team1Player1, name = "a", side = Side.Ct, team = 1 },
        });

        var keys = json.GetProperty("player").EnumerateObject().Select(p => p.Name);
        Assert.Equal(
            PlayerRefKeys.OrderBy(k => k, StringComparer.Ordinal),
            keys.OrderBy(k => k, StringComparer.Ordinal));
    }

    // Nullable vs optional

    [Fact]
    public void NullableFieldsAreWrittenAsExplicitNulls()
    {
        // zod .nullable() requires the key to exist; omitting it fails validation.
        var death = Parse(new PlayerDeath());
        Assert.Equal(JsonValueKind.Null, death.GetProperty("attacker").ValueKind);
        Assert.Equal(JsonValueKind.Null, death.GetProperty("assister").ValueKind);
        // Optional in the schema, but the plugin always states whether the victim was blinded.
        Assert.Equal(JsonValueKind.Null, death.GetProperty("victim_blind_by").ValueKind);
        Assert.Equal(0, death.GetProperty("victim_blind_remaining").GetDouble());

        var mapEnd = Parse(new MapEnd());
        Assert.Equal(JsonValueKind.Null, mapEnd.GetProperty("winner_team").ValueKind);

        var reference = Parse(new PlayerConnect()).GetProperty("player");
        Assert.Equal(JsonValueKind.Null, reference.GetProperty("team").ValueKind);
    }

    [Fact]
    public void GrenadeEndFireFieldsAreAlwaysPresentAndFireEndSerializesLowercase()
    {
        var plain = Parse(new GrenadeEnd());
        Assert.Equal(JsonValueKind.Null, plain.GetProperty("extinguished_by").ValueKind);
        Assert.Equal(JsonValueKind.Null, plain.GetProperty("fire_end").ValueKind);
        Assert.Equal(JsonValueKind.Number, plain.GetProperty("end_round_time").ValueKind);

        var fire = Parse(new GrenadeEnd { fireEnd = FireEnd.Smothered, endRoundTime = 12.5, extinguishedBy = 3 });
        Assert.Equal("smothered", fire.GetProperty("fire_end").GetString());
        Assert.Equal(12.5, fire.GetProperty("end_round_time").GetDouble());
        Assert.Equal(3, fire.GetProperty("extinguished_by").GetInt64());
    }

    // Enum wire values

    [Theory]
    [InlineData(Side.T, "t")]
    [InlineData(Side.Ct, "ct")]
    [InlineData(Side.Spec, "spec")]
    public void SidesSerializeLowercase(Side side, string expected)
    {
        var json = Parse(new MatchRestored { team1Side = side });
        Assert.Equal(expected, json.GetProperty("team1_side").GetString());
    }

    [Theory]
    [InlineData(SideChoiceKind.Stay, "stay")]
    [InlineData(SideChoiceKind.Swap, "swap")]
    public void SideChoiceKindSerializesLowercase(SideChoiceKind choice, string expected)
    {
        var json = Parse(new SideChoice { choice = choice });
        Assert.Equal(expected, json.GetProperty("choice").GetString());
    }

    [Theory]
    [InlineData(GrenadeKind.Flashbang, "flashbang")]
    [InlineData(GrenadeKind.HeGrenade, "hegrenade")]
    [InlineData(GrenadeKind.SmokeGrenade, "smokegrenade")]
    [InlineData(GrenadeKind.Molotov, "molotov")]
    [InlineData(GrenadeKind.IncGrenade, "incgrenade")]
    [InlineData(GrenadeKind.Decoy, "decoy")]
    public void GrenadeKindsMatchTheSchemaTokens(GrenadeKind kind, string expected)
    {
        var json = Parse(new GrenadeThrow { grenade = kind });
        Assert.Equal(expected, json.GetProperty("grenade").GetString());
    }

    [Theory]
    [InlineData(LogLevel.Debug, "debug")]
    [InlineData(LogLevel.Info, "info")]
    [InlineData(LogLevel.Warn, "warn")]
    [InlineData(LogLevel.Error, "error")]
    public void LogLevelsMatchTheSchemaTokens(LogLevel level, string expected)
    {
        var json = Parse(new PluginLog { level = level });
        Assert.Equal(expected, json.GetProperty("level").GetString());
    }

    [Theory]
    [InlineData(BombSite.A, "a")]
    [InlineData(BombSite.B, "b")]
    public void BombSitesAreLowercase(BombSite site, string expected)
    {
        var json = Parse(new BombPlanted { site = site });
        Assert.Equal(expected, json.GetProperty("site").GetString());
    }

    [Fact]
    public void SteamIdsTravelAsDecimalStrings()
    {
        // The schema is z.string().regex(/^\d{17}$/): never a JSON number.
        var host = new FakeGameHost();
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(), Fixture.Messages, () => { });
        var reference = match.RefFor(Fixture.Player(Fixture.Team1Player1, Side.Ct));

        var json = Parse(new PlayerConnect { player = reference });
        var steamId = json.GetProperty("player").GetProperty("steam_id");

        Assert.Equal(JsonValueKind.String, steamId.ValueKind);
        Assert.Equal(17, steamId.GetString()!.Length);
        Assert.Equal(Fixture.Team1Player1, steamId.GetString());
    }

    // Snapshots

    [Fact]
    public void PlayerDeathSnapshot()
    {
        var host = new FakeGameHost { RoundsPlayed = 4, TickCount = 8192 };
        var sent = new List<PluginMessage>();
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(sent), Fixture.Messages, () => { });
        var forwarder = new EventForwarder(match, host);

        match.GoLive();
        host.Advance(3);
        host.IsWarmup = false;
        match.OnRoundStart();
        host.RoundStartTime = host.CurrentTime;
        host.FreezePeriod = false;
        host.Advance(12.5);
        sent.Clear();

        forwarder.OnPlayerDeath(
            victim: Fixture.Player(Fixture.Team2Player1, Side.T, 2, health: 0, alive: false, name: "bravo1",
                position: new WorldPosition(-512, 1024.5f, 64), angles: new ViewAngles(10, -45)),
            attacker: Fixture.Player(Fixture.Team1Player1, Side.Ct, 0, health: 34, name: "alpha1"),
            assister: Fixture.Player(Fixture.Team1Player2, Side.Ct, 1, health: 100, name: "alpha2"),
            weapon: new WeaponReference(7, null),
            headshot: true,
            penetrated: 1,
            thruSmoke: false,
            noScope: false,
            attackerBlind: false,
            nativeFlashAssist: true,
            attackerInAir: false, victimEquipmentValue: 0, victimGrenadesHeld: [new() { weapon = 43, quantity = 2, unitValue = 200 }]);

        var json = WireJson.Serialize(Assert.Single(sent.OfType<PlayerDeath>()));

        Assert.Equal(
            """
            {"type":"player_death","timestamp":1700000015500,"tick":8192,"round":5,"round_time":12.5,"victim":{"steam_id":"76561198000000011","name":"bravo1","side":"t","team":2,"position":{"x":-512,"y":1024.5,"z":64,"pitch":10,"yaw":-45}},"attacker":{"steam_id":"76561198000000001","name":"alpha1","side":"ct","team":1,"position":null},"assister":{"steam_id":"76561198000000002","name":"alpha2","side":"ct","team":1},"weapon":{"id":7,"fallback":null},"cause":"player","headshot":true,"penetrated":1,"thru_smoke":false,"no_scope":false,"attacker_blind":false,"attacker_in_air":false,"flash_assist":true,"friendly_fire":false,"victim_blind_by":null,"victim_blind_remaining":0,"victim_equipment_value":0,"victim_grenades_held":[{"weapon":43,"quantity":2,"unit_value":200}]}
            """,
            json);
    }

    [Fact]
    public void RoundEndSnapshot()
    {
        var host = new FakeGameHost { RoundsPlayed = 0, TickCount = 16384 };
        var sent = new List<PluginMessage>();
        var match = new MatchState(host, Fixture.Config(), new FakeAgentChannel(sent), Fixture.Messages, () => { });
        var forwarder = new EventForwarder(match, host);

        match.GoLive();
        host.Advance(3);
        host.IsWarmup = false;
        match.OnRoundStart();
        host.RoundStartTime = host.CurrentTime;
        host.FreezePeriod = false;
        host.Advance(48.25);
        sent.Clear();
        // The engine has counted the round on its scoreboard when round_end fires.
        host.TeamScores[Side.T] = 1;

        forwarder.OnRoundEnd(Side.T, reason: RoundEndReason.TerroristsWin);

        Assert.Equal(
            """
            {"type":"round_end","timestamp":1700000051250,"tick":16384,"round":1,"round_time":48.25,"winner_side":"t","winner_team":2,"reason":"terrorists_win","team1_score":0,"team2_score":1,"round_length":48.25}
            """,
            WireJson.Serialize(Assert.Single(sent.OfType<RoundEnd>())));
    }

    [Fact]
    public void MapStartSnapshot()
    {
        var started = new MapStart
        {
            timestamp = 1_700_000_000_000,
            tick = 640,
            pluginVersion = "0.1.0",
            frameworkVersion = "SwiftlyS2 1.4.10",
            serverVersion = "1.40.7.7",
            mapName = "de_mirage",
        };

        Assert.Equal(
            """
            {"type":"map_start","timestamp":1700000000000,"tick":640,"plugin_version":"0.1.0","framework_version":"SwiftlyS2 1.4.10","server_version":"1.40.7.7","map_name":"de_mirage"}
            """,
            WireJson.Serialize(started));
    }

    [Fact]
    public void GoingLiveSnapshot()
    {
        var live = new GoingLive
        {
            timestamp = 1_700_000_000_000,
            tick = 640,
            team1Side = Side.Ct,
            maxRounds = 24,
            overtime = true,
            overtimeMaxRounds = 6,
            friendlyFire = true,
        };

        Assert.Equal(
            """
            {"type":"going_live","timestamp":1700000000000,"tick":640,"team1_side":"ct","max_rounds":24,"overtime":true,"overtime_max_rounds":6,"friendly_fire":true}
            """,
            WireJson.Serialize(live));
    }

    [Fact]
    public void RoundBackupSnapshot()
    {
        var backup = new RoundBackup
        {
            timestamp = 1_700_000_000_000,
            tick = 640,
            round = 14,
            team1Side = Side.T,
            team1Score = 7,
            team2Score = 6,
            rules = new MatchRules { maxRounds = 24, overtime = true, overtimeMaxRounds = 6, friendlyFire = false },
            file = "\"SaveFile\"\n{\n}\n",
            roster = new Roster
            {
                team1 = new RosterTeam { name = "Alpha", players = ["76561198000000001"] },
                team2 = new RosterTeam { name = "Bravo", players = ["76561198000000011"] },
                spectators = [],
            },
        };

        Assert.Equal(
            """
            {"type":"round_backup","timestamp":1700000000000,"tick":640,"round":14,"team1_side":"t","team1_score":7,"team2_score":6,"rules":{"max_rounds":24,"overtime":true,"overtime_max_rounds":6,"friendly_fire":false},"file":"\u0022SaveFile\u0022\n{\n}\n","roster":{"team1":{"name":"Alpha","players":["76561198000000001"]},"team2":{"name":"Bravo","players":["76561198000000011"]},"spectators":[]}}
            """,
            WireJson.Serialize(backup));
    }
}

/// <summary>Round-trips the <c>AgentToPluginMessage</c> side of the contract.</summary>
public class InboundWireTests
{
    [Fact]
    public void ExecIsParsed()
    {
        var message = WireJson.ParseInbound("""{"type":"exec","command":"say hello","request_id":"r1"}""");
        Assert.Equal("say hello", Assert.IsType<AgentMessage.Exec>(message).command);
        Assert.Equal("r1", ((AgentMessage.Exec)message).requestId);
        Assert.Null(WireJson.ParseInbound("""{"type":"exec","command":"say hello"}"""));
    }

    [Fact]
    public void MatchConfigDecodesItsFullRoster()
    {
        const string json = """
            {"match_id":"0199a0b4-5c00-7000-8000-000000000a09",
            "roster":{"team1":{"name":"Alpha","players":["76561198000000001"]},
            "team2":{"name":"Bravo","players":["76561198000000011"]},
            "spectators":["76561198000000031"]},
            "sides":"knife","knife_decision_seconds":45,"ready_mode":"manual","ready_timeout_seconds":300,"record_demo":true,"debug_capture":false,"rules":{"max_rounds":24,"overtime":true,"overtime_max_rounds":6,"friendly_fire":false},"restore":null}
            """;

        Assert.True(WireJson.TryDecodeMatchConfig(json, out var config, out var error), error);

        Assert.Equal("0199a0b4-5c00-7000-8000-000000000a09", config!.matchId);
        Assert.Equal(SidesMode.Knife, config.sides);
        Assert.Equal(45, config.knifeDecisionSeconds);
        Assert.Equal(ReadyMode.Manual, config.readyMode);
        Assert.Equal(300, config.readyTimeoutSeconds);
        Assert.Equal("Alpha", config.roster.team1.name);
        Assert.Equal(["76561198000000001"], config.roster.team1.players);
        Assert.Equal(["76561198000000031"], config.roster.spectators);
    }

    [Theory]
    [InlineData("team1_ct", SidesMode.Team1Ct)]
    [InlineData("team1_t", SidesMode.Team1T)]
    [InlineData("knife", SidesMode.Knife)]
    public void SidesModeTokensParse(string token, SidesMode expected)
    {
        Assert.True(WireJson.TryDecodeMatchConfig(FullConfig.Replace("\"sides\":\"knife\"", $"\"sides\":\"{token}\""), out var config, out var error), error);
        Assert.Equal(expected, config!.sides);
    }

    [Theory]
    [InlineData("auto", ReadyMode.Auto)]
    [InlineData("manual", ReadyMode.Manual)]
    [InlineData("external", ReadyMode.External)]
    public void ReadyModeTokensParse(string token, ReadyMode expected)
    {
        Assert.True(WireJson.TryDecodeMatchConfig(FullConfig.Replace("\"ready_mode\":\"auto\"", $"\"ready_mode\":\"{token}\""), out var config, out var error), error);
        Assert.Equal(expected, config!.readyMode);
    }

    [Fact]
    public void TheTypeMayComeAfterTheFields()
    {
        var message = WireJson.ParseInbound("""{"command":"say hello","request_id":"r1","type":"exec"}""");
        Assert.Equal("say hello", Assert.IsType<AgentMessage.Exec>(message).command);
    }

    [Theory]
    [InlineData("""{"type":"pause"}""")]
    [InlineData("not json")]
    [InlineData("[]")]
    [InlineData("{}")]
    [InlineData("""{"type":7}""")]
    [InlineData("{\"type\":\"exec\",\"command\":42}")]
    [InlineData("{\"type\":\"get_server_status\",\"request_id\":42}")]
    public void UnusableFramesParseToNull(string frame) => Assert.Null(WireJson.ParseInbound(frame));

    [Fact]
    public void MatchConfigDecodesFromJson()
    {
        const string json = """
            {"match_id":"0199a0b4-5c00-7000-8000-000000000a01","roster":{"team1":{"name":"A",
            "players":["76561198000000001"]},"team2":{"name":"B","players":["76561198000000011"]},
            "spectators":[]},"sides":"team1_t",
            "knife_decision_seconds":30,"ready_mode":"auto","ready_timeout_seconds":null,"record_demo":false,"debug_capture":false,"rules":{"max_rounds":24,"overtime":true,"overtime_max_rounds":6,"friendly_fire":false},"restore":null}
            """;

        Assert.True(WireJson.TryDecodeMatchConfig(json, out var config, out var error), error);

        Assert.Equal("0199a0b4-5c00-7000-8000-000000000a01", config!.matchId);
        Assert.Equal("76561198000000001", config.roster.team1.players.Single());
        Assert.Equal(SidesMode.Team1T, config.sides);
    }

    private const string FullConfig = """
        {"match_id":"0199a0b4-5c00-7000-8000-000000000a06","roster":{
        "team1":{"name":"A","players":["76561198000000002","76561198000000001"]},
        "team2":{"name":"B","players":["76561198000000011","76561198000000012"]},
        "spectators":[]},"sides":"knife","knife_decision_seconds":30,"ready_mode":"auto","ready_timeout_seconds":null,"record_demo":true,"debug_capture":false,"rules":{"max_rounds":24,"overtime":true,"overtime_max_rounds":6,"friendly_fire":false},"restore":null}
        """;

    [Theory]
    [InlineData(",\"send_statistics\":true", true)]
    [InlineData(",\"send_statistics\":false", false)]
    [InlineData("", false)]
    public void AMatchSendsStatisticsOnlyWhenItsConfigSaysSo(string field, bool expected)
    {
        Assert.True(WireJson.TryDecodeMatchConfig(FullConfig.Replace(",\"restore\":null", field + ",\"restore\":null"), out var config, out var error), error);
        Assert.Equal(expected, config!.sendStatistics);
    }

    [Theory]
    [InlineData("match_id")]
    [InlineData("roster")]
    [InlineData("sides")]
    [InlineData("knife_decision_seconds")]
    [InlineData("ready_mode")]
    [InlineData("ready_timeout_seconds")]
    [InlineData("record_demo")]
    [InlineData("debug_capture")]
    [InlineData("rules")]
    public void AMatchConfigMissingAFieldDoesNotDecode(string field)
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json.Remove(field);
        Assert.False(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error));
        Assert.Null(config);
        Assert.Contains(field, error);
    }

    private const string RestoreJson = """
        {"team1_side":"t","rules":{"max_rounds":30,"overtime":false,"overtime_max_rounds":6,"friendly_fire":true},"file":"\"SaveFile\"\n{\n\t\"round\"\t\t\"13\"\n}\n"}
        """;

    /// <summary>A launch from before restores existed leaves the key out, which is the same as null.</summary>
    [Fact]
    public void AMatchConfigWithoutTheRestoreKeyDecodesWithNone()
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json.Remove("restore");

        Assert.True(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error), error);

        Assert.Null(config!.restore);
    }

    [Fact]
    public void AMatchConfigDecodesItsRestore()
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json["restore"] = System.Text.Json.Nodes.JsonNode.Parse(RestoreJson);

        Assert.True(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error), error);

        var restore = config!.restore!;
        Assert.Equal(Side.T, restore.team1Side);
        Assert.Equal((30, false, 6), (restore.rules.maxRounds, restore.rules.overtime, restore.rules.overtimeMaxRounds));
        Assert.Equal("\"SaveFile\"\n{\n\t\"round\"\t\t\"13\"\n}\n", restore.file);
    }

    [Theory]
    [InlineData("team1_side", "spec", "no side")]
    [InlineData("file", "", "empty file")]
    public void AnUnusableRestoreDoesNotDecode(string field, string value, string problem)
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json["restore"] = System.Text.Json.Nodes.JsonNode.Parse(RestoreJson);
        json["restore"]![field] = value;

        Assert.False(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error));

        Assert.Null(config);
        Assert.Contains(problem, error);
    }

    [Fact]
    public void ARestoreWithoutRoundsDoesNotDecode()
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json["restore"] = System.Text.Json.Nodes.JsonNode.Parse(RestoreJson);
        json["restore"]!["rules"]!["max_rounds"] = 0;

        Assert.False(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error));

        Assert.Null(config);
        Assert.Contains("has no rounds", error);
    }

    [Theory]
    [InlineData("team1_side")]
    [InlineData("rules")]
    [InlineData("file")]
    public void ARestoreMissingAFieldDoesNotDecode(string field)
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json["restore"] = System.Text.Json.Nodes.JsonNode.Parse(RestoreJson);
        json["restore"]!.AsObject().Remove(field);

        Assert.False(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error));

        Assert.Null(config);
        Assert.Contains(field, error);
    }

    [Theory]
    [InlineData("team1")]
    [InlineData("team2")]
    public void ATeamWithoutPlayersDoesNotDecode(string team)
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json["roster"]![team]!["players"] = new System.Text.Json.Nodes.JsonArray();
        Assert.False(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error));
        Assert.Null(config);
        Assert.Contains("no players", error);
    }

    [Fact]
    public void MoreThanTwoSpectatorsDoNotDecode()
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json["roster"]!["spectators"] = new System.Text.Json.Nodes.JsonArray(
            "76561198000000031", "76561198000000032", "76561198000000033");
        Assert.False(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error));
        Assert.Null(config);
        Assert.Contains("spectators", error);

        json["roster"]!["spectators"] = new System.Text.Json.Nodes.JsonArray("76561198000000031", "76561198000000032");
        Assert.True(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out _, out error), error);
    }

    [Fact]
    public void ARosterOverTheServersPlayerSlotsDoesNotDecode()
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        json["roster"]!["team1"]!["players"] = new System.Text.Json.Nodes.JsonArray(
            Enumerable.Range(100, 63).Select(n => (System.Text.Json.Nodes.JsonNode?)$"76561198000000{n}").ToArray());
        Assert.False(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error));
        Assert.Null(config);
        Assert.Contains("64", error);
    }

    [Theory]
    [InlineData("team1", "x")]
    [InlineData("team2", "x")]
    [InlineData("team1", "🔥")]
    [InlineData("team2", "🔥")]
    public void TeamNamesAllowThirtyCodePointsButNotThirtyOne(string team, string lastCharacter)
    {
        var json = System.Text.Json.Nodes.JsonNode.Parse(FullConfig)!.AsObject();
        var name = new string('x', 29) + lastCharacter;
        json["roster"]![team]!["name"] = name;
        Assert.True(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out var config, out var error), error);
        Assert.Equal(name, team == "team1" ? config!.roster.team1.name : config!.roster.team2.name);

        json["roster"]![team]!["name"] = name + "x";
        Assert.False(WireJson.TryDecodeMatchConfig(json.ToJsonString(), out config, out error));
        Assert.Null(config);
        Assert.Contains("team name", error);
    }

    /// <summary>Captains are positional: the lobby order of each team's players must survive decoding.</summary>
    [Fact]
    public void MatchConfigKeepsEachTeamsPlayersInCaptainFirstOrder()
    {
        Assert.True(WireJson.TryDecodeMatchConfig(FullConfig, out var config, out var error), error);

        Assert.Equal(["76561198000000002", "76561198000000001"], config!.roster.team1.players);
        Assert.Equal(["76561198000000011", "76561198000000012"], config.roster.team2.players);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("   ")]
    [InlineData("not json")]
    [InlineData("[]")]
    [InlineData("null")]
    public void AnUnusableMatchConfigDoesNotDecode(string? value)
    {
        Assert.False(WireJson.TryDecodeMatchConfig(value, out var config, out var error));
        Assert.Null(config);
        Assert.NotNull(error);
    }
}
