---
title: ankor.sh
description: Where ankor.sh puts each node action, and the rules it adds for nodes.
slug: ankor-web
---

[ankor.sh](https://ankor.sh) is Ankor's controller. The node guides describe each action as the
request that a controller sends to the agent. This page shows where each action is in
[ankor.sh](https://ankor.sh). It also lists the requirements that [ankor.sh](https://ankor.sh) sets
for its nodes. The [quick start](quickstart.md) walks through a first node.

## Node actions

Manage nodes under **Settings** → **Nodes**. Each node has a menu, called node options below.

| Action                 | In [ankor.sh](https://ankor.sh)                                                                                          | Guide                                                         |
|------------------------|--------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------|
| Add a node             | **Add node**. The node token is shown once.                                                                              | [Install a node](install.md)                                  |
| Add slots              | Node options → **Edit** → **Add server**. [ankor.sh](https://ankor.sh) creates each slot's [login token](#login-tokens). | [Set up the node](install.md#set-up-the-node)                 |
| Change settings        | Node options → **Edit**.                                                                                                 | [Changing settings](configuration.md#changing-settings)       |
| Replace a slot's token | The slot's **Get a new token** link, or **Reset token** under **Settings** → **Login tokens**.                           | [Replace a slot's token](operations.md#replace-a-slots-token) |
| Install or update CS2  | Node options → **Install CS2**, or **Update CS2** once installed.                                                        | [Update CS2](operations.md#update-cs2)                        |
| Update the CS2 image   | Node options → **Update CS2 image**. The node moves to the `latest` tag.                                                 | [Update the CS2 image](operations.md#update-the-cs2-image)    |
| Update the agent       | Node options → **Update agent**. The node moves to the `latest` tag.                                                     | [Update the agent](operations.md#update-the-agent)            |
| Read logs              | Node options → **Logs & console**.                                                                                       | [Logs](operations.md#logs)                                    |
| Test a node            | In a lobby with a second player, **Aim warmup** → **Start server**. **Pause** your other nodes to pick this one.         | [Launch a test server](operations.md#launch-a-test-server)    |
| Stop a server          | The slot's stop icon → **Stop server**.                                                                                  |                                                               |
| Stop new launches      | **Pause**. Running servers keep going.                                                                                   |                                                               |
| Remove a node          | Delete it under **Settings** → **Nodes**. Only its owner can.                                                            | [Remove a node](install.md#remove-a-node)                     |

The node table also shows each node's undelivered events. If a node runs an agent major that
[ankor.sh](https://ankor.sh) no longer supports, the node is marked **Unsupported agent**. It
receives no launches until you update it.

## Demos

When you add or edit a node, **Demo upload** sets its [config:demo_upload] target:

- **This service**, the default, uploads to [ankor.sh](https://ankor.sh).
- **Elsewhere** uploads to a URL you enter.
- **Off** keeps demos on the node.

**Settings** → **Nodes** lists the node's demos, and you can delete them there.

## Your Steam Web API key

[ankor.sh](https://ankor.sh) requires a [Steam Web API key](https://steamcommunity.com/dev) from
every node owner. It sends the key with every launch on the owner's nodes, including lobbies of
users they share the nodes with. Servers download workshop maps with the key, and
[ankor.sh](https://ankor.sh) creates your [login tokens](#login-tokens) with it.

- Save it under **Settings** → **Steam key**. Adding a node needs a saved key.
- Without a key, including after **Remove key**, [ankor.sh](https://ankor.sh) starts no servers on
  your nodes.
- **Change** swaps the key for the next launch. Running servers keep the old one.
- [ankor.sh](https://ankor.sh) sends the key only to your nodes. [Trust](how-it-works.md#trust)
  explains who can read it there.

## Login tokens

For each server you add, [ankor.sh](https://ankor.sh) creates a
[Game Server Login Token](requirements.md#game-server-login-tokens) on your Steam account with your
key. It deletes the token when you remove the server.

- **Settings** → **Login tokens** lists every CS2 token on your account and the slot that uses it.
  The list is read from Steam. Reset, rename or delete tokens there.
- A token Steam refuses is replaced automatically, at most once an hour per slot.
- A slot's token is never reset while the slot runs a server, because Steam drops the old token
  immediately. A refused token is replaced when the server ends.
- [ankor.sh](https://ankor.sh) keeps no tokens. It matches a slot to its token by the slot's
  [`gslt_hint`](/reference/types/#schema-ReportedSlot).
- Memos are labels only. Changing them on Steam's page changes nothing in
  [ankor.sh](https://ankor.sh).

## Share your nodes

A node belongs to the user who added it, and only that user manages it. To let other users start
lobbies on your nodes, open **Settings** → **Sharing**, enter their SteamID64 and click **Share**.

- A share covers all of your nodes, including ones you add later.
- Your nodes appear in their node table, marked with your name. The table shows the nodes' status,
  resources, running servers and stored demos.
- They cannot configure your nodes, stop their servers, delete demos, or read their logs or console.
- A lobby uses its creator's nodes, which are checked again at every launch. The lobby keeps them
  when the lobby leader changes. It loses them when you stop sharing.
