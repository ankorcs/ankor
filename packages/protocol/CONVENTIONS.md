# Protocol conventions

## Writing JSDoc

The JSDoc is the only definition of what each message and field means, and the reference renders
it as a small markdown that the [docs site's README](../../docs/src/README.md) describes. The
container contract follows the same rules. The
[prose rules](../../docs/src/README.md#writing-prose) of the docs apply to every docblock.

Shape:

- A field says what the value is, then what the reader needs to use it, in one to three sentences.
- A message's first sentence says what it does or when it is sent, and stands alone, because it
  becomes the message's summary. A request then gives the reply, the refusals and the caveats, in
  that order.
- An enum whose values need explaining opens with one sentence, then a blank line and one
  `` - `value`: meaning. `` bullet per value in declaration order. Each bullet becomes that value's
  description in the reference.
- Three or more refusals go under "Refused with:", one `` - `code` when … `` bullet each.
- A docblock longer than about four lines splits into paragraphs or a list. A list item that wraps
  continues on lines indented by two spaces.
- Leave `type: z.literal(…)` discriminators undocumented, and document a field only when the
  docblock of its type does not already say what it means.
- JSON Schema cannot hold a `.refine()` or `.superRefine()` check, so its rule needs a sentence
  that opens with "Rejected when", "Rejected if" or "Rejected unless". The sentence goes in the
  docblock of the schema that carries the check, and a rule that spans fields goes on their object.
  Write it so that another implementation can check it: "Rejected unless `shots` equals the
  number of `ticks`." [refinements.ts](reference/refinements.ts) fails the build without one. A pattern, a
  length, a bound or an enum needs no such sentence, because JSON Schema holds it.

Defaults:

- A field that the controller sends may have a `.default(…)` when most launches use one value, or
  when absence plainly means off, none or no limit. Its docblock ends with "When absent, …".
- A new field gets a default that keeps the previous behaviour, so controllers that do not send
  it yet work as before.
- A schema that the plugin or the agent reports, such as `MatchRules` in `going_live`, has no
  defaults; a launch-only variant such as `LaunchRules` carries them.
- Senders build messages as the schema's input type (`MatchConfigInput`, `ServerLaunchConfigInput`,
  or `SessionRequest` with the session), which leaves out what has a default; receivers read the
  parsed type.

Fixed phrasing:

- Nullable: what the value is, then a separate sentence, "Null when …" or "Null for …".
- Boolean: "Whether …", with "When false, …" only when false means more than the opposite.
- Quantity: the unit is the head noun: "Seconds before …", "Bytes in use.", "Milliseconds
  between …". A time since the epoch is "Unix milliseconds …".
- Length limit: "up to {@link CONST} characters".
- Present tense and plain words, without "Note:", "e.g.", "i.e.", "etc." or em dashes.

References:

- Link every exported declaration a docblock names as `{@link Name}`, or `{@link Name | label}`
  for other text. A message or request links its schema with its wire type as the label:
  `{@link ControllerStartServer | start_server}`.
- Put backticks on field names (dotted for nested ones, such as `cs2.build_id`), enum values, error
  codes, files, paths, cvars, console commands and literals. A field cannot be a link target.
- Link to another docblock instead of repeating what it says, and do not write a value into prose
  that a constant holds. Link the constant, which the reference renders as its value, or as its
  values for a list of strings.
- A file imports what its JSDoc links to, so that the editor follows the link. An import that only
  JSDoc uses is `import type { Name } from './file.js';`, never the inline `import { type Name }`,
  which leaves a runtime import behind and can close an import cycle. A local ESLint rule counts the
  link as a use.
- The build fails on a link it cannot place. A target that the reference does not show needs an
  entry in `LINK_ALIASES` in [reference/asyncapi.ts](reference/asyncapi.ts), pointing at the schema
  built from it or at a guide's path on the docs site.

Emphasis: `**bold**` only for a caution the reader must not miss, such as a value that whoever runs
the node can read, and at most once in a docblock.
