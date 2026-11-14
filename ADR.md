# Architecture decisions

Designs that were proposed or tried and turned down, with the reason that settled each. Check here
before proposing one of them again; reopen one only when its condition below holds.

## steamcmd is not persisted on the install volume

**Proposal:** keep steamcmd's own install on the cs2 volume so updates skip its self-update.

- **Why not:** the image build runs steamcmd's self-update, so an update's self-update covers only
  the steamcmd releases since the image was built. The weekly rebuild repeats it, and nodes with
  `auto_update` on pull each rebuild, so that is at most a week of changes. A steamcmd broken by an
  interrupted self-update on the volume would break every later update until a reinstall wipes the
  volume, where each update container starts from the image's clean copy today. Its state spans two
  directories, and the game's `steamclient.so` comes from that self-update.
- **Reopen if:** the self-update at update time still costs noticeable downtime with a week-old
  copy, or the steamcmd the build copies from steamcmd/steamcmd's image can no longer update itself.

## `check_cs2_build` gets no reply

**Proposal:** make `check_cs2_build` a request with an `id`, answered with `latest_build_id` or
refused when it comes within `CS2_BUILD_CHECK_INTERVAL_MS` of the last one or Steam cannot be read.

- **Why not:** what the controller acts on is state, not the check: a changed build arrives as
  `node_status`, and `cs2.latest_build_id` is always in the last report. A reply would only add
  "nothing changed" or "dropped".
- **Reopen if:** a controller needs to show the outcome of one check, or that Steam cannot be read.

## `slot_status` carries no player count

**Proposal:** add `player_count` and `empty_since` to `slot_status`, from the plugin poll the agent
already makes every 30 s for the idle clock.

- **Why not:** occupancy changes with every join and leave, and each change would be a frame on the
  `state` budget. A match reports its players in events, and `get_server_status` answers on demand.
- **Reopen if:** a controller needs live occupancy of servers without a match across many nodes.

## Console output is not tied to its command

**Proposal:** let the `console_command` reply, or a marker the plugin prints, mark where the
command's output starts and ends in the server's log.

- **Why not:** CS2's console has no command boundary: output is lines like any other, and a marker
  rests on the `echo` the roadmap may remove. The output is for people reading the server's log.
- **Reopen if:** a controller parses command output instead of showing it.

## `hello.slot_status` keeps its name

**Proposal:** rename `hello.slot_status` to `slot_statuses`, plural like `server_exits` and `demos`.

- **Why not:** each entry is a `slot_status` frame without its `type`, and the name says so. A
  rename only costs every controller a change.
- **Reopen if:** `hello` changes shape for another reason.

## A `server_exited` is forgotten once written

**Proposal:** keep a sent `server_exited` until the next pong shows the controller read it, and
carry one still unconfirmed at a reconnect in that `hello`'s `server_exits`. Today the agent drops
it from memory and the spool once the socket takes the write, so a link that dies before the
controller reads it loses it.

- **Why not:** a lost exit costs only that allocation's `reason` and `exit_code`. The next `hello`
  shows its slot idle, the match result arrives in events, which have their own acknowledgement,
  and the controller must already handle a missing exit: one that ends while the agent is not
  running is missing too. Tracking pongs would add state and timing for a rare overlap of an exit
  and a dying link.
- **Reopen if:** a controller acts on the exit reason, such as relaunching after a crash, and sees
  unknown ends in practice.

## A dropped `log_batch` leaves no trace

**Proposal:** number each log source's lines, as `log_batch.first_seq`, so the controller sees a gap
where the agent dropped live lines over its log budget, and asks for a replay.

- **Why not:** live lines exceed the budget only in a log storm, about 1,000 lines a second from
  one source past the burst, or just after replays have spent it. What is lost is a few lines in a
  view for people; no decision rests on them, and the lines stay in the agent's ring, so the next
  replay shows them. Numbering adds a field, a counter per source and gap handling in the
  controller.
- **Reopen if:** the controller acts on log lines, or watchers report missing lines in practice.

## `shutdown` and its close both name the signal

**Proposal:** drop the `shutdown` frame and rely on the 1000 close, or keep the frame and close
without a reason, since both carry the signal that stopped the agent.

- **Why not:** each reaches a different reader. The frame arrives in order with the last reports,
  where the controller handles messages, and also reaches a stack whose WebSocket library or proxy
  hides close reasons. The close reason serves connection logs and proxies. Both cost one small
  frame per stop.
- **Reopen if:** a controller finds the frame and the close disagree.

## Demo frames name no allocation

**Proposal:** add `allocation_id` to `DemoFile`, `demo_removed`, `demo_progress` and `delete_demo`,
and key a demo by allocation and file name, as its directory on the node does.

- **Why not:** the controller gets a demo's allocation with its upload, `Ankor-Allocation-Id`, which
  is when it stores the demo, and the match is in the file name. The agent's own names hold the
  recording's start to the second and the match, so they do not collide in a slot; two only share a
  key when a `tv_record` with a fixed name runs through `console_command` in two launches of one
  slot.
- **Reopen if:** a controller needs to tie demos the node keeps, uploads off, to their server.

## Coaching

**Proposal:** let a match seat a coach per team, as CS:GO and MatchZy do.

