#!/usr/bin/env bash
# Every check a change must pass, in the checks' container (scripts/lib/in-docker.sh), the same way
# locally and in CI.
#
#   scripts/check.sh                       format, lints, tests, licences, advisories and the image's notices
#   scripts/check.sh --coverage            also line coverage
#   scripts/check.sh --mutants             also mutation testing
#   scripts/check.sh --docker              also the Docker suites, against the host's daemon
#
# The options combine.
# shellcheck source-path=SCRIPTDIR/..
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lib/in-docker.sh
source scripts/lib/protocol.sh

coverage=0
mutants=0
docker=0

for argument in "$@"; do
  case "$argument" in
    --coverage) coverage=1 ;;
    --mutants) mutants=1 ;;
    --docker) docker=1 ;;
    *)
      echo "unknown option ${argument}; see the top of ${0}" >&2
      exit 2
      ;;
  esac
done

IN_DOCKER_SOCKET="$docker"
# The corpus is written from packages/protocol, which the container sees read-only at /ankorcs.
IN_DOCKER_OPTIONS=(-v "$(cd ../.. && pwd):/ankorcs:ro")
in_docker "$@"

# The protocol and contract tests replay zod's verdicts from the corpus, which is not committed. It
# is written again when its inputs change, the ones CI keys its corpus cache on, so a contract
# change cannot leave the tests replaying old verdicts.
corpus_inputs="$( (find codegen/corpus.mjs codegen/generate.mjs codegen/protocol.mjs codegen/package-lock.json -type f | LC_ALL=C sort | xargs sha256sum; protocol_hash) | sha256sum | cut -d' ' -f1)"
corpus_stamp=crates/protocol/tests/corpus.stamp
if [[ ! -f crates/protocol/tests/corpus.jsonl || ! -f crates/agent/tests/contract_corpus.jsonl || "$(cat "$corpus_stamp" 2>/dev/null)" != "$corpus_inputs" ]]; then
  protocol_work="$(mktemp -d)"
  build_protocol "$protocol_work"
  (cd codegen && npm ci --ignore-scripts && npm run corpus)
  rm -rf "$protocol_work"
  echo "$corpus_inputs" >"$corpus_stamp"
fi

cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
# The image builds without features, so code only the test doubles use goes unlinted above.
cargo clippy --workspace -- -D warnings
cargo nextest run --workspace --all-features
# nextest does not run doctests.
cargo test --workspace --all-features --doc
cargo deny check
cargo audit
# The notices that the agent's image ships list the crates its binary bundles (about.toml). Some
# crates' licence texts have CRLF line ends, which git would change, so the file has LF only.
notices="$(cargo about generate --frozen --fail -c about.toml -m crates/agent/Cargo.toml about.hbs | tr -d '\r')"
if [[ "$notices" != "$(cat THIRD-PARTY-LICENSES.txt)" ]]; then
  echo "THIRD-PARTY-LICENSES.txt is out of date. Regenerate it with" >&2
  echo "  scripts/run.sh sh -c \"cargo about generate --frozen --fail -c about.toml -m crates/agent/Cargo.toml about.hbs | tr -d '\\\\r' > THIRD-PARTY-LICENSES.txt\"" >&2
  exit 1
fi

if ((docker)); then
  cargo nextest run -p ankor-agent --test docker_service --run-ignored only
fi

if ((coverage)); then
  cargo llvm-cov --workspace --all-features --summary-only --ignore-filename-regex '(testing|schemas|types|constants)\.rs$|testing/'
fi

if ((mutants)); then
  # cargo-mutants gives cargo its own jobserver, one task per CPU by default, which overrides
  # CARGO_BUILD_JOBS. A full machine of compilers and linkers stalls Docker Desktop.
  cargo mutants --workspace --jobserver-tasks 6
fi
