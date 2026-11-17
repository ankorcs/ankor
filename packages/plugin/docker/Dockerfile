# syntax=docker/dockerfile:1

# CS2 dedicated server + SwiftlyS2 + the Ankor plugin. SwiftlyS2 is a standalone loader (see packages/plugin/docker/README.md).
#
# Build context is packages/plugin, whose .dockerignore leaves out build output:
#   docker build --platform linux/amd64 -f packages/plugin/docker/Dockerfile \
#     --build-arg VERSION=<AnkorVersion> -t ankor-cs2:dev packages/plugin
# or just packages/plugin/docker/build.sh
#
# The image contains no game files. CS2 is installed into a volume by running the image once with
# CS2_UPDATE_ONLY=1; in run mode that volume is mounted read-only and the entrypoint writes nothing
# to it. The addon overlay stays in the image and is mounted read-only into each container instead
# of being copied into the volume. See packages/plugin/docker/README.md.

# Pin the stable Swiftly release and its checksum.
# Bump SWIFTLY_VERSION and SWIFTLY_SHA256 together (compute the new sum
# with `curl -fsSL <url> | sha256sum`), and bump SwiftlyVersion in packages/plugin/Ankor.csproj to
# match — the plugin compiles against the managed assembly the release ships. See README, "Pinned
# versions". .github/workflows/swiftlys2.yml does this for each patch release, and
# .github/workflows/cs2-beta.yml overrides both build arguments with SwiftlyS2's newest beta.
ARG SWIFTLY_VERSION=1.5.3
ARG SWIFTLY_SHA256=e170e335fe531151dc108ce840e9112310c145b6b69b21f481edb0d0d3dc32fc

# ---------------------------------------------------------------------------------------------
# Stage 1: build the plugin
# ---------------------------------------------------------------------------------------------
# Runs on the BUILD machine's architecture: the plugin compiles to IL, which is the same bytes on
# every platform, and .NET under QEMU emulation (JIT output that the translator cannot cache) is
# 10-30x slower than native. Only the final stage has to be amd64. The SDK is the one
# scripts/toolchain.Dockerfile pins, so the plugin tests run the same here and in the checks.
FROM --platform=$BUILDPLATFORM mcr.microsoft.com/dotnet/sdk:10.0 AS plugin
# The plugin compiles against the SwiftlyS2.CS2 package of the release stage 2 installs.
ARG SWIFTLY_VERSION

WORKDIR /src
COPY Directory.Build.props ./
# The image's version label must be the plugin's version, so the build refuses any other.
ARG VERSION
RUN grep -q "<AnkorVersion>${VERSION}</AnkorVersion>" Directory.Build.props \
 || { echo "VERSION must be AnkorVersion in Directory.Build.props, not '${VERSION}'" >&2; exit 1; }
COPY Ankor.slnx ./
COPY Ankor.csproj ./
COPY core/Ankor.Core.csproj core/
COPY diagnostics/Ankor.Diagnostics.csproj diagnostics/
COPY tests/Ankor.Tests.csproj tests/
COPY testing/Ankor.Testing.csproj testing/
COPY replay/Ankor.Replay.csproj replay/
COPY headless/Ankor.Headless.csproj headless/
RUN dotnet restore Ankor.slnx -p:SwiftlyVersion=${SWIFTLY_VERSION}

COPY . ./
# The tests need no game and run in seconds. They are the gate on the chat catalog: a key used in
# code but missing from resources/translations/en.jsonc fails the image build here rather than
# showing players a raw key mid-match (packages/plugin/tests/MessagesTests.cs).
RUN dotnet test tests/Ankor.Tests.csproj -c Release --no-restore -p:SwiftlyVersion=${SWIFTLY_VERSION}

RUN dotnet build Ankor.csproj -c Release --no-restore -p:SwiftlyVersion=${SWIFTLY_VERSION} \
 && test -f build/Ankor/Ankor.dll \
 && test -f build/Ankor/resources/translations/en.jsonc

# ---------------------------------------------------------------------------------------------
# Stage 2: fetch SwiftlyS2
# ---------------------------------------------------------------------------------------------
# Build-platform too: this stage only downloads and unpacks one architecture-independent zip (its
# native .so files are x86_64, but unzip does not care).
FROM --platform=$BUILDPLATFORM debian:trixie-slim AS addons

# The "with-runtimes" asset is the one to use: it bundles the .NET runtime SwiftlyS2 hosts under
# addons/swiftlys2/bin/managed/dotnet/, so the image needs no dotnet of its own.
ARG SWIFTLY_VERSION
ARG SWIFTLY_SHA256

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /overlay

