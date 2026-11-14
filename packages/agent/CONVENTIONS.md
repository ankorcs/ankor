# Conventions

How to change the Rust agent: the steps before a change, how the agent runs with the rules that
keep it so, and the conventions its code and documentation follow. The repository's rules, commits
among them, apply as well, and the root [AGENTS.md](../../AGENTS.md) lists them.
[README.md](README.md) says what the package holds.

## Before a change

1. Read the module you change and its tests. A component's code comments are its documentation.
2. If the change alters behaviour the controller, a plugin, Docker or the node's files can see,
   check it against the contract in `@ankorcs/protocol` and the [node docs](../../docs/index.md),
   and change them with it.
3. Run `scripts/check.sh`. Run the [conformance suite](crates/conformance/README.md) when behaviour
   changed. Review the diff against the recorded transcripts, and record them again only for a change you meant.

## How the agent runs

Its threads and tasks, how they talk, what they share and how they stop, with the rules that keep
each part this way. Each component's code comments give the details.

### Threads

The agent runs on **one thread**, because it mostly waits on the network and Docker. Its
correctness does not depend on that. Each check and the change it allows happen in one hold of a
lock, so they would stay together on a runtime with more threads:

- A launch checks the update guard, picks its slot, claims its allocation and marks the slot
  starting in one hold of the slot lock. An update claims the guard first and then checks for busy
  slots, so one of the two always sees the other.
- The config store runs one change of `agent.yml` at a time. Each change reads the config, writes
  the file and applies the change in memory before the next one reads. `set_config` also checks
  and applies the slot layout in that change, inside the slot lock.
- A slot's status and exit frames are queued in the same hold as the change behind them, so they
  leave in the order of the changes and before the reply to the request that caused them. The
  exception is `set_config`, whose slot statuses go out after its reply, so the reply names the new
  layout first.
- A decision taken in one hold and acted on in another names what it decided about. An idle, quit,
  config or start-timeout stop names the server it found, and leaves the slot alone if another
  server holds it by then.

Rules:

- Keep the runtime on one thread, and write code that stays correct on several. Never rely on the
  absence of an `.await` between a check and the change it allows; take both in one hold, or check
  again when acting. A test of such a race runs with `flavor = "multi_thread"`.
- Give a named thread only to a serial worker that lives as long as the agent, as the spool and
  the console do. Run a one-off blocking call on tokio's blocking pool.

Work that would block runs elsewhere: every SQLite call on the `event-spool` thread, console
writes on the `console` thread, and tar unpacking, DNS lookups, `statvfs` and `tokio::fs` calls on tokio's blocking pool. A call that still
blocks the main thread stops everything, as a synced write of a node file does.
These small synced writes stay on the main thread on purpose, because each runs inside a
synchronous call that holds a lock or the config change hold, and must land before it returns:

- `agent.yml`, through `set_config` and the config store.
- The refused tokens, through `RefusedGslts::add`.
- The failed agent update, through `AgentUpdateState::record_failure`.

`agent.yml` and the refused tokens are also read once with a synchronous call at boot, before any
launch.

The `event-spool` thread logs through the agent's logger. Its lines take the node log's lock from
that thread, as lines from the main thread do.

Console lines are written on the `console` thread, so a console that dockerd does not read never
blocks the caller. Write to the console through `Logger`. `main` writes directly only before the
logger starts or once its queue is flushed.

### Tasks

- **Long-lived loops** start at boot: the controller connection driver, the plugin relay, the event
  uploader, the slot event feed, the node report, the demo scan, the game check, the timers of the
  auto-updates, the GSLT keep-alive, the node log and the resource sampler. Each follows a child of
  the agent's root cancellation token, except the controller connection and the event pipeline,
  which shutdown closes together. The agent's own loops run on its task tracker, which shutdown
  waits for. A component's loop ends when its token is cancelled and is not joined.
- **Short tasks** start per event and end by themselves: one per request from the controller, one
  per launch, stops and exit watches, reports, Docker output readers, and the auto-update polls.
  They are not joined. The release build aborts on a panic, so a task that fails ends the process,
  and the restarted agent adopts its game servers again.
