#!/usr/bin/env bash
#
# Ankor game-node setup. Turns a fresh x86_64 Linux box into a CS2 game host:
# Docker Engine, the shared CS2 install volume, `agent.yml`, and the `ankor-agent` container
# that dials the controller and waits for matches.
#
# Docker comes from Docker's own repositories on Debian, Ubuntu, Fedora, CentOS and RHEL-compatible
# systems, and from the distribution on Arch, openSUSE and Alpine. A Docker Engine already on the
# host is kept when it can run the containers as this script lays them out (see check_docker).
#
# The script asks for the node token the controller shows when you add the node (or takes it from
# ANKOR_NODE_TOKEN) and posts it to `<controller url><setup path>` (`/api/node/setup` unless
# --setup-path says otherwise), which answers with the node's whole
# `agent.yml` as text/yaml — token, the address this node dials and both image references.
# The script saves that file and starts the agent on it. Game servers are added in the controller
# once the node is connected. Everything else is fixed (`/srv/ankor`, `ankor_cs2_data`) or comes
# out of the file the controller rendered.
#
#   curl -fsSL https://ankor.sh/node-setup.sh | bash
#
# Root is acquired by the script itself: started unprivileged it re-runs through sudo (or su when
# there is no sudo), downloading itself again; the address and the token go over stdin, never in
# argv.
#
# Every step is idempotent and logged, so running it again is how you fix a half-finished run, and
# a re-run asks for nothing: an existing `agent.yml` is the node's identity. See
# https://docs.ankor.sh/install/ for the whole flow. `--dry-run` prints what would run without
# touching anything.

set -euo pipefail

# DEFAULT_CONTROLLER_URL and SETUP_SCRIPT_URL in @ankorcs/protocol's paths.ts.
readonly DEFAULT_CONTROLLER_URL="https://ankor.sh"
readonly DEFAULT_SETUP_PATH="/api/node/setup"
readonly SETUP_SCRIPT_URL="https://ankor.sh/node-setup.sh"

# Off the environment at once, so no command this script runs inherits it.
node_token="${ANKOR_NODE_TOKEN:-}"
unset ANKOR_NODE_TOKEN

# The protocol version this script sends with the node-setup request; PROTOCOL_VERSION in
# @ankorcs/protocol's paths.ts.
readonly PROTOCOL_VERSION=0.10
# The official images' repositories, OFFICIAL_IMAGES in @ankorcs/protocol's values.ts. Unless
# agent.yml sets official_images_only to false, this script runs no other image.
readonly OFFICIAL_AGENT_IMAGE=ghcr.io/ankorcs/ankor-agent
readonly OFFICIAL_CS2_IMAGE=ghcr.io/ankorcs/ankor-cs2
readonly CS2_VOLUME="ankor_cs2_data"
readonly AGENT_CONTAINER="ankor-agent"
# Where a node keeps agent.yml and its per-slot files. Fixed: the agent's own default is this, so
# nothing has to be told about it, and a node that needs another path is a hand edit plus a
# `docker rm -f ankor-agent` and a re-run.
readonly DATA_DIR="/srv/ankor"
readonly AGENT_CONFIG="${DATA_DIR}/agent.yml"
# agentUpdateFailurePath in the agent's hostLayout.ts: a failed agent swap, which a start here ends.
readonly AGENT_UPDATE_FAILURE="${DATA_DIR}/agent-update-failed.json"
# The game runs as uid 1000 inside the cs2 image; bind mounts keep the host's ownership.
readonly GAME_UID=1000
readonly GAME_GID=1000
readonly CONNECT_TIMEOUT_SECONDS=60
readonly DAEMON_TIMEOUT_SECONDS=60
# What start_agent mounts, at the path the agent image expects it.
readonly DOCKER_SOCKET="/var/run/docker.sock"
# DOCKER_API_VERSION and MIN_DOCKER_ENGINE in the agent's dockerApi.ts: the API version it names.
readonly DOCKER_API_VERSION="1.44"
readonly MIN_DOCKER_ENGINE="25.0"

# The CLI talks to the daemon the agent will, whatever context or DOCKER_HOST root's shell carries.
export DOCKER_HOST="unix://${DOCKER_SOCKET}"
unset DOCKER_CONTEXT

# Filled by detect_os.
os_id=""
os_name=""
# apt, dnf, pacman, zypper or apk.
os_family=""
# The download.docker.com/linux/<repo> tree, or empty where the distribution packages Docker.
docker_repo=""
docker_suite=""
in_lxc=0
readonly CONTROLLER_HINT="--controller takes the controller's address with its protocol, like https://cs.example.com: use https:// for a controller on the internet; http:// is only for a controller on your LAN"
readonly SETUP_PATH_HINT="--setup-path takes the path on the controller that answers with agent.yml, like /api/node/setup"
readonly LXC_HINT="in a Proxmox LXC, enable nesting on the Proxmox host (pct set <vmid> --features nesting=1) and restart the container"

# From --controller; empty means DEFAULT_CONTROLLER_URL, or the address in an existing agent.yml.
controller_url=""
# From --setup-path; empty means DEFAULT_SETUP_PATH.
setup_path=""
do_update=0
no_pull=0
dry_run=0

# Read out of the agent.yml the controller rendered (or the one already on disk), never assembled
# here.
agent_image=""
cs2_image=""

# ---------------------------------------------------------------------------------------------
# Output and command execution
# ---------------------------------------------------------------------------------------------

# `paint <color> <text>`: the text in one terminal color, for the helpers below.
paint() {
  local code
  case "$1" in
    cyan) code=36 ;;
    yellow) code=33 ;;
    red) code=31 ;;
    dim) code=2 ;;
  esac
  printf '\033[%sm%s\033[0m' "$code" "$2"
}

