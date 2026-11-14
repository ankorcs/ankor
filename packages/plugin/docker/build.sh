#!/usr/bin/env bash
#
# Builds the CS2 game-server image from packages/plugin, the build context, whose first stage
# compiles the plugin.
#
#   packages/plugin/docker/build.sh                  -> ankor-cs2:dev
#   packages/plugin/docker/build.sh ankor-cs2:1.2.3  -> that tag
#
# CS2 is x86_64-only, so --platform linux/amd64 is always passed. On an ARM host the image builds
# under emulation and the game itself will not run; use it to verify the image contents only.
#
# The plugin's xunit tests run inside the Dockerfile's first stage, before the release build, so
# they gate this script too and no .NET SDK is needed on the host. Among other things they fail the
# build when a chat message key has no translation (packages/plugin/tests/MessagesTests.cs).

set -euo pipefail

TAG="${1:-ankor-cs2:dev}"
[ $# -gt 0 ] && shift
PLUGIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

exec docker build \
  --platform linux/amd64 \
  -f "$PLUGIN/docker/Dockerfile" \
  -t "$TAG" \
  "$@" \
  "$PLUGIN"
