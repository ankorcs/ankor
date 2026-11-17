---
title: Setup endpoint
description: The endpoint that lets node-setup.sh fetch a node's agent.yml with its token.
slug: protocol/node-setup
---

The setup endpoint lets operators set up a node for your controller with one command. It is an
HTTP endpoint on your controller that answers with the node's `agent.yml`. When
an operator points Ankor's setup script at your controller with `--controller`, the script posts
the node token to this endpoint. The script saves your answer as `/srv/ankor/agent.yml` on the
host and starts the agent with it. [Setup script](../setup-script.md) covers the operator's side,
including how the token is entered and every option.

The script asks your controller for the file because your controller owns the node's
configuration: its token, the controller address that the agent connects to, and the agent and
CS2 images that the node runs. Step 3 of [A minimal controller](minimal-controller.md#3-answer-the-setup-request)
shows where the endpoint fits among the other setup steps.

The endpoint is optional. If your controller doesn't have one (yet), operators have to write
[the `agent.yml` file](../setup-script.md#write-agentyml-yourself) on the node manually before they
run the script.

The script calls the endpoint once per host, during the first setup. It fetches the file after it
has checked the host and before it installs Docker, so a wrong token stops the run early. When the
host already has `/srv/ankor/agent.yml`, a re-run keeps that file and doesn't call the endpoint.

## `POST /api/node/setup`

The script sends a `POST` with an empty body and these headers:

- `Authorization: Bearer <node token>`, with the token that the operator entered.
- `Accept: text/yaml`.
- `Ankor-Protocol`, the [protocol version](versioning.md) that the script speaks.

The script posts to `/api/node/setup` by default. If you serve the endpoint at another path,
include `--setup-path <path>` in the setup command that your controller shows operators. The path
must start with `/` and contain only letters, digits and `._~/%-`. The script doesn't follow
redirects, so serve the endpoint at the exact URL that the script requests.

A request and a successful reply look like this:

<div class="message-cards">

<div class="message-card from-agent">
<p class="message-card-head"><code>POST /api/node/setup</code><span class="message-card-direction">setup script → controller</span></p>

```http
POST /api/node/setup HTTP/1.1
Host: cs.example.com
Authorization: Bearer 3f1c…
Accept: text/yaml
Ankor-Protocol: <protocol version>
Content-Length: 0
```

</div>

<div class="message-card from-controller">
<p class="message-card-head"><code>200 OK</code><span class="message-card-direction">controller → setup script</span></p>

```http
HTTP/1.1 200 OK
Content-Type: text/yaml

token: 3f1c…
controller:
  host: cs.example.com
  tls: true
cs2_image: ghcr.io/ankorcs/ankor-cs2:0.10
agent_image: ghcr.io/ankorcs/ankor-agent:0.10
slots: []
```

</div>
</div>

`bearerNodeToken` and `isCompatibleProtocol` from `@ankorcs/protocol/helpers` read the two
headers. Step 4 of [A minimal controller](minimal-controller.md#4-accept-the-socket-and-welcome-the-node)
uses them for the socket upgrade, and the setup endpoint checks the same headers.

### Answer with the file

When the token belongs to a node, answer with any `2xx` status and the node's whole `agent.yml` as
`text/yaml`. The script saves the body unchanged. [Configuration](../configuration.md#agentyml)
lists the keys of the file.

- Put the token from the request in the file as [config:token]. The agent connects to your
  controller with this token.
- Keep the token valid after the request, and answer every request that carries it. For example,
  an operator who sets the node up again on a fresh host sends the same token again.
- Name the official images in `cs2_image` and `agent_image`. The script stops if either key is
  missing or names another image. Only the node's operator can allow other images, by turning
  [config:official_images_only] off. If your file turns it off, the script stops too.
- Answer in block-style YAML, with `cs2_image` and `agent_image` each on its own unindented line,
  such as `cs2_image: ghcr.io/ankorcs/ankor-cs2:0.10`. The setup script runs before Docker is
  installed, so it reads the two images with a line match instead of a YAML parser. A JSON or
  flow-style file passes the schema, but setup stops on it with "has no `cs2_image:` line".
- You can send an empty [config:slots] array and add the slots with [agent:set_config] once the
  node connects. Step 5 of [A minimal controller](minimal-controller.md#5-add-slots) shows that
  request.

If the script stops on your file, it doesn't save it, so the next run fetches the file again.

<div class="hint">

In TypeScript, `renderAgentYml` from `@ankorcs/protocol/agent-yml` writes the file in this layout.
It throws on input that the agent would reject.

</div>

### Refuse the request

To refuse the request, answer with one of these statuses. Put a JSON body on each refusal. The
script stops and prints a message for the operator:

- Answer `401` when you don't know the token. The script tells the operator to copy the token
  again.
- Answer `409` with `{ "error": "node_connected" }` while the node is connected to your controller.
  The script tells the operator that the node is connected from another host.
- Answer `409` with any other JSON body when you cannot read the node's stored configuration. The
  script tells the operator to ask the controller's operator.
- Answer `426` when `Ankor-Protocol` is missing or names an incompatible version.
  [Versioning](versioning.md) says which versions are compatible. The script reports that your
  controller uses an incompatible protocol version.
- Answer `429` when one address sends too many requests. The script asks the operator to wait a
  minute and run the command again.

The script tells the two `409` answers apart by the `"node_connected"` string in the body. The
answer for a connected node looks like this:

<div class="message-cards">

<div class="message-card from-controller refused">
<p class="message-card-head"><code>409 Conflict</code><span class="message-card-direction">controller → setup script</span></p>

```http
HTTP/1.1 409 Conflict
Content-Type: application/json

{ "error": "node_connected" }
```

</div>
</div>

Any other status, a redirect included, stops the script with a message that names the status. If
the script cannot reach your controller at all, it asks the operator to check that the host can
reach the controller.
