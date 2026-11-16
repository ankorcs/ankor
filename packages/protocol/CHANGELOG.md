# Changelog

## 0.10.0

- `between_rounds` refuses a `restore_round` sent after a round's `round_end` and before the next
  `round_start`: a backup the engine loads in that window is never restarted into.

## 0.9.0

Initial release.
