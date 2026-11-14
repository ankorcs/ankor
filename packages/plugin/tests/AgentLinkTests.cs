using System.Net.WebSockets;
using System.Text.Json;
using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public class AgentLinkTests
{
    private const string AllocationId = "0199a0b4-5c00-7000-8000-00000000b001";

    private sealed class Log : AgentLink.ILog
    {
        public readonly List<string> Lines = [];
        public void NoSocket(string frame) => Lines.Add($"no socket {frame}");
        public void Exec(string command) => Lines.Add($"exec {command}");
        public void SkippingType(string type) => Lines.Add($"skipping {type}");
        public void IgnoredFrame(string excerpt) => Lines.Add($"ignored {excerpt}");
    }

    /// <summary>A loaded match whose commands answer as told, and whose capture scopes are recorded.</summary>
    private sealed class Match(MatchState state, string? startRefusal = null, string? restoreRefusal = null) : AgentLink.IMatch
    {
        public readonly List<string> Scopes = [];
        public readonly List<int> Restores = [];
        public MatchState State => state;
        public IDisposable? BeginCapture(string name)
        {
            Scopes.Add(name);
            return null;
        }
        public string? RestoreRound(int round)
        {
            Restores.Add(round);
            return restoreRefusal;
        }
        public string? StartMatch() => startRefusal;
    }

    private sealed record Server(AgentLink.IMatch? Match, bool Ready, string? ConfigError) : AgentLink.IServer;

    private static MatchState State(FakeGameHost host) => new(host, Fixture.Config(), new FakeAgentChannel(), Fixture.Messages, () => { });

    /// <summary>A started link and the agent's end of its socket, past the status sent on connecting.</summary>
    private static async Task<(AgentLink Link, WebSocket Peer, JsonElement Status)> Connect(MockEventAgent agent, FakeGameHost host,
        AgentLink.IMatch? match, Log log, bool ready = false, string? configError = null)
    {
        var link = new AgentLink(host, AllocationId, new Server(match, ready, configError), log);
        link.Start(agent.Url);
        var peer = await agent.AcceptAsync();
        var status = JsonDocument.Parse(await MockEventAgent.ReceiveAsync(peer)).RootElement;
        return (link, peer, status);
    }

    private static async Task<JsonElement> Reply(WebSocket peer) => JsonDocument.Parse(await MockEventAgent.ReceiveAsync(peer)).RootElement;

    [Fact]
    public async Task SendsItsStatusUnaskedOnConnectingAndAnswersAStatusRequestWithItsId()
    {
        await using var agent = await MockEventAgent.StartAsync();
        var host = new FakeGameHost();
        var (link, peer, status) = await Connect(agent, host, null, new Log(), ready: true, configError: "bad roster");
        await using var _ = link;

        Assert.Equal("server_status", status.GetProperty("type").GetString());
        Assert.Equal(JsonValueKind.Null, status.GetProperty("request_id").ValueKind);
        Assert.True(status.GetProperty("ready").GetBoolean());
        Assert.Equal("bad roster", status.GetProperty("config_error").GetString());

        link.HandleInbound("""{"type":"get_server_status","request_id":"s1"}""");
        Assert.Equal("s1", (await Reply(peer)).GetProperty("request_id").GetString());
    }

    [Fact]
    public async Task PassesCommandsToTheMatchInsideACaptureScopeAndRefusesThemWithoutOne()
    {
        await using var agent = await MockEventAgent.StartAsync();
        var host = new FakeGameHost();
        var match = new Match(State(host), startRefusal: CommandResult.TeamEmpty, restoreRefusal: CommandResult.NoBackup);
        var (link, peer, _) = await Connect(agent, host, match, new Log());
        await using var _link = link;

        link.HandleInbound("""{"type":"start_match","request_id":"m1"}""");
        Assert.Equal("team_empty", (await Reply(peer)).GetProperty("error").GetString());
        link.HandleInbound("""{"type":"restore_round","round":4,"request_id":"r1"}""");
        Assert.Equal("no_backup", (await Reply(peer)).GetProperty("error").GetString());
        Assert.Equal([4], match.Restores);
        Assert.Equal(["control.inbound", "control.inbound"], match.Scopes);

        await using var bare = await MockEventAgent.StartAsync();
        var (without, withoutPeer, _) = await Connect(bare, host, null, new Log());
        await using var _without = without;
        without.HandleInbound("""{"type":"start_match","request_id":"m2"}""");
        Assert.Equal("no_match", (await Reply(withoutPeer)).GetProperty("error").GetString());
    }

    [Fact]
    public async Task AnswersAnExecBeforeItRunsTheCommand()
    {
        await using var agent = await MockEventAgent.StartAsync();
        var host = new FakeGameHost();
        var log = new Log();
        var (link, peer, _) = await Connect(agent, host, null, log);
        await using var _link = link;

        link.HandleInbound("""{"type":"exec","command":"say hi","request_id":"e1"}""");
        var result = await Reply(peer);

        Assert.Equal("e1", result.GetProperty("request_id").GetString());
        Assert.Equal(JsonValueKind.Null, result.GetProperty("error").ValueKind);
        Assert.Contains(host.Commands, command => command.Command == AgentControl.ExecLine("say hi"));
        Assert.Contains("exec say hi", log.Lines);
    }

    [Fact]
    public async Task RefusesAnExecThatNamesABlockedCommandOnlyWhileAMatchIsLoaded()
    {
        await using var agent = await MockEventAgent.StartAsync();
        var host = new FakeGameHost();
        var (link, peer, _) = await Connect(agent, host, new Match(State(host)), new Log());
        await using var _link = link;

        link.HandleInbound("""{"type":"exec","command":"say hi; mp_restartgame 1","request_id":"b1"}""");
        var result = await Reply(peer);

        Assert.Equal("b1", result.GetProperty("request_id").GetString());
        Assert.Equal("command_blocked", result.GetProperty("error").GetString());
        Assert.DoesNotContain(host.Commands, command => command.Command.Contains("say hi"));

        await using var freeAgent = await MockEventAgent.StartAsync();
        var freeHost = new FakeGameHost();
        var (free, freePeer, _) = await Connect(freeAgent, freeHost, null, new Log());
        await using var _free = free;
        free.HandleInbound("""{"type":"exec","command":"mp_restartgame 1","request_id":"b2"}""");
        Assert.Equal(JsonValueKind.Null, (await Reply(freePeer)).GetProperty("error").ValueKind);
        Assert.Contains(freeHost.Commands, command => command.Command == AgentControl.ExecLine("mp_restartgame 1"));
    }

    [Theory]
    [InlineData("mp_restartgame 1", "mp_restartgame")]
    [InlineData("MP_WARMUP_END", "MP_WARMUP_END")]
    [InlineData("say hi;mp_swapteams", "mp_swapteams")]
    [InlineData("say hi\n  \"changelevel\" de_dust2", "changelevel")]
    [InlineData("\talias x \"mp_restartgame 1\"", "alias")]
    [InlineData("say \"a;map de_nuke\"", "map")]
    [InlineData("say mp_restartgame", null)]
    [InlineData("mp_restartgame_extra 1", null)]
    [InlineData("restart", "restart")]
    [InlineData("mp_pause_match", null)]
    [InlineData("status", null)]
    public void FindsABlockedCommandAtTheStartOfAnyCommandInTheLine(string line, string? blocked) =>
        Assert.Equal(blocked, AgentControl.BlockedCommand(line));

    [Fact]
    public async Task RefusesAnUnknownRequestLogsItsTypeOnceAndIgnoresAFrameItCannotRead()
    {
        await using var agent = await MockEventAgent.StartAsync();
        var host = new FakeGameHost();
        var log = new Log();
        var (link, peer, _) = await Connect(agent, host, null, log);
        await using var _link = link;

        link.HandleInbound("""{"type":"teleport","request_id":"u1"}""");
        Assert.Equal("unsupported_request", (await Reply(peer)).GetProperty("error").GetString());
        link.HandleInbound("""{"type":"teleport","request_id":"u2"}""");
        Assert.Equal("u2", (await Reply(peer)).GetProperty("request_id").GetString());
        link.HandleInbound("not json");

        Assert.Equal(["skipping teleport", "ignored not json"], log.Lines);
    }

    [Fact]
    public void LogsAFrameThatHasNoSocketToGoTo()
    {
        var log = new Log();
        var link = new AgentLink(new FakeGameHost(), AllocationId, new Server(null, false, null), log);

        link.Send(new KnifeStart());

        Assert.Single(log.Lines, line => line.StartsWith("no socket ", StringComparison.Ordinal));
        Assert.Equal(0, link.PendingCount);
    }
}
