# shellcheck shell=bash
# Sourced by the scripts that read the contract: protocol-sync.sh and check.sh. `build_protocol`
# builds @ankorcs/protocol from the repository that the container sees at /ankorcs, in a folder of
# its own, and exports ANKOR_PROTOCOL_DIR, which codegen reads (codegen/protocol.mjs). The mount is
# read-only, so the repository's node_modules and dist stay as they are.

# The checkout's packages/protocol and the root files its build reads: tracked and untracked files,
# so an uncommitted contract change counts, without ignored ones such as node_modules and dist.
protocol_files() {
  git -c safe.directory='*' -C /ankorcs ls-files -z -co --exclude-standard -- \
    package.json package-lock.json tsconfig.base.json packages/protocol
}

# A hash of protocol_files' contents, which changes with any of them.
protocol_hash() {
  (cd /ankorcs && protocol_files | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1)
}

build_protocol() {
  local work="$1"

  [[ -f /ankorcs/packages/protocol/package.json ]] || { echo "/ankorcs does not hold packages/protocol" >&2; exit 2; }
  mkdir -p "$work/ankorcs"
  protocol_files | tar -C /ankorcs --null --ignore-failed-read -T - -cf - | tar -C "$work/ankorcs" -xf -
  (cd "$work/ankorcs" && npm ci --ignore-scripts --no-audit --no-fund --no-update-notifier \
    --workspace @ankorcs/protocol --include-workspace-root >/dev/null)
  # Only the modules: codegen reads no AsyncAPI document.
  (cd "$work/ankorcs/packages/protocol" && npx tsc -p tsconfig.build.json)
  export ANKOR_PROTOCOL_DIR="$work/ankorcs/packages/protocol"
}