- **Work that callers share** runs in a task of its own: a build read, a GSLT check, a demo scan
  and an image poll.

Rules:

- Give a long-lived component a child of the root `CancellationToken` when it is built, and end its
  loop when the token is cancelled, as `stop.run_until_cancelled(ticks.tick())` does. Put the loops `agent.rs` starts on the task tracker. Return
  early from a method that starts work once the token is cancelled.
- Only the controller connection and the event pipeline close through `shutdown` and `close`. Add no
  other `stop` or `dispose` methods.
- Spawn work that callers share and keep a `Shared` future of it in the component's state, so
  callers join the run in progress and it finishes even when every caller stops waiting.
- Spawn short tasks untracked.

### Queues

Each queue is bounded, or the table below says what keeps it small:

| Queue                          | Kept small by                                                                                                                                                                       |
|--------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Controller connection commands | A byte cap on log frames, past which new ones are dropped, so a slow controller costs log lines rather than memory. Frames wait only while a socket is open (`controller_connection`). |
| Frames to one plugin           | Only commands and acks go this way.                                                                                                                                                 |
| Slot events, node changes      | Their consumers handle each one at once, without waiting.                                                                                                                           |

The bounded ones are the relay's queue per game (over the limit, the game replays from its last
acknowledged frame), the spool's waiting jobs (over the limit, a call is refused as busy), the
console's lines (when full, a line is dropped and counted) and the node log (rings, a byte
budget per frame and a token bucket).

Rules:

- Use `mpsc` for ordered work, `oneshot` for one reply and `watch` for state others follow.
- A new unbounded channel needs a line in the table above saying what keeps it small.

### Shared state

Shared state sits behind locks that are held briefly and never across an `.await`. Locks that nest
are always taken in one order, so they cannot deadlock:

- The config store's change lock, then the slot lock, then the config in memory.
- The slot lock, then the guard, the config in memory, the reporters, the disk-space state, the
  overlay's references (as a settled slot gives its overlay back) or the spool's job queue and the clock of its waiting calls. Callbacks into other components run after the slot lock is released, apart
  from the change report, launch admission with its spool allocation query, and `set_config`'s
  write of `agent.yml`, which belong to the same hold.
- The maintenance guard's state, then its `watch`, so two changes publish in the order they
  happened.
- Any lock, then the node log, through logging.
- In the relay, its connection map or commit queue, then one connection.
- A change reporter's state, then its sender. The controller connection's sender takes no lock,
  only an atomic and its command queue.
- The spool client's job sender, then the clock of its waiting calls, as a job it could not queue
  is dropped.

Rules:

- Keep a component's state in one `std::sync::Mutex<State>`, or one per entity it keeps, such as
  the relay's lock per connection. Take each through `crate::sync::lock`. Hold it briefly, and
  never across an `.await`. Clippy enforces the helper and the `.await` rule.
- Use a `Mutex<()>` turnstile only for work that changes something outside the process (Docker,
  Steam, SQLite, the socket, `agent.yml`). Use tokio's for async work and std's for synchronous
  work. Data inside the process goes in the mutex that guards it.
- Do not call into another component while holding a lock, apart from logging and the slot lock's
  exceptions above. A new exception goes into that list.
- Collect what to send or call under the lock, release it, then act.
- Take nested locks in the order above. A new nesting goes into that list.
- Use atomics only for flags, counters and values set once that are read without a lock, always
  with `Ordering::SeqCst`. A counter may number reads or sockets so that only the newest acts. When
  every use is already under a lock, keep the value in that lock's state instead.

