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
