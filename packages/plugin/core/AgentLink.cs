using Ankor.Core.Wire;

namespace Ankor.Core;

/// <summary>
/// The plugin's socket to its agent: what goes out on it, and the answers to the agent's frames.
/// The SwiftlyS2 adapter runs it, and a host without the engine can run it the same way. The adapter
/// passes in what only it has: the server's state, with its loaded match, and its logger.
/// </summary>
public sealed class AgentLink : IAsyncDisposable, IAgentChannel
{
    /// <summary>The loaded match, as the agent's commands reach it.</summary>
    public interface IMatch
    {
        MatchState State { get; }

        /// <summary>Names the work that follows in the debug capture, until the scope is disposed.</summary>
        IDisposable? BeginCapture(string name);

        string? RestoreRound(int round);

        string? StartMatch();
    }

    /// <summary>The server's state, as its status reports it.</summary>
    public interface IServer
    {
        /// <summary>The loaded match, or null on a server without one.</summary>
        IMatch? Match { get; }

        /// <summary>Whether the server is ready.</summary>
        bool Ready { get; }

        /// <summary>Why the launch's match config was refused, for every status.</summary>
        string? ConfigError { get; }
    }

    /// <summary>The lines the link logs through the adapter's logger.</summary>
    public interface ILog
    {
        /// <summary>A frame that had no socket to go to.</summary>
        void NoSocket(string frame);

        void Exec(string command);

        /// <summary>The first frame of a type this plugin does not know.</summary>
        void SkippingType(string type);

        void IgnoredFrame(string excerpt);
    }

    private readonly IGameHost _host;
    private readonly string _allocationId;
    private readonly IServer _server;
    private readonly ILog _log;
    private WsClient? _ws;

    /// <summary>Message types a newer agent sent that this plugin does not know, each logged once. Past
    /// <see cref="MaxSkippedTypes"/> no new one is logged, since a newer release adds only a few.</summary>
    private readonly HashSet<string> _skippedTypes = new(StringComparer.Ordinal);
    private const int MaxSkippedTypes = 32;
    private const int MaxSkippedTypeChars = 64;

    public AgentLink(IGameHost host, string allocationId, IServer server, ILog log)
    {
        _host = host;
        _allocationId = allocationId;
        _server = server;
        _log = log;
    }

    public long PendingCount => _ws?.PendingCount ?? 0;

    public double? OldestPendingAgeSeconds => _ws?.OldestPendingAgeSeconds;

    public void Send(PluginMessage message)
    {
        if (_ws is null) _log.NoSocket(WireJson.Serialize(message));
        else _ws.Enqueue(message);
    }

    public void ReportMatch(MatchReport report) => _ws?.ReportMatch(report);

    public Task SendQuitting() => _ws?.SendQuitting() ?? Task.CompletedTask;

    public void SendServerStatus(string? requestId = null) =>
        _ws?.ReportServerStatus(ServerStatus.Serialize(_host, _server.Ready, _server.Match?.State, requestId, _server.ConfigError));

    internal void HandleInbound(string json)
    {
        var message = WireJson.ParseInbound(json);
        using var capture = _server.Match?.BeginCapture("control.inbound");
        switch (message)
        {
            case AgentMessage.GetServerStatus request:
                SendServerStatus(request.requestId);
                break;
            case AgentMessage.RestoreRound restore:
                _ws?.ReportCommandResult(CommandResult.Serialize(restore.requestId, _server.Match is { } restored ? restored.RestoreRound(restore.round) : CommandResult.NoMatch));
                break;
            case AgentMessage.StartMatch start:
                _ws?.ReportCommandResult(CommandResult.Serialize(start.requestId, _server.Match is { } match ? match.StartMatch() : CommandResult.NoMatch));
                break;
            case AgentMessage.Exec exec:
                if (_server.Match is not null && AgentControl.BlockedCommand(exec.command) is { } blocked)
                {
                    // Refused whole: no part of the line reaches the engine.
                    _host.Log(LogLevel.Warn, $"exec from the controller refused, {blocked} is blocked during a match: {exec.command}");
                    _ws?.ReportCommandResult(CommandResult.Serialize(exec.requestId, CommandResult.CommandBlocked));
                    break;
                }
                _log.Exec(exec.command);
                // Answered first, since a command such as quit or changelevel can take the plugin down with it.
                _ws?.ReportCommandResult(CommandResult.Serialize(exec.requestId, null));
                _host.ExecuteCommand(AgentControl.ExecLine(exec.command));
                break;
            case null when WireJson.UnknownMessage(json) is var (type, requestId):
                if (requestId is not null)
                    _ws?.ReportCommandResult(CommandResult.Serialize(requestId, CommandResult.UnsupportedRequest));
                var shown = type.Length > MaxSkippedTypeChars ? type[..MaxSkippedTypeChars] : type;
                if (_skippedTypes.Count < MaxSkippedTypes && _skippedTypes.Add(shown))
                    _log.SkippingType(shown);
                break;
            case null:
                _log.IgnoredFrame(AgentControl.Excerpt(json));
                break;
        }
    }

    /// <summary>Dials the agent at <paramref name="url"/>, and keeps dialling it until the link is disposed.</summary>
    public void Start(Uri url)
    {
        WsClient? socket = null;
        void OnGameThread(Action action)
        {
            var disconnected = socket?.ConnectionClosed ?? new CancellationToken(true);
            _host.NextFrame(() =>
            {
                if (!disconnected.IsCancellationRequested) action();
            });
        }
        _ws = socket = new WsClient(
            url,
            _allocationId,
            // Marshal inbound commands onto the game thread.
            onInbound: json => OnGameThread(() => HandleInbound(json)),
            log: (level, message) => _host.Log(level, message),
            onConnected: () => OnGameThread(() => SendServerStatus()),
            // Enqueue runs on the game thread, so this does too: the hole goes into the record.
            onRecovered: dropped => { if (_server.Match is { } match) match.State.LogEvent(LogLevel.Warn,
                $"{dropped} events were lost while the agent was not acknowledging"); });

        _ws.Start();
    }

    /// <summary>Flushes and closes the socket, as the plugin unloads.</summary>
    public async ValueTask DisposeAsync()
    {
        // Free of the game thread's context: the plugin blocks on this as it unloads.
        if (_ws is { } ws) await ws.DisposeAsync().ConfigureAwait(false);
        _ws = null;
    }
}
