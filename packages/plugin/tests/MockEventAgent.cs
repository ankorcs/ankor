using System.Net;
using System.Net.WebSockets;
using System.Text;
using System.Text.Json;
using System.Threading.Channels;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.Extensions.Logging;

namespace Ankor.Tests;

internal sealed class MockEventAgent : IAsyncDisposable
{
    private readonly WebApplication _app;
    private readonly CancellationTokenSource _stop = new();
    private readonly Channel<WebSocket> _connections = Channel.CreateUnbounded<WebSocket>();
    public static readonly TimeSpan OperationTimeout = TimeSpan.FromSeconds(10);
    public volatile bool AcceptingConnections;
    public Uri Url => new UriBuilder(_app.Urls.Single()) { Scheme = "ws", Path = "/ws/plugin" }.Uri;

    private MockEventAgent(bool acceptingConnections)
    {
        AcceptingConnections = acceptingConnections;
        var builder = WebApplication.CreateSlimBuilder();
        builder.Logging.ClearProviders();
        builder.WebHost.ConfigureKestrel(options => options.Listen(IPAddress.Loopback, 0));
        _app = builder.Build();
        _app.UseWebSockets();
        _app.Run(async context =>
        {
            if (!AcceptingConnections)
            {
                context.Response.StatusCode = 503;
                return;
            }
            using var socket = await context.WebSockets.AcceptWebSocketAsync();
            await _connections.Writer.WriteAsync(socket);
            // The HTTP request must stay alive while the test owns the upgraded connection.
            try { await Task.Delay(System.Threading.Timeout.Infinite, _stop.Token); }
            catch (OperationCanceledException) when (_stop.IsCancellationRequested) { }
        });
    }

    public static async Task<MockEventAgent> StartAsync(bool acceptingConnections = true)
    {
        var agent = new MockEventAgent(acceptingConnections);
        try
        {
            await agent._app.StartAsync();
            return agent;
        }
        catch
        {
            await agent.DisposeAsync();
            throw;
        }
    }

    public Task<WebSocket> AcceptAsync() => _connections.Reader.ReadAsync().AsTask().WaitAsync(OperationTimeout);

    public static async Task<string> ReceiveAsync(WebSocket socket)
    {
        using var deadline = new CancellationTokenSource(OperationTimeout);
        using var message = new MemoryStream();
        var buffer = new byte[4096];
        WebSocketReceiveResult result;
        do
        {
            result = await socket.ReceiveAsync(buffer, deadline.Token);
            if (result.MessageType != WebSocketMessageType.Text)
                throw new InvalidDataException($"Expected text, received {result.MessageType}");
            message.Write(buffer, 0, result.Count);
        } while (!result.EndOfMessage);
        return Encoding.UTF8.GetString(message.ToArray());
    }

    public static async Task SendAsync(WebSocket socket, string text,
        WebSocketMessageType type = WebSocketMessageType.Text, bool endOfMessage = true)
    {
        using var deadline = new CancellationTokenSource(OperationTimeout);
        await socket.SendAsync(Encoding.UTF8.GetBytes(text), type, endOfMessage, deadline.Token);
    }

    public static Task AckAsync(WebSocket socket, long toSeq) =>
        SendAsync(socket, JsonSerializer.Serialize(new { type = "ack", to_seq = toSeq }));

    public async ValueTask DisposeAsync()
    {
        await _stop.CancelAsync();
        await _app.StopAsync().WaitAsync(OperationTimeout);
        await _app.DisposeAsync();
        _stop.Dispose();
    }
}
