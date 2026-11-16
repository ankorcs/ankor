---
title: Setup script
description: What node-setup.sh does, its options and when to change them.
slug: setup-script
---

`node-setup.sh` is the setup script that turns a Linux host into a game node. On the host, the
script fetches the node's `agent.yml` from the controller with the node token. It then installs
Docker if needed and starts the agent. This page describes what the script does and its options.
[Install a node](install.md) gives the steps around the script in order, from the packages each
distribution needs to the first test server. If you're building your own controller, point the
script at it with [`--controller`](#options).

To set up a node for [ankor.sh](https://ankor.sh), run this command on the host:

```sh
curl -fsSL https://ankor.sh/node-setup.sh | bash
```

Run the script as root, or as a user who can use sudo. If you start it as another user, the script
downloads itself again and runs as root through sudo, or through su when sudo is missing.

You can run the script again safely, for example to finish a run that stopped halfway. Setup skips
the steps that are already done, but it always replaces the `ankor-agent` container. Game servers
that are running [keep running](how-it-works.md#updates-and-restarts) under the new agent.

## What it sets up

Setup installs or creates these on the host:

- Docker Engine. Setup installs Docker with the package manager of the host's
  [distribution](install.md#distributions). If the host already runs Docker Engine, setup keeps it,
  as long as it [can run the agent's containers](install.md#existing-docker).
- The `/srv/ankor` folder. It holds `agent.yml`, the files of each game server, and the node's
  plugins and cfgs.
- The `ankor_cs2_data` Docker volume. It holds the CS2 install that all game servers on the node
  share.
- The `ankor-agent` container, which runs the agent.

The `/srv/ankor` and `ankor_cs2_data` paths are fixed, and setup has no option to change them.
Setup doesn't install CS2. The agent installs CS2 later, when the controller sends
[agent:update_cs2].

Setup doesn't change the host's firewall. You must open each slot's game port yourself, so that
players can reach the servers. Step 5 of [Install a node](install.md#set-up-the-node) covers the
ports, the router and testing on your LAN.

## The node token

The node token identifies the node to the controller. The controller should present the token when you add
the node, so it can be provided to the host's install script process. The script sends the token to the
controller, and the controller must reply with the node's `agent.yml` after verifying the token.

The script asks for the node token on the terminal and hides what you type. For an unattended
install, set the token in `ANKOR_NODE_TOKEN` instead:

```sh
curl -fsSL https://ankor.sh/node-setup.sh | ANKOR_NODE_TOKEN=<token> bash
```

- If there is neither a terminal nor `ANKOR_NODE_TOKEN`, setup stops and tells you to set the
  variable.
- If the token is malformed, setup stops before it changes anything. A node token must be 32 to 128
  letters and digits.
- If the host already has `/srv/ankor/agent.yml`, setup keeps the file and doesn't ask for a token.
  Setup then ignores `ANKOR_NODE_TOKEN` and warns that it does. To set the node up with a new
  token, either delete `/srv/ankor/agent.yml` and run setup again or edit the file manually to use the new token.

## Write `agent.yml` yourself

If your controller doesn't have a [setup endpoint](protocol/node-setup.md) (yet), you'll have to
write `/srv/ankor/agent.yml` on the node manually before you run the script. Setup then finds the
file, skips the fetch and doesn't ask for a token.

- Write the keys that [Configuration](configuration.md#agentyml) lists, including the node token.
- Set `cs2_image` and `agent_image` to the images that your controller gives you.
  [Images](operations.md#images) explains their tags. Setup stops if either key is missing. It
  also stops on an image that isn't an official one, unless you turn [config:official_images_only]
  off.
- You can leave [config:slots] empty and let the controller add the slots later.
- Make uid 1000 the owner of the file, and set its mode to `0600`. The
  [agent's user](operations.md#permissions) is uid 1000, and it must be able to read the file. The
  mode keeps the node token private.

Create the folder, write the file, then set its owner and mode:

```sh
mkdir -p /srv/ankor
# Write /srv/ankor/agent.yml, then:
chown 1000:1000 /srv/ankor/agent.yml
chmod 0600 /srv/ankor/agent.yml
```

## Options

Pass options after `bash -s --`. Bash reads the script from the pipe, and `-s --` hands the words
after it to the script as its options. A dry run for a LAN controller IP, for example, looks like this:

```sh
curl -fsSL https://ankor.sh/node-setup.sh | bash -s -- --dry-run --controller http://192.168.1.24:3000
```

Your controller shows you the setup command with the options that its nodes need. The table lists
each option and when to change it.

| Option                | Default            | Change it when                                                                                                                                                                                                         |
|-----------------------|--------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `--controller <url>`  | `https://ankor.sh` | The node belongs to another controller (not ankor.sh). Pass its address with `https://`, or with `http://` for a controller on your LAN. Add a port if the controller listens on one, like `http://192.168.1.24:3000`. |
| `--setup-path <path>` | `/api/node/setup`  | If the controller serves its [setup endpoint](protocol/node-setup.md) at another path. Setup ignores this option for `https://ankor.sh`.                                                                               |
| `--update`            |                    | You want to pull the agent image that `agent.yml` names and re-create the agent. An update needs no token. If the pull fails, the running agent stays in place. The update fails on a host that was never set up.      |
| `--no-pull`           |                    | The agent image is already loaded on the host, for example on a host without registry access. Setup or the update fails if the image is missing.                                                                       |
| `--dry-run`           |                    | You want to see every command that setup would run, without running any. A dry run asks for no token.                                                                                                                  |
| `--help`              |                    | You want to see the options in the terminal.                                                                                                                                                                           |

Setup uses `--controller` and `--setup-path` only when it fetches `agent.yml`. On a host that
already has the file, setup still checks them but doesn't use them.
