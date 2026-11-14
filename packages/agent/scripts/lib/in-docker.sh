# shellcheck shell=bash
# Sourced by every script in scripts/. `in_docker "$@"` runs the calling script in the checks'
# container (docker/ci.Dockerfile), with this repository at /src and the same arguments, and exits
# with its status. Inside the container it returns, and the script carries on. A script therefore
# runs the same way on a workstation and in CI, and a contributor needs only Docker.
#
# Before the call, a script may set:
#   IN_DOCKER_SOCKET=1                 hand the container the host's Docker socket
#   IN_DOCKER_OPTIONS=(option ...)     more `docker run` options, such as a folder to mount
#
# ANKOR_* and FUZZ_* variables reach the script.
#
# The build output, the cargo registry and the npm cache live in Docker volumes, apart from a host
# build. CI points CARGO_TARGET_HOST and CARGO_REGISTRY_HOST at folders it caches between runs, and
# sets CI_IMAGE_BUILT once it has built the image with its layer cache.
# The script runs as the caller's user, not root: root ignores file permissions, so tests that
# check a refused write would pass without checking it, and the agent drops root on a node too.
# Cargo runs 4 jobs, as many as GitHub's runner has cores: more parallel links of the large test
# binaries outgrow the memory of Docker Desktop's VM. CARGO_BUILD_JOBS overrides it.

in_docker() {
  if [[ -n "${ANKOR_CHECK_CONTAINER:-}" ]]; then
    return 0
  fi

  command -v docker >/dev/null || { echo "$(basename "$0") runs in Docker; install Docker first" >&2; exit 1; }

  local image=ankorcs-rst-ci
  local owner target registry npm dir gid
  owner="$(id -u):$(id -g)"
  target="${CARGO_TARGET_HOST:-ankorcs-rst-target}"
  registry="${CARGO_REGISTRY_HOST:-ankorcs-rst-registry}"
  npm=ankorcs-rst-npm

  if [[ -z "${CI_IMAGE_BUILT:-}" ]]; then
    docker build -q -f docker/ci.Dockerfile -t "$image" . >/dev/null
  fi

  # A host folder is created here, as the caller, before Docker would create it as root.
  for dir in "$target" "$registry"; do
    if [[ "$dir" == */* ]]; then
      mkdir -p "$dir"
    fi
  done

  # A new volume, or one an earlier run filled as root, belongs to root: it is handed to the caller
  # once, when its top folder has another owner.
  docker run --rm --user 0 -v "$target":/t -v "$registry":/r -v "$npm":/n "$image" \
    sh -c "for dir in /t /r /n; do [ \"\$(stat -c %u:%g \$dir)\" = $owner ] || chown -R $owner \$dir; done"

  local options=()

  if [[ "${IN_DOCKER_SOCKET:-0}" == 1 ]]; then
    # The caller joins the group that owns the socket inside the container.
    gid="$(docker run --rm -v /var/run/docker.sock:/var/run/docker.sock "$image" stat -c %g /var/run/docker.sock)"
    options+=(-v /var/run/docker.sock:/var/run/docker.sock --group-add "$gid")
  fi

  options+=(${IN_DOCKER_OPTIONS[@]+"${IN_DOCKER_OPTIONS[@]}"})

  # Opt-in tests and the fuzzer read ANKOR_* and FUZZ_* settings.
  local name
  while IFS= read -r name; do
    [[ "$name" == ANKOR_CHECK_CONTAINER ]] || options+=(-e "$name")
  done < <(compgen -e | grep -E '^(ANKOR|FUZZ)_' || true)

  # A terminal keeps cargo's colours and lets Ctrl-C reach the script.
  if [[ -t 0 && -t 1 ]]; then
    options+=(-it)
  fi

  local status=0
  docker run --rm --init \
    --user "$owner" \
    -e HOME=/tmp \
    -v "$PWD":/src \
    -v "$target":/src/target \
    -v "$registry":/usr/local/cargo/registry \
    -v "$npm":/tmp/.npm \
    ${options[@]+"${options[@]}"} \
    -e CARGO_TERM_COLOR \
    -e CARGO_INCREMENTAL \
    -e CARGO_PROFILE_DEV_DEBUG \
    -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
    "$image" "scripts/$(basename "$0")" "$@" || status=$?
  exit "$status"
}