log() { printf '%s %s\n' "$(paint cyan '[node-setup]')" "$*"; }
skip() { printf '%s %s %s\n' "$(paint cyan '[node-setup]')" "$*" "$(paint dim '(already done)')"; }
warn() { printf '%s %s\n' "$(paint yellow '[node-setup] warning:')" "$*" >&2; }
die() {
  printf '%s %s\n' "$(paint red '[node-setup] error:')" "$*" >&2
  exit 1
}
# What a dry run would do, dimmed.
shown() { printf '%s\n' "$(paint dim "+ $*")"; }

# Runs a command, or prints it under --dry-run. Every mutating step goes through this, so a
# dry run is a complete, readable list of what the script would do to the machine.
run() {
  if ((dry_run)); then
    shown "$*"
    return 0
  fi
  "$@"
}

# Same, for a shell snippet that needs a pipe or a redirect.
run_sh() {
  if ((dry_run)); then
    shown "sh -c $(printf '%q' "$1")"
    return 0
  fi
  sh -c "$1"
}

have() { command -v "$1" >/dev/null 2>&1; }

usage() {
  cat <<'EOF'
Usage: node-setup.sh [--controller <url>] [--setup-path <path>] [--no-pull] [--dry-run]   # set this host up as a node
       node-setup.sh --update [--no-pull] [--dry-run]                              # update the agent

  curl -fsSL https://ankor.sh/node-setup.sh | bash

Setup asks for the node token the controller shows when you add the node. For an unattended
install, set it in ANKOR_NODE_TOKEN instead:

  curl -fsSL https://ankor.sh/node-setup.sh | ANKOR_NODE_TOKEN=<token> bash

Options
  --controller <url>   The controller the node belongs to; https://ankor.sh when absent.
                       https:// for one on the internet, http:// only for one on your LAN.
  --setup-path <path>  Where that controller answers setup; /api/node/setup when absent.
  --update             Re-create the agent from the image agent.yml names; running games keep
                       running. Needs no token, and fails on a host that was never set up.
  --no-pull            Use the agent image already on this host instead of pulling it.
  --dry-run            Print every command instead of running it. Asks for no token.
  -h, --help           This text.

A re-run keeps /srv/ankor/agent.yml and asks for no token.
More: https://docs.ankor.sh/setup-script/
EOF
}

# ---------------------------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------------------------

# Kept verbatim so an unprivileged start can re-run itself as root with the same arguments.
original_args=("$@")

while (($# > 0)); do
  case "$1" in
    --update) do_update=1 && shift ;;
    --no-pull) no_pull=1 && shift ;;
    --dry-run) dry_run=1 && shift ;;
    --controller)
      [[ -n "${2:-}" ]] || die "$CONTROLLER_HINT"
      controller_url="$2"
      shift 2
      ;;
    --setup-path)
      [[ -n "${2:-}" ]] || die "$SETUP_PATH_HINT"
      setup_path="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1 (--help for the list)" ;;
  esac
done

