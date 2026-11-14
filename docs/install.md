---
title: Install a node
description: Set up a Linux host as a game node with the setup script.
slug: install
---

You install a node with one command on the host. [Setup script](setup-script.md) describes what
that command does. Each step below names the request that the controller sends. On
[ankor.sh](https://ankor.sh), follow the [quick start](quickstart.md) instead.

## Prerequisites

- A host that meets the [requirements](requirements.md).

## Set up the node

1. Prepare the host as its [distribution](#distributions) needs, usually by installing `curl` and CA
   certificates.
2. Add the node in the controller and copy its setup command and node token.
3. Run the setup command on the host and paste the node token when it asks. Setup logs
   `agent connected.` and prints a summary. If the agent has not connected within a minute, setup
   warns that the agent keeps retrying. It still prints the summary. Follow the agent with
   `docker logs -f ankor-agent`. [Setup script](setup-script.md) lists the options and unattended
   installs.
4. Add the node's slots in the controller, each with a game port, a GOTV port and a login token.
   The controller sends them with [agent:set_config]. A node without slots connects and reports,
   but cannot run a server.
5. Open each slot's game port (UDP) in the firewall and router. Open its GOTV port as well if
   matches on the slot are to be watched.
6. Install CS2 from the controller, with [agent:update_cs2]. This is the large download that
   [Capacity](requirements.md#capacity) sizes. From then on CS2
   [updates itself](operations.md#update-cs2).
7. [Launch a test server](operations.md#launch-a-test-server).

## Distributions

Setup installs Docker with your distribution's package manager. Each distribution therefore has
its own packages and quirks. Before you run setup, install the packages listed for yours and read
its notes.

Setup checks the architecture, distribution, init system and LXC nesting before it changes
anything. If the host is not ready, setup stops and explains why.

### Debian and Ubuntu

```sh
apt-get update && apt-get install -y curl ca-certificates
```

- Docker comes from Docker's repository for the release's codename. Setup stops on a release
  for which Docker publishes no packages. Docker lists its
  [supported releases](https://docs.docker.com/engine/install/debian/).
- Ubuntu derivatives such as Linux Mint and Pop!_OS use the Ubuntu release they track. Debian
  derivatives with their own codename, such as Kali, are not in Docker's repository.
- Remove Docker from the snap store first, with `snap remove docker`.
  [Existing Docker](#existing-docker) explains why.

### Fedora, RHEL, Rocky, AlmaLinux, CentOS Stream

```sh
dnf install -y curl ca-certificates
```

- Setup refuses Fedora's `moby-engine`, because it runs with [SELinux labelling](#existing-docker).
- On RHEL and its rebuilds, setup first removes Podman, runc and any package that depends on them,
  as Docker's guide prescribes. Fedora keeps Podman and loses only `podman-docker`.

### Arch

```sh
pacman -Syu --needed curl ca-certificates
```

- Setup runs a full `pacman -Syu` before it installs Docker, because Arch supports no partial
  upgrades.
- Proxmox's Arch template needs its keyring first: `pacman-key --init && pacman-key --populate archlinux`.
- In LXC, pacman's download sandbox fails with "Landlock is not supported by the kernel". Turn it
  off: `sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf`.

### openSUSE

```sh
zypper install -y curl ca-certificates
```

### Alpine

```sh
apk add bash curl ca-certificates
```

- Docker comes from the community repository, which setup enables.
- Alpine needs a VM, because in LXC OpenRC cannot hand the cgroup controllers to Docker.

## Proxmox LXC

- Enable nesting with `pct set <vmid> --features nesting=1`. keyctl is not needed.
- Unprivileged containers work.
- The node reports the container's CPUs and memory, not the Proxmox host's.

## Existing Docker

Setup keeps a Docker Engine that runs as the system daemon on `/var/run/docker.sock`. It also
enables that engine at boot. On any setting below, setup stops and names the setting at fault. By
then it has installed the base packages and written `agent.yml`. Fix the setting and run setup
again.

- Docker Engine older than 25, because the agent needs Engine API 1.44.
- Rootless Docker, because the agent container mounts the system daemon's socket and the game
  servers share the host network.
- `userns-remap` in `/etc/docker/daemon.json`, because it maps uid 1000 away from the owner of
  `/srv/ankor` and refuses host networking.
- SELinux labelling (`selinux-enabled`), because SELinux then blocks the containers' access to the
  Docker socket and to `/srv/ankor`.
- Docker from the snap store, because its confinement cannot bind `/srv/ankor`.

Setup replaces Podman's `docker` shim with Docker Engine.

## Remove a node

A node exists in two places: on its host and as an entry in the controller. Deleting the entry
only deauthorises the node. The agent stays installed but can no longer connect. The node
therefore receives no new launches.

To remove the node from its host as well, stop its servers and run the commands below. They remove
every Ankor container, image and volume. They also remove the node's files, including events that
it has not delivered. Docker stays installed. The node stops reporting to the controller, and you
can then delete its entry.

```sh
docker rm -f $(docker ps -aq --filter label=ankor.role)
docker rmi $(docker images -q --filter label=ankor.image)
docker volume rm ankor_cs2_data
rm -rf /srv/ankor
```

To move a node to another host, or to start a host over, remove the node and set the host up as a
new one. [One host per node](operations.md#one-host-per-node) explains why.
