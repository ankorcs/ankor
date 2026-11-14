#!/usr/bin/env bash
#
# CS2 game-server entrypoint. Two modes:
#
#   CS2_UPDATE_ONLY=1   one-shot install/update of the GAME. Needs the install volume mounted
#                       READ-WRITE. Runs steamcmd, patches gameinfo.gi, records the build in
#                       .ankor/installed.json, creates the empty mountpoints, exits 0.
#
#   (default)           run mode. The install volume is mounted READ-ONLY and shared by every
#                       container on the host, so this writes NOTHING to it: no steamcmd, no
#                       gameinfo.gi patch. It verifies the install and then `exec`s the server, so
#                       the game is PID 1 and docker's signals reach it directly.
#
# The addon overlay — SwiftlyS2, the Ankor plugin, the Ankor cfgs — stays out of the
# volume in both modes. It lives in this image at /opt/ankor/overlay, the agent extracts it once
# per image id, and it is bind-mounted READ-ONLY over game/csgo/addons/ and
# game/csgo/cfg/ankor/ at run time. So the volume holds only the game plus our two small records,
# a game update can never touch our code, and a compromised server has nothing of ours in reach.
#
# packages/plugin/docker/README.md explains the image. The env vars and their defaults are below;
# docs/configuration.md has the mount layout.

set -euo pipefail

