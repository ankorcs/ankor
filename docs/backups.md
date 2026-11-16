---
title: Backups and restores
description: How a match's rounds are backed up, and how a match plays on from one of them.
slug: backups
---

A live match is backed up round by round. If a match's server dies, the match can play on from the
start of a round it reached, on any node. A running match can also play a round again.

## Round backups

As each live round starts, CS2 writes a backup of it into the slot. The backup holds the round, the
scores, and the money and equipment of every player. Ankor's plugin reads the file and sends it as
[event:round_backup]. The event adds team 1's side, the match's rules and the roster, which the
file does not hold. Warmup and the knife round have no backups.

- The files are in the slot's `ankor/backups/` directory, one `ankor_roundNN.txt` per round. Each
  file is numbered by the rounds played before it. [Configuration and files](configuration.md)
  shows the slot's layout. The agent replaces the files at the next launch in the slot.
- The [event:round_backup] events are the copy that lasts. With the map's launch, the newest one
  is all that a restore on another node needs.
- When a round's file is missing, empty or older than the round, the plugin sends no backup for
  that round. It sends an `error` [event:log] instead.
- CS2 writes no file for a restored round, and the plugin sends no [event:round_backup] for it.
  After an [event:match_restored], the match stands at the backup of the round it was restored
  to. The server sent that backup when the round first started. The match stays at it until a
  later round sends a new one. Keep every backup of the map so a controller can find it.

## Restore a running match

[agent:restore_round] plays a running match on from one of its earlier rounds, on the same server.
The plugin loads the file that CS2 wrote as that round started. The match must be live, and neither
in the halftime break nor between a round's end and the next round's start. The reference entry
lists the refusals.

1. The reply comes once the plugin has asked CS2 to load the backup.
2. The restored round opens paused in its freeze time, and the plugin lifts the pause shortly
   after.
3. The plugin sends [event:match_restored] ahead of the restored round's [event:round_start]. From
   that round start on, the server's events replace everything it sent for that round and the
   rounds after it.

The node keeps the backup of every round the server has played, including rounds after the one it
was restored to. A restore can therefore also go forward, to play that the match has since left.
The next section says when that is safe.

## The space-time continuum

**Never move a match forward in time after you go back and play any part of the loaded round.**

Once the restored round is played, the server's events replace everything it sent for that round
and the rounds after it. The later backups on the node still describe the old play. Loading one of
them creates two separate timelines in one match. Their scores, money and events no longer agree,
and the problems that follow are beyond counting.

A forward restore is safe only while no part of the restored round has been played. After that,
a controller should refuse it.

## Restore on a new server

When a match's server has died, launch the same [`match_id`](/reference/types/#schema-MatchConfig)
again with [agent:start_server]. Use any node and a new `allocation_id`. Set
[`match.restore`](/reference/types/#schema-MatchConfig) from the newest [event:round_backup] of the
map. [`RestoreFile`](/reference/types/#schema-RestoreFile) lists its fields.

A match without statistics sends each backup in [agent:backup] instead. That backup is lost if the
plugin or the agent is disconnected when the round starts.

1. The agent writes the backup into the slot as `ankor/restore.txt`.
2. Warmup runs as usual, and the [ready system](ready-system.md) decides when it ends. The plugin
   seats team 1 on the backup's side.
3. After the countdown, the server skips the knife round and the side choice. It starts round 1 and
   loads the backup from inside it.
4. The plugin sends [event:match_restored], and the match plays on under the restore's `rules`.

Until the backup is loaded, the server reports the match as `pending_restore`. If the load fails,
the plugin gives the reason in an [event:log]. The server stays `pending_restore` until a load is
tried again or the controller stops the server.

## Loads from the console

**Use [agent:restore_round] instead.** A load typed at the console bypasses the controller. The
controller learns of it only from the events that follow. Nothing checks that the file belongs to
this match or is the round you meant. A wrong file plays the match on from a state it never
reached. The events record that play, so the statistics then do not match the match.

If you must, load a backup from the [server console](operations.md#server-console) with
`mp_backup_restore_load_file` and the file's path relative to the game directory:

```
mp_backup_restore_load_file ankor/backups/ankor_round04.txt
```

A file is numbered by the rounds played before it, so the file of round _n_ is
`ankor_round` followed by _n_ − 1 in at least two digits. The command above plays round 5 again.
`ankor_round00.txt` restores round 1. The `"round"` key inside the file holds the same number.

The plugin follows a console load the same way as its own, and sends [event:match_restored]. It
refuses to load a file that does not exist. It also refuses a load during the knife round, the side
choice or the halftime break, and after the match has ended. It logs why on the server.
