---
title: Ready system
description: How a match's warmup ends, the players' ready commands, force ready and the ready deadline.
slug: ready-system
---

A launch with a [`match`](/reference/types/#schema-MatchConfig) object starts in warmup. Your
controller launches the server with [agent:start_server]. The players can connect and warm up, but
the match starts only when both teams are ready. The roster is the list of players that the match
lets in, by team, with the captain of each team first. Ankor's plugin decides when a team is ready,
by the rules of the match's ready mode. When both teams are ready, the plugin counts down for 15
seconds and then starts the knife round or live play.

Your controller chooses the ready-up settings as fields of the launch's `match`.

## Ready modes

The match's [`ready_mode`](/reference/types/#schema-MatchConfig) decides when a team is ready.
[`ReadyMode`](/reference/types/#schema-ReadyMode) lists the modes.

### Auto

Under `auto`, a team is ready as soon as all of its rostered players are connected. Players have no
ready commands, and the match does not wait for spectators. `auto` is the default when the match leaves
`ready_mode` out.

### Manual

Under `manual`, each rostered player must also type `!ready` or `!r` in chat. To
withdraw a ready, the player types `!unready`, `!notready` or `!ur`. When the match sets
[`spectators_ready_up`](/reference/types/#schema-MatchConfig), the match also waits for every
rostered spectator to connect and type `!ready`.

While the match waits, the plugin shows who is ready:

- A player's `!ready` lasts until the player leaves the server. If the player reconnects, they must
  type `!ready` again.
- Every 30 seconds, the plugin names in chat the connected players who are not ready yet, and the
  spectators when the match waits for them.
- The scoreboard shows `READY` or `NOT READY` as the clan tag of each player that the match waits
  for. The plugin clears the tags when the countdown starts.

### External

Under `external`, only your controller starts the match, by sending [agent:start_match]. Players
have no ready commands.

## Force ready

Force ready lets a match start when a rostered player does not show up. The match allows
`!forceready` only under `manual`, and only with
[`allow_force_ready`](/reference/types/#schema-MatchConfig) on. The match then waits for
every rostered player's `!ready`.

If enabled, a team's captain can type `!forceready`. The captain is the first player of the team in the
roster. The command readies the captain's team with whichever of its players are connected. The
team then stops waiting for the rest of its roster and for their `!ready`. The countdown starts once
the other team is ready too, either the normal way or by `!forceready` from its captain.

When the match waits for spectators, any rostered spectator can also type `!forceready`. A
spectator's `!forceready` readies the spectators and no team. So a rostered spectator who never
connects does not block the match. If no rostered spectator connects, only the ready deadline
or [agent:start_match] ends the warmup.

- A force-ready lasts for the rest of the warmup, and `!unready` cannot withdraw it.
- A team with none of its players connected is never ready, even after a force-ready.
- When the match sends statistics, the plugin reports each force-ready as a `warn` [event:log].
  The log line names the team, or the spectators, and the player who typed the command.

## Start from the controller

Your controller can start the countdown in every mode by sending [agent:start_match]. The plugin
then does not wait for anyone to be ready, so an admin can always start a match whose players are
stuck in warmup. The plugin refuses the start until at least one rostered player of each team is
connected. The reply comes when the countdown has started.

## Ready deadline

By default, warmup has no time limit. To set one, give
[`ready_timeout_seconds`](/reference/types/#schema-MatchConfig) in the match. It is a deadline,
counted from the start of warmup, for both teams to be ready. The warmup clock in game counts the
deadline down. As the deadline approaches, the plugin warns in chat every minute, and every 30
seconds in the last five minutes. Once the countdown starts, the deadline no longer applies.

When the deadline passes, the plugin tells the players that the match was canceled. It ignores
any ready-up after that, and kicks any player who joins. The plugin then ends warmup, ends the
match with the game's end screen and ends the map. Ten seconds later it kicks every player, and
the server shuts down.

If the match sends statistics, the plugin reports the end of the map in [event:map_end]. Its
`reason` is `ready_timeout`, and `teams` says how many players of each team were connected and
whether the team was ready. A force-readied team counts as ready. The map names no winner, so your
controller decides the outcome, such as a win for the ready team or a draw. Without statistics,
the plugin sends no [event:map_end], and your controller sees the match move from `warmup` to
`post_live` in [agent:phase_change].

- `ready_timeout_seconds` must be null under `external`, because only your controller starts the
  match in that mode. A launch that sets both is rejected.
- If nobody joins, the deadline still passes on the empty server. If the launch's
  [`idle_shutdown_seconds`](/reference/types/#schema-ServerLaunchConfig) is shorter, the agent
  stops the empty server first. The agent then reports [agent:server_exited] instead.