# The archive is rooted at swiftlys2-linux-v<version>-with-runtimes/addons/, so the wrapper
# directory is stripped and what lands in /overlay/addons/ merges straight into game/csgo/.
RUN curl -fsSL -o /tmp/swiftly.zip \
      "https://github.com/swiftly-solution/swiftlys2/releases/download/v${SWIFTLY_VERSION}/swiftlys2-linux-v${SWIFTLY_VERSION}-with-runtimes.zip" \
 && echo "${SWIFTLY_SHA256}  /tmp/swiftly.zip" | sha256sum -c - \
 && unzip -q -o /tmp/swiftly.zip -d /tmp/swiftly \
 && mv "/tmp/swiftly/swiftlys2-linux-v${SWIFTLY_VERSION}-with-runtimes/addons" /overlay/addons \
 && rm -rf /tmp/swiftly.zip /tmp/swiftly \
 && test -f /overlay/addons/swiftlys2/bin/linuxsteamrt64/swiftlys2.so \
 && test -f /overlay/addons/swiftlys2/bin/linuxsteamrt64/libserver.so \
 && test -f /overlay/addons/swiftlys2/bin/managed/SwiftlyS2.CS2.dll \
 && test -f /overlay/addons/swiftlys2/gamedata/cs2/core/signatures.jsonc

# The release zip carries no license of SwiftlyS2's own, so its GPL-3.0 text is vendored in
# docker/licenses/SwiftlyS2, taken from the pinned tag; fetch it again if a release changes it. The
# image points at the source of the exact release it ships, as GPL-3.0 section 6 asks.
RUN mkdir -p /licenses \
 && printf 'SwiftlyS2 %s, unmodified. Source: https://github.com/swiftly-solution/swiftlys2/tree/v%s\n' \
      "${SWIFTLY_VERSION}" "${SWIFTLY_VERSION}" > /licenses/SOURCE

# SwiftlyS2 ships its configs as *.example.jsonc and its native Configuration::InitializeExamples
# copies each one to the real name at boot *if it is missing*. The install volume and the addon
# overlay are both read-only at run time, so every real file has to exist up front or that copy
# fails. `core.jsonc` and `database.jsonc` are opened by name and the other three are read through
# the config root, so all five are materialised here.
#
# AutoHotReload is turned off: it puts a FileSystemWatcher on a read-only bind mount that nothing
# will ever write to, and a plugin swap in production is a `docker pull` plus a new container.
#
# SteamAuth is strict: in the default flexible mode, IPlayer.SteamID returns the SteamID the client
# claims before Steam has verified it, to every plugin on the server and to SwiftlyS2's permissions.
# Strict returns 0 until Steam authorizes the player. Ankor's plugin alone reads the claim, to place
# a player who loads before Steam answers, and kicks the player if Steam has not verified it in time.
RUN cd /overlay/addons/swiftlys2/configs \
 && for f in *.example.jsonc; do cp "$f" "${f%.example.jsonc}.jsonc"; done \
 && sed -i 's/"AutoHotReload": true/"AutoHotReload": false/' core.jsonc \
 && grep -q '"AutoHotReload": false' core.jsonc \
 && sed -i 's/"Mode": "flexible"/"Mode": "strict"/' core.jsonc \
 && grep -q '"Mode": "strict"' core.jsonc \
 && test -f core.jsonc && test -f database.jsonc && test -f permissions.jsonc \
 && test -f confilter.jsonc && test -f command_overrides.jsonc

# The three directories SwiftlyS2 writes into at run time. They do not ship in the archive, and the
# agent bind-mounts a per-slot read-write directory over each of them (`shadowPaths` in
# packages/agent/crates/agent/src/host_layout.rs) — a bind mount needs its mountpoint to exist on the parent's
# own filesystem, which is this overlay, so they are created here rather than being left to the
# agent alone. See README, "Run mode".
RUN mkdir -p /overlay/addons/swiftlys2/logs \
             /overlay/addons/swiftlys2/dumps \
             /overlay/addons/swiftlys2/data

# ---------------------------------------------------------------------------------------------
# Stage 3: the runtime image
# ---------------------------------------------------------------------------------------------
FROM debian:trixie-slim
# Read by the agent's image sweep: it outlives the tag, which a reload under the same name takes away.
LABEL ankor.image=cs2
# Shown on the registry's package page.
LABEL org.opencontainers.image.description="Ankor CS2 game server: Counter-Strike 2 with SwiftlyS2 and the Ankor match plugin."
LABEL org.opencontainers.image.licenses="GPL-3.0-only"
LABEL org.opencontainers.image.source="https://github.com/ankorcs/ankor"
LABEL org.opencontainers.image.url="https://ankor.sh"
LABEL org.opencontainers.image.documentation="https://docs.ankor.sh"

