#!/usr/bin/env bash
#
# The protocol, the agent and the CS2 image release on one line: they share their major.minor, and
# only their patch versions differ. ci.yml and release.yml run this before anything else, so a
# change that moves one of them to another minor alone fails its checks and releases nothing.
#
#   scripts/release-line.sh   # prints the shared line, or fails naming each version

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

protocol="$(sed -n 's/^  "version": "\(.*\)",$/\1/p' packages/protocol/package.json)"
agent="$(scripts/nodes.sh version agent)"
cs2="$(scripts/nodes.sh version cs2)"

line() { printf '%s' "${1%.*}"; }

if [[ "$(line "$protocol")" != "$(line "$agent")" || "$(line "$agent")" != "$(line "$cs2")" ]]; then
  echo "release-line: the protocol (${protocol}), the agent (${agent}) and the CS2 image (${cs2}) must share their major.minor" >&2
  exit 1
fi

echo "release line $(line "$protocol"): protocol ${protocol}, agent ${agent}, CS2 image ${cs2}"
