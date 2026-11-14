#!/usr/bin/env bash
#
# The SwiftlyS2 plugin from the workstation: the replay fixture, C# coverage and mutation testing,
# the check of its contract values against packages/protocol, and the event captures a development
# game server wrote.
#
#   scripts/plugin.sh replay [OUTPUT]          # regenerate the committed replay fixture (or write elsewhere)
#   scripts/plugin.sh coverage                 # C# tests with an HTML coverage report under coverage/plugin
#   scripts/plugin.sh mutants                  # mutation testing of Ankor.Core, report under coverage/plugin-mutants
#   scripts/plugin.sh contract                 # check the plugin's copy of the contract's values, and the replay fixture, against packages/protocol
#   scripts/plugin.sh captures IMAGE [LAUNCH]  # list the dev captures, or print one JSONL stream
#
# `captures` runs against the game node's Docker (use its context, or DOCKER_HOST=ssh://<node>)
# with the cs2 image installed there; the volume is the one the agent mounts for captures.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly REPLAY_FIXTURE=packages/plugin/tests/fixtures/plugin-replay.jsonl
readonly CAPTURE_VOLUME=ankor_dev_event_captures

usage() {
  sed -n '3,/^$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

case "${1:-}" in
  replay)
    # The fixture is what the plugin emits for the C# test scenario; consumers ingest it without
    # .NET. It is committed, and the plugin's tests fail until it is regenerated here after a
    # change to what the plugin sends.
    [[ $# -le 2 ]] || { echo "Usage: scripts/plugin.sh replay [OUTPUT]" >&2; exit 2; }
    exec dotnet run --project packages/plugin/replay/Ankor.Replay.csproj -- "${2:-$REPLAY_FIXTURE}"
    ;;
  contract)
    # The plugin's tests cannot read packages/protocol, so the comparison runs here, against the
    # built package.
    npm run build -w @ankorcs/protocol >&2
    exec node scripts/plugin-contract.mjs
    ;;
  coverage)
    # The tool manifest is the plugin's (packages/plugin/.config), so the tool commands run there.
    report_dir="$PWD/coverage/plugin"
    coverage_results="$(mktemp -d "${TMPDIR:-/tmp}/ankor-coverage.XXXXXX")"
    trap 'rm -rf "$coverage_results"' EXIT
    cd packages/plugin
    dotnet tool restore
    # The shipped code that tests can load: the adapter needs CS2, and Ankor.Testing is the harness.
    dotnet test tests --collect:"XPlat Code Coverage" \
      --results-directory "$coverage_results" \
      -- DataCollectionRunSettings.DataCollectors.DataCollector.Configuration.Include='[Ankor.Core]*,[Ankor.Diagnostics]*'
    dotnet tool run reportgenerator "-reports:$coverage_results/**/coverage.cobertura.xml" \
      "-targetdir:$report_dir" '-reporttypes:Html;TextSummary;Cobertura'
    cat "$report_dir/Summary.txt"
    ;;
  mutants)
    report_dir="$PWD/coverage/plugin-mutants"
    cd packages/plugin/tests
    dotnet tool restore
    exec dotnet tool run dotnet-stryker -- --config-file ../stryker-config.json --output "$report_dir"
    ;;
  captures)
    capture_image="${2:?Usage: scripts/plugin.sh captures IMAGE [LAUNCH]}"
    capture_launch="${3:-}"
    if [[ -n "$capture_launch" && ! "$capture_launch" =~ ^[A-Za-z0-9_-]+$ ]]; then
      echo "Invalid capture launch id" >&2
      exit 1
    fi
    docker volume inspect "$CAPTURE_VOLUME" >/dev/null
    exec docker run --rm --pull never --network none --read-only --cap-drop ALL \
      --security-opt no-new-privileges --pids-limit 64 \
      --mount "type=volume,source=${CAPTURE_VOLUME},target=/captures,readonly" \
      --entrypoint /bin/sh "$capture_image" -c \
      'if [ -z "$1" ]; then ls -1 /captures; else cat "/captures/$1.jsonl"; fi' sh "$capture_launch"
    ;;
  -h | --help | '')
    usage
    exit 0
    ;;
  *)
    echo "plugin: unknown command \"$1\" (replay, coverage, mutants, contract, captures)" >&2
    exit 1
    ;;
esac
