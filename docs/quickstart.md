---
title: Quick start
description: From a fresh Linux host to a lobby on ankor.sh.
slug: quickstart
---

This page takes you from a Linux machine to your first lobby on [ankor.sh](https://ankor.sh).

## Before you start

- A host that meets the [requirements](requirements.md).
- A Steam account that can create [Game Server Login Tokens](requirements.md#game-server-login-tokens),
  and its [Steam Web API key](https://steamcommunity.com/dev).

## 1. Set up your account

1. Sign in to [ankor.sh](https://ankor.sh) with Steam.
2. Under **Settings** → **Steam key**, save your key.
   [Your Steam Web API key](ankor-web.md#your-steam-web-api-key) explains why
   [ankor.sh](https://ankor.sh) needs it.

## 2. Install the node

1. Install the packages the host's [distribution](install.md#distributions) needs.
2. Under **Settings** → **Nodes** → **Add node**, add the node. Copy its setup command and node
   token.
3. Run the setup command on the host and paste the token. The node shows as online under
   **Settings** → **Nodes**.
4. Under node options → **Edit** → **Add server**, add a slot for each game server.
   [ankor.sh](https://ankor.sh) creates each slot's login token. Then open the slot's game port
   (UDP) to players.
5. Under node options → **Install CS2**, click **Start install**.

[Set up the node](install.md#set-up-the-node) has the details for each step.

## 3. Start a lobby

1. On the front page, click **Create a lobby**. A node of yours, or one shared with you, must be
   online.
2. Copy the **Invite link** and send it to the other players.
3. Once a second player has joined, start an **Aim warmup** server for anyone to practise on. It
   also tests your node.
4. Choose the match setup: how teams are picked, the map selection, sides, overtime and friendly
   fire.
5. Once the lobby is full, move it through **Teams**, **Maps** and **Sides**. The match starts on
   your node, and every player gets a **Connect** button.

## Next

- [Share your nodes](ankor-web.md#share-your-nodes) so friends can start lobbies on them.
- [ankor.sh](ankor-web.md) lists where each node action is.
- [Operating a node](operations.md) covers updates, restarts and demos.
- [Troubleshooting](troubleshooting.md) helps when something does not connect.