| State                              | Owner                         | Changed by                                                                                                      |
|------------------------------------|-------------------------------|-----------------------------------------------------------------------------------------------------------------|
| `agent.yml` and its copy in memory | Config store                  | `set_config`, image switches and agent updates, one at a time. Every other part reads through a `ConfigReader`. |
| Slots, launches in flight          | Slot manager                  | Launches, stops, exits, `set_config`.                                                                           |
| The maintenance guard              | Install                       | Updates and image switches claim it; a launch checks it while it picks its slot.                                |
| Relay connections and queues       | Relay                         | Plugin sockets, commits.                                                                                        |
| Upload state                       | Event uploader, demo uploader | Their loops, the controller's answers, `delete_demo`.                                                           |

Boot builds the parts in the order their links need. The controller connection's handle exists first
and refuses every frame but a hello or a reply until the controller welcomes a socket. The plugin
relay binds its port before the slots exist, so a launch can name it, and accepts plugins only once
the slots are built.

### Concurrency primitives

| Primitive                                       | Used for                                                                                                              | Where                                                                                                                                                                            |
|-------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `std::sync::Mutex`, through `crate::sync::lock` | State, held briefly                                                                                                   | Every component with state                                                                                                                                                       |
| `tokio::sync::Mutex<()>` turnstile              | One piece of async work on something outside the process at a time                                                    | The relay's commits (SQLite, in order), the demo reports (the controller socket, in order), overlay extraction and cleanup (Docker and the overlay folders), GSLT logons (Steam) |
| `std::sync::Mutex<()>` turnstile                | One synchronous change of `agent.yml` at a time                                                                       | `ConfigStore`                                                                                                                                                                    |
| Atomics, `SeqCst`                               | Flags, counters and values set once that are read without a lock; numbers that let only the newest read or socket act | The relay's port and stop flag, the spool's count of calls in flight, the controller socket's number, the node report's reads                                                    |
| `tokio::sync::mpsc`                             | Ordered work, unbounded only as [Queues](#queues) lists                                                               | The controller connection's commands, a plugin's outbox, slot events, node changes                                                                                               |
| `std::sync::mpsc`                               | Jobs for the spool's thread, limited by a count of calls in flight                                                    | `SpoolClient`                                                                                                                                                                    |
| `std::sync::mpsc::sync_channel`                 | Console lines for the console thread, in a bounded queue                                                              | `logger`                                                                                                                                                                         |
| `oneshot`                                       | One reply                                                                                                             | Requests to the controller, relay commands, spool jobs, helper containers, overlay and image-update steps                                                                        |
| `watch`                                         | State others follow                                                                                                   | The maintenance guard, a launch's phase                                                                                                                                          |
| `tokio::sync::OnceCell`                         | A result made once that later callers wait for                                                                        | The shutdown's result, which a second call returns                                                                                                                               |
| `tokio::sync::Notify`                           | A wake-up without data                                                                                                | A helper or update container's output, through `Activity`, which resets its quiet limit; a plugin socket's close; the end of a request that a shutdown waits for                 |
| `CancellationToken`, `TaskTracker`              | Stopping, and waiting for the loops                                                                                   | The root token from `main`, the tracker in `agent.rs`                                                                                                                            |
| `AbortOnDropHandle`                             | A task that ends with its owner                                                                                       | The controller connection's hook tasks; the slot manager's reservation and start-timeout timers                                                                                  |
| `Shared` future, spawned                        | Work callers share, through `crate::sync::join_or_spawn` when each run empties its slot as it ends                    | The CS2 update check, a GSLT check, a demo scan, a disk space read; the build read and the image poll, which keep or repeat a run, build theirs by hand                          |
| `OnceLock`, `LazyLock`                          | Statics built on first use                                                                                            | The console's sender, the TLS client config, the regexes                                                                                                                         |

Accepted departures from the rules above:

- `ConfigStore`'s turnstile is a std mutex, because a change reads the config, writes `agent.yml`
  and applies the patch with no `.await`. A change never takes the turnstile again, because a std
  mutex is not reentrant.
- The relay and the event uploader keep stop tokens of their own instead of children of the root
  token, because the event pipeline closes in order: the relay commits what is queued, the
  uploader stops, then the spool closes its file.
- `SpoolClient` times how long its calls wait with `std::time::Instant`, because tokio's clock
  cannot see a thread outside the runtime and stands still in paused tests. The check that reads it
  runs on a tokio interval.
- `flush_console` waits with `std::time::Instant` and `std::thread::sleep`, because it runs after
  the runtime has shut down, or in the panic hook, where no runtime clock runs.

### Shutdown

On SIGINT, SIGTERM or SIGQUIT the agent cancels the root token, so no loop goes on, no timer runs
out and no launch, update, poll or guard claim starts. It sends the controller its `shutdown` at
once and answers a request that arrives after it with `shutting_down`. The requests it is running
get their replies over the open socket for up to the contract's `SHUTDOWN_REQUEST_WAIT_MS`; one
still running then is cut off, and the next boot adopts what it left. The agent then closes the
controller socket, and after it the event pipeline, so a request that ran until then could still
store what it wrote: the relay stores what is queued, the uploader stops, and the spool closes its
file. It waits for the tracked loops, which each end their work in progress on the token, and exits.
All of this fits inside Docker's 10 s stop timeout, unless the disk hangs or a frame to the
controller was already stalled when the signal came. Game servers keep running, and the next boot
adopts them; their plugins replay from what the spool acknowledged.

The agent handles these signals from its first moment. One that arrives during boot ends the boot
where it is, without the steps above; the next boot adopts what it left, as after a crash.

### What the design guards against

| Failure                                                | Guard                                                                                                                                                    |
|--------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------|
| The controller stops reading (half-open link)          | A missed pong or a stuck write ends the socket, and the agent redials (`controller_connection`).                                                         |
| The controller refuses the node                        | `401`/`426` and `agent_deleted` stop the dialling, and the log tells the operator what to do (`controller_connection`).                                  |
| A game floods events                                   | The relay's queue per game, then a replay, and the spool's limit on waiting jobs (`plugin_relay`, `spool_client`).                                       |
| The spool fails                                        | Launches are refused, and a spool that stops answering marks the node `unavailable` with `event_storage` (`spool_client`).                               |
| The disk fills up                                      | Below `DISK_SPACE_MIN_BYTES` the node is `unavailable` with `disk_space`, so no match starts that would lose its events or demo (`disk_space`).          |
| The controller keeps refusing one server's batches     | That server backs off alone, and its events are dropped after the retention period, with the drop logged (`event_uploader`).                             |
| One server sends events without pause                  | Uploads take one batch per server in turn, so its backlog holds no other server back (`event_uploader`).                                                 |
| A launch is replayed                                   | The spool records the allocation before Docker is asked for anything, so a launch that cannot be recorded does not run (`slot_manager`).                 |
| A demo upload stalls                                   | A watchdog aborts it, and it is retried with backoff (`demo_uploader`).                                                                                  |
| A pull, update or overlay copy hangs                   | Each has a limit, after which it fails and frees the maintenance guard. An update container that was not removed keeps the guard (`install`, `overlay`). |
| The relay cannot accept a plugin                       | The accept is retried, so plugins reconnect once it clears (`plugin_relay`).                                                                             |
| A token moves slots while it logs on                   | Logons are counted per token, and Steam's answer applies only to a slot that still has that token (`slot_manager`).                                      |
| Docker stops answering                                 | Every call has an answer limit, and a create or start that timed out cleans up by name (`docker_api`, `docker`).                                         |
| Docker refuses a stop                                  | The slot stays `stopping`, and each check tries the stop again until the server exits (`slot_manager`).                                                  |
| An image switch runs while a launch prepares           | The sweep keeps the image the launch picked until its server is gone (`self_update`).                                                                    |
| A self-update swap meets a leftover or a silent daemon | Leftovers are removed, and a step without an answer counts as done, so the rollback can still restore the old agent (`self_update_helper`).              |
| The self-update helper dies mid-swap                   | Docker restarts it, and the rerun finishes or rolls back the swap (`self_update`).                                                                       |
| An outside service answers with too much               | Answers are read up to a limit (`http_body`).                                                                                                            |
| The agent restarts mid-match                           | Boot adopts the running containers, plugins replay unacknowledged events, and demos upload after the restart (`agent`, `slot_manager`).                  |

## Code conventions

### Dependencies between components

- A component is a `Clone` handle over `Arc<Inner>`. Required collaborators come in an `XDeps`
  struct; optional ones in an `XOptions` struct built with `::new` and then set field by field.
- Use ports (traits used as `Arc<dyn Trait>`, with `async_trait` when they are async) for external
  systems and collaborators with several calls, and a `pub type` closure alias for a single
  callback. Ports, closures, clocks and timings are what tests replace, through the constructor.
- Constructors do not fail. A client or thread that cannot start makes every call fail with the
  reason.
- Give a component only what it uses. Parts that read the config get a `ConfigReader` or a read
  closure; only the writers get the `ConfigStore`.
- Build and connect components in `agent.rs` only. Logic that is not wiring goes in a module.

### Derived values and keys

Both rules prevent connascence of algorithm and of meaning: two places that must agree on a
computation or on a piece of text, so that one change needs edits in both and a missed edit fails
silently.

- **Repeated derivation.** Derive a value that several places must agree on in one function, and
  call it from each. Container names, paths, labels, file names and ids are the usual cases. Two
  inline `format!` or string transforms of the same input are the pattern to look for.
  `host_layout.rs` does this for paths.
- **Display-string keys.** Branch, look up and compare on an enum, a constant or an id, never on
  text written for people, such as a log line, a label or an error message. When a peer sends a
  value the contract defines, such as a close reason, parse it into the generated enum in
  `ankor_protocol::types` and match the variant.

### Errors and crashes

- A module with failure cases of its own gives them a `thiserror` enum. A module that only passes
  on I/O errors or another module's errors returns those.
- At the request boundary, return `OpError::Refused` with the code the controller acts on,
  `OpError::Step` for an update step with an outcome of its own, or `OpError::Failed`.
- Return an error or log it, not both. The controller connection logs a failed request once;
  background work logs `could not …: {error}` and carries on.
- Log through `log.child(..)`, in lower case without a full stop. Lines about a slot start with
  `slot {id}: `.
- Code outside tests does not `unwrap`, `expect`, `panic!` or index unchecked. A justified
  exception carries `#[allow(..., reason = "...")]` at the line.

### Tests

- Use `tokio::time` for every duration and timer, so tests can pause time. The exceptions, the
  spool's clock and the console's flush, are listed with the
  [accepted departures](#concurrency-primitives).
  Wall-clock readings come through an injected clock.
- Use the fakes in `crates/agent/src/testing.rs` and paused time. Stop a component in a test by
  cancelling the token you passed in.
- Tests that reach an outside service are `#[ignore]`d and run only when asked; README.md lists them.
  The conformance sandbox rejects every outbound connection the suite does not fake.
- Do not add flags, branches or methods that only tests call. Tests replace collaborators through
  constructors and builder methods that return a new value, such as `ConfigStore::with_persist`;
  nothing changes a running component for a test.
- Module tests live in `crates/agent/tests/<module>.rs`. A test that is easier to write against
  private items, such as a helper or a component's internal state, may sit in an inline
  `#[cfg(test)] mod tests`. The doubles in `testing.rs` sit behind the `testing` feature.

### The protocol

- `schemas.rs`, `types.rs` and `constants.rs` in `crates/protocol` and in
  `crates/agent/src/contract` are generated. Change them through `scripts/protocol-sync.sh`, never
  by hand. The rest of `crates/protocol` (`lib.rs`, `refine.rs`, `helpers.rs`, `validate.rs`,
  `support.rs`, `config.rs`) is written by hand to match the protocol package.
- Take limits, timers and codes from `ankor_protocol::constants`. A local constant is only for a
  value the contract does not define. Cut text sent to the controller with `failure::truncate` and
  the matching `*_MAX_CHARS`.

## Documentation

- Markdown files explain conventions, architecture and structure, which change rarely. Details
  (fields, flags, limits, step-by-step behaviour) go in code comments beside the code, so they
  change with it.