log() { printf '[entrypoint] %s\n' "$*"; }
die() { printf '[entrypoint] ERROR: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------------------------

readonly STEAMAPPID=730
readonly CS2_INSTALL_DIR=/home/steam/cs2-dedicated
readonly STEAMCMD_DIR=/home/steam/steamcmd

: "${CS2_UPDATE_ONLY:=0}"
: "${CS2_VALIDATE:=1}"
# Empties the install directory before steamcmd runs, so a download that cannot be resumed past a
# stuck manifest state starts from nothing.
: "${CS2_REINSTALL:=0}"
# Whole gigabytes required free before the first steamcmd attempt.
: "${CS2_MIN_FREE_GB:=20}"

: "${CS2_PORT:=27015}"
: "${CS2_TV_PORT:=27020}"
# GOTV starts with the map, so it is set on the command line. Off unless the launch runs it.
: "${CS2_TV_ENABLE:=0}"
: "${CS2_MAXPLAYERS:=12}"
# Competitive unless the launch names another mode.
: "${CS2_GAME_TYPE:=0}"
: "${CS2_GAME_MODE:=1}"
: "${CS2_STARTMAP:=de_mirage}"
# Workshop map id, empty for a stock map. Never passed as CS2_STARTMAP: `+map <numeric id>` is
# answered with "invalid map name" and the engine never starts hosting — and `host_workshop_map`
# needs Steam, which the server only logs on to once it *is* hosting (tried: `+host_workshop_map`
# alone leaves it in "Idle (console)" forever). So CS2_STARTMAP boots a stock map (the agent sends
# de_dust2) and this id is appended after it: dust2 loads from disk, the Steam logon completes,
# then the workshop map downloads and the server changes level onto it. CS2_AUTHKEY (a Steam Web
# API key, `-authkey`) is what lets the download happen at all.
: "${CS2_WORKSHOP_MAP:=}"
: "${CS2_AUTHKEY:=}"
: "${CS2_PASSWORD:=}"
: "${CS2_EXTRA_ARGS:=}"
: "${SRCDS_TOKEN:=}"

GAME_DIR="${CS2_INSTALL_DIR}/game"
CSGO_DIR="${GAME_DIR}/csgo"
GAMEINFO="${CSGO_DIR}/gameinfo.gi"
SERVER_BIN="${GAME_DIR}/bin/linuxsteamrt64/cs2"
APP_MANIFEST="${CS2_INSTALL_DIR}/steamapps/appmanifest_${STEAMAPPID}.acf"
# Our own metadata about the install, read by the agent. Never touched by Steam.
ANKOR_INSTALL_STATE_DIR="${CS2_INSTALL_DIR}/.ankor"
INSTALLED_JSON="${ANKOR_INSTALL_STATE_DIR}/installed.json"
# The gameinfo.gi search path that loads SwiftlyS2 (see patch_gameinfo). A real tab between the
# key and the value, matching how Valve writes the neighbouring lines.
SWIFTLY_LINE=$'Game\tcsgo/addons/swiftlys2'
SWIFTLY_LINE_PATTERN='^[[:space:]]*Game[[:space:]]+csgo/addons/swiftlys2[[:space:]]*$'

# ---------------------------------------------------------------------------------------------
# Steam client library
#
# The game server reaches Steam through steamclient.so, which only steamcmd's self-update
# produces. Update mode runs steamcmd anyway and keeps a copy in the install volume; run mode only
# links it where the engine looks, so starting a server never runs steamcmd.
# ---------------------------------------------------------------------------------------------

STEAMCLIENT_DIR="${ANKOR_INSTALL_STATE_DIR}/steamclient"

store_steamclient() {
  local bits
  for bits in 64 32; do
    [ -f "${STEAMCMD_DIR}/linux${bits}/steamclient.so" ] || continue
    mkdir -p "${STEAMCLIENT_DIR}/linux${bits}"
    cp -f "${STEAMCMD_DIR}/linux${bits}/steamclient.so" "${STEAMCLIENT_DIR}/linux${bits}/steamclient.so"
  done
  [ -f "${STEAMCLIENT_DIR}/linux64/steamclient.so" ] || die "steamcmd left no linux64/steamclient.so"
  log "steamclient.so stored in the install"
}

link_steamclient() {
  [ -f "${STEAMCLIENT_DIR}/linux64/steamclient.so" ] \
    || die "the install has no steamclient.so; run a CS2 update (CS2_UPDATE_ONLY=1) to add it"
  local bits
  for bits in 64 32; do
    [ -f "${STEAMCLIENT_DIR}/linux${bits}/steamclient.so" ] || continue
    mkdir -p "${HOME}/.steam/sdk${bits}"
    ln -sfT "${STEAMCLIENT_DIR}/linux${bits}/steamclient.so" "${HOME}/.steam/sdk${bits}/steamclient.so"
    ln -sfT "${STEAMCLIENT_DIR}/linux${bits}/steamclient.so" "${HOME}/.steam/sdk${bits}/steamservice.so"
  done
}

# ---------------------------------------------------------------------------------------------
# Install / update (update-only mode)
# ---------------------------------------------------------------------------------------------

# Whole gigabytes available on the filesystem holding the install directory, or empty when df
# cannot answer. `|| true` because `set -o pipefail` would otherwise make an unanswerable df fatal.
free_gb() {
  df -kP "$CS2_INSTALL_DIR" 2>/dev/null | awk 'NR==2 {printf "%d", int($4 / 1048576)}' || true
}

# Anonymous logins get a manifest request code only for a depot's current manifest, and a delta
# update needs the installed one too. steamcmd keeps its copy under HOME, gone with the container,
# so an install one build behind fails with state 0x6 unless the manifests live in the volume.
persist_depotcache() {
  local cache="${ANKOR_INSTALL_STATE_DIR}/depotcache" dir
  mkdir -p "$cache"
  for dir in "${HOME}/Steam/depotcache" "${STEAMCMD_DIR}/depotcache"; do
    if [ -d "$dir" ] && [ ! -L "$dir" ]; then
      cp -rn "$dir"/. "$cache"/ || die "could not move ${dir} into ${cache}"
      rm -rf "$dir"
    fi
    mkdir -p "$(dirname "$dir")"
    ln -sfn "$cache" "$dir"
  done
  log "depot manifests are cached in ${cache}"
}

install_with_steamcmd() {
  local validate=""
  # steamcmd writes every file itself in reinstall mode, so re-hashing them buys nothing.
  [ "$CS2_VALIDATE" = "1" ] && [ "$CS2_REINSTALL" != "1" ] && validate="validate"

  log "installing app ${STEAMAPPID} with steamcmd into ${CS2_INSTALL_DIR}"

  if [ "$CS2_REINSTALL" = "1" ]; then
    log "CS2_REINSTALL=1: emptying ${CS2_INSTALL_DIR} and downloading the whole game again"
    # The contents, never the directory: it is the volume's mountpoint.
    find "$CS2_INSTALL_DIR" -mindepth 1 -delete
  fi

  # After the wipe: a reinstall starts with no manifests and caches the ones it downloads.
  persist_depotcache

  # A 67 GB install that runs out of room fails with the same opaque state codes as everything
  # else, so the free space is on the record before the first attempt.
  log "install volume space: $(df -h "$CS2_INSTALL_DIR" 2>/dev/null | tail -n1 || echo 'unknown')"

  local free
  free="$(free_gb)"

  if [ -n "$free" ] && [ "$free" -lt "$CS2_MIN_FREE_GB" ]; then
    if [ "$CS2_REINSTALL" = "1" ]; then
      die "a reinstall needs at least ${CS2_MIN_FREE_GB} GB free; free space is ${free} GB after the wipe. Free space on the filesystem holding ${CS2_INSTALL_DIR} and re-run."
    fi

    die "an update needs at least ${CS2_MIN_FREE_GB} GB free to stage changed files; free space is ${free} GB. Free space on the filesystem holding ${CS2_INSTALL_DIR} and re-run, or reinstall from /nodes to start the install over."
  fi

  # Retried IN PLACE, and nothing is ever deleted between attempts. steamcmd resumes from its own
  # `steamapps/` manifests, so a retry costs the missing files and nothing else — whereas removing
  # `steamapps/` makes the next attempt see a 67 GB install as absent, delete the game and download
  # it all again. (An earlier version of this script did that; two update containers collided on our
  # node, attempt 1 reported state 0x6 and attempt 2 state 0x202, and the retry wiped the install.)
  local attempt status
  for attempt in 1 2 3; do
    status=0
    "${STEAMCMD_DIR}/steamcmd.sh" \
        +force_install_dir "${CS2_INSTALL_DIR}" \
        +@bClientTryRequestManifestWithoutCode 1 \
        +login anonymous \
        +app_update "${STEAMAPPID}" ${validate} \
        +quit \
      || status=$?

    [ "$status" -eq 0 ] && return 0

    log "steamcmd attempt ${attempt} failed (exit ${status})"

    # Backing off matters for the one failure that retrying can actually fix: another steamcmd
    # holding the same install directory. 15 s, then 45 s.
    case "$attempt" in
      1) log "retrying in 15s"; sleep 15 ;;
      2) log "retrying in 45s"; sleep 45 ;;
    esac
  done

  die "steamcmd failed after 3 attempts (last exit ${status}); nothing was deleted, so the install in ${CS2_INSTALL_DIR} is whatever the last attempt left. Steam's own state codes (0x202, 0x6) mean another steamcmd is using this directory — only ONE update container per host volume, ever — or the filesystem is full (0x202 wants the whole staged size free), or Steam no longer serves the installed manifests to anonymous logins (an install more than one build behind with an empty ${ANKOR_INSTALL_STATE_DIR}/depotcache; the content log says 'Failed to get manifest request code'); check the space line above and re-run, or reinstall from /nodes to start the install over."
}

