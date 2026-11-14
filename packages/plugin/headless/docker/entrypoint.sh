#!/bin/sh
# The headless image's entrypoint, in the two modes the agent starts a cs2 image in.
#
#   CS2_UPDATE_ONLY=1   stands in for the image's update: prints the lines the real entrypoint and
#                       steamcmd print, then waits for an instruction file to say how it ends. With
#                       {"op":"install","build":"<id>"} it records that build in the install as the
#                       real entrypoint does; with {"op":"fail","code":<n>} it fails as steamcmd does.
#   otherwise           runs the plugin on the test engine (ankor-headless).
set -eu

readonly INSTALL_DIR=/home/steam/cs2-dedicated
readonly INSTRUCTION="${ANKOR_HEADLESS_UPDATE_INSTRUCTION:-/tmp/ankor-headless-update.json}"

say() { printf '%s\n' "$*"; }

if [ "${CS2_UPDATE_ONLY:-0}" != "1" ]; then
  exec /opt/ankor/headless/ankor-headless
fi

say "[entrypoint] update-only mode"
say "[entrypoint] installing app 730 with steamcmd into ${INSTALL_DIR}"
say "[entrypoint] depot manifests are cached in ${INSTALL_DIR}/.ankor/depotcache"
say "[entrypoint] install volume space: /dev/vda1       911G  121G  745G  14% ${INSTALL_DIR}"

# As bash would, killed by the signal the agent's stop sends.
trap 'exit 143' TERM
while [ ! -s "$INSTRUCTION" ]; do sleep 0.1; done
instruction="$(cat "$INSTRUCTION")"

for line in \
  "Redirecting stderr to '/home/steam/Steam/logs/stderr.txt'" \
  "Logging directory: '/home/steam/Steam/logs'" \
  "[  0%] Checking for available updates..." \
  "[----] Verifying installation..." \
  "Steam Console Client (c) Valve Corporation - version 1792617903" \
  "-- type 'quit' to exit --" \
  "Loading Steam API...OK" \
  "Connecting anonymously to Steam Public...OK" \
  "Waiting for client config...OK" \
  "Waiting for user info...OK"; do
  say "$line"
done

field() { printf '%s' "$instruction" | sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p"; }

if [ "$(field op)" != "install" ]; then
  # steamcmd's exit code; the entrypoint tries three times, waiting 15 s and 45 s between attempts,
  # which this skips, then dies with 1 whatever steamcmd returned.
  code="$(field code)"; code="${code:-8}"
  for attempt in 1 2 3; do
    say "Error! App '730' state is 0x202 after update job."
    say "[entrypoint] steamcmd attempt ${attempt} failed (exit ${code})"
    case "$attempt" in 1) say "[entrypoint] retrying in 15s" ;; 2) say "[entrypoint] retrying in 45s" ;; esac
  done
  printf '%s\n' "[entrypoint] ERROR: steamcmd failed after 3 attempts (last exit ${code}); nothing was deleted, so the install in ${INSTALL_DIR} is whatever the last attempt left. Steam's own state codes (0x202, 0x6) mean another steamcmd is using this directory — only ONE update container per host volume, ever — or the filesystem is full (0x202 wants the whole staged size free), or Steam no longer serves the installed manifests to anonymous logins (an install more than one build behind with an empty ${INSTALL_DIR}/.ankor/depotcache; the content log says 'Failed to get manifest request code'); check the space line above and re-run, or reinstall from /nodes to start the install over." >&2
  exit 1
fi

build="$(field build)"; build="${build:-0}"
for line in \
  " Update state (0x5) verifying install, progress: 12.50 (3739 / 29913)" \
  " Update state (0x61) downloading, progress: 42.00 (12563 / 29913)" \
  " Update state (0x61) downloading, progress: 87.00 (26024 / 29913)" \
  " Update state (0x81) verifying update, progress: 96.00 (28716 / 29913)" \
  "Success! App '730' fully installed."; do
  say "$line"
done

mkdir -p "${INSTALL_DIR}/steamapps" "${INSTALL_DIR}/.ankor/steamclient/linux64"
printf '"AppState"\n{\n\t"appid"\t\t"730"\n\t"StateFlags"\t\t"4"\n\t"buildid"\t\t"%s"\n}\n' "$build" > "${INSTALL_DIR}/steamapps/appmanifest_730.acf"
printf 'fake steamclient\n' > "${INSTALL_DIR}/.ankor/steamclient/linux64/steamclient.so"
say "[entrypoint] steamclient.so stored in the install"
installed="$(printf '{\n  "buildid": "%s",\n  "installed_at": "2026-01-01T00:00:00Z"\n}\n' "$build")"
printf '%s\n' "$installed" > "${INSTALL_DIR}/.ankor/installed.json"
say "[entrypoint] recorded install: $(printf '%s' "$installed" | tr -d '\n')"
say "[entrypoint] gameinfo.gi patched"
say "[entrypoint] update complete"
