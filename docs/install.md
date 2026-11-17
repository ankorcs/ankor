---
title: Install a node
description: Set up a Linux host as a game node with the setup script.
slug: install
---

These steps turn a Linux host into a game node that your controller can launch CS2 servers on. You
run one setup command on the host, and the controller does the rest through the agent's
connection. [Setup script](setup-script.md) describes what that command does. Where a step happens
in the controller, it names the request that the controller sends to the agent. If you're building
your own controller, [A minimal controller](protocol/minimal-controller.md) covers the
controller's side of these steps. If you use [ankor.sh](https://ankor.sh), follow the
[quick start](quickstart.md) instead.

## Prerequisites

- A host that meets the [requirements](requirements.md).

## Set up the node

1. On the host, install the packages that its [distribution](#distributions) needs. Usually that's
   `curl` and CA certificates.
2. In the controller, add the node. Copy its setup command and its node token.
3. On the host, run the setup command and paste the node token when it asks for it. Setup then
   installs Docker if needed and starts the agent. When the agent reaches the controller, setup
   logs `agent connected.` and prints a summary.
   - If the agent has not connected within a minute, setup warns that the agent keeps retrying.
     Setup still prints the summary. Follow the agent's log with `docker logs -f ankor-agent` to
     see why it hasn't connected.
   - If the agent rejects its configuration or its token, setup stops with an error. If the node
     was deleted in the controller, you must add it again.
   - [Setup script](setup-script.md) lists the options and explains unattended installs.
4. In the controller, add the node's slots. A slot is room for one game server. Give each slot a
   game port, a GOTV port and a [login token](requirements.md#game-server-login-tokens). The
   controller sends the slots to the agent with [agent:set_config]. A node without slots still
   connects and reports to the controller, but it cannot start a server.
5. Open each slot's game port (UDP) in the host's firewall and in your router, so that players can
   reach the server. If you want people to watch matches on the slot, open its GOTV port as well.
   - For a test on your LAN, you can skip the router's port forwarding and connect to the node's
     LAN IP. The host's firewall must still allow the ports. Players connect to the node's public
     IPv4 address by default. To hand out the LAN address instead, set it as `pinned_ip` in the
     node's [configuration](configuration.md#agentyml).
6. In the controller, install CS2 by sending [agent:update_cs2] to the node. CS2 is a large
   download, so the node needs the disk space that [Capacity](requirements.md#capacity) lists. From
   then on, the node [keeps CS2 up to date](operations.md#update-cs2) by itself.
7. [Launch a test server](operations.md#launch-a-test-server) to check that players can join.

## Distributions

Setup installs Docker with your distribution's package manager, so the packages and quirks differ
between distributions. Before you run setup, install the packages listed for your distribution and
read its notes.

Before it changes anything, setup checks the host's architecture, init system and LXC nesting,
and reads which distribution it runs. If a check fails, setup stops and explains why.

### Debian and Ubuntu

Install the packages that setup needs:

```sh
apt-get update && apt-get install -y curl ca-certificates
```

- Setup installs Docker from Docker's repository for your release's codename. If Docker publishes
  no packages for your release, setup stops. Docker lists its
  [supported releases](https://docs.docker.com/engine/install/debian/).
- On an Ubuntu derivative, such as Linux Mint or Pop!_OS, setup uses the Ubuntu release that the
  derivative tracks. Debian derivatives that have their own codename, such as Kali, are not in
  Docker's repository.
- If Docker is installed from the snap store, remove it first with `snap remove docker`.
  [Existing Docker](#existing-docker) explains why.

### Fedora, RHEL, Rocky, AlmaLinux, CentOS Stream

Install the packages that setup needs:

```sh
dnf install -y curl ca-certificates
```

- Setup refuses Fedora's `moby-engine`, because it runs with [SELinux labelling](#existing-docker).
- On RHEL and its rebuilds, setup first removes Podman, runc and any package that depends on them,
  as Docker's install guide prescribes. On Fedora, setup keeps Podman and removes only
  `podman-docker`.

### Arch

Install the packages that setup needs:

```sh
pacman -Syu --needed curl ca-certificates
```

- Setup runs a full `pacman -Syu` before it installs Docker, because Arch does not support partial
  upgrades.
- On Proxmox's Arch template, initialise the keyring first:
  `pacman-key --init && pacman-key --populate archlinux`.
- In LXC, pacman's download sandbox may fail with "Landlock is not supported by the kernel".
  Turn the sandbox off with `sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf`.

### openSUSE

Install the packages that setup needs:

```sh
zypper install -y curl ca-certificates
```

### Alpine

Install the packages that setup needs. Alpine also needs `bash`, because the setup script is a
bash script:

```sh
apk add bash curl ca-certificates
```

- Setup installs Docker from the community repository, and enables that repository if it's off.
- Alpine must run in a VM. In an LXC container, OpenRC cannot hand the cgroup controllers to
  Docker.

## Proxmox LXC

You can run a node in a Proxmox LXC container. Perform these steps in Proxmox, then follow the steps
above for the OS the LXC container runs.

- Enable nesting on the Proxmox host with `pct set <vmid> --features nesting=1`. You don't need
  keyctl.
- Unprivileged containers work.
- The node reports the CPUs and memory of the LXC container, not those of the Proxmox host.

## Existing Docker

If the host already runs Docker Engine as the system daemon on `/var/run/docker.sock`, setup keeps
it and enables it at boot. Some Docker settings stop the agent's containers from working, though.
If setup finds one of them, it stops and names the setting. By then, setup has installed the base
packages and written `agent.yml`. Fix the setting and run setup again.

Setup stops on these settings:

- Docker Engine older than 25, because the agent needs at least Engine API 1.44.
- Rootless Docker, because the agent container mounts the system daemon's socket and the game
  servers share the host network.
- `userns-remap` in `/etc/docker/daemon.json`, because it maps uid 1000 away from the owner of
  `/srv/ankor` and refuses host networking.
- SELinux labelling (`selinux-enabled`), because SELinux then blocks the containers' access to the
  Docker socket and to `/srv/ankor`.
- Docker from the snap store, because its confinement cannot bind `/srv/ankor`.

If the `docker` command on the host is Podman's shim, setup replaces it with Docker Engine.

## Remove a node

A node exists in two places: as the agent on its host, and as an entry in the controller. When you
delete the entry in the controller, the controller only deauthorises the node. The agent stays
installed on the host, but it can no longer connect, so the node receives no new launches.

To remove the node from its host as well, stop its servers first. Then run the commands below on
the host. They remove every Ankor container, image and volume. They also delete the node's files,
including any events that the node has not delivered to the controller yet. Docker stays
installed. Once the node stops reporting to the controller, you can delete its entry there.

```sh
docker rm -f $(docker ps -aq --filter label=ankor.role)
docker rmi $(docker images -q --filter label=ankor.image)
docker volume rm ankor_cs2_data
rm -rf /srv/ankor
```

To move a node to another host, or to start a host over, remove the node and set up the host as a
new node. [One host per node](operations.md#one-host-per-node) explains why.