- **Why not:** CS2 gives a coach no working place. What was tried in October 2026:

  - A coach on the spectator team with `m_iCoachingTeam` set to its team's side, as in CS:GO, is an
    ordinary free spectator in CS2: the engine does not act on the field.
  - `sv_coaching_enabled` is in `libserver.so` with its CS:GO help text, but it is not a usable
    convar in CS2.
  - `mp_spectators_max` tops out at 2, and coaches on the spectator team share those places with
    listed spectators, so two coaches per team cannot coexist with two spectators.
  - `mp_forcecamera 1` limits dead team members, not the spectator team. That suggested keeping
    coaches on their team as permanently dead players.
  - MatchZy does that by letting coaches spawn, freezing, stripping and teleporting them, then
    `CommitSuicide` just before freeze time ends (suicide penalty and death freeze-cam zeroed,
    C4 handed on, death notice hidden). Rejected here: every round produces a death.
  - Tried instead: hooking the controller's virtual `Respawn` (`CCSPlayerController::Respawn`,
    vtable index from SwiftlyS2's game data) and skipping it for coaches. The hook fired on the
    warmup team join and at going live, yet the coach still spawned, lying down, and after freeze
    time no player could move. The engine spawns through paths SwiftlyS2's game data does not
    cover.
  - What would remain: a hook on the pawn's own spawn with a signature or vtable index we find and
    maintain across CS2 updates, or coaches back on the spectator team with the plugin forcing
    their camera onto their team (blocking `spec_mode`/`spec_next`/`spec_player`, correcting the
    observer target) inside the two-place cap.
- **Reopen if:** a maintained hook on the pawn's spawn exists, or forcing a spectator's camera onto
  one team is acceptable within the two-place spectator cap.

## The agent does not match JavaScript's whitespace rules

**Proposal:** make the Rust agent treat U+FEFF, U+0085 and U+212A (the Kelvin sign) as JavaScript
does, so its validation agrees with zod in every case: a JavaScript-style trim, U+FEFF in
`hostname_of`, and `\s` and case-insensitive matching in generated patterns translated to
JavaScript's rules.

- **Why not:** Rust's `trim`, `\s` and case folding differ from JavaScript's only on these three
  code points, which reach the agent only through a hand-edited `agent.yml` or a controller that
  skips zod. Most cases change which `controller.host` or URL values pass, so the operator sees a refusal
  for input that looks right. The one that changes behaviour is a `console_command` of only
  U+FEFF, which zod refuses and the agent runs. The server console receives an invisible command,
  which does nothing.
- **Reopen if:** an operator hits it, a case appears that runs something zod refuses with an
  effect, or the corpus starts checking the contract's verdicts on these code points.

## Port refusals do not format numbers as JavaScript does

**Proposal:** write the numbers in the agent's port refusals (`refine.rs`) as JavaScript would, so
`1e21` and `-0` read the same in both.

- **Why not:** only values that are invalid as ports anyway reach that text, and the refusal's code
  and path stay the same. Only the wording of the detail differs.
- **Reopen if:** a controller starts matching on the detail text of a refusal.

## A demo upload does not abort its connection task

**Proposal:** abort the task that drives a demo upload's HTTP connection when the upload ends or is
aborted, so a TLS connection cannot outlive it.

- **Why not:** hyper closes the connection itself when the request is dropped. Tests that aborted
  an upload while it waited for an answer, and while its body was stalled, passed without the
  change.
- **Reopen if:** an aborted upload's connection is seen still open, or a hyper upgrade changes how a
  dropped request ends its connection.

## The agent does not vendor the protocol

**Tried:** a copy of `@ankorcs/protocol` in the agent package, at first the packed tarball and then
the plain files codegen reads, so that codegen read nothing outside the agent package.

- **Why not:** the copy duplicated the protocol beside the generated Rust that is committed anyway,
  and every contract change added a new 200–270 KB tarball to the history. Codegen and the corpus
  now read `packages/protocol` from the checkout, the one exception to the package rule in
  AGENTS.md, and `protocol-sync.sh` checks the committed Rust against it.
- **Reopen if:** the agent moves to a repository of its own, where it would read the published
  package instead.

## The generated Rust contract is committed

**Proposal:** generate the Rust contract during the build, in `build.rs` or an image stage, and
commit none of it.

- **Why not:** every `cargo build`, test and lint would then need Node and a built protocol first,
  and a contract change would no longer show as a diff of what the agent compiles. Committed, the
  agent and its image build with cargo alone, and the CI check fails while it is out of date.
- **Reopen if:** keeping it in step costs more than the Node step would, such as frequent conflicts
  in the generated files.

## The Rust contract is generated from zod, not from JSON Schema

**Proposal:** make JSON Schema, an AsyncAPI generator, a neutral IDL such as TypeSpec or Protobuf, or
hand-written serde with a validation crate the source of the Rust types and validators.

- **Why not:** in November 2026 the zod contract was converted with `z.toJSONSchema()` and the corpus
  replayed against it. JSON Schema matched zod on 97.3% of 36,824 cases: it drops the refinements,
  formats such as URLs disagree, and defaults are annotations only. Each alternative would still
  need the hand-written refinements and the corpus, behind one more translation. The generator reads
  zod's internals, which the pinned zod version and the corpus guard.
- **Reopen if:** zod changes its internals beyond what the generator can follow, or a standard
  format expresses the refinements.

## A receiver does not accept unknown enum values

**Proposal:** let receivers read an enum value they do not know as `unknown`, or let a sender send a
new value or message only to a peer whose minor has it.

- **Why not:** reading unknown values needs an open enum in every schema, in the generated Rust and
  in the plugin, and a value that tells the receiver what to do cannot be guessed. Tracking each
  peer's minor in every sender is easy to get wrong. A new enum value is a major change instead, as
  [Versioning](docs/protocol/versioning.md) states, while new keys, requests and messages stay
  minor, because receivers ignore or skip what they do not know.
- **Reopen if:** additions of enum values become frequent enough that each major costs too much.
