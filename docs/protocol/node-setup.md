---
title: Setup endpoint
description: The endpoint that lets node-setup.sh fetch a node's agent.yml with its token.
slug: protocol/node-setup
---

**Optional.** When an operator points Ankor's [setup script](../setup-script.md) at your controller
with `--controller`, the script fetches the node's `agent.yml` from this endpoint with the node token. It
fetches the file from you because you own the node's configuration: its token, the address it
dials, and the agent and CS2 images it runs. Without the endpoint, operators
[write `agent.yml` themselves](../setup-script.md#write-agentyml-yourself).

## `POST /api/node/setup`

The script sends `Authorization: Bearer <token>` and the `Ankor-Protocol` header, with no body. It
posts to `/api/node/setup` by default. To serve the endpoint at another path, have operators pass
that path with `--setup-path`.

- Answer with any `2xx` status and the node's whole `agent.yml` as `text/yaml`. The script saves it
  unchanged. [Configuration](../configuration.md#agentyml) lists its keys. [config:slots] may be
  empty.
- The token stays valid, so answer it again on every request.
- Answer an unknown token with `401`. The script tells the operator to copy it again.
- Answer `409` with `{ "error": "node_connected" }` while the node is connected. The script says
  the node is connected from another host.
- Answer `409` with any other JSON body when you cannot read the node's stored configuration. The
  script tells the operator to ask the controller's operator.
- Answer `426` for a missing or incompatible [`Ankor-Protocol`](versioning.md).
- Answer `429` when an address sends too many requests. The script asks the operator to wait.
- Any other status stops the script with that status.