if [[ -n "$controller_url" ]]; then
  controller_url="${controller_url%/}"
  [[ "$controller_url" =~ ^https?://[^/[:space:]]+$ ]] || die "${CONTROLLER_HINT} (got \"${controller_url}\")"
  # The docs' example address: a command pasted unchanged would send the node token there.
  if [[ "$(printf '%s' "$controller_url" | tr '[:upper:]' '[:lower:]')" =~ ^https?://cs\.example\.com(:[0-9]+)?$ ]]; then
    die "--controller ${controller_url} is the example address from the docs; use your controller's own address, or remove --controller to connect to the official ankor.sh platform"
  fi
fi

# ankor.sh answers at the default path only.
if [[ -n "$setup_path" && ( -z "$controller_url" || "$controller_url" == "$DEFAULT_CONTROLLER_URL" ) ]]; then
  warn "--setup-path is ignored for ${DEFAULT_CONTROLLER_URL}, which answers at ${DEFAULT_SETUP_PATH}"
  setup_path=""
fi

# Kept to characters that need no quoting in curl's config.
if [[ -n "$setup_path" ]]; then
  [[ "$setup_path" =~ ^/[A-Za-z0-9._~/%-]*$ ]] || die "${SETUP_PATH_HINT} (got \"${setup_path}\")"
fi

# ---------------------------------------------------------------------------------------------
# Preconditions
# ---------------------------------------------------------------------------------------------

require_root() {
  [[ "$(id -u)" == 0 ]] && return 0
  if ((dry_run)); then
    warn "not root; a real run escalates through sudo or su"
    return 0
  fi

  # Started unprivileged. Re-run as root: the script arrived over a pipe, so download it again and
  # hand over the same arguments.
  #
  # The address and the token go over **stdin** of the elevated shell, the address into curl's
  # config: argv is world readable in `ps`, and sudo strips the environment. sudo and su both read
  # their password from the terminal, so stdin is free.
  local -a args=("${original_args[@]}")
  # Expanded by the *elevated* shell, not by this one: `"$@"` are its arguments.
  # shellcheck disable=SC2016
  local relay='set -o pipefail
IFS= read -r url || true
IFS= read -r token || true
printf "url = \"%s\"\n" "$url" | curl -fsSL -K - | ANKOR_NODE_TOKEN="$token" bash -s -- "$@"'

  # A plain call rather than `exec`: the elevated shell is on the far side of a pipe, so exec
  # would replace that subshell and leave *this* one to carry on unprivileged. The status of the
  # elevated run is this run's status.
  local status=0
  if have sudo; then
    log "not root; re-running through sudo"
    printf '%s\n%s\n' "$SETUP_SCRIPT_URL" "$node_token" |
      sudo bash -c "$relay" _ ${args[@]+"${args[@]}"} || status=$?
    exit "$status"
  fi
  if have su; then
    log "not root and no sudo; re-running through su (root password prompt follows)"
    local quoted=""
    ((${#args[@]} == 0)) || quoted="$(printf '%q ' "${args[@]}")"
    # `-s /bin/bash`: `printf %q` emits bash's `$'…'` form, which root's *login* shell has to
    # understand — and root's shell is very often /bin/sh or /bin/dash, which would take the
    # `$'…'` literally. Both util-linux `su` and shadow's accept `-s`.
    printf '%s\n%s\n' "$SETUP_SCRIPT_URL" "$node_token" |
      su root -s /bin/bash -c "bash -c $(printf '%q' "$relay") _ ${quoted}" || status=$?
    exit "$status"
  fi
  die "run this script as root: neither sudo nor su is available"
}

require_x86_64() {
  local arch
  arch="$(uname -m)"
  [[ "$arch" == x86_64 ]] && return 0
  ((dry_run)) && {
    warn "this host is ${arch}; a real run refuses anything but x86_64"
    return 0
  }
  die "CS2 and steamcmd are x86_64-only; this host is ${arch}"
}

# ---------------------------------------------------------------------------------------------
# agent.yml
#
# The controller builds the file: `POST <controller url><setup path>` with the node token renders
# the whole of it from the registry row and answers with it as text/yaml. What is read back out of
# it here is what the rest of the run needs: the configured image references.
# ---------------------------------------------------------------------------------------------

# Read the rendered top-level image fields, including preserved comments.
# These fields contain no whitespace or # characters; the controller writes one unindented key per
# line.
yaml_scalar() {
  local key="$1" file="$2" value
  value="$(sed -n "s/^${key}:[[:space:]]*//p" "$file" | head -n 1 |
    sed 's/^#.*$//; s/[[:space:]]#.*$//; s/[[:space:]]*$//')"
  value="${value%\"}"
  value="${value#\"}"
  value="${value%\'}"
  value="${value#\'}"
  printf '%s' "$value"
}

# An image reference without its digest, and without a tag after its last `/`.
image_repository() {
  local name="${1%%@*}"
  if [[ "${name##*/}" == *:* ]]; then
    name="${name%:*}"
  fi
  printf '%s' "$name"
}

# Everything the run needs from the file, whether the controller just sent it or it was already
# here.
read_agent_config() {
  local file="$1"
  cs2_image="$(yaml_scalar cs2_image "$file")"
  agent_image="$(yaml_scalar agent_image "$file")"

  [[ -n "$cs2_image" ]] || die "${file} has no \"cs2_image:\" line (the cs2 image the slots run)"
  [[ -n "$agent_image" ]] || die "${file} has no \"agent_image:\" line (the agent image this node runs)"

  if [[ "$(yaml_scalar official_images_only "$file")" != false ]]; then
    [[ "$(image_repository "$agent_image")" == "$OFFICIAL_AGENT_IMAGE" ]] ||
      die "${file} names the agent image ${agent_image}, which is not from ${OFFICIAL_AGENT_IMAGE}; set official_images_only: false in it to run images of your own"
    [[ "$(image_repository "$cs2_image")" == "$OFFICIAL_CS2_IMAGE" ]] ||
      die "${file} names the cs2 image ${cs2_image}, which is not from ${OFFICIAL_CS2_IMAGE}; set official_images_only: false in it to run images of your own"
  fi
}

have_tty() { { : </dev/tty; } 2>/dev/null; }

# The shape AgentConfigSchema.token requires.
valid_token() {
  [[ "$1" =~ ^[A-Za-z0-9]{32,128}$ ]]
}

# On the terminal, hidden: stdin is the script itself when it arrives through a pipe.
ask_token() {
  have_tty ||
    die "no terminal to ask for the node token on; set ANKOR_NODE_TOKEN: curl -fsSL ${SETUP_SCRIPT_URL} | ANKOR_NODE_TOKEN=<token> bash"
  printf 'Enter token: ' >/dev/tty
  IFS= read -r -s node_token </dev/tty || true
  printf '\n' >/dev/tty
}

# Fetches this node's agent.yml from `$1` with the node token and saves it.
fetch_agent_config() {
  local origin="$1" answer

  if ((dry_run)); then
    log "a real run fetches this node's configuration from ${origin}; using a placeholder"
    answer="$(placeholder_config "$origin")"
    shown "write ${AGENT_CONFIG}:"
    printf '%s\n' "$answer" | sed 's/^/    /'
    # The rest of the dry run reads the file, so it goes somewhere this run may write.
    local scratch
    scratch="$(mktemp "${TMPDIR:-/tmp}/ankor-agent-yml.XXXXXX")"
    printf '%s\n' "$answer" >"$scratch"
    read_agent_config "$scratch"
    rm -f "$scratch"
    return 0
  fi

  answer="$(request_agent_config "$origin")"

  # Written before Docker is installed, so the data directory may not exist yet.
  install -d -m 0755 "$DATA_DIR"
  chown "${GAME_UID}:${GAME_GID}" "$DATA_DIR"
  # Not a pipe: a pipeline would run it in a subshell and lose the images read_agent_config sets.
  save_agent_config <<<"$answer"
}

# The node's agent.yml as `$1` renders it for the node token. The token goes into curl's config on
# stdin, so it stays out of `ps`; the answer is the file, verbatim.
request_agent_config() {
  local origin="$1" answer
  log "asking ${origin} for this node's configuration" >&2

  local response status=000 escaped="${node_token//\\/\\\\}"
  escaped="${escaped//\"/\\\"}"
  if response="$(printf 'url = "%s"\nheader = "Authorization: Bearer %s"\n' "${origin}${setup_path:-$DEFAULT_SETUP_PATH}" "$escaped" |
    curl -sS --data-binary '' -H 'Accept: text/yaml' -H "Ankor-Protocol: ${PROTOCOL_VERSION}" \
      -w '\n%{http_code}' -K - 2>/dev/null)"; then
    status="${response##*$'\n'}"
    answer="${response%$'\n'*}"
  fi

  case "$status" in
    2[0-9][0-9]) ;;
    401) die "${origin} does not recognise this node token; copy it again from where you saved it" ;;
    409) refused_conflict "$origin" "$answer" ;;
    426) die "${origin} uses a protocol version incompatible with this script's" ;;
    429) die "${origin} refused too many setup attempts from this address; wait a minute and run the command again" ;;
    000) die "could not reach ${origin}; check that this host can reach it and run the command again" ;;
    *) die "${origin} did not answer with this node's configuration (HTTP ${status})" ;;
  esac
  printf '%s' "$answer"
}

