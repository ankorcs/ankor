#!/usr/bin/env bash
# Runs the agent's conformance suite (packages/agent/crates/conformance). It builds the images the
# suite tests on the host's Docker: the plugin's headless image, the suite's game image on top of
# it, and the agent's image. Then it runs the suite in the agent's checks container
# (packages/agent/scripts/run.sh --conformance). Docker is all it needs. Arguments go to the suite:
#
#   scripts/conformance.sh                      # every scenario
#   scripts/conformance.sh --scenario launch    # one, or several
#   scripts/conformance.sh --update             # rewrite the golden transcripts
set -euo pipefail
cd "$(dirname "$0")/.."

# The sandbox the suite starts never pulls, so its image is pulled here, at the version the suite pins.
docker pull -q "$(sed -n 's/^pub const DIND_IMAGE: &str = "\(.*\)";$/\1/p' packages/agent/crates/conformance/src/sandbox.rs)" >/dev/null
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' packages/agent/crates/agent/Cargo.toml)"
docker build -q -f packages/plugin/headless/docker/Dockerfile -t ankor-cs2-headless:latest packages/plugin >/dev/null
docker build -q -f packages/agent/crates/conformance/docker/fake-cs2.Dockerfile -t ankor-conformance/fake-cs2:latest packages/agent >/dev/null
docker build -q -f packages/agent/docker/Dockerfile --build-arg VERSION="$version" -t ankor-conformance/agent:latest packages/agent >/dev/null

exec packages/agent/scripts/run.sh --conformance cargo run -q -p ankor-conformance --bin ankor-conformance -- "$@"
