---
title: Versioning
description: Which versions of the protocol, the agent, the CS2 image and a controller work together, and what each part does with a newer peer.
slug: protocol/versioning
---

Ankor has four parts that release separately: the `@ankorcs/protocol` package, the agent image,
the CS2 image and your controller. The protocol and both images share their `major.minor`, and
only their patch versions differ, so releases with the same `major.minor` work together under
[SemVer](https://semver.org/). The protocol's version decides whether two parts can talk.
[Images](../operations.md#images) explains the image tags and how a node moves between releases.

## The protocol version

Every request between the parts carries the protocol's version in the `Ankor-Protocol` HTTP
header. The value is the `major.minor` of `@ankorcs/protocol`, such as `Ankor-Protocol: 0.10`. These
requests carry it:

- the agent's [socket upgrade](index.md#1-accept-the-connection), [event uploads](events.md) and
  [demo uploads](demos.md);
- the [setup script's](node-setup.md) request for `agent.yml`;
- the plugin's [socket upgrade](/reference/plugin/#connecting) to its agent.

The receiver accepts a peer whose version has the same major as its own, at any minor. It answers
`426 Upgrade Required` to any other. This holds for `0.x` too, so a breaking change goes to the
next major there as well.

The receiver can keep the peer's version. A controller then knows which features each node has.

## What a release may change

| Change                                                        | Release   |
|---------------------------------------------------------------|-----------|
| A new optional key                                            | Minor     |
| A new request type                                            | Minor     |
| A new message or gameplay event that is not a request         | Minor     |
| A new enum value                                              | **Major** |
| A new required key, or a key that is removed or renamed       | **Major** |
| A key or value whose meaning changes                          | **Major** |

A new enum value is a major, because an older receiver rejects a value it does not know.

## Receiving from a newer peer

Between compatible versions, a peer may be on a newer minor than the receiver. The controller, the agent and the
plugin each handle what they do not know in the same way:

- A key the receiver does not know is ignored.
- A request whose `type` it does not know is answered with
  [`unsupported_request`](/reference/types/#schema-AgentErrorCode).
- Any other message or gameplay event whose `type` it does not know is skipped, and the type is
  logged once.
- A skipped gameplay event still takes its sequence number, so its server's stream has no gap.

## In a controller

1. Check `Ankor-Protocol` with `isCompatibleProtocol` from `@ankorcs/protocol/helpers`, and answer `426`
   when it fails. Keep the node's version from `protocolVersionOf`.
2. Read the agent's frames with `@ankorcs/protocol/session`. It returns `unsupported` for a message
   that this version does not have. Skip that frame instead of reporting an error.
3. In an [event upload](events.md), test each event with
   `unknownMessageType(PluginToControllerMessage, frame.event)`. Count the `seq` of an unknown event
   as received, and store or quarantine nothing of it.
4. Read an `unsupported_request` reply as a node that does not have the feature yet. Offer a
   feature only on nodes whose protocol version has it.