# The two 409s: the node is connected from another host, or the controller cannot read its
# configuration.
refused_conflict() {
  local origin="$1" body="$2"
  if [[ "$body" == *'"node_connected"'* ]]; then
    die "This token belongs to a node that is connected from another host."
  fi
  die "${origin} cannot read this node's stored configuration; ask the controller's operator"
}

# Replaces AGENT_CONFIG with stdin in one step.
save_agent_config() {
  local next="${AGENT_CONFIG}.new"
  (umask 077 && cat >"$next")
  # Only the node's operator may let the node run other images, so the controller may not.
  if [[ "$(yaml_scalar official_images_only "$next")" == false ]]; then
    rm -f "$next"
    die "the controller's configuration turns official_images_only off, which only this node's operator may do"
  fi
  # The node token is in here, and the agent reads it as uid 1000.
  chown "${GAME_UID}:${GAME_GID}" "$next"
  # A wrong answer stays out of agent.yml, where every later run would refuse it.
  read_agent_config "$next"
  mv -f "$next" "$AGENT_CONFIG"
}

# What a dry run shows: the shape of the answer, with no real values.
placeholder_config() {
  cat <<EOF
# placeholder: a real run saves what ${1} rendered
token: <redacted>
controller:
  host: <the controller's host>
  tls: false
cs2_image: ${OFFICIAL_CS2_IMAGE}:<the release this deployment runs>
agent_image: ${OFFICIAL_AGENT_IMAGE}:<the release this deployment runs>
slots: []
EOF
}

# The file on disk is the node's identity, so a re-run reads it rather than fetching it again.
load_or_fetch_agent_config() {
  if [[ -e "$AGENT_CONFIG" ]]; then
    read_agent_config "$AGENT_CONFIG"
    skip "${AGENT_CONFIG} exists; keeping it"
    return 0
  fi
  fetch_agent_config "${controller_url:-$DEFAULT_CONTROLLER_URL}"
}
# ---------------------------------------------------------------------------------------------
# Distribution
# ---------------------------------------------------------------------------------------------

# ID first, then ID_LIKE, so a derivative lands on the family it is built from: Linux Mint on
# Ubuntu (with UBUNTU_CODENAME, which names the Ubuntu release it tracks), Rocky on RHEL,
# Manjaro on Arch.
detect_os() {
  [[ -r /etc/os-release ]] || die "cannot read /etc/os-release to tell which distribution this is"

  local like codename ubuntu_codename candidate
  {
    IFS= read -r os_id
    IFS= read -r like
    IFS= read -r codename
    IFS= read -r ubuntu_codename
    IFS= read -r os_name
  } < <(
    # shellcheck source=/dev/null
    . /etc/os-release
    printf '%s\n' "${ID:-}" "${ID_LIKE:-}" "${VERSION_CODENAME:-}" "${UBUNTU_CODENAME:-}" "${PRETTY_NAME:-${ID:-unknown}}"
  )

  for candidate in $os_id $like; do
    case "$candidate" in
      ubuntu) os_family=apt docker_repo=ubuntu docker_suite="${ubuntu_codename:-$codename}" ;;
      debian) os_family=apt docker_repo=debian docker_suite="$codename" ;;
      fedora) os_family=dnf docker_repo=fedora ;;
      centos) os_family=dnf docker_repo=centos ;;
      rhel | rocky | almalinux | ol) os_family=dnf docker_repo=rhel ;;
      arch) os_family=pacman ;;
      opensuse* | suse | sles) os_family=zypper ;;
      alpine) os_family=apk ;;
      *) continue ;;
    esac
    break
  done

  # Docker in LXC depends on features only the LXC host can grant; failures below name them.
  if [[ "$(systemd-detect-virt -c 2>/dev/null || true)" == lxc* ]] || grep -qa 'container=lxc' /proc/1/environ 2>/dev/null; then
    in_lxc=1
    log "this host runs ${os_name} in an LXC container"
  else
    log "this host runs ${os_name}"
  fi
}

lxc_hint() {
  if ((in_lxc)); then
    printf '; %s' "$LXC_HINT"
  fi
}

# Docker and the agent come back after a reboot only through an init system that starts dockerd.
require_init_system() {
  [[ "$(init_system)" == none ]] || return 0
  if ((dry_run)); then
    warn "no systemd or OpenRC runs as PID 1; a real run stops here"
    return 0
  fi
  die "no systemd or OpenRC runs as PID 1 on this host, so Docker could not start at boot"
}

# Without nesting, the LXC refuses the overlay mounts containerd unpacks every image with. The same
# mount in a throwaway namespace answers before setup changes anything, Docker installed or not.
require_lxc_nesting() {
  ((in_lxc)) || return 0
  have unshare || return 0

  local dir mounted=0
  dir="$(mktemp -d /var/tmp/ankor-overlay.XXXXXX)"
  mkdir "${dir}/lower" "${dir}/upper" "${dir}/work" "${dir}/merged"
  if unshare --mount sh -c "mount -t overlay overlay -o lowerdir=${dir}/lower,upperdir=${dir}/upper,workdir=${dir}/work ${dir}/merged" 2>/dev/null; then
    mounted=1
  fi
  rm -rf "$dir"

  ((mounted)) || die "this LXC cannot mount the overlay filesystems Docker runs every container on$(lxc_hint)"
  log "this LXC can mount overlay filesystems"
}

