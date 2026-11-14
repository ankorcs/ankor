using Ankor.Core;
using Ankor.Core.Wire;
using Xunit;

namespace Ankor.Tests;

public sealed class GameplayLifecycleTests
{
    private readonly FakeGameHost _host = new();
    private readonly List<PluginMessage> _sent = [];
    private readonly MatchState _match;
    private readonly DamageTracker _damage;
    private readonly GameplayCallbacks _gameplay;
    private int _cleared;
    private int _shutdowns;

    public GameplayLifecycleTests()
    {
        _match = new MatchState(_host, Fixture.Config(SidesMode.Team1Ct), new FakeAgentChannel(_sent), Fixture.Messages, () => { });
        var forwarder = new EventForwarder(_match, _host);
        _damage = new DamageTracker(forwarder);
        var teams = new TeamLock(_match, _host, Fixture.Messages);
        _gameplay = new GameplayCallbacks(_match, _host, forwarder, _damage, teams, () => _cleared++, () => _shutdowns++, () => { }, () => { });
    }

    [Fact]
    public void TeamChangeEnforcesPlacementAndStartsReadyMatch()
    {
        _match.EnterWarmup();
        var player = Fixture.Player(Fixture.Team1Player1, Side.T);
        _host.PlayerList.AddRange([
            player,
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 1),
            Fixture.Player(Fixture.Team2Player1, Side.T, 2),
            Fixture.Player(Fixture.Team2Player2, Side.T, 3),
        ]);
        _gameplay.Handle(new PlayerTeamInput(player, Side.Spec, Side.T));
        // The team lock seats the player.
        Assert.Equal(Side.Ct, Assert.Single(_host.SwitchedTeams).Side);
        _host.Advance(14);
        Assert.Equal(MatchPhase.Warmup, _match.Phase);
        _host.IsWarmup = false;
        _match.OnRoundStart();
        Assert.Equal(MatchPhase.Live, _match.Phase);
    }

    [Fact]
    public void DisposalCancelsQueuedDepartureDecision()
    {
        BringLive();
        _host.DeferNextFrames = true;
        _gameplay.Handle(new PlayerDisconnectInput(Fixture.Player(Fixture.Team1Player1, Side.Ct), "disconnect_by_user"));
        _gameplay.Dispose();
        _host.RunNextFrame();
        Assert.Empty(_sent.OfType<MapEnd>());
        Assert.Equal(0, _shutdowns);
    }

    [Fact]
    public void NewMapCancelsOldStartupAndResetsPendingDamageWork()
    {
        var pendingRan = false;
        _damage.Enter(1);
        _damage.RunAfterDamage(() => pendingRan = true);
        _gameplay.Handle(new MapLoadInput());
        var oldStartup = Assert.Single(_host.Timers);
        _damage.Exit();
        Assert.False(pendingRan);
        _host.Advance(0.5);
        _gameplay.Handle(new MapLoadInput());
        oldStartup.Callback();
        _host.Advance(0.5);
        Assert.Equal(MatchPhase.Loading, _match.Phase);
        _host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.T));
        _host.Advance(0.5);
        Assert.Equal(MatchPhase.Warmup, _match.Phase);
        Assert.Equal(2, _cleared);
        Assert.Single(_host.SwitchedTeams);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void MapStartupChecksPlayersWhoConnectedBeforeWarmup(bool completeRoster)
    {
        _gameplay.Handle(new MapLoadInput());
        _host.PlayerList.AddRange([
            Fixture.Player(Fixture.Team1Player1, Side.Ct),
            Fixture.Player(Fixture.Team1Player2, Side.Ct, 1),
            Fixture.Player(Fixture.Team2Player1, Side.T, 2),
        ]);
        if (completeRoster) _host.PlayerList.Add(Fixture.Player(Fixture.Team2Player2, Side.T, 3));
        foreach (var player in _host.PlayerList) _gameplay.Handle(new PlayerConnectInput(player));
        Assert.Equal(MatchPhase.Loading, _match.Phase);
        _host.Advance(1);
        Assert.Empty(_host.SwitchedTeams); // Correct placement produces no further team callback.
        _host.Advance(14);
        Assert.Equal(MatchPhase.Warmup, _match.Phase);
        Assert.Equal(completeRoster ? 1 : 0,
            _host.Commands.Count(c => c.Command == $"mp_warmuptime {MatchState.CountdownSeconds}"));
        Assert.Equal(completeRoster, _host.CommandNames.Contains("mp_warmup_end"));
        if (completeRoster)
        {
            _host.IsWarmup = false;
            _match.OnRoundStart();
            Assert.Equal(MatchPhase.Live, _match.Phase);
        }
    }

    [Fact]
    public void DisposingDuringMapStartupPreventsDelayedPlacement()
    {
        _gameplay.Handle(new MapLoadInput());
        _host.PlayerList.Add(Fixture.Player(Fixture.Team1Player1, Side.T));
        _gameplay.Dispose();
        _host.Advance(1);
        Assert.Empty(_host.SwitchedTeams);
        Assert.Empty(_host.Kicked);
        Assert.NotEqual(MatchPhase.Warmup, _match.Phase);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void DepartureRechecksAfterCleanupAndHonorsReconnect(bool reconnect)
    {
        BringLive();
        _host.DeferNextFrames = true;
        var player = Fixture.Player(Fixture.Team1Player1, Side.Ct);
        _host.PlayerList.Add(player);
        _gameplay.Handle(new PlayerDisconnectInput(player, "disconnect_by_user"));
        Assert.Empty(_sent.OfType<MapEnd>());
        _host.PlayerList.Clear();
        if (reconnect) _host.PlayerList.Add(player);
        _host.RunNextFrame();
        Assert.Empty(_sent.OfType<MapEnd>());
        _host.RunNextFrame();
        Assert.Equal(reconnect ? 0 : 1, _sent.OfType<MapEnd>().Count());
        Assert.Equal(reconnect ? 0 : 1, _shutdowns);
    }

    [Fact]
    public void ABotLeavingAnEmptyLiveServerIsNotADeparture()
    {
        BringLive();

        _gameplay.Handle(new PlayerDisconnectInput(Fixture.Bot(Side.Spec, 10), "disconnect_by_user"));

        Assert.Empty(_sent.OfType<PlayerDisconnect>());
        Assert.Empty(_sent.OfType<MapEnd>());
        Assert.Equal(0, _shutdowns);
    }

    [Fact]
    public void ForcedEndWaitsForDamageAndEmitsOnlyOnce()
    {
        BringLive();
        _host.DeferNextFrames = true;
        _damage.Enter(1);
        EveryoneLeaves();
        EveryoneLeaves();
        _host.RunNextFrame();
        Assert.Empty(_sent.OfType<MapEnd>());
        _damage.Exit();
        Assert.Empty(_sent.OfType<MapEnd>());
        _host.RunNextFrame();
        Assert.Single(_sent.OfType<MapEnd>());
        Assert.Equal(1, _shutdowns);
        Assert.Equal(MatchPhase.PostLive, _match.Phase);
    }

    [Fact]
    public void DisposalCancelsQueuedMapFinalization()
    {
        BringLive();
        _host.DeferNextFrames = true;
        EveryoneLeaves();
        _host.RunNextFrame();
        _gameplay.Dispose();
        _host.RunNextFrame();
        Assert.Empty(_sent.OfType<MapEnd>());
        Assert.Equal(0, _shutdowns);
    }

    /// <summary>The match is live only from the engine's first round start after the plugin ends warmup.</summary>
    private void BringLive()
    {
        _match.GoLive();
        _host.IsWarmup = false;
        _match.OnRoundStart();
    }

    /// <summary>The last rostered player leaves: the way a match ends outside the win panel.</summary>
    private void EveryoneLeaves()
    {
        _host.PlayerList.Clear();
        _gameplay.Handle(new PlayerDisconnectInput(Fixture.Player(Fixture.Team1Player1, Side.Ct), "disconnect_by_user"));
    }

    private void BeginSideChoice(bool open)
    {
        _host.IsWarmup = false;
        _match.StartKnifeRound();
        _match.FinishKnifeRound(new KnifeTally(Side.Ct, 1, 2, 100, 200), Side.Ct);
        if (open) _match.OnRoundStart();
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void ForceEndClosesSideChoiceWhileFinalizationWaitsForDamage(bool open)
    {
        BeginSideChoice(open);
        var timer = _host.Timers.SingleOrDefault(timer => !timer.Fired && !timer.Cancelled);
        _host.DeferNextFrames = true;
        _damage.Enter(1);
        EveryoneLeaves();
        _host.RunNextFrame();
        if (timer is not null) Assert.True(timer.Cancelled);

        _match.OnRoundStart();
        Assert.False(_match.TrySideChoice(Fixture.Player(Fixture.Team1Player1, Side.Ct), SideChoiceKind.Swap));
        timer?.Callback();
        _host.Advance(60);
        Assert.Empty(_host.Timers);
        Assert.Empty(_sent.OfType<SideChoice>());
        Assert.Empty(_sent.OfType<MapEnd>());
        _host.RunNextFrame();
        Assert.Empty(_sent.OfType<MapEnd>());
        _damage.Exit();
        Assert.Equal(MatchPhase.PostLive, _match.Phase);
        Assert.Single(_sent.OfType<MapEnd>());
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void MapLoadClosesSideChoiceBeforeDeferredWarmup(bool engineAlreadyCancelled)
    {
        BeginSideChoice(open: true);
        var timer = Assert.Single(_host.Timers, timer => !timer.Fired && !timer.Cancelled);
        if (engineAlreadyCancelled) _host.ChangeMap("de_nuke");
        _gameplay.Handle(new MapLoadInput());
        Assert.True(timer.Cancelled);
        _match.OnRoundStart();
        Assert.False(_match.TrySideChoice(Fixture.Player(Fixture.Team1Player1, Side.Ct), SideChoiceKind.Stay));
        var commands = _host.CommandNames.ToArray();
        timer.Callback();
        Assert.Equal(commands, _host.CommandNames.ToArray());
        Assert.Empty(_sent.OfType<SideChoice>());
        _host.Advance(1);
        Assert.Equal(MatchPhase.Warmup, _match.Phase);
        Assert.DoesNotContain(_host.Timers, timer => !timer.Fired && !timer.Cancelled);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void DisposingTheCoordinatorClosesPendingAndOpenSideChoice(bool open)
    {
        BeginSideChoice(open);
        var timer = _host.Timers.SingleOrDefault(timer => !timer.Fired && !timer.Cancelled);
        _gameplay.Dispose();
        _gameplay.Dispose();
        _match.Dispose();
        if (timer is not null) Assert.True(timer.Cancelled);
        timer?.Callback();
        _gameplay.Handle(new MapLoadInput());
        _match.OnRoundStart();
        _host.Advance(60);
        Assert.Empty(_host.Timers);
        Assert.Empty(_sent.OfType<SideChoice>());
        Assert.Equal(MatchPhase.PostLive, _match.Phase);
    }

    [Fact]
    public void RejectedConnectionInWarmupIsKickedUnreportedAndDoesNotStartTheMatch()
    {
        _match.EnterWarmup();
        _gameplay.Handle(new PlayerConnectInput(Fixture.Player(Fixture.Stranger, Side.Ct)));
        Assert.Single(_host.Kicked);
        Assert.Equal(MatchPhase.Warmup, _match.Phase);
        Assert.DoesNotContain(_sent, message => message is PlayerConnect);
    }
}
