# Changelog

## 0.10.0

- The plugin refuses a `restore_round` with `between_rounds` when it arrives between a round's
  `round_end` and the next `round_start`. The engine applies the scores of a backup loaded then,
  but the server stays loading. Send the restore again once the next round has started.
- With `auto_update` on, the agent looks for a rebuild of the running version under that version's
  `X.Y.Z` tag. It used to compare the tag in `agent.yml`, so a node on `latest` that ran a newer
  version than `latest` was moved back to `latest`.

## 0.9.0

Initial release.