init_system() {
  if [[ -d /run/systemd/system ]]; then
    printf systemd
  elif have rc-service && have rc-update; then
    printf openrc
  else
    printf none
  fi
}

pkg_installed() {
  case "$os_family" in
    apt) dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q 'install ok installed' ;;
    dnf | zypper) rpm -q --quiet "$1" 2>/dev/null ;;
    pacman) pacman -Q "$1" >/dev/null 2>&1 ;;
    apk) apk info -e "$1" >/dev/null 2>&1 ;;
    *) return 1 ;;
  esac
}

pkg_install() {
  case "$os_family" in
    apt) run env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" ;;
    dnf) run dnf install -y -q "$@" ;;
    # Arch supports no partial upgrades: refreshing the database means upgrading with it.
    pacman) run pacman -Syu --needed --noconfirm "$@" ;;
    zypper) run zypper --non-interactive --quiet install "$@" ;;
    apk) run apk add --quiet "$@" ;;
    *) die "no package manager this script knows; install $* yourself and re-run" ;;
  esac
}

pkg_remove() {
  case "$os_family" in
    apt) run env DEBIAN_FRONTEND=noninteractive apt-get remove -y -qq "$@" ;;
    dnf) run dnf remove -y -q "$@" ;;
    pacman) run pacman -Rns --noconfirm "$@" ;;
    zypper) run zypper --non-interactive --quiet remove "$@" ;;
    apk) run apk del --quiet "$@" ;;
  esac
}

# ---------------------------------------------------------------------------------------------
# Steps
# ---------------------------------------------------------------------------------------------

# curl for Docker's keys and repositories, flock for lock_setup.
install_prerequisites() {
  local -a wanted=()
  [[ "$os_family" != apt ]] || pkg_installed ca-certificates || wanted+=(ca-certificates)
  have curl || wanted+=(curl)
  have flock || wanted+=(util-linux)
  if ((${#wanted[@]} == 0)); then
    skip "base tools (curl, flock, CA certificates)"
    return 0
  fi
  [[ -n "$os_family" ]] || die "missing ${wanted[*]} and no package manager this script knows; install them and re-run"

  log "base packages: ${wanted[*]}"
  case "$os_family" in
    apt) run apt-get update -qq ;;
    apk) run apk update --quiet ;;
  esac
  pkg_install "${wanted[@]}"
}

# What an existing Docker has to be for the agent: a rootful Docker Engine on DOCKER_SOCKET, which
# the agent container mounts, running containers with the host's uids and without SELinux labels,
# because the slots bind ${DATA_DIR} files owned by uid ${GAME_UID} and share the host network.
# Sets docker_state to ready, missing (install it) or podman (replace the Podman shim).
docker_state=""
# Whether an API version, major.minor, is at least the one the agent names.
docker_api_supported() {
  [[ "$1" =~ ^([0-9]+)\.([0-9]+)$ ]] || return 1
  local major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}"
  [[ "$DOCKER_API_VERSION" =~ ^([0-9]+)\.([0-9]+)$ ]]
  ((major > BASH_REMATCH[1] || (major == BASH_REMATCH[1] && minor >= BASH_REMATCH[2])))
}

