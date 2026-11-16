# CS2 game-server image

The image runs Ankor's game servers. It holds CS2's runtime libraries, SteamCMD, SwiftlyS2, the
Ankor plugin, its cfgs and its translations. **It does not contain CS2 itself.** The game lives in
one install volume on each x86_64 game host, which every game on that host shares.

## Two modes

The [entrypoint](entrypoint.sh) runs in one of two modes, and the agent chooses which.

- **Update mode** (`CS2_UPDATE_ONLY=1`) installs or updates the game. It mounts the install volume
  read-write and runs SteamCMD. It also writes the game build to `.ankor/installed.json` and leaves
  the empty mountpoints that run mode binds over, because a read-only volume cannot have new ones
  created on it. One updater uses a volume at a time.
- **Run mode**, the default, starts one game server. It mounts the install volume read-only and
  writes nothing to it. It checks the install and the overlay, then starts CS2 as the container's
  main process, so Docker's signals reach the game directly.

## The overlay

SwiftlyS2, the plugin and the cfgs never go into the install volume. They sit in the image at
`/opt/ankor/overlay`, and the agent extracts them once per image. In run mode the agent mounts them
read-only over `game/csgo/addons/` and `game/csgo/cfg/ankor/`, so a game server has nothing of
Ankor's it can write to. [File layout](https://docs.ankor.sh/configuration/#file-layout) shows
where every file sits on a node.

## The command line

The entrypoint builds CS2's command line from environment variables, so it is defined in one place.
It holds the startup flags and what needs a map load to take effect, such as the game type and mode
and whether GOTV runs, ahead of the map. The [entrypoint](entrypoint.sh) lists every variable and its default.

## Game containers

- Game files and addons are read-only. The slot's `ankor/` directory and SwiftlyS2's data get
  writable mounts, owned by the game's user, uid 1000.
- Games use host networking, dropped capabilities, process limits and no elevated privileges.
- A workshop server boots a stock map before it switches. The download needs a Steam Web API key
  and the node's writable workshop cache.
- The framework's configuration directory must not be hidden by an empty mount, since SwiftlyS2
  reads its own and the plugin's config from it.

## Building the image

[build.sh](build.sh) builds the image from `packages/plugin` and needs only Docker. Any further
arguments go to `docker build`.

```sh
packages/plugin/docker/build.sh                  # tags ankor-cs2:dev
packages/plugin/docker/build.sh ankor-cs2:1.2.3  # tags ankor-cs2:1.2.3
```

- The build always targets `linux/amd64`, because CS2 runs only on x86_64. On an ARM host it builds
  under emulation, and the game does not run in the result. Use such a build to check the image's
  contents only.
- The plugin's tests run in the Dockerfile's first stage, so a failing test fails the build. Among
  them, a chat message key without a translation fails it.
- The image is labelled `org.opencontainers.image.version` with `AnkorVersion` from
  [Directory.Build.props](../Directory.Build.props). The agent reads that label to follow image
  releases, so the Dockerfile refuses a `VERSION` build argument that differs. build.sh passes it.

## Changing the image

- Rules and phase cfgs are in [cfg](../cfg). Changing them changes the overlay, so it needs a new image.
- The SwiftlyS2 version and checksum in the [Dockerfile](Dockerfile) change together with its
  package reference in [the plugin project](../Ankor.csproj).
- SwiftlyS2's `core.SteamAuth.Mode` must stay `strict`. In the default `flexible` mode, every
  plugin on the server and SwiftlyS2's permissions trust the SteamID that a client claims before
  Steam verifies it. With `strict`, that SteamID reads 0 until then. Ankor's plugin alone reads
  the claim, so a player who loads before Steam answers is placed on the team. The plugin kicks
  the player when Steam has not verified the claim within `TeamLock.AuthorizationGraceSeconds`.
  The timer also covers a rejection, because SwiftlyS2 raises no event for one. The build fails
  when the setting is missing.
- SwiftlyS2's release zip has no license of its own, so its license is vendored in
  [licenses/SwiftlyS2](licenses/SwiftlyS2). Fetch it again from the new tag when a release changes it.
- The plugin's identity, assembly name and installation directory stay aligned.
- The agent's [host layout](../../agent/crates/agent/src/host_layout.rs) defines the mounts, and its
  [container setup](../../agent/crates/agent/src/container_env.rs) the values each launch passes.
