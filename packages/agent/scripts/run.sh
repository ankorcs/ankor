#!/usr/bin/env bash
# Runs one command in the checks' container (scripts/lib/in-docker.sh), for what check.sh does not
# cover: one test, an opt-in suite, a cargo command. `--docker` also hands it the host's Docker
# socket. `--conformance` does too, and sets the container up for the conformance suite, which the
# repository's scripts/conformance.sh runs through it. ANKOR_* variables reach the command.
#
#   scripts/run.sh cargo nextest run -p ankor-agent node_log
#   scripts/run.sh cargo nextest run -p ankor-agent --test registry_live --run-ignored only
#   scripts/run.sh --docker cargo nextest run -p ankor-agent --test docker_service --run-ignored only
# shellcheck source-path=SCRIPTDIR/..
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lib/in-docker.sh

mode=""
case "${1:-}" in
  --docker)
    IN_DOCKER_SOCKET=1
    mode="$1"
    shift
    ;;
  --conformance)
    IN_DOCKER_SOCKET=1
    # The suite serves the fakes on the ports the agent calls, 80 and 443, as the caller's user, and
    # its transcripts, diffs and logs land in target/conformance on the host, not the build volume.
    mkdir -p target/conformance
    IN_DOCKER_OPTIONS=(--sysctl net.ipv4.ip_unprivileged_port_start=0 -v "$PWD/target/conformance:/src/target/conformance")
    mode="$1"
    shift
    ;;
esac

[[ $# -gt 0 ]] || { echo "usage: scripts/run.sh [--docker | --conformance] <command> [arguments]" >&2; exit 2; }

if [[ -z "${ANKOR_CHECK_CONTAINER:-}" ]]; then
  in_docker ${mode:+"$mode"} "$@"
fi

exec "$@"