check_docker() {
  docker_state=missing
  have docker || return 0

  if [[ "$(command -v docker)" == /snap/* ]]; then
    die "Docker is the snap package, whose confinement cannot bind ${DATA_DIR}: remove it (snap remove docker) and re-run to install Docker Engine"
  fi
  if docker --version 2>/dev/null | grep -qi podman; then
    docker_state=podman
    return 0
  fi

  if ! docker info >/dev/null 2>&1; then
    docker_service_exists || return 0
    log "Docker Engine is installed but not answering on ${DOCKER_SOCKET}; starting it"
    start_docker
    if ((dry_run)); then
      docker_state=ready
      return 0
    fi
    wait_for_daemon || die "Docker Engine did not start; see the output of: $(docker_log_hint)$(lxc_hint)"
  fi

  if docker version --format '{{range .Server.Components}}{{.Name}} {{end}}' 2>/dev/null | grep -qi podman; then
    docker_state=podman
    return 0
  fi

  local security
  security="$(docker info --format '{{range .SecurityOptions}}{{.}} {{end}}' 2>/dev/null)"
  [[ "$security" != *name=rootless* ]] ||
    die "the daemon on ${DOCKER_SOCKET} is rootless Docker; the agent needs the system daemon (systemctl enable --now docker)"
  [[ "$security" != *name=userns* ]] ||
    die "the daemon runs with userns-remap, which maps uid ${GAME_UID} away from the owner of ${DATA_DIR} and refuses host networking; remove \"userns-remap\" from /etc/docker/daemon.json, restart Docker and re-run"
  [[ "$security" != *name=selinux* ]] ||
    die "the daemon runs with SELinux labelling, which denies containers the Docker socket and ${DATA_DIR}; set \"selinux-enabled\": false in /etc/docker/daemon.json (or drop --selinux-enabled from the daemon's options), restart Docker and re-run"

  local api
  api="$(docker version --format '{{.Server.APIVersion}}' 2>/dev/null)"
  docker_api_supported "$api" ||
    die "Docker Engine $(docker version --format '{{.Server.Version}}' 2>/dev/null) supports API ${api:-unknown}; the agent needs Docker Engine ${MIN_DOCKER_ENGINE} or newer (API ${DOCKER_API_VERSION}): upgrade Docker and re-run"

  local limits
  limits="$(docker info --format '{{.MemoryLimit}} {{.CPUShares}}' 2>/dev/null)"
  [[ "$limits" != false* ]] || warn "the daemon reports no memory limit support; a memory_mb cap in agent.yml is not enforced"
  [[ "$limits" != *" false" ]] || warn "the daemon reports no CPU shares support; game servers get no CPU priority over other containers"

  docker_state=ready
}

docker_service_exists() {
  case "$(init_system)" in
    systemd) systemctl cat docker.service >/dev/null 2>&1 ;;
    openrc) [[ -e /etc/init.d/docker ]] ;;
    *) return 1 ;;
  esac
}

docker_log_hint() {
  case "$(init_system)" in
    systemd) printf 'journalctl -u docker.service' ;;
    openrc) printf 'cat /var/log/docker.log' ;;
    *) printf 'dockerd' ;;
  esac
}

# Starts the daemon now and at every boot: the agent and the slots come back through their
# restart policy, which only runs once dockerd does. On systemd that is docker.service itself;
# docker.socket alone would start the daemon on first use, and nothing uses it after a reboot.
start_docker() {
  case "$(init_system)" in
    systemd)
      mask_wait_online_in_container
      run systemctl enable --now docker.service
      ;;
    openrc)
      run rc-update add docker default
      run rc-service docker start
      ;;
    *) warn "no systemd or OpenRC on this host: start dockerd yourself, and make it start at boot" ;;
  esac
}

ensure_docker_at_boot() {
  case "$(init_system)" in
    systemd)
      if ! systemctl cat docker.service >/dev/null 2>&1; then
        warn "no docker.service unit; make sure dockerd starts at boot"
      elif [[ "$(systemctl is-enabled docker.service 2>/dev/null)" == enabled ]]; then
        skip "docker.service starts at boot"
      else
        log "enabling docker.service at boot"
        run systemctl enable docker.service
      fi
      ;;
    openrc)
      if rc-update show default 2>/dev/null | grep -qw docker; then
        skip "docker starts at boot (OpenRC default runlevel)"
      else
        log "adding docker to the OpenRC default runlevel"
        run rc-update add docker default
      fi
      ;;
    *) warn "no systemd or OpenRC on this host: make sure dockerd starts at boot" ;;
  esac
}

wait_for_daemon() {
  ((dry_run)) && return 0
  local deadline=$((SECONDS + DAEMON_TIMEOUT_SECONDS))
  until docker info >/dev/null 2>&1; do
    ((SECONDS < deadline)) || return 1
    sleep 1
  done
}

# Inside an LXC (Proxmox and friends) the host configures the network before PID 1 starts, so
# systemd-networkd sees every link as "unmanaged" and systemd-networkd-wait-online never
# finishes. docker.service is After=network-online.target, and docker-ce's postinst starts it
# with a blocking `systemctl start` — the install hangs on that job forever. Masking the unit is
# correct here rather than a workaround: in a container the network is online by definition.
mask_wait_online_in_container() {
  systemd-detect-virt --container --quiet || return 0
  systemctl cat systemd-networkd-wait-online.service >/dev/null 2>&1 || return 0
  [[ "$(systemctl is-enabled systemd-networkd-wait-online.service 2>/dev/null)" != masked ]] || return 0
  log "container detected ($(systemd-detect-virt -c)); masking systemd-networkd-wait-online so docker.service can start"
  run systemctl mask --now systemd-networkd-wait-online.service
}

# The distribution's own Docker and Podman's docker shim conflict with Docker's packages, or
# would answer on the socket instead of Docker Engine.
remove_conflicting_packages() {
  local -a candidates=() installed=()
  case "$os_family" in
    apt) candidates=(docker.io docker-compose docker-compose-v2 docker-doc docker-buildx podman-docker containerd runc) ;;
    dnf)
      candidates=(docker docker-client docker-client-latest docker-common docker-latest docker-latest-logrotate
        docker-logrotate docker-selinux docker-engine-selinux docker-engine moby-engine podman-docker)
      # Docker's RHEL and CentOS guides remove Podman and runc as well: containerd.io replaces runc.
      [[ "$docker_repo" == fedora ]] || candidates+=(podman runc)
      ;;
    pacman | zypper | apk) candidates=(podman-docker) ;;
  esac

  local candidate
  for candidate in "${candidates[@]}"; do
    pkg_installed "$candidate" && installed+=("$candidate")
  done
  if ((${#installed[@]} > 0)); then
    log "removing packages that conflict with Docker Engine: ${installed[*]}"
    pkg_remove "${installed[@]}"
  fi

  # podman-docker's tmpfiles entry points the Docker socket at Podman until the next boot.
  if [[ -L "$DOCKER_SOCKET" ]]; then
    log "removing ${DOCKER_SOCKET}, a link to $(readlink "$DOCKER_SOCKET")"
    run rm -f "$DOCKER_SOCKET"
  fi
}

# docs.docker.com/engine/install/{debian,ubuntu}
install_docker_apt() {
  [[ -n "$docker_suite" ]] || die "cannot read VERSION_CODENAME from /etc/os-release"
  local arch
  arch="$(dpkg --print-architecture)"
  log "installing Docker Engine from download.docker.com (${docker_repo} ${docker_suite})"
  run install -m 0755 -d /etc/apt/keyrings
  run curl -fsSL "https://download.docker.com/linux/${docker_repo}/gpg" -o /etc/apt/keyrings/docker.asc
  run chmod a+r /etc/apt/keyrings/docker.asc
  run_sh "printf 'Types: deb\nURIs: https://download.docker.com/linux/%s\nSuites: %s\nComponents: stable\nArchitectures: %s\nSigned-By: /etc/apt/keyrings/docker.asc\n' \
    '${docker_repo}' '${docker_suite}' '${arch}' > /etc/apt/sources.list.d/docker.sources"
  run apt-get update -qq ||
    die "apt could not read Docker's ${docker_repo} repository for \"${docker_suite}\"; check https://docs.docker.com/engine/install/${docker_repo}/ for the supported releases"
  pkg_install docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
}

# docs.docker.com/engine/install/{fedora,centos,rhel}. The repo file is fetched directly, which
# works the same under dnf 4 and dnf 5, whose config-manager syntaxes differ.
install_docker_dnf() {
  log "installing Docker Engine from download.docker.com (${docker_repo})"
  run curl -fsSL "https://download.docker.com/linux/${docker_repo}/docker-ce.repo" -o /etc/yum.repos.d/docker-ce.repo
  pkg_install docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin ||
    die "Docker's ${docker_repo} repository has no packages for ${os_name}; check https://docs.docker.com/engine/install/${docker_repo}/ for the supported releases"
}

# wiki.alpinelinux.org/wiki/Docker: the packages live in the community repository.
enable_alpine_community() {
  grep -Eq '^[^#]*/community/?$' /etc/apk/repositories && return 0
  local community
  community="$(sed -n 's#^\([^#].*\)/main/\{0,1\}$#\1/community#p' /etc/apk/repositories | head -n 1)"
  [[ -n "$community" ]] || die "no main repository in /etc/apk/repositories to derive the community one from"
  log "enabling the Alpine community repository (${community})"
  run_sh "printf '%s\n' '${community}' >> /etc/apk/repositories"
  run apk update --quiet
}

install_docker() {
  check_docker
  case "$docker_state" in
    ready)
      skip "Docker Engine $(docker version --format '{{.Server.Version}}' 2>/dev/null) answers on ${DOCKER_SOCKET}"
      ensure_docker_at_boot
      return 0
      ;;
    podman) log "the docker command on this host is Podman's; replacing it with Docker Engine" ;;
  esac

  [[ -n "$os_family" ]] ||
    die "Docker is missing and ${os_name} is not a distribution this script installs it on; install Docker Engine (https://docs.docker.com/engine/install/) and re-run"

  # Before the packages: docker-ce's postinst starts the daemon on Debian and Ubuntu.
  if [[ "$(init_system)" == systemd ]]; then
    mask_wait_online_in_container
  fi
  remove_conflicting_packages

  case "$os_family" in
    apt) install_docker_apt ;;
    dnf) install_docker_dnf ;;
    # wiki.archlinux.org/title/Docker
    pacman)
      log "installing Docker from the Arch repositories"
      pkg_install docker docker-buildx docker-compose
      ;;
    zypper)
      log "installing Docker from the openSUSE repositories"
      pkg_install docker docker-compose
      ;;
    apk)
      enable_alpine_community
      log "installing Docker from the Alpine repositories"
      pkg_install docker docker-cli-compose
      ;;
  esac

  start_docker
  ((dry_run)) && return 0
  wait_for_daemon || die "Docker Engine did not start after the install; see the output of: $(docker_log_hint)$(lxc_hint)"
  check_docker
  [[ "$docker_state" == ready ]] || die "the docker command still does not reach Docker Engine on ${DOCKER_SOCKET}"
  log "Docker Engine $(docker version --format '{{.Server.Version}}') is running"
}