# ---------------------------------------------------------------------------------------------
# Recording which build is installed  ->  ${CS2_INSTALL_DIR}/.ankor/installed.json
#
# The agent reports the installed build to the controller, and the controller compares it against
# the current public build to decide whether the host needs updating. steamcmd's own
# `steamapps/appmanifest_730.acf` already carries the number, so this file is not the only
# source — it is ours: written atomically by us at a moment we know the install finished,
# carrying `installed_at` as well, and out of reach of anything Steam rewrites. The agent prefers
# it and falls back to the app manifest.
# ---------------------------------------------------------------------------------------------

# The app manifest is a VDF: `"buildid"<tab>"20037731"`.
buildid_from_appmanifest() {
  [ -f "$APP_MANIFEST" ] || return 1
  local id
  id="$(sed -n 's/^[[:space:]]*"buildid"[[:space:]]*"\([0-9][0-9]*\)".*/\1/p' "$APP_MANIFEST" | head -n1)"
  [ -n "$id" ] || return 1
  printf '%s' "$id"
}

# Atomic, and owned by the container's uid (1000) because that is who runs this script.
write_installed_json() {
  local buildid="$1" tmp
  mkdir -p "$ANKOR_INSTALL_STATE_DIR"
  tmp="${INSTALLED_JSON}.tmp"

  {
    printf '{\n'
    if [ -n "$buildid" ]; then
      printf '  "buildid": "%s",\n' "$buildid"
    fi
    printf '  "installed_at": "%s"\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '}\n'
  } > "$tmp"

  chmod 644 "$tmp"
  mv -f "$tmp" "$INSTALLED_JSON"
  log "recorded install: $(tr -d '\n' < "$INSTALLED_JSON")"
}

