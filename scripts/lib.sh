# shellcheck shell=bash disable=SC2154
#
# What the workstation scripts share; sourced, never run. A script that takes the common options
# sets `script_name` and an `options_help` line for --dirty, then calls `parse_options "$@"`, which
# leaves the command and its arguments in `args`.

image_prefix="ghcr.io/ankorcs"
dirty_ok=0
dry_run=0
args=()

usage() {
  # The header comment, up to its first blank line.
  sed -n '3,/^$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
  cat <<USAGE

Options (before the command):
  --image-prefix <p>  Where the image names start (default ghcr.io/ankorcs, the names nodes run).
  --dirty             ${options_help}
  --dry-run           Print the commands instead of running them.
USAGE
}

die() {
  echo "${script_name}: $*" >&2
  exit 1
}

run() {
  if ((dry_run)); then
    printf '+ %s\n' "$*"
  else
    "$@"
  fi
}

parse_options() {
  while (($# > 0)); do
    case "$1" in
      --image-prefix) [[ -n "${2:-}" ]] || die "--image-prefix needs a value"; image_prefix="$2"; shift 2 ;;
      --dirty) dirty_ok=1; shift ;;
      --dry-run) dry_run=1; shift ;;
      -h | --help) usage; exit 0 ;;
      -*) die "unknown option $1 (--help for the list)" ;;
      *) break ;;
    esac
  done
  args=("$@")
  ((${#args[@]} > 0)) || { usage >&2; exit 1; }
}

image_repo() {
  printf '%s' "${image_prefix%/}"
}

# The tag is the short commit, so it names what was built; a dirty tree is refused unless --dirty.
git_tag() {
  local sha
  sha="$(git rev-parse --short HEAD 2>/dev/null)" || die "not a git checkout; the tag is the commit"
  if [[ -n "$(git status --porcelain)" ]]; then
    ((dirty_ok)) || die "uncommitted changes: commit them so the tag names what was built, or pass --dirty"
    sha="${sha}-dirty"
  fi
  printf '%s' "$sha"
}

# A tag in this machine's image store can be a multi-platform index, so the amd64 entry is what
# is asked for; an index whose amd64 layers were never pulled answers with an empty platform.
require_local_amd64() {
  local platform
  platform="$(docker image inspect --platform linux/amd64 --format '{{.Os}}/{{.Architecture}}' "$1" 2>/dev/null)" ||
    die "$1 is not on this machine; run \`scripts/${script_name}.sh build\` first"
  [[ "$platform" == "linux/amd64" ]] || die "$1 has no linux/amd64 image here (got '${platform}'); the hosts are x86_64"
}

# Stream images to a host; ones already there (same id) are skipped.
load_images() {
  local target="$1"
  shift
  local image missing=()
  for image in "$@"; do
    local local_id remote_id
    # The amd64 entry's id is what the host sees after loading it, index or not.
    local_id="$(docker image inspect --platform linux/amd64 --format '{{.Id}}' "$image" 2>/dev/null || true)"
    [[ -n "$local_id" ]] || die "$image is not on this machine"
    remote_id="$(ssh "$target" "docker image inspect --format '{{.Id}}' $(printf '%q' "$image") 2>/dev/null" || true)"
    if [[ "$remote_id" == "$local_id" ]]; then
      echo "    ${image} already on ${target}"
    else
      missing+=("$image")
    fi
  done
  ((${#missing[@]} > 0)) || return 0
  echo "    loading ${missing[*]} onto ${target}"
  if ((dry_run)); then
    printf '+ docker save --platform linux/amd64 %s | ssh %s docker load\n' "${missing[*]}" "$target"
  else
    docker save --platform linux/amd64 "${missing[@]}" | ssh "$target" 'docker load'
  fi
}
