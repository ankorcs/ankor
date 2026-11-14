using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Ankor.Diagnostics;
using Session = Ankor.Tests.GameplayTestSession;

namespace Ankor.Tests;

/// <summary>Reusable engine-free scenario, shared by the C# assertions and the committed replay fixture.</summary>
public static class GameplayScenario
{
    public static (GameplayTestSession Live, GameplayTestSession Replay) Run()
    {
        var live = new Session();
        // Last T killed; CT subsequently defuses.
        live.StartRound(1);
        live.Send(new BombBeginPlantInput(live.T, BombSite.A));
        live.Send(new BombPlantedInput(live.T, BombSite.A));
        live.Kill(live.T, live.Ct);
        live.Send(new DamageExitInput());
        live.Send(new BombBeginDefuseInput(live.Ct, false));
        live.Send(new BombDefusedInput(live.Ct, 10, false));
        live.EndRound(Side.Ct, 7);

        // Same kill, opposite outcome. Explosion damage arrives after round_end.
        live.StartRound(2);
        live.Send(new BombPlantedInput(live.T, BombSite.B));
        live.Kill(live.T, live.Ct);
        live.Send(new DamageExitInput());
        live.Send(new BombExplodedInput());
        live.EndRound(Side.T, 1);
        // CS2 names no attacker for the bomb.
        live.Kill(live.Ct, null, 49);
        live.Send(new DamageExitInput());

        // Active fire extinguished by smoke; a second throw spans halftime.
        live.StartRound(3);
        live.Send(new GrenadeSpawnedInput(live.T, GrenadeKind.Molotov, 10));
        live.Send(new GrenadeDetonateInput(10, new(1, 1, 1)));
        live.Send(new GrenadeFireInput(10, 11));
        live.Send(new GrenadeRemovedInput(10));
        live.Send(new GrenadeSpawnedInput(live.Ct, GrenadeKind.SmokeGrenade, 12));
        live.Send(new GrenadeDetonateInput(12, new(1, 1, 1)));
        live.Send(new FireExtinguishedInput(11, new(1, 1, 1)));
        live.Send(new GrenadeRemovedInput(11));
        live.Send(new WeaponFireInput(live.Ct, new(45, null)), 356080.890625);
        live.Kill(live.T, live.Ct);
        live.EndRound(Side.Ct, 8);
        live.Send(new DamageExitInput(), 356083);
        live.Send(new GrenadeSpawnedInput(live.Ct, GrenadeKind.SmokeGrenade, 13), 356088);

        // Ordinary round-end delay still tracks a decoy to detonation/removal.
        live.StartRound(4);
        live.EndRound(Side.Ct, 8);
        live.Send(new GrenadeSpawnedInput(live.Ct, GrenadeKind.Decoy, 20));
        live.Send(new GrenadeDetonateInput(20, new(0, 0, 0)));
        live.Send(new GrenadeRemovedInput(20));

        live.StartRound(5);
        live.Send(new GrenadeSpawnedInput(live.Ct, GrenadeKind.Flashbang, 30));
        live.Send(new GrenadeDetonateInput(30, new(0, 0, 0)));
        live.Send(new PlayerBlindInput(live.T, live.Ct, 2, 30));
        live.Send(new GrenadeRemovedInput(30));
        live.Send(new GrenadeSpawnedInput(live.Ct, GrenadeKind.HeGrenade, 31));
        live.Send(new GrenadeDetonateInput(31, new(0, 0, 0)));
        live.Send(new GrenadeRemovedInput(31));
        live.EndRound(Side.Ct, 8);

        // Match-winning defuse flushes smoke/fire before map_end, then drains and quits.
        live.StartRound(6);
        live.Send(new BombPlantedInput(live.T, BombSite.A));
        live.Send(new GrenadeSpawnedInput(live.T, GrenadeKind.IncGrenade, 40));
        live.Send(new GrenadeDetonateInput(40, new(50, 50, 0)));
        live.Send(new GrenadeFireInput(40, 41));
        live.Send(new GrenadeRemovedInput(40));
        live.Send(new GrenadeSpawnedInput(live.Ct, GrenadeKind.SmokeGrenade, 42));
        live.Send(new GrenadeDetonateInput(42, new(500, 500, 0)));
        live.Send(new BombBeginDefuseInput(live.Ct, true));
        live.Send(new BombDefusedInput(live.Ct, 4, true));
        live.EndRound(Side.Ct, 7);
        live.Send(new MapEndInput()); // Win-panel duplicate cannot finish twice.
        live.Send(new GrenadeSpawnedInput(live.Ct, GrenadeKind.SmokeGrenade, 43));
        live.Engine.Advance(61);

        var replay = new Session(replay: true, gameDir: live.GameDir);
        // Exercise the actual capture writer/reader, not only our in-memory observation list.
        var stream = new MemoryStream();
        using (var capture = new DebugCapture(stream, "fixture", new { }, message => throw new Exception(message)))
            foreach (var item in live.Tape) capture.Observe(item.Name, item.Value);
        var lines = System.Text.Encoding.UTF8.GetString(stream.ToArray()).Split('\n', StringSplitOptions.RemoveEmptyEntries);
        GameplayReplay.Run(GameplayReplay.ReadCapture(lines), replay.Callbacks, replay.Replay!);
        return (live, replay);
    }
}