# `docker` must work before anything below it does.
require_docker() {
  have docker && return 0
  ((dry_run)) && return 0
  die "docker is still not on PATH after the install step"
}

create_storage() {
  # The operator copies node plugins and cfgs in as root; the agent creates folders inside them.
  local dirs=("$DATA_DIR" "${DATA_DIR}/slots" "${DATA_DIR}/cfg" "${DATA_DIR}/swiftlys2"
    "${DATA_DIR}/swiftlys2/plugins" "${DATA_DIR}/swiftlys2/configs" "${DATA_DIR}/swiftlys2/configs/plugins")
  log "data directory ${DATA_DIR} (agent.yml, per-slot files, node plugins and cfgs), owned by uid ${GAME_UID}"
  # chown, not `install -o`: uutils install (Ubuntu 26.04) refuses a uid with no passwd entry.
  run install -d -m 0755 "${dirs[@]}"
  run chown "${GAME_UID}:${GAME_GID}" "${dirs[@]}"

  if docker volume inspect "$CS2_VOLUME" >/dev/null 2>&1; then
    skip "Docker volume ${CS2_VOLUME} exists"
  else
    log "creating the shared CS2 install volume ${CS2_VOLUME}"
    run docker volume create "$CS2_VOLUME"
  fi
}

# One setup or update at a time on a host.
lock_setup() {
  ((dry_run)) && return 0
  exec 9>/var/lock/ankor-node-setup.lock
  flock -n 9 || die "another node setup/update is running"
}

# Setup can reuse a loaded image; updates refresh it unless --no-pull was requested.
# The cs2 image belongs to the agent: it pulls it when the first install needs it.
prepare_agent_image() {
  if ((no_pull)); then
    log "using the local agent image ${agent_image} (--no-pull)"
    if ((dry_run)); then
      run docker image inspect "$agent_image"
    elif ! docker image inspect "$agent_image" >/dev/null; then
      die "cannot use ${agent_image}: --no-pull requires an available local image (load it first)"
    fi
    return 0
  fi
  if ((do_update == 0 && dry_run == 0)) && docker image inspect "$agent_image" >/dev/null 2>&1; then
    skip "${agent_image} is on this host; not pulled"
    return 0
  fi
  log "pulling ${agent_image}"
  run docker pull "$agent_image"
}

# The agent's own flags on a throwaway container that prints the agent's version and exits, before
# start_agent removes the running agent. `--cpu-shares` needs the cpu controller delegated to the
# LXC; without it Docker fails here, with a clearer error than the agent's restart loop.
probe_lxc_containers() {
  ((in_lxc)) || return 0
  if ((dry_run)); then
    shown "docker run --pull=never --rm --network host --cpu-shares 1024 ${agent_image} --version"
    return 0
  fi
  local output
  if ! output="$(docker run --pull=never --rm --network host --cpu-shares 1024 "$agent_image" --version 2>&1)"; then
    printf '%s\n' "$output" >&2
    die "a test container could not start in this LXC$(lxc_hint)"
  fi
  log "containers start in this LXC"
}

