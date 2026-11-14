---
title: Setup script
description: What node-setup.sh does, its options and when to change them.
slug: setup-script
---

`node-setup.sh` turns a Linux host into a game node. It fetches the node's `agent.yml` with the
node token, installs Docker if needed, and starts the agent. With [`--controller`](#options), the same
script sets up nodes for your own controller.

```sh
curl -fsSL https://ankor.sh/node-setup.sh | bash
```

Run it as root or as a user with sudo. Running it again is safe. It skips the steps that are
already done and always replaces the `ankor-agent` container. Running servers
[keep running](how-it-works.md#updates-and-restarts) under the new agent.

## What it sets up

- Docker Engine, as each [distribution](install.md#distributions) gets it. Setup keeps a Docker
  Engine already on the host if it [can run the agent's containers](install.md#existing-docker).
- `/srv/ankor`, with `agent.yml` and the game servers' files. The `ankor_cs2_data` volume holds
  the shared CS2 install. Both paths are fixed.
- The `ankor-agent` container. CS2 itself is installed later through the agent, from the
  controller.

Setup leaves the firewall alone. Open the game ports yourself. [Install a node](install.md) has the
step.

## The node token

The script asks for the node token on the terminal, with the input hidden. For an unattended
install, set it in `ANKOR_NODE_TOKEN` instead:

```sh
curl -fsSL https://ankor.sh/node-setup.sh | ANKOR_NODE_TOKEN=<token> bash
```

- With neither a terminal nor the variable, setup stops and tells you to set the variable.
- A malformed token stops setup before it changes anything.
- A host that already has `/srv/ankor/agent.yml` keeps it and asks for no token.

## Write `agent.yml` yourself

If your controller has no [setup endpoint](protocol/node-setup.md), write `/srv/ankor/agent.yml`
yourself, then run the script. The script finds the file and skips the fetch.

- Write the file as [Configuration](configuration.md#agentyml) describes, token included.
- Make the [agent's user](operations.md#permissions) its owner, and set mode `0600`.
- Use the `cs2_image` and `agent_image` that your controller gives you, as
  [Images](requirements.md#images) explains.
- [config:slots] may be empty when the controller adds the slots later.

## Options

The script reads its options after `bash -s --`, because it arrives through a pipe:

```sh
curl -fsSL https://ankor.sh/node-setup.sh | bash -s -- --dry-run
```

| Option                | Default            | Change it when                                                                                                                                                                       |
|-----------------------|--------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `--controller <url>`  | `https://ankor.sh` | The node belongs to another controller. Its address with `https://`, or `http://` for one on your LAN.                                                                               |
| `--setup-path <path>` | `/api/node/setup`  | That controller answers setup at another path. Ignored for `https://ankor.sh`.                                                                                                       |
| `--update`            |                    | You want the agent image set in `agent.yml` pulled and the agent re-created. Needs no token. A failed pull leaves the running agent in place. Fails on a host that was never set up. |
| `--no-pull`           |                    | The agent image is already loaded on the host, for example on a host without registry access. Setup or update fails if it is missing.                                                |
| `--dry-run`           |                    | You want to see every command without running any. Asks for no token.                                                                                                                |
| `--help`              |                    | You want the options in the terminal.                                                                                                                                                |

The controller gives you the command with the options its nodes need.
