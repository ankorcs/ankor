#!/usr/bin/env bash
# Keeps the generated Rust contract in step with packages/protocol, the contract it is generated
# from. codegen/ builds the protocol from the checkout (scripts/lib/protocol.sh) and writes the
# Rust types, validators and constants of the wire contract and the container contract, which are
# committed so the agent builds with cargo alone. It runs in the checks' container
# (scripts/lib/in-docker.sh), which sees the repository read-only at /ankorcs.
#
#   scripts/protocol-sync.sh            # fails when the generated Rust is out of date
#   scripts/protocol-sync.sh --update   # regenerates it, writes the corpus and runs the protocol tests
# shellcheck source-path=SCRIPTDIR/..
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lib/in-docker.sh
source scripts/lib/protocol.sh

update=0
if [[ "${1:-}" == "--update" ]]; then
  update=1
  shift
fi

[[ $# -eq 0 ]] || { echo "usage: scripts/protocol-sync.sh [--update]" >&2; exit 2; }

if [[ -z "${ANKOR_CHECK_CONTAINER:-}" ]]; then
  IN_DOCKER_OPTIONS=(-v "$(cd ../.. && pwd):/ankorcs:ro")
  if ((update)); then in_docker --update; else in_docker; fi
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

build_protocol "$work"
version="$(node -p "require('$ANKOR_PROTOCOL_DIR/package.json').version")"

# What codegen writes, the crate versions included, which name the contract a build compiles.
generated=(
  crates/protocol/src/types.rs crates/protocol/src/schemas.rs crates/protocol/src/constants.rs
  crates/agent/src/contract/types.rs crates/agent/src/contract/schemas.rs crates/agent/src/contract/constants.rs
  codegen/hooks.md crates/protocol/Cargo.toml crates/agent/Cargo.toml crates/conformance/Cargo.toml
)

regenerate() {
  sed -i "s/^version = \".*\"$/version = \"$version\"/" crates/protocol/Cargo.toml
  sed -i "s/^ankor-protocol = { version = \"[^\"]*\"/ankor-protocol = { version = \"$version\"/" \
    crates/agent/Cargo.toml crates/conformance/Cargo.toml
  (cd codegen && npm ci --ignore-scripts --no-audit --no-fund --no-update-notifier >/dev/null && npm run generate >/dev/null)
}

if ((update == 0)); then
  # Generated over the checkout, compared and put back, so the check leaves the files as they were.
  mkdir -p "$work/before"
  tar -cf - "${generated[@]}" | tar -C "$work/before" -xf -
  regenerate
  stale=()
  for file in "${generated[@]}"; do
    cmp -s "$file" "$work/before/$file" || stale+=("$file")
  done
  tar -C "$work/before" -cf - "${generated[@]}" | tar -xf -

  if ((${#stale[@]} > 0)); then
    for file in "${stale[@]}"; do
      echo "protocol: $file is out of date with packages/protocol $version" >&2
    done
    echo "Run scripts/protocol-sync.sh --update" >&2
    exit 1
  fi

  echo "The generated contract matches packages/protocol $version"
  exit 0
fi

# Regenerate the Rust contract and the corpus, then hold the Rust validators to zod's verdicts.
regenerate
(cd codegen && npm run corpus)
cargo test -p ankor-protocol
cargo build --workspace

echo "Updated to @ankorcs/protocol $version. Review the generated changes with git diff."
