#!/usr/bin/env bash
#
# The parts of the repository that a change touches, for ci.yml and release.yml. Prints one
# `part=true` or `part=false` line per part, ready for $GITHUB_OUTPUT.
#
#   scripts/ci-changes.sh BASE HEAD   # the parts that the commits from BASE to HEAD change
#
# Every part counts as changed when git cannot compare BASE, such as on a new branch. A change to
# ci.yml, release.yml, this script or the toolchain the checks run in also changes every part.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

[[ $# -eq 2 ]] || { echo "Usage: scripts/ci-changes.sh BASE HEAD" >&2; exit 2; }

readonly PARTS='protocol agent cs2 docs'
readonly EVERY='^\.github/workflows/(ci|release)\.yml$|^scripts/ci-changes\.sh$|^scripts/toolchain\.Dockerfile$|^compose\.yaml$|^\.github/actions/toolchain/'

# The paths each part reads. When a package starts reading another path, add it here. Every part
# that reads @ankorcs/protocol lists packages/protocol/ and the root install.
pattern() {
  case "$1" in
    protocol) echo '^packages/protocol/|^package(-lock)?\.json$|^tsconfig\.json$|^eslint\.config\.ts$|^\.nvmrc$|^scripts/npm-readme\.mjs$|^scripts/markdown-links\.mjs$|^\.github/workflows/check-protocol\.yml$' ;;
    agent) echo '^packages/agent/|^packages/protocol/|^package(-lock)?\.json$|^\.github/workflows/check-agent\.yml$' ;;
    cs2) echo '^packages/plugin/|^packages/protocol/|^package(-lock)?\.json$|^\.nvmrc$|^scripts/plugin(\.sh|-contract\.mjs)$|^\.github/workflows/check-cs2\.yml$' ;;
    docs) echo '^docs/|^packages/protocol/|^package(-lock)?\.json$|^eslint\.config\.ts$|^\.nvmrc$|^\.github/workflows/(check|deploy)-docs\.yml$' ;;
  esac
}

if ! git cat-file -e "${1}^{commit}" 2>/dev/null; then
  for part in $PARTS; do
    echo "${part}=true"
  done
  exit 0
fi

changed="$(git diff --name-only "$1" "$2")"

# Markdown beside the code (READMEs, conventions, architecture) is in no image or package, so it
# changes only the docs site. npm ships the protocol's README and CHANGELOG, so they change the
# protocol too.
code="$(grep -Ev '\.md$' <<<"$changed" || true)"
code+=$'\n'"$(grep -Ex 'packages/protocol/(README|CHANGELOG)\.md' <<<"$changed" || true)"

for part in $PARTS; do
  files="$code"
  [[ "$part" == docs ]] && files="$changed"
  if grep -Eq "$(pattern "$part")|${EVERY}" <<<"$files"; then
    echo "${part}=true"
  else
    echo "${part}=false"
  fi
done
