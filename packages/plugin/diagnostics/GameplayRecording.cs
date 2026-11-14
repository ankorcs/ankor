using Ankor.Core;
using System.Reflection;
using System.Runtime.ExceptionServices;
using System.Text.Json;

namespace Ankor.Diagnostics;

public sealed record GameplayRecord(GameplayInput input);
public sealed record GameplayHostRead(string key, JsonElement value);
public sealed record GameplayScheduled(int id);

/// <summary>
/// Record only reads the core actually makes, not speculative native probes. The existing capture
/// owns ordering and durability. Wrapping scheduled callbacks also records when the engine fires
/// them; replay must not infer that from wall time or automatically run timers between inputs.
/// </summary>
public class GameplayRecording : DispatchProxy
{
    public static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        TypeInfoResolver = GameplaySerialization.Resolver,
        RespectRequiredConstructorParameters = true,
        RespectNullableAnnotations = true,
    };

    private IGameHost _host = null!;
    private Action<string, object> _observe = null!;
    private int _scheduled;
    private Action<string> _fail = null!;
    private bool _failed;

    public static IGameHost Wrap(IGameHost host, Action<string, object> observe, Action<string> fail)
    {
        var proxy = Create<IGameHost, GameplayRecording>();
        var recording = (GameplayRecording)proxy;
        recording._host = host;
        recording._observe = observe;
        recording._fail = fail;
        return proxy;
    }

    /// <summary>The host calls whose answers a capture keeps, so replay can give them back.</summary>
    public static bool IsRecordedRead(MethodInfo method) =>
        method.Name.StartsWith("get_", StringComparison.Ordinal)
        || method.Name.StartsWith("GetConVar", StringComparison.Ordinal)
        || method.Name is nameof(IGameHost.ReadInventory) or nameof(IGameHost.ReadPlayerOrKick) or nameof(IGameHost.ReadPlayersOrKick)
            or nameof(IGameHost.ReadTeamScore);

    /// <summary>Where each scheduling call takes its callback: a repeating timer has its delay and period first.</summary>
    public static int CallbackIndex(string method) => method switch
    {
        nameof(IGameHost.AddRepeatingTimer) => 2,
        nameof(IGameHost.AddTimer) => 1,
        _ => 0,
    };

    public static string ReadKey(MethodInfo method, object?[] args) =>
        method.Name + ":" + JsonSerializer.Serialize(args.Select(arg => arg is HostPlayer player ? (object)player.SteamIdString : arg), Json);

    protected override object? Invoke(MethodInfo? targetMethod, object?[]? args)
    {
        var method = targetMethod ?? throw new ArgumentNullException(nameof(targetMethod));
        args ??= [];
        if (method.Name is nameof(IGameHost.AddTimer) or nameof(IGameHost.AddRepeatingTimer) or nameof(IGameHost.NextFrame) or nameof(IGameHost.AfterCommands))
        {
            var index = CallbackIndex(method.Name);
            var action = (Action)args[index]!;
            var id = ++_scheduled;
            args[index] = (Action)(() =>
            {
                Record(() => _observe(CaptureNames.Gameplay.Scheduled, new GameplayScheduled(id)));
                action();
            });
        }
        object? result;
        try { result = method.Invoke(_host, args); }
        catch (TargetInvocationException ex) when (ex.InnerException is not null)
        {
            ExceptionDispatchInfo.Capture(ex.InnerException).Throw();
            throw;
        }
        if (IsRecordedRead(method))
            Record(() => _observe(CaptureNames.Gameplay.HostRead, new GameplayHostRead(ReadKey(method, args),
                JsonSerializer.SerializeToElement(result, method.ReturnType, Json))));
        return result;
    }

    // Recording must never change host results or prevent a scheduled gameplay callback.
    // Keep engine calls outside this catch so their exceptions retain their normal meaning.
    private void Record(Action record)
    {
        if (_failed) return;
        try { record(); }
        catch (Exception ex)
        {
            _failed = true;
            try { _fail($"Gameplay recording failed: {ex.Message}"); }
            catch (Exception) { /* A diagnostic reporter must not interrupt gameplay either. */ }
        }
    }

    public static GameplayInput Decode(JsonElement value)
    {
        var record = value.Deserialize<GameplayRecord>(Json) ?? throw new JsonException("Missing gameplay record");
        return record.input;
    }
}
