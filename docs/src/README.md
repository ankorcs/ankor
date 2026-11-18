# The docs site

The site is published at the root of <https://docs.ankor.sh> ([site.ts](site.ts)); controllers link
to it there. [check-docs.yml](../../.github/workflows/check-docs.yml) builds it for a pull request.
[deploy-docs.yml](../../.github/workflows/deploy-docs.yml) builds and deploys it to GitHub Pages for
a push to `master`. Each runs only when the change touches the docs site's paths
in [ci-changes.sh](../../scripts/ci-changes.sh). Those include `docs/`, the root install and the
contract that the protocol reference is rendered from. Preview it at <http://localhost:4321/> with
`docker compose run --rm -p 127.0.0.1:4321:4321 toolchain npm run docs:dev`, which rebuilds
`@ankorcs/protocol` when its `src/` or `reference/` changes. `npm run docs:build` writes
`docs/dist`, indexes it for search with Pagefind, and fails on a link to a missing file, page or
anchor; search works in the built site (`npm run docs:preview`). The build also writes
`sitemap.xml`, `llms.txt`, `llms-full.txt` and a Markdown copy of each guide (`/<page>.md`) from
[llms.ts](llms.ts).

- Pages link to each other and to source by relative path, so they read the same on GitHub. The
  site turns these into page and repository links. Links from the site root, such as
  `/reference/node/` in markdown or a card's HTML, get the site's base added.
- Astro caches rendered markdown in `docs/node_modules/.astro`; delete it, and restart the dev
  server, after changing a markdown plugin or the glossary.
- Terms that guides use without explaining them are in [glossary.ts](glossary.ts). The
  [glossaryTerms.ts](markdown/glossaryTerms.ts) plugin marks the first use of each in an `h2`
  section with a dotted underline and the definition as a tooltip. It skips headings, links, code,
  table headers, and raw HTML whose class names a card or diagram (`part`, `parts`, `message-card`,
  `diagram`). A new card class that holds no running prose belongs in its `NOT_PROSE` pattern.
