using Ankor.Diagnostics;
using System.Reflection;
using System.Text.Json;
using Ankor.Core;

namespace Ankor.Tests;

/// <summary>
/// Replay host reads by method/arguments inside one callback, not by a brittle global call count.
/// Repeated reads reuse the last supplied value; new required reads fail instead of inventing data.
/// Timers fire only at recorded scheduler markers, never as a side effect of changing the clock.
/// </summary>
public class GameplayReplayHost : DispatchProxy
{
    private sealed class Timer(Action cancel) : IHostTimer
    {
        public bool IsCancellationRequested { get; private set; }
        public void Cancel()
        {
            IsCancellationRequested = true;
            cancel();
        }
    }
    private FakeGameHost _bootstrap = null!;
    private readonly Dictionary<string, Queue<JsonElement>> _reads = [];
    private readonly Dictionary<int, Action> _scheduled = [];
    /// <summary>The repeating timers, which fire under one id until they are cancelled.</summary>
    private readonly HashSet<int> _repeating = [];
    private int _nextSchedule;
    public bool Replaying { get; set; }

    public static (IGameHost Host, GameplayReplayHost Replay) Create(FakeGameHost bootstrap)
    {
        var host = Create<IGameHost, GameplayReplayHost>();
        var replay = (GameplayReplayHost)host;
        replay._bootstrap = bootstrap;
        return (host, replay);
    }

    public void SetReads(IEnumerable<GameplayHostRead> reads)
    {
        _reads.Clear();
        foreach (var read in reads)
        {
            if (!_reads.TryGetValue(read.key, out var values)) _reads[read.key] = values = new();
            values.Enqueue(read.value);
        }
    }

    public void Fire(int id)
    {
        var found = _repeating.Contains(id) ? _scheduled.TryGetValue(id, out var action) : _scheduled.Remove(id, out action);
        if (!found) throw new InvalidDataException($"Unknown/cancelled scheduled callback {id}");
        action!();
    }

    protected override object? Invoke(MethodInfo? targetMethod, object?[]? args)
    {
        var method = targetMethod!;
        args ??= [];
        if (method.Name is nameof(IGameHost.AddTimer) or nameof(IGameHost.AddRepeatingTimer) or nameof(IGameHost.NextFrame) or nameof(IGameHost.AfterCommands))
        {
            var id = ++_nextSchedule;
            var index = GameplayRecording.CallbackIndex(method.Name);
            var action = (Action)args[index]!;
            _scheduled[id] = action;
            if (method.Name == nameof(IGameHost.AddRepeatingTimer)) _repeating.Add(id);
            if (!Replaying)
            {
                args[index] = (Action)(() => Fire(id));
                return method.Invoke(_bootstrap, args);
            }
            return method.Name is nameof(IGameHost.AddTimer) or nameof(IGameHost.AddRepeatingTimer)
                ? new Timer(() =>
                {
                    _scheduled.Remove(id);
                    _repeating.Remove(id);
                })
                : null;
        }
        if (!Replaying) return method.Invoke(_bootstrap, args);
        if (GameplayRecording.IsRecordedRead(method))
        {
            var key = GameplayRecording.ReadKey(method, args);
            if (!_reads.TryGetValue(key, out var values))
                throw new InvalidDataException($"Missing recorded host read: {key}");
            var value = values.Count > 1 ? values.Dequeue() : values.Peek();
            return value.Deserialize(method.ReturnType, GameplayRecording.Json);
        }
        // Commands/logs are observations, never instructions to the real engine.
        return method.Invoke(_bootstrap, args);
    }
}

public sealed record ReplayObservation(string Name, JsonElement Value);

public static class GameplayReplay
{
    public static void Run(IReadOnlyList<ReplayObservation> observations, GameplayCallbacks callbacks, GameplayReplayHost host)
    {
        host.Replaying = true;
        for (var i = 0; i < observations.Count; i++)
        {
            var step = observations[i];
            if (step.Name is not (CaptureNames.Gameplay.Input or CaptureNames.Gameplay.Scheduled)) continue;
            var reads = new List<GameplayHostRead>();
            var j = i + 1;
            for (; j < observations.Count && observations[j].Name is not (CaptureNames.Gameplay.Input or CaptureNames.Gameplay.Scheduled); j++)
                if (observations[j].Name == CaptureNames.Gameplay.HostRead)
                    reads.Add(observations[j].Value.Deserialize<GameplayHostRead>(GameplayRecording.Json)!);
            host.SetReads(reads);
            if (step.Name == CaptureNames.Gameplay.Input) callbacks.Handle(GameplayRecording.Decode(step.Value));
            else host.Fire(step.Value.Deserialize<GameplayScheduled>(GameplayRecording.Json)!.id);
            i = j - 1;
        }
    }

    public static IReadOnlyList<ReplayObservation> ReadCapture(IEnumerable<string> lines)
    {
        var observations = new List<ReplayObservation>();
        long expected = 0;
        var complete = false;
        var callbacks = new Stack<long>();
        foreach (var line in lines)
        {
            if (complete) throw new InvalidDataException("Records after capture completion");
            using var document = JsonDocument.Parse(line);
            var r = document.RootElement;
            var phase = r.GetProperty("phase").GetString();
            if (phase == CaptureNames.Phase.End)
            {
                complete = r.GetProperty("complete").GetBoolean();
                if (!complete || callbacks.Count != 0) throw new InvalidDataException("Incomplete capture");
                continue;
            }
            if (r.GetProperty("sequence").GetInt64() != expected++) throw new InvalidDataException("Capture sequence gap");
            if (phase == CaptureNames.Phase.Enter) callbacks.Push(r.GetProperty("callbackId").GetInt64());
            if (phase == CaptureNames.Phase.Exit && (!callbacks.TryPop(out var id) || id != r.GetProperty("callbackId").GetInt64()))
                throw new InvalidDataException("Unbalanced callback scope");
            if (phase != CaptureNames.Phase.Observation) continue;
            var data = r.GetProperty("data");
            var name = data.GetProperty("name").GetString()!;
            if (name.StartsWith(CaptureNames.Gameplay.Prefix, StringComparison.Ordinal))
                observations.Add(new(name, data.GetProperty("value").Clone()));
        }
        if (!complete) throw new InvalidDataException("Missing successful completion marker");
        return observations;
    }
}
