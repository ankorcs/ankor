using System.Text.Json.Serialization;
using System.Text.Json.Serialization.Metadata;
using Ankor.Core;

namespace Ankor.Diagnostics;

/// <summary>Capture format metadata stays outside the gameplay contracts it serializes.</summary>
internal static class GameplaySerialization
{
    public static DefaultJsonTypeInfoResolver Resolver { get; } = CreateResolver();

    private static DefaultJsonTypeInfoResolver CreateResolver()
    {
        var resolver = new DefaultJsonTypeInfoResolver();
        resolver.Modifiers.Add(info =>
        {
            if (info.Type != typeof(GameplayInput)) return;
            info.PolymorphismOptions = new JsonPolymorphismOptions
            {
                TypeDiscriminatorPropertyName = "$input",
                DerivedTypes =
                {
                    new JsonDerivedType(typeof(RoundPrestartInput), "RoundPrestart"),
                    new JsonDerivedType(typeof(RoundStartInput), "RoundStart"),
                    new JsonDerivedType(typeof(RoundFreezeEndInput), "RoundFreezeEnd"),
                    new JsonDerivedType(typeof(RoundEndInput), "RoundEnd"),
                    new JsonDerivedType(typeof(RoundMvpInput), "RoundMvp"),
                    new JsonDerivedType(typeof(WeaponFireInput), "WeaponFire"),
                    new JsonDerivedType(typeof(GrenadeSpawnedInput), "GrenadeSpawned"),
                    new JsonDerivedType(typeof(GrenadeDetonateInput), "GrenadeDetonate"),
                    new JsonDerivedType(typeof(GrenadeFireInput), "GrenadeFire"),
                    new JsonDerivedType(typeof(GrenadeRemovedInput), "GrenadeRemoved"),
                    new JsonDerivedType(typeof(FireExtinguishedInput), "FireExtinguished"),
                    new JsonDerivedType(typeof(FireExpiredInput), "FireExpired"),
                    new JsonDerivedType(typeof(PlayerBlindInput), "PlayerBlind"),
                    new JsonDerivedType(typeof(ExitBuyZoneInput), "ExitBuyZone"),
                    new JsonDerivedType(typeof(BombBeginPlantInput), "BombBeginPlant"),
                    new JsonDerivedType(typeof(BombAbortPlantInput), "BombAbortPlant"),
                    new JsonDerivedType(typeof(BombPlantedInput), "BombPlanted"),
                    new JsonDerivedType(typeof(BombBeginDefuseInput), "BombBeginDefuse"),
                    new JsonDerivedType(typeof(BombAbortDefuseInput), "BombAbortDefuse"),
                    new JsonDerivedType(typeof(BombDefusedInput), "BombDefused"),
                    new JsonDerivedType(typeof(BombExplodedInput), "BombExploded"),
                    new JsonDerivedType(typeof(PlayerHurtInput), "PlayerHurt"),
                    new JsonDerivedType(typeof(PlayerDeathInput), "PlayerDeath"),
                    new JsonDerivedType(typeof(DamageEnterInput), "DamageEnter"),
                    new JsonDerivedType(typeof(DamageSnapshotInput), "DamageSnapshot"),
                    new JsonDerivedType(typeof(DamageExitInput), "DamageExit"),
                    new JsonDerivedType(typeof(DamageReportedInput), "DamageReported"),
                    new JsonDerivedType(typeof(MapEndInput), "MapEnd"),
                    new JsonDerivedType(typeof(MapLoadInput), "MapLoad"),
                    new JsonDerivedType(typeof(PlayerConnectInput), "PlayerConnect"),
                    new JsonDerivedType(typeof(PlayerDisconnectInput), "PlayerDisconnect"),
                    new JsonDerivedType(typeof(PlayerTeamInput), "PlayerTeam"),
                    new JsonDerivedType(typeof(ClientAuthorizedInput), "ClientAuthorized"),
                    new JsonDerivedType(typeof(SideChoiceInput), "SideChoice"),
                    new JsonDerivedType(typeof(SideChoiceForSideInput), "SideChoiceForSide"),
                    new JsonDerivedType(typeof(ForceReadyInput), "ForceReady"),
                    new JsonDerivedType(typeof(ReadyInput), "Ready"),
                },
            };
        });
        return resolver;
    }
}
