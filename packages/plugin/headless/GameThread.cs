using System.Collections.Concurrent;
using Ankor.Core;
using Ankor.Core.Wire;
using Ankor.Tests;

namespace Ankor.Headless;

/// <summary>
/// The engine as the plugin reaches it, on one game thread. <see cref="FakeGameHost"/> keeps the
/// clock, timers, console and players; this makes it safe to reach from the socket's threads, as
/// the engine is: work for the next frame is queued, and log lines go to the console.
/// </summary>
internal sealed class GameThreadHost(FakeGameHost engine, Action onQuit, Action<string> onRecord) : IGameHost
{
    private readonly ConcurrentQueue<Action> _nextFrame = new();
    private static readonly Lock Console = new();

    public FakeGameHost Engine => engine;

    public void NextFrame(Action action) => _nextFrame.Enqueue(action);

    /// <summary>Runs the work queued for this frame, on the game thread.</summary>
    public void RunFrame()
    {
        var count = _nextFrame.Count;
        for (var i = 0; i < count && _nextFrame.TryDequeue(out var action); i++) action();
    }

    public void Log(LogLevel level, string message)
    {
        lock (Console) System.Console.WriteLine($"[ankor] {level.ToString().ToUpperInvariant()} {message}");
    }

    public void ExecuteCommand(string command)
    {
        engine.ExecuteCommand(command);
        // tv_record writes the demo from the moment it starts, under the game directory.
        foreach (var part in command.Split('\n', ';').Select(part => part.Trim()))
            if (part.StartsWith("tv_record ", StringComparison.Ordinal)) Record(part["tv_record ".Length..].Trim());
        // The engine runs quit after the commands queued with it, and the process ends.
        if (command.Split('\n', ';').Any(part => part.Trim() == "quit")) onQuit();
    }

    private void Record(string name)
    {
        var path = Path.Combine(ServerPaths.GameDir, name + ".dem");
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            File.WriteAllText(path, string.Concat(Enumerable.Repeat("HL2DEMO headless recording\n", 4096)));
            onRecord(path);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            Log(LogLevel.Error, $"could not record {path}: {error.Message}");
        }
    }

    public long NowMs => engine.NowMs;
    public double SimulationTick => engine.SimulationTick;
    public double RoundElapsedTime => engine.RoundElapsedTime;
    public double CurrentTime => engine.CurrentTime;
    public string MapName => engine.MapName;
    public bool SteamLoggedOn => engine.SteamLoggedOn;
    public string? ServerSteamId => engine.ServerSteamId;
    public string ServerVersion => engine.ServerVersion;
    public string FrameworkVersion => engine.FrameworkVersion;
    public IReadOnlyList<HostPlayer> Players => engine.Players;
    public HostPlayer? ReadPlayerOrKick(int slot, string reason) => engine.ReadPlayerOrKick(slot, reason);
    public IReadOnlyList<HostPlayer> ReadPlayersOrKick(string reason) => engine.ReadPlayersOrKick(reason);
    public PlayerInventory? ReadInventory(HostPlayer player) => engine.ReadInventory(player);
    public void AfterCommands(Action action) => engine.AfterCommands(action);
    public void PrintToChatAll(string message) => engine.PrintToChatAll(message);
    public void PrintToChat(int slot, string message) => engine.PrintToChat(slot, message);
    public bool IsWarmup => engine.IsWarmup;
    public int RoundsPlayed => engine.RoundsPlayed;
    public int? ReadTeamScore(Side side) => engine.ReadTeamScore(side);
    public bool LoadingRoundBackup => engine.LoadingRoundBackup;
    public void SwitchTeam(HostPlayer player, Side side) => engine.SwitchTeam(player, side);
    public void ChangeTeam(HostPlayer player, Side side) => engine.ChangeTeam(player, side);
    public void Kick(HostPlayer player, string reason) => engine.Kick(player, reason);
    public void AbortMatch() => engine.AbortMatch();
    public void SetClanTag(HostPlayer player, string tag) => engine.SetClanTag(player, tag);
    public IHostTimer AddTimer(double seconds, Action callback, bool stopOnMapChange = true) => engine.AddTimer(seconds, callback, stopOnMapChange);
    public IHostTimer AddRepeatingTimer(double delaySeconds, double periodSeconds, Action callback, bool stopOnMapChange = true) =>
        engine.AddRepeatingTimer(delaySeconds, periodSeconds, callback, stopOnMapChange);
    public void SetConVarString(string name, string value) => engine.SetConVarString(name, value);
    public string? GetConVarString(string name) => engine.Console.Values.TryGetValue(name, out var text) ? text : engine.GetConVarString(name);
    // A convar a cfg or command set reads as set, as on a real server; the rest keep the engine's
    // values.
    public int? GetConVarInt(string name) => Set(name) is { } value ? (int)value : engine.GetConVarInt(name);
    public bool? GetConVarBool(string name) => Set(name) is { } value ? value != 0 : engine.GetConVarBool(name);
    public double? GetConVarNumber(string name) => Set(name) ?? engine.GetConVarNumber(name);

    private double? Set(string name) =>
        engine.Console.Values.TryGetValue(name, out var text)
        && double.TryParse(text, System.Globalization.NumberStyles.Float, System.Globalization.CultureInfo.InvariantCulture, out var value)
            ? value
            : null;
}
