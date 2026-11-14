# Working on Ankor game nodes

This repository holds the public parts of Ankor: the agent, the plugin, the protocol and the node
docs. [README.md](README.md) says what Ankor is; [Development](DEVELOPMENT.md) has the repository
layout, setup, checks and the workstation scripts.

## Before making changes

1. Read the guide for the part you are changing.
2. Check [Development](DEVELOPMENT.md#conventions) for repository conventions. For a change to the
   Rust agent, also check its [Conventions](packages/agent/CONVENTIONS.md), and for a change to the
   protocol, the protocol's [Conventions](packages/protocol/CONVENTIONS.md).
3. Make the change and update any documentation it makes inaccurate.
4. Run the [checks](DEVELOPMENT.md#checks).
5. Commit only when asked. The working tree belongs to the user.

## Which guide to read

| Working on                                          | Read                                                                            | Source                                             |
|-----------------------------------------------------|---------------------------------------------------------------------------------|----------------------------------------------------|
| Proposing a refactor or a design change             | [Decisions](ADR.md)                                                             |                                                    |
| Planned agent, plugin or contract work              | [Roadmap](ROADMAP.md)                                                           |                                                    |
| Containers, slots, game files or host recovery      | [Agent](packages/agent/README.md), [Conventions](packages/agent/CONVENTIONS.md) | `packages/agent/crates`                            |
| The wire contract or the controller's agent session | [Protocol](packages/protocol/README.md)                                         | `packages/protocol/src`                            |
| The AsyncAPI document and the agent.yml schema      | [Protocol](packages/protocol/README.md)                                         | `packages/protocol/reference`                      |
| Match logic or native engine calls                  | [Plugin](packages/plugin/README.md)                                             | `packages/plugin/core`, `packages/plugin/GameHost` |
| The CS2 image, server cfgs and SwiftlyS2            | [CS2 image](packages/plugin/docker/README.md)                                   | `packages/plugin/docker`, `packages/plugin/cfg`    |
| Installing or operating a game host                 | [Node docs](docs/index.md)                                                      | `docs/*.md`, `packages/protocol/node-setup.sh`     |
| Checks, the node images and their releases          | [Development](DEVELOPMENT.md)                                                   | `scripts`                                          |
| The docs site and its protocol reference pages      | [Docs site](docs/src/README.md)                                                 | `docs/src`, `docs/tools`                           |

## Repository rules

- Each package builds, tests and publishes on its own. Package code and package tests read nothing
  outside their package; a package uses another only through its published entry points. The one
  exception is the agent's codegen and its check scripts, which read `packages/protocol` from the
  checkout to generate the committed Rust contract and its corpus.
- Repository-level scripts and checks may span packages.
- Controllers, [ankor.sh](https://ankor.sh) among them, consume the published packages. Nothing here imports or reads
  a controller's code.
- Committed files carry no details of a contributor's environment (hosts, addresses, local paths).
- Coding agents should not co-sign the commits they make.
- Start a commit message with its type in parentheses: `(feat)`, `(fix)`, `(nit)`, `(docs)`,
  `(refactor)`, `(test)` or `(chore)`. Follow it with one short line saying what changed, and add
  at most a few lines more when the reason is not obvious. Do not write a paragraph.
  `(fix) Game servers start without -nomaster, which blocks server queries`.
- Coding agents write, update or delete memories only once the user has approved each one in chat.

## Writing documentation

- `docs/` is for node operators and controller implementers: what the package does, setup,
  operation and the protocol. Contributor notes (conventions, decisions, caveats, code layout)
  go in the package README beside the code.
- Operators run the official agent and CS2 images. Building or modifying images belongs in
  contributor docs.
- Wire semantics live in JSDoc on the zod contract, the only definition; the protocol reference
  is generated from it at build time.

- Give each component a short overview. Use bullets for rules and numbered steps for tasks.
- Write overview pages as prose. Cut a sentence the previous one already implies.
- Put message, field, type, value, file and cvar names in backticks, including plain words such as
  `hello` when they name the message.
- Label trade-offs with the overview's **Pros** / **Cons** labels (`choice-label`).
- Include the commands or UI actions needed to do the task, with prerequisites and expected results.
- Keep useful examples and recovery caveats. Cut obvious explanations, repeated warnings and
  summaries that restate the steps above them.
- Do not turn implementation details or conversation history into documentation by default.
- Describe internal behaviour as an explanation, not instructions for the reader.
- Link to source for field lists, flags and implementation details. Keep code rationale beside the code.
- Do not enumerate values the contract defines (reasons, codes, triggers) in prose; link to the
  reference entry that lists them, so a new value cannot leave a stale list.
- Name protocol frames in guides as tags: `[agent:hello]`, `[event:map_end]`, `[upload:event_batch]`.
  The docs build links each to its reference entry and fails on a frame that does not exist.
- Link to other guides instead of repeating their contents.
- Check commands and behaviour against the source. Keep historical research clearly labelled.

## Writing prose

- Give each sentence one idea, in about 15 to 20 words, and split a sentence over 25 words. When a
  sentence chains three facts with "and", "because", "so" or "which", make it two sentences or a
  list. When three or more items each carry their own description, list them as bullets. Put the
  point first and the reason after it. "A node with no free slot refuses the launch,
  and the controller tries another." → "If the node has no free slot, it refuses the launch. The
  controller tries another node."
- Write the connective instead of a colon that stands for because, so or for example; a bare colon
  makes the reader guess the relation. Keep colons for lists, labels and literal values. "A node
  runs on one host at a time: an agent on a second host keeps retrying" → "… at a time. An agent
  on a second host …".
- Keep "that", "which" and "whose", and use one relative clause per sentence; a dropped
  relativiser reads as a garden path. "A reservation no new token follows runs out." → "If no new
  token follows, the reservation runs out."
- Name the subject again when it changed in the previous sentence or the sentence opens a
  paragraph or list item. "It revokes the old token" (the controller, two steps back) → "The controller
  revokes the old token".
- Say what happens instead of a light or figurative verb. "gets no games", "holds the node", "goes
  with its container" → "receives no launches", "keeps the node connected", "is deleted with its
  container". An established term stays once it is defined.
- Give a link its own short sentence, or put the linked phrase inline; "; see [X]" bolted on
  hides the link. `Enough CPU, memory and disk; see [Capacity]` → `Enough [CPU, memory and
  disk](#capacity)`.
- Make a table cell a clause that its column header completes. "The same." and "None." make the
  reader look up a row → "Runs the same match flow as a match with a match id."
- Name the thing instead of a "what" clause. "how much Valve changed" → "the size of the update";
  "its slots and what runs in them" → "its slots and their servers".
- Use at most one possessive per noun phrase. "the [agent:hello]'s `server_exits`" → "`server_exits`
  in the [agent:hello]".
- State what is true before what is not; use a negative only where the reader would assume the
  opposite. "Nothing undoes this afterwards" → "The damage is permanent, because …".
- Word a requirement as one: "must", "is rejected when", "only with". A requirement worded as a
  fact reads as a description. "An `external` ready-up has no deadline" → "`ready_timeout_seconds`
  must be null with an `external` ready-up".
- Drop "own", "its own" and "up to you" unless they contrast two owners in the same sentence. "the
  agent's own log" → "the agent's log".
- Prefer plain words to idioms with two readings: "immediately" or "together" for "at once", "Ankor
  web" or "your controller" for a bare "web", and a plain verb for "for good", "gone" or "says so".
- Place "only" next to the word it limits, and write it only when the source guarantees it.
- Vary rule sentences: open with "When…", "If…", an imperative or the actor, not "A/An X that…"
  every time.
- Define or link each term at its first use in reading order ([nav.ts](docs/src/nav.ts)). Node-side terms
  are defined in the Terminology block of [How it works](docs/how-it-works.md).
- Call the CS2 process a launch starts a game server ("server" once named), one launched with a
  match a match, and the Docker unit a container only where Docker matters; keep "game" for
  in-game state and fixed phrases such as game port and gameplay events.
