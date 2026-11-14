#!/usr/bin/env bash
#
# The game-node images, from the workstation: build them and copy them to the nodes over SSH. No
# registry is involved and the nodes never build; releases to the registry are
# .github/workflows/release.yml. Each image carries its own version: the agent's Cargo.toml and the
# plugin's AnkorVersion. `load` tags it there as X.Y.Z, X.Y, X and latest, the tags a registry would
# hold. A node moves to the load with node-setup.sh --update --no-pull, or from its controller with
# update on its tag, pull off.
#
#   scripts/nodes.sh build [agent|cs2 ...]    # linux/amd64 images tagged with the commit
#   scripts/nodes.sh load root@host ...       # copy both images to the nodes, tagged there with their versions
#   scripts/nodes.sh deploy root@host ...     # build, then load
#   scripts/nodes.sh version agent|cs2        # print the image's X.Y.Z version
#
# The images are named ghcr.io/ankorcs/ankor-agent and ankor-cs2, as releases and agent.yml name
# them, so a loaded build replaces the release tags on a node; --image-prefix names others, which a
# node runs only with official_images_only: false in its agent.yml.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly script_name=nodes
readonly options_help="Allow an uncommitted tree; the tag gets a -dirty suffix."
# shellcheck source=scripts/lib.sh
source scripts/lib.sh

image_ref() {
  printf '%s/ankor-%s:%s' "$(image_repo)" "$1" "$2"
}

# The image's semantic version, X.Y.Z, from the package that owns it.
image_version() {
  local version
  case "$1" in
    agent) version="$(sed -n 's/^version = "\(.*\)"$/\1/p' packages/agent/crates/agent/Cargo.toml)" ;;
    cs2) version="$(sed -n 's:.*<AnkorVersion>\(.*\)</AnkorVersion>.*:\1:p' packages/plugin/Directory.Build.props)" ;;
  esac
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "no X.Y.Z version for the $1 image"
  printf '%s' "$version"
}

# The commands that tag a loaded image X.Y.Z, X.Y, X and latest on the node.
version_tags() {
  local name="$1" source="$2" version
  version="$(image_version "$name")"
  local tag
  for tag in "$version" "${version%.*}" "${version%%.*}" latest; do
    printf 'docker tag %q %q && ' "$source" "$(image_ref "$name" "$tag")"
  done
}

# Each node image belongs to the package that owns it: cs2's first stage compiles the plugin.
dockerfile_for() {
  case "$1" in
    agent) printf 'packages/agent/docker/Dockerfile' ;;
    cs2) printf 'packages/plugin/docker/Dockerfile' ;;
    *) die "unknown image \"$1\"" ;;
  esac
}

# Each image builds from its own package.
context_for() {
  case "$1" in
    agent) printf 'packages/agent' ;;
    cs2) printf 'packages/plugin' ;;
    *) die "unknown image \"$1\"" ;;
  esac
}

# ---------------------------------------------------------------------------------------------
# build — linux/amd64 only (steamcmd and CS2 are x86_64, and so are the nodes).
# ---------------------------------------------------------------------------------------------
cmd_build() {
  local selection=("$@") tag name ref
  ((${#selection[@]} > 0)) || selection=(agent cs2)
  for name in "${selection[@]}"; do
    [[ "$name" =~ ^(agent|cs2)$ ]] || die "unknown image \"$name\" (agent or cs2)"
  done
  tag="$(git_tag)"
  for name in "${selection[@]}"; do
    ref="$(image_ref "$name" "$tag")"
    echo "==> ${ref}"
    run docker buildx build --load --platform linux/amd64 -f "$(dockerfile_for "$name")" \
      --build-arg VERSION="$(image_version "$name")" -t "$ref" "$(context_for "$name")"
  done
}

# ---------------------------------------------------------------------------------------------
# load — both images over SSH, then tagged with their versions on the node so node-setup.sh has
# nothing to pull. A running node moves to them as the header says.
# ---------------------------------------------------------------------------------------------
cmd_load() {
  local targets=("$@")
  ((${#targets[@]} > 0)) || die "no node: pass each as root@host"
  local tag agent_image cs2_image
  tag="$(git_tag)"
  agent_image="$(image_ref agent "$tag")"
  cs2_image="$(image_ref cs2 "$tag")"
  require_local_amd64 "$agent_image"
  require_local_amd64 "$cs2_image"

  local target
  for target in "${targets[@]}"; do
    echo "==> ${target}: images"
    load_images "$target" "$agent_image" "$cs2_image"
    run ssh "$target" "$(version_tags agent "$agent_image")$(version_tags cs2 "$cs2_image")true"
  done
  echo "Loaded agent $(image_version agent) and cs2 $(image_version cs2) (${tag}). Move a running node to them with node-setup.sh --update --no-pull, or update on its tag with pull off in its controller."
}

cmd_deploy() {
  (($# > 0)) || die "no node: pass each as root@host"
  cmd_build
  cmd_load "$@"
}

# The release workflow reads the version an image would be released as here.
cmd_version() {
  [[ $# -eq 1 && "${1:-}" =~ ^(agent|cs2)$ ]] || die "version takes one image (agent or cs2)"
  image_version "$1"
  echo
}

parse_options "$@"
set -- "${args[@]}"
command="$1"
shift
case "$command" in
  build) cmd_build "$@" ;;
  load) cmd_load "$@" ;;
  deploy) cmd_deploy "$@" ;;
  version) cmd_version "$@" ;;
  *) die "unknown command \"$command\" (build, load, deploy, version)" ;;
esac