record_installed_build() {
  # steamcmd has just rewritten the app manifest, so this is the authoritative number. Missing it
  # is recorded as "installed, build unknown" rather than failing the update: the agent then
  # reports no build and the controller asks for another update, which is recoverable.
  local buildid
  buildid="$(buildid_from_appmanifest || true)"
  [ -n "$buildid" ] \
    || log "WARNING: no buildid in ${APP_MANIFEST}; recording the install without one"

  write_installed_json "$buildid"
}

# ---------------------------------------------------------------------------------------------
# Mountpoints
#
# Every writable path a game container needs is a bind mount from outside the read-only install
# volume, and Docker refuses to create a mountpoint on a read-only mount ("create mountpoint …:
# read-only file system"). So update mode leaves an EMPTY directory (or an empty file) at each of
# the mountpoints that live in the volume, while the volume is still writable.
#
# Two of the mountpoints are directories the agent mounts the addon overlay OVER — addons/ and
# cfg/ankor/ — and they stay empty forever: what a server loads from them comes from the image,
# not from here.
#
# The mountpoints NOT created here are the ones nested inside those two overlay mounts
# (addons/swiftlys2/logs/, addons/swiftlys2/dumps/, addons/swiftlys2/data/,
# addons/swiftlys2/configs/plugins/Ankor/config.jsonc). At run time the overlay is mounted over their parent, so the volume's
# copy is invisible and only the overlay's own copy can serve as a mountpoint; those ship in the
# image's overlay and the agent creates any that are missing. See packages/plugin/docker/README.md,
# "Run mode".
# ---------------------------------------------------------------------------------------------

# Directories, relative to game/csgo/.
CSGO_MOUNTPOINT_DIRS=(
  # The addon overlay's two mount points. Empty by design.
  "addons"
  "cfg/ankor"
  # The node's own cfgs, read-only in every game: `exec node/<name>`. New files need no mountpoint.
  "cfg/node"
  # The slot's own directory (match config, demos, round backups), shadowed per slot. The agent
  # lays out what is inside it, so this is the only mountpoint those need.
  "ankor"
)

create_mountpoints() {
  log "creating the mountpoints the agent binds over"

  local rel
  for rel in "${CSGO_MOUNTPOINT_DIRS[@]}"; do
    mkdir -p "${CSGO_DIR}/${rel}"
  done

  # `cfg/ankor_launch.cfg` is a FILE bind (the per-launch cfg) and sits directly in the game's own
  # cfg/, not under the overlay, so its placeholder belongs here. The plugin execs it on every map
  # load; a stub comment rather than an empty file keeps that a no-op if it is ever unmounted.
  mkdir -p "${CSGO_DIR}/cfg"
  [ -f "${CSGO_DIR}/cfg/ankor_launch.cfg" ] \
    || printf '// replaced per slot by the agent\n' > "${CSGO_DIR}/cfg/ankor_launch.cfg"

  # Workshop maps: the engine resolves steamapps/ against its binary directory and downloads
  # into game/bin/linuxsteamrt64/steamapps/workshop/content/730/<id>/. The agent mounts a
  # node-wide writable cache over that steamapps/, so the mountpoint has to exist here.
  mkdir -p "${CS2_INSTALL_DIR}/game/bin/linuxsteamrt64/steamapps"

  log "mountpoints ready: ${CSGO_MOUNTPOINT_DIRS[*]} cfg/ankor_launch.cfg game/bin/linuxsteamrt64/steamapps"
}

