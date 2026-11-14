---
title: Ready system
description: How a match's warmup ends, the players' ready commands, force ready and the ready deadline.
slug: ready-system
---

A match starts in warmup. The players warm up while Ankor's plugin waits for both teams to be
ready. Once they are, the plugin counts down and starts the knife round or live play. A player who
leaves during the countdown does not stop it.

## Ready modes

The match's [`ready_mode`](/reference/types/#schema-MatchConfig) decides when a team is ready.
[`ReadyMode`](/reference/types/#schema-ReadyMode) describes each mode. In every mode, bots and
players outside the roster never make a team ready.

### Auto

A team is ready once all its rostered players are connected. Players have no ready commands, and
the match does not wait for spectators.

### Manual

Each rostered player also types `!ready` in chat, and `!unready` withdraws it. When the match sets
[`spectators_ready_up`](/reference/types/#schema-MatchConfig), the match also waits for every
rostered spectator to connect and type `!ready`.

- A `!ready` lasts until the player leaves the server.
- The plugin regularly names the players, and the spectators it waits for, who are not ready.
- The scoreboard shows `READY` or `NOT READY` as each one's clan tag.

### External

Players have no ready commands, and only the controller starts the match, with
[agent:start_match].

## Force ready

A match allows `!forceready` only with
[`allow_force_ready`](/reference/types/#schema-MatchConfig) on. Without it, the command does
nothing, and the match waits for every rostered player's `!ready`.

Under `manual`, a team's captain can type `!forceready`. The captain is the first player of the
team in the roster. The command readies the captain's team with whichever of its players are
connected. The team then stops waiting for the rest of its roster and for their `!ready`. The
countdown starts once the other team is ready too, either the normal way or by its own captain's
`!forceready`.

When the match waits for spectators, any rostered spectator can also type `!forceready`. It readies
the spectators and no team. A rostered spectator who does not show up then does not block the
match, as long as another one is connected. When none of them connects, only the ready deadline or
[agent:start_match] ends the warmup.

- A force-ready lasts for the rest of the warmup, and `!unready` cannot withdraw it.
- A team with none of its players connected is never ready, force-readied or not.
- The plugin sends each force-ready as a `warn` [event:log] that names the team, or the
  spectators, and who typed it.

## Start from the controller

[agent:start_match] starts the countdown in every mode without waiting for anyone to be ready. An
admin can therefore always start a match whose players are stuck in warmup. The plugin refuses it
until someone from each team is connected. The reply comes when the countdown has started.

## Ready deadline

[`ready_timeout_seconds`](/reference/types/#schema-MatchConfig) in the match sets a deadline for
both teams to be ready. The warmup clock in game counts the deadline down, and the plugin warns in
chat as it approaches.

When the deadline passes, the plugin tells players that the match was canceled and ignores any later
ready-up. It ends warmup, ends the match with the game's end screen and ends the map with
[event:map_end]. Ten seconds later it kicks every player.

The `reason` in [event:map_end] is `ready_timeout`, and `teams` says how many players of each team
were connected and whether the team was ready. A force-readied team counts as ready. The map names
no winner, so your controller decides the outcome, such as a win for the ready team or a draw.

- `ready_timeout_seconds` must be null under `external`, because only the controller starts the
  match there.
- If nobody joins, the deadline still passes on the empty server. The agent's
  [`idle_shutdown_seconds`](/reference/types/#schema-ServerLaunchConfig) stops an empty server
  first when it is shorter, and the agent then reports [agent:server_exited] instead.