# steamcmd is a 32-bit binary, hence the i386 architecture. The 64-bit libraries are what
# game/bin/linuxsteamrt64/cs2 and its shared objects link against; libicu is required by the .NET
# runtime SwiftlyS2 hosts out of addons/swiftlys2/bin/managed/dotnet/ (setting
# DOTNET_SYSTEM_GLOBALIZATION_INVARIANT is not a substitute — the runtime wants real ICU).
RUN dpkg --add-architecture i386 \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      tar \
      gawk \
      procps \
      tzdata \
      lib32gcc-s1 \
      lib32stdc++6 \
      lib32z1 \
      libc6-i386 \
      libcurl4t64 \
      libicu76 \
      liblzma5 \
      libncurses6 \
      libsdl2-2.0-0 \
      libtinfo6 \
      zlib1g \
 && rm -rf /var/lib/apt/lists/*

# Non-root, uid 1000, so a bind-mounted volume owned by the host's first user just works.
RUN useradd --uid 1000 --create-home --home-dir /home/steam --shell /bin/bash steam

ENV HOME=/home/steam
# Build-time only: the entrypoint and the agent contract (IMAGE_OVERLAY_DIR) name the same paths.
ARG STEAMCMD_DIR=/home/steam/steamcmd
ARG CS2_INSTALL_DIR=/home/steam/cs2-dedicated
ARG OVERLAY_DIR=/opt/ankor/overlay

# steamcmd: the one installer, already self-updated in steamcmd/steamcmd's image. Its links into
# /root are dropped, as the steam user cannot read them.
COPY --from=steamcmd/steamcmd:debian-13@sha256:289dbb94b4fc92b77bf4acaaf5a7886dd631a2394f4afd950334d7a9fee7cc23 \
     /root/.local/share/Steam/steamcmd ${STEAMCMD_DIR}
RUN find "$STEAMCMD_DIR" -lname '/root/*' -delete \
 && "$STEAMCMD_DIR/steamcmd.sh" +quit \
 && test -f "$STEAMCMD_DIR/linux64/steamclient.so"

# The addon overlay: SwiftlyS2, the plugin and the Ankor cfgs. It is NEVER
# copied into the install volume. The agent extracts it out of this image once per image id
# (`packages/agent/crates/agent/src/overlay.rs`) and bind-mounts it read-only over game/csgo/addons/ and
# game/csgo/cfg/ankor/ per container, so the volume holds nothing but the game and a game update
# cannot touch our code. The agent extracts it from IMAGE_OVERLAY_DIR in its contract.
COPY --from=addons /overlay/addons/ ${OVERLAY_DIR}/addons/
# SwiftlyS2 loads `plugins/<dir>/<dir>.dll`, so the directory name has to be exactly Ankor —
# which is also the [PluginMetadata] Id and the AssemblyName. There is no manifest file.
COPY --from=plugin /src/build/Ankor/ ${OVERLAY_DIR}/addons/swiftlys2/plugins/Ankor/
# The plugin's own SwiftlyS2 config (chat tag). Its configuration service reads
# configs/plugins/<Id>/<file> and creates the directory when it is missing, which a read-only
# overlay cannot allow, so the file ships here rather than being seeded at first start.
COPY configs/ ${OVERLAY_DIR}/addons/swiftlys2/configs/plugins/Ankor/
COPY cfg/ ${OVERLAY_DIR}/cfg/ankor/
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY LICENSE /usr/share/licenses/ankor-cs2/LICENSE
COPY docker/licenses/SwiftlyS2/ /usr/share/licenses/ankor-cs2/SwiftlyS2/
COPY --from=addons /licenses/SOURCE /usr/share/licenses/ankor-cs2/SwiftlyS2/SOURCE

# The install mountpoint must exist and be owned by `steam` in the image: Docker seeds a fresh named
# volume from the image's directory, ownership included, and would otherwise create it as root:root
# and leave the unprivileged server unable to write during an update.
RUN mkdir -p "$CS2_INSTALL_DIR" /home/steam/ankor-event-captures

RUN chmod +x /usr/local/bin/entrypoint.sh \
 && chown -R steam:steam /home/steam \
 && chmod -R a+rX ${OVERLAY_DIR} \
 && test -f ${OVERLAY_DIR}/addons/swiftlys2/plugins/Ankor/Ankor.dll \
 && test -f ${OVERLAY_DIR}/addons/swiftlys2/plugins/Ankor/resources/translations/en.jsonc \
 && test -f ${OVERLAY_DIR}/addons/swiftlys2/bin/linuxsteamrt64/swiftlys2.so \
 && test -f ${OVERLAY_DIR}/addons/swiftlys2/configs/core.jsonc \
 && test -f ${OVERLAY_DIR}/addons/swiftlys2/configs/plugins/Ankor/config.jsonc \
 && test -f ${OVERLAY_DIR}/cfg/ankor/base.cfg \
 && test -f ${OVERLAY_DIR}/cfg/ankor/reset.cfg \
 && test -f ${OVERLAY_DIR}/cfg/ankor/warmup.cfg \
 && test -f ${OVERLAY_DIR}/cfg/ankor/knife.cfg

USER steam
WORKDIR /home/steam

# These come last, because a changed build argument reruns every RUN after it. A new commit then
# reruns none of this stage. The agent's auto-update reads the version label. REVISION is the
# commit, which the build cannot read itself, and it is empty when none is passed.
ARG VERSION
ARG REVISION
LABEL org.opencontainers.image.version="${VERSION}" org.opencontainers.image.revision="${REVISION}"
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