# ---------------------------------------------------------------------------------------------
# gameinfo.gi
#
# SwiftlyS2 is loaded by adding ONE search path: it ships a `libserver.so` shim under
# addons/swiftlys2/bin/linuxsteamrt64/, and a search path listed before `Game csgo` makes the engine
# load that instead of the game's own — the shim then loads both the real libserver.so and
# swiftlys2.so. That is the entire install mechanism.
#
# Valve rewrites gameinfo.gi on every game update, so this runs after every install.
# ---------------------------------------------------------------------------------------------

gameinfo_is_patched() {
  [ -f "$GAMEINFO" ] && grep -Eq "$SWIFTLY_LINE_PATTERN" "$GAMEINFO"
}

patch_gameinfo() {
  [ -f "$GAMEINFO" ] || die "${GAMEINFO} not found"

  local tmp

  if gameinfo_is_patched; then
    log "gameinfo.gi already carries the SwiftlyS2 search path"
    return 0
  fi

  log "patching ${GAMEINFO}"

  # The line goes inside SearchPaths and BEFORE the `Game csgo` entry, which is what lets
  # addons/swiftlys2/bin/linuxsteamrt64/libserver.so shadow the game's own. Anchor on
  # Game_LowViolence when it is there (what every working CS2 setup does — it sits directly above
  # `Game csgo`), and fall back to the line after `SearchPaths {`.
  tmp="$(mktemp)"

  if grep -Eq '^[[:space:]]*Game_LowViolence[[:space:]]' "$GAMEINFO"; then
    gawk -v line="$SWIFTLY_LINE" '
      { print }
      /^[[:space:]]*Game_LowViolence[[:space:]]/ && !done { print "\t\t\t" line; done = 1 }
    ' "$GAMEINFO" > "$tmp"
  else
    gawk -v line="$SWIFTLY_LINE" '
      { print }
      /^[[:space:]]*SearchPaths[[:space:]]*$/ { pending = 1 }
      pending && /^[[:space:]]*\{[[:space:]]*$/ && !done { print "\t\t\t" line; done = 1; pending = 0 }
    ' "$GAMEINFO" > "$tmp"
  fi

  if ! grep -Eq "$SWIFTLY_LINE_PATTERN" "$tmp"; then
    rm -f "$tmp"
    die "could not find an insertion point in ${GAMEINFO}; patch it by hand and re-run"
  fi

  cat "$tmp" > "$GAMEINFO"
  rm -f "$tmp"
  log "gameinfo.gi patched"
}

# ---------------------------------------------------------------------------------------------
# Update-only mode
# ---------------------------------------------------------------------------------------------

if [ "$CS2_UPDATE_ONLY" = "1" ]; then
  log "update-only mode"

  mkdir -p "$CS2_INSTALL_DIR"
  [ -w "$CS2_INSTALL_DIR" ] || die "${CS2_INSTALL_DIR} is not writable; mount the install volume read-write for CS2_UPDATE_ONLY=1"

  # Created while the volume is writable so run mode only ever reads it.
  mkdir -p "$ANKOR_INSTALL_STATE_DIR"

  install_with_steamcmd
  store_steamclient
  record_installed_build

  create_mountpoints
  patch_gameinfo

  log "update complete"
  exit 0
fi

# ---------------------------------------------------------------------------------------------
# Run mode — read-only install volume, no writes to it
# ---------------------------------------------------------------------------------------------

log "run mode"
link_steamclient

[ -e "$SERVER_BIN" ] \
  || die "${SERVER_BIN} not found. Run this image once with CS2_UPDATE_ONLY=1 and the install volume mounted read-write."
[ -x "$SERVER_BIN" ] \
  || die "${SERVER_BIN} exists but is not executable (mode $(stat -c '%a' "$SERVER_BIN" 2>/dev/null || echo '?')). steamcmd sets the modes itself, so this is a volume something else has written; re-run with CS2_UPDATE_ONLY=1 and the install volume mounted read-write."

# All three of these come from the addon overlay, which is mounted read-only over
# game/csgo/addons/ and game/csgo/cfg/ankor/ — the volume's own copies of those directories are
# empty by design. So a failure here means the OVERLAY MOUNT is missing, not that the volume needs
# updating. The agent mounts <data_dir>/overlay/<image id>/{addons,cfg/ankor}; for a hand-run
# see packages/plugin/docker/README.md, "Run mode".
OVERLAY_HINT="the addon overlay must be bind-mounted read-only at ${CSGO_DIR}/addons (the agent does this per launch; for a hand-run see packages/plugin/docker/README.md, \"Run mode\")"

[ -f "${CSGO_DIR}/addons/swiftlys2/bin/linuxsteamrt64/swiftlys2.so" ] \
  || die "SwiftlyS2 is not there: ${OVERLAY_HINT}."

[ -f "${CSGO_DIR}/addons/swiftlys2/plugins/Ankor/Ankor.dll" ] \
  || die "The Ankor plugin is not there. It ships in the same overlay: ${OVERLAY_HINT}."

[ -f "${CSGO_DIR}/cfg/ankor/base.cfg" ] \
  || die "The Ankor cfgs are not there. They ship in the addon overlay, which must be bind-mounted read-only at ${CSGO_DIR}/cfg/ankor."

gameinfo_is_patched \
  || die "${GAMEINFO} is missing the 'Game csgo/addons/swiftlys2' search path, so SwiftlyS2 will not load and the server would boot as a plain CS2 server. Re-run with CS2_UPDATE_ONLY=1 (a game update rewrites this file)."

# SwiftlyS2 creates logs/, dumps/ and data/<plugin id>/ inside its own addon directory, which lives
# in the read-only overlay, so each one needs a per-slot read-write bind. Warned about rather than
# fatal: a missing shadow costs log lines and crash dumps, not the match.
for swiftly_writable in logs dumps data; do
  if [ ! -w "${CSGO_DIR}/addons/swiftlys2/${swiftly_writable}" ]; then
    log "WARNING: ${CSGO_DIR}/addons/swiftlys2/${swiftly_writable} is not writable; SwiftlyS2 writes"
    log "WARNING: there at boot. Mount a per-slot read-write directory over it (see README, \"Run mode\")."
  fi
done

# The slot's directory is in the read-only volume until the agent mounts it. Warned about rather
# than fatal: a warmup server needs nothing in it.
if [ ! -w "${CSGO_DIR}/ankor" ]; then
  log "WARNING: ${CSGO_DIR}/ankor is not writable; a match server has no match, demos or backups."
  log "WARNING: Mount a per-slot read-write directory over it (see README, \"Run mode\"). A volume"
  log "WARNING: from before it existed has no mountpoint there: re-run with CS2_UPDATE_ONLY=1."
fi

[ -n "$SRCDS_TOKEN" ] || die "SRCDS_TOKEN is empty: every server logs in to Steam with a Game Server Login Token."
STEAM_ACCOUNT_ARGS=(+sv_setsteamaccount "$SRCDS_TOKEN")

# Arrays rather than word-split strings, so an id or key with anything odd in it stays one
# argument, and so the ordering of the map commands is explicit and cannot be shuffled.
MAP_ARGS=(+map "$CS2_STARTMAP")
MAP_LABEL="$CS2_STARTMAP"
if [ -n "$CS2_WORKSHOP_MAP" ]; then
  MAP_ARGS+=(+host_workshop_map "$CS2_WORKSHOP_MAP")
  MAP_LABEL="${CS2_STARTMAP} → workshop ${CS2_WORKSHOP_MAP}${CS2_AUTHKEY:+ (authkey set)}"
  [ -n "$CS2_AUTHKEY" ] || log "WARNING: CS2_AUTHKEY is empty; the workshop download will likely fail"
fi
AUTHKEY_ARGS=()
[ -n "$CS2_AUTHKEY" ] && AUTHKEY_ARGS=(-authkey "$CS2_AUTHKEY")

PASSWORD_ARGS=()
[ -n "$CS2_PASSWORD" ] && PASSWORD_ARGS=(+sv_password "$CS2_PASSWORD")

# Ahead of the map on the command line: GOTV has to be on before the map spawns. The rest of
# GOTV comes from ankor_launch.cfg, which the plugin runs at map load.
TV_ARGS=(+tv_enable "$CS2_TV_ENABLE" +tv_port "$CS2_TV_PORT")

# ---------------------------------------------------------------------------------------------
# CPU weight
#
# The game's scheduling advantage is its cgroup's weight, set by the daemon (the agent passes
# `HostConfig.CpuShares`, which runc converts to `cpu.weight` on cgroup v2) — so there is nothing
# for this script to apply, only something to report. With cgroup v2 and docker's default private
# cgroup namespace, the container's own cgroup is the root of what it can see, so its weight reads
# straight out of /sys/fs/cgroup/cpu.weight.
#
# Why weight and not `nice`, which this replaced: every container is its own cgroup, the kernel
# schedules BETWEEN cgroups by weight, and a nice value only ranks tasks within one cgroup — where
# the game is the only task. (The engine also resets its own threads to nice 0 during init.)
#
# Never fatal, never a warning: an unreadable file means cgroup v1 or a shared cgroup namespace,
# and the weight the daemon asked for still applies either way.
# ---------------------------------------------------------------------------------------------
if [ -r /sys/fs/cgroup/cpu.weight ]; then
  log "cpu weight $(cat /sys/fs/cgroup/cpu.weight 2>/dev/null || echo unreadable)"
else
  log "cpu weight: not visible (cgroup v1 or shared cgroup namespace)"
fi

log "starting cs2 on port ${CS2_PORT} (gotv ${CS2_TV_PORT}, map ${MAP_LABEL})"
log "ankor: ws=${ANKOR_WS_URL:+set} allocation=${ANKOR_ALLOCATION_ID:-none} match=$([ -s "${CSGO_DIR}/ankor/match.json" ] && echo set || echo none)"

# What Valve's cs2.sh sets up for a Linux dedicated server. The wrapper itself runs the game as a
# child and ignores SIGTERM, so `docker stop` could only end the container by SIGKILL.
cd "$GAME_DIR"
export LD_LIBRARY_PATH="${GAME_DIR}/bin/linuxsteamrt64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export ENABLE_PATHMATCH=1
# Raising the hard limit needs a capability the game container lacks; a lower one is no reason to stop.
ulimit -n 65535 2>/dev/null || log "WARNING: could not raise the open-file limit to 65535 (now $(ulimit -n))"
ulimit -Ss 2048

EXTRA_ARGS=()
read -r -a EXTRA_ARGS <<< "$CS2_EXTRA_ARGS"

# The game is PID 1: docker's SIGTERM shuts it down as `quit` does, and its exit status is the container's.
# Line-buffered, or its output reaches the agent's log forwarding only when the buffer fills.
# Since CS2's 14 October 2026 update, -allow_round_backup_files is what lets the engine write the
# round backups mp_backup_round_auto asks for. Hibernation is off from the start: an empty server
# simulates no frames, so the plugin's timers, which would run base.cfg, would never fire.
exec stdbuf -oL -eL "$SERVER_BIN" \
  -dedicated \
  -console \
  -port "$CS2_PORT" \
  -maxplayers "$CS2_MAXPLAYERS" \
  -allow_round_backup_files \
  "${AUTHKEY_ARGS[@]}" \
  +game_type "$CS2_GAME_TYPE" \
  +game_mode "$CS2_GAME_MODE" \
  +sv_hibernate_when_empty 0 \
  "${TV_ARGS[@]}" \
  "${MAP_ARGS[@]}" \
  "${STEAM_ACCOUNT_ARGS[@]}" \
  "${PASSWORD_ARGS[@]}" \
  "${EXTRA_ARGS[@]}"
