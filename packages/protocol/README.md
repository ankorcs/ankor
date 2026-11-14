# @ankorcs/protocol

The wire protocol between Ankor game-host agents and the controller that runs them: the agent
socket, the event-batch upload and the gameplay events it carries, `agent.yml`, and the paths,
limits and timings both ends share. It holds zod schemas, constants and types, and an `agent.yml`
renderer.

```sh
npm install @ankorcs/protocol
```

```ts
import { AgentToControllerMessage, PROTOCOL_VERSION } from '@ankorcs/protocol';

const message = AgentToControllerMessage.parse(JSON.parse(frame));
```

- `@ankorcs/protocol` is the wire: schemas and constants. The
  [protocol reference](https://docs.ankor.sh/reference/) documents every message from their JSDoc.
- `@ankorcs/protocol/helpers` has functions both ends share, such as slot layout checks and
  `gsltHint`. It is not part of the protocol reference.
- `@ankorcs/protocol/agent-yml` renders the `agent.yml` a service hands a node. It is the only
  module that loads `yaml`.
- `@ankorcs/protocol/session` is the controller's session on top of these schemas, described under
  [Agent session](#agent-session).
- `@ankorcs/protocol/container` is the interface between the agent and the game containers it runs:
  the environment the CS2 image and the plugin read, the local plugin socket's messages, the cfg
  files both sides exec and the handoff to a replacement agent. It is not part of the wire to the
  controller.
- `@ankorcs/protocol/node-setup.sh` is the script that installs a game node, which a controller
  serves to operators at [`SETUP_SCRIPT_PATH`](src/paths.ts).
- `@ankorcs/protocol/asyncapi.yaml` and `@ankorcs/protocol/agent-yml.schema.json` are the
  protocol's AsyncAPI 3.1 document and the JSON Schema of `agent.yml`, built from the schemas and
  their JSDoc.

## Agent session

`@ankorcs/protocol/session` pairs each request with its reply on the agent socket and types the
reply's `data` by the request. The [protocol overview](../../docs/protocol/index.md) shows how a
controller uses it, and [A minimal controller](../../docs/protocol/minimal-controller.md)
uses it step by step. Each export's JSDoc documents it.

## Changing the protocol

1. Edit the zod schemas in [`src`](src). JSDoc on a schema or key becomes its description in the
   reference; per-event semantics belong there, not in the guides.
   [Conventions](CONVENTIONS.md) says how to write it.
2. Add or update the message's example in [reference/examples.json](reference/examples.json), and a
   new message's support and group in [reference/asyncapi.ts](reference/asyncapi.ts).
3. Run `npm run build`. After `tsc`, it writes `dist/asyncapi.yaml` and `dist/agent-yml.schema.json`.
   It fails on a missing or mismatched example, an unlisted message, a link it cannot place, or a
   refinement whose JSDoc does not state its rule. Then
   [reference/dist-links.ts](reference/dist-links.ts) makes the guide links in the comments of
   `dist/` absolute, and [tsconfig.dist.json](tsconfig.dist.json) checks that `dist/` still parses
   and typechecks. The package ships `dist/` and no source. Preview the reference with `npm run docs:dev`
   from the repository root, at `/reference/`.

- `unsupportedAgentYmlKeys` stays in agent-config.ts, beside the private `agent.yml` schema it reads.
- values.ts must not import helpers.ts: the cycle through agent-socket.ts would leave its schemas
  undefined at load. What both need lives in [checks.ts](src/checks.ts).
- Make a field only as strict as the agent needs. A rule both ends depend on is an exported value
  the agent itself uses, not JSDoc prose.
- `PROTOCOL_VERSION` is the package's `major.minor`, and a test holds it to `package.json`.
  [Versioning](../../docs/protocol/versioning.md) says what a minor may change and how a receiver
  treats what a newer peer adds.
- Keep a schema a plain `z.object`, so that unknown keys are ignored, and check a new union with
  `unknownMessageType` from the receiver's side.
