---
title: Requirements
description: What a game node needs before setup, and what it gets from the controller.
slug: requirements
---

Before you turn a host into a game node, check that it has what it needs: the machine, its
capacity, its network and the Steam tokens for its game servers. Check these before you follow
[Install a node](install.md). A game node can be almost any x86_64 Linux machine, such as a spare
PC, a VM or a container on a home server, or a rented server. It needs:

- A supported [host](#host).
- Enough [CPU, memory and disk](#capacity) for its game servers.
- A [network](#network) where players can reach its game ports.
- A Steam account that can hold [Game Server Login Tokens](#game-server-login-tokens).
- A [node token](setup-script.md#the-node-token) from the controller. The node token identifies
  the node to its controller.

## Host

The setup script installs Docker if needed and runs the agent in a Docker container, so the host
must be able to run Docker Engine. Before it changes anything, setup checks the architecture, the
init system and LXC nesting. If a check fails, setup stops and explains why.

- **Architecture:** x86_64. CS2 and SteamCMD are built only for x86_64, so setup refuses any other
  architecture, such as arm64.
- **Operating system:** Linux with systemd or OpenRC as PID 1. Setup needs one of them to start
  Docker at boot. Setup supports Debian, Ubuntu, Fedora, RHEL and its rebuilds, Arch, openSUSE and
  Alpine. [Distributions](install.md#distributions) has the packages and notes for each.
- **Machine:** bare metal, a VM or an LXC container with nesting enabled. Without nesting, the LXC
  container refuses the overlay mounts that Docker runs every container on.
  [Proxmox LXC](install.md#proxmox-lxc) explains how to enable it. Alpine must run in a VM.
- **Packages:** install bash, curl and CA certificates before you run setup. Setup installs Docker
  Engine itself and needs root or sudo.
- **Docker:** Docker Engine 25 or newer, because the agent needs at least Engine API 1.44. If
  Docker is [already installed](install.md#existing-docker), it must run as the system daemon.

## Capacity

Each slot on the node runs one game server, and every running server needs CPU and memory. All
servers share [one CS2 installation](how-it-works.md#one-installation-many-servers) on disk.

- **CPU and memory:** give the host at least 2 vCPUs, or equivalent, and 2 GB of RAM. Add about
  one core and 1 GB for each game server. A 1v1 uses about 700 MB. You can cap the memory of each
  game container with [config:memory_mb], in MiB. A cap of 1536 leaves a 5v5 room to spare.
- **Disk:** [limit:NODE_MIN_DISK_GB] GB beyond the operating system. The CS2 installation takes
  about 70 GB. The rest is room for an [update](operations.md#update-cs2) to stage changed files
  in. The installation is in the Docker volume `ankor_cs2_data`. Leave the volume where Docker
  puts it.
- **Servers per node:** the cores, memory and network of the host set the limit. A large
  workstation can run dozens. Add slots once you have seen the node's load during real matches.

### Node size versus node count

Each node has [one CS2 installation](how-it-works.md#one-installation-many-servers) for all its
slots. You can spread the same capacity over a few large nodes or many small ones:

<div class="choices">
<div class="choice">
<p class="choice-name">Fewer, larger nodes</p>
<p class="choice-sub">many slots per host</p>

<p class="choice-label pros">Pros</p>

- Many servers share one CS2 download.
- You have fewer hosts to install and keep patched.

<p class="choice-label cons">Cons</p>

- One outage or update stops many servers.

</div>
<div class="choice">
<p class="choice-name">More, smaller nodes</p>
<p class="choice-sub">few slots per host</p>

<p class="choice-label pros">Pros</p>

- One outage or update stops only a few servers.
- Each host is simpler to replace.

<p class="choice-label cons">Cons</p>

- Each node needs its own [disk](#capacity) and CS2 download.
- You have more hosts to install and patch.

</div>
</div>

## Network

The agent opens every connection to the controller itself, so the controller never connects to the
node. Players connect straight to the game servers on the UDP ports of their slots. The node needs
these connections:

| Direction | Port                                                                                   | Purpose                                                                                               |
|-----------|----------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------|
| Outbound  | TCP 443 (or the controller's port)                                                     | Carries the agent's WebSocket and its event uploads.                                                  |
| Outbound  | The [config:demo_upload] target's port                                                 | Carries [demo uploads](protocol/demos.md) to wherever the target points.                              |
| Outbound  | TCP 443 to `ghcr.io`                                                                   | Pulls the [agent and CS2 images](operations.md#images) that `agent.yml` names.                        |
| Outbound  | TCP 443 to `api.steamcmd.net`                                                          | Checks for new CS2 builds.                                                                            |
| Outbound  | Steam's [client ports](https://help.steampowered.com/en/faqs/view/2EA8-4D75-DA21-31EB) | Downloads CS2 and logs the slots' tokens on to Steam.                                                 |
| Outbound  | TCP 443 to `ipinfo.io`, over IPv4                                                      | Looks up the node's public address and country.                                                       |
| Inbound   | UDP, each slot's game port                                                             | Lets players join the slot's server.                                                                  |
| Inbound   | UDP, each slot's GOTV port                                                             | Lets GOTV viewers watch, when a launch makes GOTV public.                                             |
| Loopback  | TCP [limit:EVENT_RELAY_PORT]                                                           | Carries the plugin's connection to the agent over [host networking](how-it-works.md#host-networking). |

- You set up firewalls, routing and port forwarding. Behind NAT, forward only the UDP ports of the
  slots.
- TCP on a slot's game port is RCON, which is disabled. Keep it closed.
- Players connect to the node's public IPv4 address. The agent looks the address up, or you can
  pin it with `pinned_ip` in the node's [configuration](configuration.md#agentyml). If the lookup
  fails, the node has no address for players until you pin one or the agent restarts. IPv6 is not
  supported.
- For a test on your LAN, you can skip port forwarding and pin the node's LAN address.
  [Install a node](install.md#set-up-the-node) describes the LAN test in step 5.

## Game Server Login Tokens

A Game Server Login Token (GSLT) lets a CS2 server log in to Steam. The server must log in for
players to join it over the internet, and for Steam authentication and team-locking to work. Every
slot has its own GSLT, which the controller sends to the node.

The Steam account that creates the GSLTs must own CS2 and have a phone number. You can create the
GSLTs in one of two ways:

- By hand: on [Steam's game server page](https://steamcommunity.com/dev/managegameservers), create
  one GSLT for each slot with app ID `730` and a memo that names the slot. Then give each GSLT to
  the controller.
- Through the controller: a controller can create GSLTs with the account's Steam Web API key.
  [ankor.sh](ankor-web.md#login-tokens) creates them this way.
  [A minimal controller](protocol/minimal-controller.md#5-add-slots) shows the Steam Web API calls.

Keep these in mind when you manage GSLTs:

- A Steam Web API key can read, create and revoke every GSLT on the account. Give it only to a
  controller that you trust.
- Use each GSLT on one server only. If two servers log in with the same GSLT, they log each other
  off Steam.
- The node keeps idle GSLTs from expiring and reports whether Steam accepted each one. The slot's
  `gslt_state` in [agent:slot_status] shows the result. When a server becomes ready, Steam has
  accepted its GSLT, because CS2 does not log on anonymously when Steam refuses one.
- When Steam refuses a GSLT, the slot receives no launches until the controller
  [replaces](operations.md#replace-a-slots-token) the GSLT.
- If you change the password of the Steam account, Steam regenerates all its GSLTs. Every slot then
  reports its GSLT refused until the controller has replaced it.