- Diagrams are HTML in the markdown, built from the `diagram-*` blocks in
  [global.css](styles/global.css) so they share text sizes and colours: a `figure.diagram` with an
  `aria-label`, `diagram-card`s for things, a dashed `diagram-place` for where they run, a
  `diagram-link` between them (pointing up at the side that opens it, or `down` for the next step
  of a flow), `diagram-seq` lanes for an exchange, `diagram-chip`s for numbered items and a `diagram-list` for an
  ordered list inside a card. A `diagram-seq three` holds three sides, its messages placed with `between-1-2`,
  `between-2-3` or `between-1-3`, as in [Update the agent](../operations.md#update-the-agent).
  Links and messages take `from-agent`, `from-controller`, `refused`, `quiet` or `implied`, as the
  message cards do; see the [overview](../index.md) and [Event uploads](../protocol/events.md).
- An aside to the steps, such as the helper that does a step in TypeScript, is a `hint`: a `div`
  with a blank line inside each edge, so the markdown in it renders. Keep it to a sentence or two.
  A `callout` is for something the reader must not miss, and a `callout warning` for something
  that breaks when done.
- Message examples in the guides are cards: a `message-card` `div` with `from-agent` or `from-controller`
  (plus `refused` for a refusal or failure), a head with the message's reference name linked to its
  entry or an HTTP request or status line, a `message-card-direction` label, and a short `json` or
  `http` block with `…` for the rest. `message-cards` puts two side by side; see [the protocol
  overview](../protocol/index.md).
- The protocol reference under `/reference/` is rendered from the AsyncAPI 3.1 document that
  `@ankorcs/protocol` builds and exports as `@ankorcs/protocol/asyncapi.yaml`: one page per message
  group, plus shared types. The site reads it and `@ankorcs/protocol/agent-yml.schema.json` through
  those exports, and serves the document at `/asyncapi.yaml`. The protocol's
  [Changing the protocol](../../packages/protocol/README.md#changing-the-protocol) describes the
  build and its checks.
- The document's links stand on their own outside the site. A link to a schema is
  `#/components/schemas/Name`, and a link to a guide is its address under <https://docs.ankor.sh>.
  [schema.ts](reference/schema.ts) turns the first into the schema's reference entry and the second
  into the guide's path on this site, and the build fails on a schema the reference does not show.
- JSDoc in the contract renders as a small markdown, in
  [schema.ts](reference/schema.ts). Wrapped lines run on and a blank line starts a paragraph.
  `- ` and `1. ` lines start list items, and a following line that starts no item continues the
  one before it. A backslash at the end of a line breaks the line there. Inline, it renders
  `**bold**`, code spans and links. A code span naming a schema links to it. In an enum's
  docblock, a `` - `value`: meaning `` bullet for one of its values becomes that value's description
  in the reference's list of values, and leaves the description.
- `{@link Name}` and `{@link Name | label}` link to the reference entry of `Name`, or render a
  constant's value. The protocol's [Writing JSDoc](../../packages/protocol/CONVENTIONS.md#writing-jsdoc)
  describes them.
- An `@internal` tag in a key's docblock leaves that key out of the reference, and its examples
  leave it out too. Use it for development options such as `debug_capture`.
- Values guides would copy from the contract are tags, from
  [contractTags.ts](markdown/contractTags.ts): `[limit:MAX_SLOTS]` and its `-size`, `-time`,
  `-burst` and `-refill` forms become a constant's value, and a `[table:agent-yml]` paragraph
  becomes the `agent.yml` key table, built from `@ankorcs/protocol/agent-yml.schema.json`.
  A tag naming no constant fails the build.

## Writing documentation

- `docs/` is for node operators and controller implementers: what the package does, setup,
  operation and the protocol. Contributor notes (conventions, decisions, caveats, code layout)
  go in the package README beside the code.
- Operators run the official agent and CS2 images. Building or modifying images belongs in
  contributor docs.
- Wire semantics live in JSDoc on the zod contract, the only definition; the protocol reference
  is generated from it at build time.
- Use bullets for rules, numbered steps for tasks and prose for overview pages.
- Put message, field, type, value, file and cvar names in backticks, including plain words such as
  `hello` when they name the message.
- Name protocol frames in guides as tags: `[agent:hello]`, `[event:map_end]`, `[upload:event_batch]`.
  Write a contract value as its tag, such as `[limit:STOP_TIMEOUT_SECONDS]` or
  `[config:controller.socket_path]`, never as a copied number. The docs build links or fills in
  each tag and fails on one that does not exist. "the reservation runs out" → "the reservation
  runs out after `[limit:SLOT_RESERVATION_SECONDS]` seconds".
- Choose "a" or "an" by the name a tag renders as, not by the tag's prefix. `[agent:start_server]`
  renders as `start_server`, so write "a `[agent:start_server]`", and "an `[agent:update_cs2]`".
- A `[limit-time:…]` tag renders a single unit with its article ("a second", "an hour") and more
  than one as a count ("30 seconds"). Write it where both forms read correctly, such as after
  "within", "after" or "for", or before "apart". Don't put "every", "each" or an article before it.
  "once every `[limit-time:DEMO_PROGRESS_INTERVAL_MS]`" → "at least
  `[limit-time:DEMO_PROGRESS_INTERVAL_MS]` apart".
- Do not enumerate values the contract defines (reasons, codes, triggers) in prose; link to the
  reference entry that lists them, so a new value cannot leave a stale list.
- Show a request and its reply together as message cards (`message-cards`, `from-controller`,
  `from-agent`), as in step 7 of [A minimal controller](../protocol/minimal-controller.md).
- Write a code example that uses `@ankorcs/protocol` as a file in [examples](../examples), and put
  its tag, such as `[example:controller/stop]`, on a line of its own where the page shows it. The
  page shows each part between `// #region example` and `// #endregion`; the rest of the file only
  makes it compile. `npm run typecheck` type-checks the examples as their own TypeScript project
  against the protocol package, and ESLint lints them, so an example that the contract no longer
  allows fails the checks. Declare the controller's own functions, which the examples mark with a
  "Your code" comment, in [your-code.d.ts](../examples/controller/your-code.d.ts). `npm run
  docs:dev` does not re-render a page when only its example changes; restart it to see the edit.
- Label trade-offs with the overview's **Pros** / **Cons** labels (`choice-label`).
- Link to other guides instead of repeating their contents, and tell the reader what the linked
  page covers. "[Install a node] has the step." → "The operator sets up the host by following
  [Install a node]. That page covers everything that happens on the host, including the packages
  each distribution needs, opening the game ports and testing on your LAN."
- Link to source or the reference for field lists, flags and implementation details. Keep code
  rationale beside the code.
- Do not turn implementation details or conversation history into documentation by default.
  Describe internal behaviour as an explanation, not as instructions for the reader.
- Cut repeated warnings and summaries that restate the steps above them. Keep every explanation
  that a newcomer needs, every useful example and every recovery caveat.
- State each fact once per page. When two sections repeat a fact, keep it in the section that has a
  diagram or table, and cut the prose copy. Keep a copy elsewhere only when a requirement there
  depends on it.
- Check commands and behaviour against the source. Never invent a figure; a duration or size goes
  in only when the source or a maintainer states it. Keep historical research clearly labelled.
- Keep headings and their anchors, because other pages and the reference link to them. Before you
  rename a heading, search the repository for its anchor and update every link.
- Wrap prose at about 100 columns.

## Writing for the reader

The reader knows their own stack (Linux, Docker, HTTP, websockets, programming language) but not Ankor,
the protocol or Steam server administration. Write so that they can follow each step without reading the source.

- Open a page with its subject: what the reader will build or achieve, or what the thing is.
  "You install a node with one command on the host." → "These steps turn a Linux host into a game
  node that your controller can launch CS2 servers on."
- Don't open with "This page is for…" or "This page explains…". Name the audience only when
  readers would otherwise read the wrong page, and point elsewhere only when another page fits
  them better: "If you use ankor.sh, follow the quick start instead."
- Introduce a concept before its rules when the reader needs it to follow them. A list of rules
  about something new opens with a short paragraph that says what the thing is and why the rules
  exist. A short section about a familiar thing needs no intro. "Launches and stops race with the node's reports:" →
  "Your controller and the node work at the same time, so the messages for one server do not
  always arrive in the order you expect:". Before a list of settings, say what the list is:
  "Setup stops on these settings:".
- Define a term in a short sentence at its first use. "Add the node's slots" → "Add the node's
  slots. A slot is room for one game server."; "A node keeps several logs, called sources. A
  source is the agent's log, the output of one CS2 install or update, or the output of one
  server."
- Address the reader as "you", and use an imperative when the reader acts. Contractions are fine.
  "Without the endpoint, the operator writes this file" → "If you don't have a setup endpoint on
  your controller (yet), you'll have to write the `agent.yml` file on the node manually".
- Say where each step happens, and open the step with the place: "On the host, …", "In the
  controller, …", "Perform these steps in Proxmox, …". "Add the node in the controller" → "In the
  controller, add the node."
- Make the actor the subject. Replace a sentence whose subject is a thing that only appears or
  happens with one that names who does it: setup, the agent, the node, your controller or you.
  "Docker comes from the community repository" → "Setup installs Docker from the community
  repository"; "A batch arrives again when its receipt is lost" → "The node can send the same
  event batch twice."
- Put a condition first, as an "If" or "When" clause, and write a permission as "You can".
  "Remove Docker from the snap store first" → "If Docker is installed from the snap store, remove
  it first"; "`slots` may be empty" → "You can send an empty `slots` array and populate it later."
- Spell out the situation that a compressed rule leaves the reader to reconstruct, with an example
  when it helps. "A reply may never come." → "The reply to a request may never come, for example
  when the connection drops."; "A node may reconnect mid-launch." → "The node may disconnect and
  reconnect while a launch is in progress."
- Give the purpose of a step and the consequence of skipping a rule. "Open each slot's game port"
  → "… so that players can reach the server"; "Wait for its [agent:slot_status] instead of
  retrying on a different node" → "…, or you may inadvertently launch the allocation twice".
- Tell the reader what to do with a fact, and which field shows it. "It can repeat what the
  `hello` already told you." → "You can receive the same exit twice … Record each exit by its
  `allocation_id` and ignore one you already have."; "reports whether Steam accepted it" → "The
  slot's `gslt_state` in [agent:slot_status] tells you whether Steam accepted it."
- Say what a command does while it runs and how the reader sees that it worked. Put each other
  outcome in a sub-bullet under the step, with the next action. "Setup logs `agent connected.`" →
  "Setup then installs Docker if needed and starts the agent. When the agent reaches the
  controller, setup logs `agent connected.`", with a sub-bullet for each failure.
- Explain the reason a newcomer would miss, in a clause or a sentence. "Its session runs the
  exchanges for you" → "…contains helper functions that run the exchange for you, since a
  WebSocket does not have the normal request-response cycle an HTTP request does."; "Alpine also
  needs `bash`, because the setup script is a bash script".
- Give concrete expectations and the practical options: how long a step usually takes, how to test
  on a LAN, if applicable, the manual alternative to an automated path and the recommended choice.
  "The install can take tens of minutes." → "The installation usually takes about 30-40 minutes"; "Pick a new
  `allocation_id`" → "Pick a new `allocation_id` (prefer UUIDv7)"; "You may also manually create
  and assign your GSLTs".
- Introduce every code block with a sentence or label that says what it is or does. "With the
  `ws` package:" → "With the `ws` package, it would look like this:"; a bare `apt-get` block →
  "Install the prerequisites:" above it.
- Comment example programs at each step, in full sentences that say what the branch does and why.
  "// Unknown token; just reply 401 and close."; "// When the connection is established, the
  node will always start by sending a hello. You must reply to it promptly, or the node will
  disconnect."

## Writing prose

- Give each sentence one main idea, in about 15 to 25 words, and split a sentence over about 30
  words. Keep the words that say who acts or why. Two facts joined by "because" or "so" make one
  sentence; when a sentence chains three facts, make it two sentences or a list. "Alpine needs a
  VM, because in LXC OpenRC cannot hand the cgroup controllers to Docker." → "Alpine must run in
  a VM. In an LXC container, OpenRC cannot hand the cgroup controllers to Docker."
- When three or more items each carry their own description, list them as bullets.
- Use plain, literal verbs instead of jargon or figures of speech. An established term stays once
  it is defined. "dials" → "connects to"; "redial" → "reconnect"; "Fan them out to viewers" →
  "Forward them over the bus to viewers"; "ends the server" → "shuts down the server"; "grows past
  what one process can hold" → "can serve more nodes and viewers than one process can".
- Name the specific thing instead of a generic noun or a bare "it", and finish a half-named
  reference. "the tokens" → "the GSLTs"; "Turn it off" → "Turn the sandbox off"; "Batches are
  plain HTTP" → "Event batches are plain HTTP requests"; "explains the reports" → "explains the
  reports sent to your controller during an update".
- Name the subject again when it changed in the previous sentence or the sentence opens a
  paragraph or list item. "Fedora keeps Podman and loses only `podman-docker`" → "On Fedora,
  setup keeps Podman and removes only `podman-docker`".
- Write relative clauses out with "that", "which" or "whose", and use one per sentence. "the
  requests waiting for a reply" → "the requests that are waiting for a reply"; "Debian
  derivatives with their own codename" → "Debian derivatives that have their own codename".
- Word a requirement as one: "must", "is rejected when", "only with". "has to happen once" →
  "must happen exactly once".
- Write the connective instead of a colon that stands for because, so or for example; a bare colon
  makes the reader guess the relation. Keep colons for lists, labels, code-block lead-ins and
  literal values. "A node runs on one host at a time: an agent on a second host keeps retrying" →
  "… at a time. An agent on a second host …".
- Give a link its own short sentence, or put the linked phrase inline; "; see [X]" bolted on
  hides the link. "[Write `agent.yml` yourself] has the steps." → "you'll have to write [the
  `agent.yml` file] on the node manually".
- Use at most one possessive per noun phrase. "the container's CPUs" → "the CPUs and memory of
  the LXC container".
- State what is true before what is not; use a negative only where the reader would assume the
  opposite. "Nothing undoes this afterwards" → "The damage is permanent, because …".
- Make a table cell a clause that its column header completes. "The same." → "Runs the same
  match flow as a match with a match id."
- Drop "own", "its own" and "up to you" unless they contrast two owners. "the agent's own log" →
  "the agent's log"; "If you're building your own controller" (against ankor.sh) stays.
- Prefer plain words to idioms with two readings: "right away" or "together" for "at once", "Ankor
  web" or "your controller" for a bare "web", and a plain verb for "for good", "gone" or "says so".
- Place "only" next to the word it limits, and write it only when the source guarantees it.
- Vary rule sentences: open with "When…", "If…", an imperative or the actor, not "A/An X that…"
  every time.
- Define or link each term at its first use in reading order ([nav.ts](nav.ts)). Node-side
  terms are defined in the Terminology block of [How it works](../how-it-works.md).
- Call the CS2 process a launch starts a game server ("server" once named), one launched with a
  match a match, and the Docker unit a container only where Docker matters; keep "game" for
  in-game state and fixed phrases such as game port and gameplay events.
