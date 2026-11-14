---
title: Requirements
description: What a game node needs before setup, and what it gets from the controller.
slug: requirements
---

A game node can be almost any x86_64 Linux machine: a spare PC, a VM or a container on a home
server, or a rented server. It needs:

- A supported [host](#host).
- Enough [CPU, memory and disk](#capacity) for its game servers.
- A [network](#network) where players can reach its game ports.
- A Steam account that can hold [Game Server Login Tokens](#game-server-login-tokens).
- Its [node token](setup-script.md#the-node-token) from the controller.

## Host

- **Architecture:** x86_64. CS2 has no support for the arm64 architecture.
- **Operating system:** Linux with systemd or OpenRC as PID 1. Setup supports Debian, Ubuntu,
  Fedora, RHEL and its rebuilds, Arch, openSUSE and Alpine. [Distributions](install.md#distributions)
  has the notes for each.
- **Machine:** bare metal, a VM or an LXC container with nesting enabled
  ([Proxmox LXC](install.md#proxmox-lxc)). Alpine needs a VM.
- **Packages:** bash, curl and CA certificates. Setup installs Docker Engine and needs root or sudo.
- **Docker:** Docker Engine 25 or newer (Engine API 1.44). An
  [existing one](install.md#existing-docker) must run as the system daemon.

## Capacity

- **CPU and memory:** at least 2 vCPUs, or equivalent, and 2 GB of RAM. Add about one core and
  1 GB per game server. A 1v1 uses about 700 MB. [config:memory_mb] caps each game container, and
  1536 leaves a 5v5 room to spare.
- **Disk:** [limit:NODE_MIN_DISK_GB] GB beyond the operating system. About 70 GB holds the CS2
  installation. The rest is room for an [update](operations.md#update-cs2) to stage changed files
  in. The installation is in a Docker volume. Leave it where Docker puts it.
- **Servers per node:** the host's cores, memory and network set the limit. A large workstation can
  run dozens. Add slots once you have seen a node's load during real matches.

### Node size versus node count

Each node has [one CS2 installation](how-it-works.md#one-installation-many-servers) for all its
slots. You choose how to split capacity:

| Fewer, larger nodes                      | More, smaller nodes                                     |
|------------------------------------------|---------------------------------------------------------|
| One CS2 download covers many servers.    | Each node needs its own [disk](#capacity) and download. |
| Fewer hosts to install and keep patched. | More hosts, but each is simpler to replace.             |
| An outage or update stops many servers.  | An outage or update stops a few servers.                |

## Network

| Direction | Port                                                                                   | Purpose                                                                                  |
|-----------|----------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------|
| Outbound  | TCP 443 (or the controller's port)                                                     | The agent's WebSocket and its event uploads.                                             |
| Outbound  | The [config:demo_upload] target's port                                                 | Demo uploads, wherever the target points.                                                |
| Outbound  | TCP 443 to `ghcr.io`                                                                   | Pulling the [agent and CS2 images](#images) set in `agent.yml`.                          |
| Outbound  | TCP 443 to `api.steamcmd.net`                                                          | Checking for new CS2 builds.                                                             |
| Outbound  | Steam's [client ports](https://help.steampowered.com/en/faqs/view/2EA8-4D75-DA21-31EB) | Downloading CS2 and logging the slots' tokens on to Steam.                               |
| Outbound  | TCP 443 to `ipinfo.io`, over IPv4                                                      | Looking up the node's public address and country.                                        |
| Inbound   | UDP, each slot's game port                                                             | Players joining.                                                                         |
| Inbound   | UDP, each slot's GOTV port                                                             | GOTV viewers, when a launch makes GOTV public.                                           |
| Loopback  | TCP [limit:EVENT_RELAY_PORT]                                                           | The plugin talking to the agent over [host networking](how-it-works.md#host-networking). |

- You set up firewalls, routing and port forwarding. Behind NAT, forward only the slots'
  UDP ports. The controller never connects to the node.
- TCP on a slot's game port is RCON, which is disabled. Keep it closed.
- Players connect to the node's public IPv4 address. The agent looks it up, or you can pin it. If
  the lookup fails, the node has no address for players until you pin one or the agent restarts.
  IPv6 is not supported.

## Game Server Login Tokens

A login token lets a CS2 server log in to Steam. The server needs that for players to join over the
internet, and for Steam authentication and team-locking to work. Every slot has its own token,
which the controller sends to the node.

Steam creates tokens only for an account that owns CS2 and has a qualifying phone number. The
account must not be limited, community banned or locked. Create the tokens on
[Steam's game server page](https://steamcommunity.com/dev/managegameservers) with app ID `730` and a
memo naming the slot. Then give each one to the controller. A controller can also create them with
the account's Steam Web API key, as [ankor.sh](ankor-web.md#login-tokens) does.

- Steam holds the account responsible for every server that logs in with its tokens. Steam's
  [terms](https://steamcommunity.com/dev/managegameservers) forbid giving tokens to third parties.
  Give your tokens, or a Steam Web API key that creates them, only to the controller that your
  nodes connect to. A key can read, create and revoke every token on the account.
- Use a token on no other server. Two servers logged in with one token log each other off Steam.
- The node keeps idle tokens from expiring and reports whether Steam accepted each one. A server
  that becomes ready shows that Steam accepted its token, because CS2 does not log on anonymously
  when Steam refuses one. The controller [replaces](operations.md#replace-a-slots-token) a refused
  token.
- Changing the Steam account's password regenerates all its tokens, so every slot reports its token
  refused until the controller has replaced it.
- If Steam bans a token's server account, the owning account can be restricted from playing CS2.

## Images

Nodes run the official `ghcr.io/ankorcs/ankor-agent` and `ghcr.io/ankorcs/ankor-cs2` images. The
controller sets their tags in the node's `agent.yml`, such as `ghcr.io/ankorcs/ankor-agent:0.9`.
Setup pulls the agent image, and the agent pulls the CS2 image. The node therefore needs access to
`ghcr.io`.

- Each image has a semantic version and is tagged `X.Y.Z`, `X.Y` and `X`. `ankor-agent:1` is the
  newest 1.x release, and `ankor-agent:1.2.0` is one release. The newest release of all is also
  tagged `latest`, which crosses majors.
- A release line is a major, such as `ankor-agent:1`. For a `0.x` image it is a minor, such as
  `ankor-agent:0.9`, because [SemVer](https://semver.org/#spec-item-4) lets a `0.x` minor break.
- Both images and the protocol share their release line. Only their patch versions differ, so
  `ankor-agent:0.9` and `ankor-cs2:0.9` work together.
- Only the newest release of each image is maintained. A CS2 update can break the plugin or
  SwiftlyS2 at any time, and only the newest release receives the fix. Run the newest line, and
  move to a new line when it ships.
- The newest release is rebuilt weekly on updated base images under the same tags. Older releases
  keep their last build. A node with [config:auto_update] on pulls each rebuild of the release it
  runs.
- To keep a node on an older build, such as for a LAN event, turn [config:auto_update] off or name
  the images by digest, as [Keep a node on its versions](operations.md#keep-a-node-on-its-versions)
  describes.
- Only a release on a new line breaks something:
  - For the agent, the protocol. [Versioning](protocol/versioning.md) lists the releases that support
    each protocol version.
  - For the CS2 image, the software it carries, such as a SwiftlyS2 major that node plugins must be
    rebuilt for.
- The controller chooses the line and can move nodes between versions with [agent:update_agent]
  and [agent:update_cs2_image]. Updating a node on its line's tag brings the newest release of that
  line.
- A node with [config:auto_update] on moves each image to the newest release of its line by
  itself, as [Follow image releases](operations.md#follow-image-releases) describes.
