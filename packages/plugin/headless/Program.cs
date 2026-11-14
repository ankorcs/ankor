using System.Collections.Concurrent;
using System.Diagnostics;
using System.Net.WebSockets;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json.Nodes;
using System.Threading.Channels;
using Ankor.Core.Wire;
using Ankor.Headless;
using Ankor.Tests;

// A game server process without the engine: the plugin as shipped, on the test engine, driven by
// instructions on a control socket (ANKOR_HEADLESS_CONTROL_URL). Without one, the server starts and
// loads its plugin by itself and plays nothing. The engine's clock runs in real time, from now or
// from ANKOR_HEADLESS_START_MS; with ANKOR_HEADLESS_CLOCK=manual it moves only on `advance`, so a
// driver sees the same times every run.

const double FrameSeconds = 1.0 / 64;

var reports = Channel.CreateUnbounded<JsonObject>();
// The plugin's first frame on the relay is its status from before the map loads; the map loads after it.
var pluginSpoke = false;
void Report(JsonObject report)
{
    if (report.ContainsKey("plugin_frame")) Volatile.Write(ref pluginSpoke, true);
    reports.Writer.TryWrite(report);
}
var ops = new ConcurrentQueue<JsonObject>();
var exitCode = new TaskCompletionSource<int>();

var allocationId = AnkorEnv.ParseAllocationId(Environment.GetEnvironmentVariable(AnkorEnv.AllocationId));
var relay = AnkorEnv.ParseWsUrl(Environment.GetEnvironmentVariable(AnkorEnv.WsUrl) ?? "");
using var proxy = relay is null ? null : new RelayProxy(relay, Report);
proxy?.Start();

// The engine's clock starts now, or where ANKOR_HEADLESS_START_MS says, so a run can name its demos the same way every time.
var startMs = long.TryParse(Environment.GetEnvironmentVariable("ANKOR_HEADLESS_START_MS"), out var start) ? start : DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
var engine = new FakeGameHost { NowMs = startMs };
var manualClock = Environment.GetEnvironmentVariable("ANKOR_HEADLESS_CLOCK") == "manual";
GameThreadHost host = null!;
Server server = null!;
host = new GameThreadHost(engine, onQuit: () => ops.Enqueue(new JsonObject { ["op"] = "signal", ["name"] = "quit" }),
    onRecord: path => Report(new JsonObject { ["recording"] = path }));
server = new Server(host, allocationId, proxy, Report);

var controlUrl = Environment.GetEnvironmentVariable("ANKOR_HEADLESS_CONTROL_URL");
var sending = Task.CompletedTask;
if (string.IsNullOrWhiteSpace(controlUrl)) ops.Enqueue(new JsonObject { ["op"] = "connect" });
else _ = Task.Run(() => ControlAsync(new Uri($"{controlUrl}?allocation={allocationId}")));

Report(new JsonObject { ["started"] = true, ["match"] = File.Exists(MatchConfigFile.Path) });

using var terminate = PosixSignalRegistration.Create(PosixSignal.SIGTERM, context =>
{
    context.Cancel = true;
    ops.Enqueue(new JsonObject { ["op"] = "signal", ["name"] = "SIGTERM" });
});
using var interrupt = PosixSignalRegistration.Create(PosixSignal.SIGINT, context =>
{
    context.Cancel = true;
    ops.Enqueue(new JsonObject { ["op"] = "signal", ["name"] = "SIGINT" });
});

// The game thread: instructions, then the frame's queued work, then the clock and its timers.
var clock = Stopwatch.StartNew();
var last = clock.Elapsed;
while (!exitCode.Task.IsCompleted)
{
    while (ops.TryDequeue(out var op))
    {
        switch ((string?)op["op"])
        {
            case "signal":
                // A stopped server, or one that ran quit, unloads its plugin, which flushes what it has not sent.
                Report(new JsonObject { ["exiting"] = (string?)op["name"], ["code"] = 0 });
                server.Unload();
                exitCode.TrySetResult(0);
                break;
            case "exit":
                // The process dies with the code, without unloading its plugin.
                var code = (int?)op["code"] ?? 0;
                Report(new JsonObject { ["exiting"] = "exit", ["code"] = code });
                exitCode.TrySetResult(code);
                break;
            default:
                // An instruction the server cannot carry out is reported, and the server plays on.
                try { server.Apply(op); }
                catch (Exception error) when (error is not OutOfMemoryException)
                {
                    Report(new JsonObject { ["op_failed"] = op.DeepClone(), ["error"] = error.Message });
                }
                break;
        }
        if (exitCode.Task.IsCompleted) break;
    }
    if (Volatile.Read(ref pluginSpoke)) server.Boot();
    host.RunFrame();
    var now = clock.Elapsed;
    if (!manualClock) engine.Advance((now - last).TotalSeconds);
    last = now;
    await Task.Delay(TimeSpan.FromSeconds(FrameSeconds));
}

// The reports already queued reach the driver before the process ends.
reports.Writer.TryComplete();
await Task.WhenAny(sending, Task.Delay(TimeSpan.FromSeconds(2)));
return await exitCode.Task;

async Task ControlAsync(Uri url)
{
    using var socket = new ClientWebSocket();
    for (var attempt = 0; ; attempt++)
    {
        try
        {
            await socket.ConnectAsync(url, CancellationToken.None);
            break;
        }
        catch (WebSocketException) when (attempt < 40)
        {
            await Task.Delay(250);
        }
    }

    sending = Task.Run(async () =>
    {
        await foreach (var report in reports.Reader.ReadAllAsync())
            await socket.SendAsync(Encoding.UTF8.GetBytes(report.ToJsonString()), WebSocketMessageType.Text, true, CancellationToken.None);
    });

    var buffer = new byte[64 * 1024];
    using var message = new MemoryStream();
    while (socket.State == WebSocketState.Open)
    {
        var result = await socket.ReceiveAsync(buffer, CancellationToken.None);
        if (result.MessageType == WebSocketMessageType.Close) return;
        message.Write(buffer, 0, result.Count);
        if (!result.EndOfMessage) continue;
        try
        {
            if (JsonNode.Parse(message.ToArray()) is JsonObject op) ops.Enqueue(op);
            else Report(new JsonObject { ["unknown_op"] = Encoding.UTF8.GetString(message.ToArray()) });
        }
        catch (System.Text.Json.JsonException)
        {
            Report(new JsonObject { ["unknown_op"] = Encoding.UTF8.GetString(message.ToArray()) });
        }
        message.SetLength(0);
    }
}
