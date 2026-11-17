---
title: Backups and restores
description: How a match's rounds are backed up, and how a match plays on from one of them.
slug: backups
---

Ankor backs up a live match round by round, and a match can play on from one of those backups. If a
match's server dies, your controller can launch the match again from the start of a round that it reached, on
any node. Your controller can also make a running match play a round again. [Launch a game](protocol/index.md#6-launch-a-game)
covers launches in general.

## Round backups

A round backup is a file that CS2 writes as each live round starts. The file holds the round, the
scores, and the money and equipment of every player. Ankor's plugin reads the file and sends it to
your controller as [event:round_backup]. The event also carries team 1's side, the match's rules
and the roster, because the CS2 file does not hold them. The plugin turns backups on only when a
match goes live.

- CS2 writes the files to the slot's `ankor/backups/` directory, one `ankor_roundNN.txt` per round.
  Each file is numbered by the rounds played before it, so round 5 is in `ankor_round04.txt`.
  [Configuration and files](configuration.md) shows where the directory sits in the slot. When the
  next launch in the slot starts, the agent empties the directory.
- Keep the [event:round_backup] events on your controller, because they are the copy that lasts.
  Together with the map's launch, the newest event holds everything that a restore on another node
  needs.
- If the plugin cannot send a round's file, for example because the file is missing, empty or
  older than the round, it sends no backup for that round. The plugin sends an `error` [event:log]
  with the reason instead.
- CS2 writes no file for a restored round, so the plugin sends no [event:round_backup] for it.
  After a [event:match_restored], the match stands at the backup that the server sent when the
  restored round first started. The match stays at that backup until a later round sends a new
  one. Keep every backup of the map, so that your controller can find the backup that the match
  stands at.

## Restore a running match

[agent:restore_round] plays a running match on from one of its earlier rounds, on the same server.
The plugin loads the file that CS2 wrote as that round started. The match must be live. It must
not be in the halftime break, or between the end of a round and the start of the next round. The
reference entry for [agent:restore_round] lists every reason that the node can refuse the request.

After you send the request, the restore runs in these steps:

1. The agent replies once the plugin has asked CS2 to load the backup.
2. The restored round opens paused in its freeze time. The plugin lifts the pause about a second
   later.
3. The plugin sends [event:match_restored] ahead of the [event:round_start] of the restored round.
   From that round start on, the events of the server replace everything that it sent for that
   round and the rounds after it.

The node keeps the backup of every round that the server has played. That includes rounds after
the one it was restored to, so a restore can also go forward, to play that the match has since
left.

**Never move a match forward in time after you go back and play any part of the loaded round.**

Once players play the restored round, the events of the server replace everything that it sent for
that round and the rounds after it. The later backups on the node still describe the old play. If
you load one of them, one match has two separate timelines. Their scores, money and events no
longer agree, and the problems that follow are beyond counting. Like if you went back to 1955
and told your parents not to have you.

A forward restore is safe only while no part of the restored round has been played. The node
accepts a forward restore, because the contract leaves this rule to the controller. After any part
of the restored round has been played, your controller should refuse a forward restore.

## Restore on a new server

When a match's server has died, launch the same [`match_id`](/reference/types/#schema-MatchConfig)
again with [agent:start_server]. You can use any node, and the launch must have a new
`allocation_id`. Set [`match.restore`](/reference/types/#schema-MatchConfig) from the newest
[event:round_backup] of the map. After a restore, use the backup that the match stands at instead,
as [Round backups](#round-backups) describes. [`RestoreFile`](/reference/types/#schema-RestoreFile)
lists the fields of `match.restore`.

If the match does not send statistics, the plugin sends each backup in [agent:backup] instead.
That message is sent once, so a backup is lost if the plugin or the agent is disconnected when the
round starts. The next round's backup replaces it.

The new server then plays on from the backup in these steps:

1. The agent writes the backup into the slot as `ankor/restore.txt`.
2. Warmup runs as usual, and the [ready system](ready-system.md) decides when it ends. The plugin
   seats team 1 on the side that the backup records.
3. After the countdown, the server skips the knife round and the side choice. It starts round 1
   and loads the backup from inside that round.
4. The plugin sends [event:match_restored]. The match plays on under the `rules` of the restore,
   which replace the rules of the launch.

Until the backup is loaded, the server reports the match as `pending_restore`. If the load fails,
the plugin sends the reason in a [event:log], and the match stays `pending_restore`. The plugin
tries the load again at the start of the next round. The match leaves `pending_restore` when a
load succeeds, or when your controller stops the server.

## Loads from the console

**Use [agent:restore_round] instead.** When someone types a load at the server console, the load
bypasses the controller. The controller learns of the load only from the events that follow it.
Nothing checks that the file belongs to this match, or that it is the round you meant. If you load
the wrong file, the match plays on from a state that it never reached. The events record that
play, so the statistics then do not match the match.

If you must load a backup by hand, open the [server console](operations.md#server-console). Run
`mp_backup_restore_load_file` with the path of the file, relative to the game directory. This
command plays round 5 again:

```
mp_backup_restore_load_file ankor/backups/ankor_round04.txt
```

Each file is numbered by the rounds played before it. The file of round _n_ is `ankor_round`
followed by _n_ − 1 in at least two digits, so `ankor_round00.txt` restores round 1. The `"round"`
key inside the file holds the same number as the file name.

The plugin follows a console load the same way as its own load, and sends [event:match_restored].
The plugin refuses a console load in these cases:

- The file does not exist.
- The match is in the knife round, the side choice or the halftime break.
- A round has ended, and the next round has not started yet.
- The match has ended.

When the plugin refuses a load, it logs the reason on the server.