start_agent() {
  if docker inspect "$AGENT_CONTAINER" >/dev/null 2>&1; then
    log "replacing the existing ${AGENT_CONTAINER} container"
    run docker rm -f "$AGENT_CONTAINER"
  fi

  # The agent this starts is no failed swap's, so the record of one goes; first, as the agent reads it at boot.
  run rm -f "$AGENT_UPDATE_FAILURE"

  log "starting ${AGENT_CONTAINER} from ${agent_image}"
  # `--network host`: the agent dials the controller out of this box and its game containers want the
  # host's network anyway. The data dir is mounted at *the same host path* because the game
  # containers are siblings — dockerd resolves their bind mounts by host path, not ours.
  # agent.yml lives in it, where the agent replaces it whole when the controller edits the node. meminfo, the cgroup tree and os-release are the host's, read-only, for the
  # node's resource ticks and hello. The CS2 volume is mounted read-only at /cs2-install, where the
  # agent reads the installed build. The docker group is the image's business: the agent
  # reads the gid off the mounted socket and drops to its unprivileged user with it,
  # which is one fewer host detail for this script to guess.
  # The label is how the agent finds its own container when it replaces itself.
  # Non-blocking logging: dockerd drops console lines it cannot keep up with, rather than block the
  # agent on a write. The agent gives its replacement and its game containers the same options.
  run docker run --pull=never -d \
    --name "$AGENT_CONTAINER" \
    --label ankor.role=agent \
    --cpu-shares 2598 \
    --restart unless-stopped \
    --log-driver local \
    --log-opt mode=non-blocking \
    --log-opt max-buffer-size=4m \
    --network host \
    -v "${DOCKER_SOCKET}:${DOCKER_SOCKET}" \
    -v /proc/meminfo:/host/proc/meminfo:ro \
    -v /sys/fs/cgroup:/host/sys/fs/cgroup:ro \
    -v /etc/os-release:/host/etc/os-release:ro \
    -v "${DATA_DIR}:${DATA_DIR}" \
    -v "${CS2_VOLUME}:/cs2-install:ro" \
    "$agent_image"
}

# Wait for the agent to say it reached the controller. The agent retries forever on its own, so a
# timeout here is a hint to go read the logs, not a failure of the setup.
wait_for_hello() {
  if ((dry_run)); then
    shown "docker logs -f --tail 50 ${AGENT_CONTAINER}   # until \"connected to\", max ${CONNECT_TIMEOUT_SECONDS}s"
    return 0
  fi

  log "waiting up to ${CONNECT_TIMEOUT_SECONDS}s for the agent to connect"
  local deadline=$((SECONDS + CONNECT_TIMEOUT_SECONDS)) logs
  while ((SECONDS < deadline)); do
    logs="$(docker logs --tail 200 "$AGENT_CONTAINER" 2>&1 || true)"
    if printf '%s' "$logs" | grep -q 'connected to '; then
      printf '%s\n' "$logs" | tail -n 20
      log "agent connected."
      return 0
    fi
    if printf '%s' "$logs" | grep -qE 'unauthorized|401|invalid agent\.yml|invalid /'; then
      printf '%s\n' "$logs" | tail -n 20
      die "the agent rejected its configuration or its token; check ${AGENT_CONFIG}; a node deleted in the controller has to be added again"
    fi
    sleep 2
  done

  docker logs --tail 30 "$AGENT_CONTAINER" 2>&1 || true
  warn "the agent has not reported a connection yet. It keeps retrying;"
  warn "watch it with: docker logs -f ${AGENT_CONTAINER}"
}

summary() {
  cat <<EOF

  The node is set up. Before it can host games, you must configure server slots for it.

  Next:
    follow the agent:        docker logs -f ${AGENT_CONTAINER}
    update the agent image:  run this script again with --update
    install/update CS2:      Install CS2 (later Update CS2) for this node in the controller
    re-run this setup:       safe, and asks for nothing — agent.yml stays as it is

  You must ensure connectivity from the clients to the assigned ports on this host yourself.

EOF
}

# ---------------------------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------------------------

if ((do_update)); then
  log "update mode"
  require_root
  require_docker
  if [[ -e "$AGENT_CONFIG" ]]; then
    read_agent_config "$AGENT_CONFIG"
  elif ((dry_run)); then
    warn "no ${AGENT_CONFIG} on this host; a real run needs one (set the node up first)"
    agent_image="<from agent.yml>"
    cs2_image="<from agent.yml>"
  else
    die "no ${AGENT_CONFIG} on this host: run the setup form of this script first"
  fi
  # Checked before the pull, so a host that was never set up is left exactly as it was.
  docker inspect "$AGENT_CONTAINER" >/dev/null 2>&1 || ((dry_run)) ||
    die "no ${AGENT_CONTAINER} container on this host: run the setup form of this script first"
  lock_setup
  prepare_agent_image
  log "restarting ${AGENT_CONTAINER} onto ${agent_image}"
  # Re-created from the agent.yml already on disk, so an update needs no arguments at all.
  start_agent
  log "update done."
  exit 0
fi

# The token is asked for before root, so the elevated run gets it over the relay; nothing else runs
# unprivileged, because the elevated run would do it again. agent.yml is fetched once the host
# passed its checks and before Docker is installed, so a wrong token stops the run before it
# changes much.
if [[ -e "$AGENT_CONFIG" ]]; then
  [[ -z "$node_token" ]] || warn "${AGENT_CONFIG} exists and is kept; ANKOR_NODE_TOKEN is not used"
  node_token=""
else
  [[ -n "$node_token" ]] || ((dry_run)) || ask_token
  [[ -z "$node_token" ]] || valid_token "$node_token" ||
    die "that is not a node token (32 to 128 letters and digits); copy it again from the controller"
fi
require_root
require_x86_64
detect_os
require_init_system
require_lxc_nesting
install_prerequisites
lock_setup
load_or_fetch_agent_config
install_docker
require_docker
create_storage
prepare_agent_image
probe_lxc_containers
start_agent
wait_for_hello
summary
