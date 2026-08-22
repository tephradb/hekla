# kiln architecture

kiln is a rewrite of [umari](https://github.com/tqwewe/umari), a Rust event-sourcing / DCB
(Dynamic Consistency Boundary) runtime where you author **commands**, **projectors**, and
**effects**. umari runs those as WASM component modules (Wasmtime, WIT, Rust/TS SDKs) on the
[tephra](https://crates.io/crates/tephra) event log. kiln keeps umari's conceptual model and its
tephra + SQLite substrate but replaces WASM with **embedded Starlark**.

The swap is not cosmetic. Starlark is pure and sandboxed: no clock, no randomness, no I/O except
host-injected builtins. That makes command and projector determinism structural rather than
policed, makes deployment source text (no build step, no compile cache), and enables a
Temporal-style durable-execution model for effects that umari could not express.

This document describes the system as designed. Delivery is phased in [ROADMAP.md](./ROADMAP.md).

## 1. What kiln is

A single-app, event-sourced runtime. Business logic is written as small Starlark files (commands,
projectors, effects) over an immutable tephra event log. Commands are the only writers. Projectors
build queryable SQLite read models. Effects react to events with durable, replay-safe side effects.
Determinism and sandboxing come from Starlark itself, not from runtime policing.

## 2. Concepts and vocabulary

One word per concept, no synonyms.

- **Event**: an immutable fact in the tephra log. Carries a type, tags, and a JSON payload, plus a
  host-stamped envelope (below). Events are the single source of truth.
- **Tag**: the routing vocabulary everywhere, matching DCB and tephra. Tags are `key:value` pairs
  stored and indexed on events. Authors never write tag strings: event definitions declare which
  fields are tags, emitting auto-tags them, and queries pass structured `tags = {...}` that the
  runtime encodes. There is no separate "domain ID" term; a tag is a tag.
- **Consistency boundary**: the set of events a command reads, expressed as event types plus
  structured tags. Optimistic concurrency is enforced by appending under a tephra `AppendCondition`
  over the same boundary.
- **Command, Projector, Effect**: the three module kinds (sections 5 to 7).
- **Fold**: an in-handler reduction over the boundary's events that recovers decision state.
- **Read model**: a projector's SQLite database. A rebuildable cache, never a source of truth.
- **Journal**: an effect's durable record of its side-effect calls, keyed by content hash, used to
  replay the effect deterministically after a crash. Lives in the operational DB, never the log.
- **Envelope**: host-stamped per-event metadata: `correlation_id`, `causation_id`, an optional
  `triggering_event_id`, and an append `timestamp`. The `id` and `position` are tephra's. The
  idempotency key is not on the event; it is request plumbing, stored operationally.

## 3. Module layout and deployment

Kind comes from the directory, name from the file stem, and one file is one unit of behaviour.

```
project/
  events/              # shared event definitions (importable)
  lib/                 # shared pure helpers (importable)
  commands/            # public commands (HTTP-routed and invokable by effects)
  commands/internal/   # internal commands (invokable by effects, NOT HTTP-routed)
  projectors/
  effects/
  kiln.toml            # operational config (optional)
```

- **`load()` is restricted to `events/` and `lib/`.** A command can never import another command,
  so one file stays one unit. The host `load()` resolver builds a load graph, used for fast
  incremental re-validation (editing an event file re-validates only its importers), and caches
  evaluated modules (a shared events file evaluates once, not once per importer).
- **Public vs internal is structural, not a flag.** `commands/internal/` keeps effect-completion
  commands (for example `record-shipping-label`) off the HTTP surface, so nobody can POST a
  fabricated `shipping.label_created` with a tracking number that was never issued. It also keeps
  generated OpenAPI honest. Retrofitting this later would be breaking, so it is in from v1.
- **Deploy is restart.** v1 loads at startup; there is no hot reload. Reload raises the same
  checkpoint, schema-change, and in-flight-invocation questions as deployment, and answering them
  under a file watcher is how the mechanism everything depends on gets subtly wrong. Restart is
  instant and correct. Graceful shutdown drains effects first (section 7).
- **Configuration (`kiln.toml`)**: a small project-level file for operational knobs that are not
  code: the effect blocking-pool size, and the retention windows for effect journals and command
  idempotency keys. Defaults are sensible, so a project runs with no config.

## 4. Events and schema

Event definitions live in `events/` and are imported with `load()`. An event declares its type, its
typed fields, and which fields are tags.

**Field type system** (shared across event schemas, command input `schema()`, and entity schemas):
`text`, `i64`, `u64`, `bool`, `uuid`, `timestamp`, `money`, `json`, `one_of`, `optional`. This is
kiln's existing `FieldKind` set. Two representations are pinned so they are not decided
inconsistently in two places:

- **`money`**: a decimal string on the wire (JSON event payloads and read-API responses), an integer
  count of minor units in storage.
- **`one_of`**: the runtime validates value membership only (a written value must be in the declared
  set). It does not validate that a transition between values is legal; transition rules, if ever
  needed, are the author's job in `handle`.

**Emit via the event-def constructor**: `emit(user_registered(user_id = ..., email = ...))`. The
runtime validates the payload against the field schema and auto-derives tags. Missing or extra
fields fail fast.

**Envelope**: the tephra payload is a JSON envelope wrapping `data` with `correlation_id`,
`causation_id`, an optional `triggering_event_id`, and the append `timestamp`. The host stamps these
at append; Starlark never sets them.

**Deploy-time validation** is the reason event definitions are shared. A query that filters an event
type on a field that type does not declare as a tag is a hard error, never a silent empty result.
Emit constructors are checked against field schemas, and projector indexes are checked against
declared fields.

## 5. Commands

A command validates input, checks invariants against replayed state, and appends events. It is the
only writer.

**Shape** (everything but `handle` is optional):

- `query(input)` returns the boundary: event types plus structured tags, or a list of those OR-ed
  together.
- `initial` is a literal or a function producing the fold's starting state.
- `fold(state, event)` reduces the boundary's events into decision state.
- `handle(input, state)` decides and returns one of three terminal outcomes:
  - `emit([...])` appends events.
  - `reject(code, message)` is a state-dependent refusal: the input was well-formed but the current
    state forbids it (for example, email already taken). Maps to HTTP 422, kept distinct from the 409
    a concurrency conflict returns, so the status alone tells a client whether a retry can help.
  - `invalid_input(message)` means the input is malformed regardless of state, a shape or parse-level
    problem. Maps to HTTP 400.

Commands with no invariants omit `query` and `fold` entirely.

**Determinism**: `query` and `fold` are pure and clock-free, because `fold` replays history and a
clock there would break determinism. `now()` is available only in `handle`, pinned once per request
so repeated calls agree. It is for time as domain data (`expires_at`, `due_date`), not for restating
the host-stamped append timestamp; putting the wall clock in the payload would duplicate what the
envelope already holds.

**Ids**: new-entity ids are client-supplied in the input. A retried request carries the same id, so
the command's own boundary rejects the duplicate, and idempotency for creation falls out of DCB with
no extra layer. Starlark mints no ids and has no randomness. Host-minted ids would mint a fresh one
per retry, creating two entities from one intent.

**Append and DCB**: emitted events are appended under an `AppendCondition` over the boundary. A
concurrent write inside the boundary fails the append, and the caller retries.

**Built-in idempotency key** is distinct from id-based dedupe. It exists for commands where nothing
in the input distinguishes intent (approving a claim twice with identical input could be one retry
or two deliberate approvals, and no domain check can resolve that). `execute` accepts an idempotency
key; the runtime looks it up in a per-command idempotency table in the operational DB. The lookup is
global per command, not scoped to the boundary, because a retry of a rejected command produced no
event and there would be nothing in the boundary to find. On a hit it returns the original outcome,
including rejections (a retried `email_taken` comes back as `email_taken`, not success). Keys are
scoped per command so the same key on two commands does not collide, and they are swept on a
retention window. The key never goes on an event as a tag; tags are domain identity, this is request
plumbing.

**Commands never invoke commands.** Sharing a boundary would make the callee's query a lie, and
separate appends give partial failure with no rollback. Chaining goes through the log: a command
emits an event, an effect reacts, and the effect invokes the next command. That path is durable and
independently retryable.

## 6. Projectors

A projector consumes events and builds a queryable read model.

**Shape**: entities are declared with `entity(key, fields, indexes)` and collected implicitly from
module scope. `source` is the event subscription. `handle(event)` returns `put` / `patch` / `delete`
ops, and may call `get(entity, key)` to read the current row first.

**`get()` reads through uncommitted writes in the current batch.** If a handler `put`s a row and a
later event in the same batch reads it, it must see the write, or batching would silently change
behaviour versus processing one event at a time. Read-modify-write (running totals, or reading a row
for its foreign key before updating a summary) stays in Starlark. There is no arithmetic-op
vocabulary: a projector is a single sequential writer owning its own database, so there are no
concurrent writers for an atomic increment to protect against.

**Checkpoint format from day one** is a watermark plus a set of completed positions above it. The
set is always empty under the sequential model, but building the format now means parallel lanes
never require a live migration ("resume from position N" stops being expressible once lanes complete
out of order). The checkpoint is written in the same SQLite transaction as the state it describes,
so a crash cannot leave state and position disagreeing and silently skip events. The watermark is the
subscription's, which jumps past a non-matching tail: a caught-up projector advances its checkpoint to
head even when the latest events are ones its `source` does not select, so a selective projector reads
as caught up (honest `/status` lag, and read-your-writes resolves against it) rather than stalling at
its last matching event. That empty-batch advance persists on its own, outside any op transaction.

**Storage**: one SQLite database per projector, holding both the read-model tables and the
checkpoint. Co-location is what makes the single-transaction commit possible. The projector thread is
the only writer, running the database in WAL so the read API can open read-only connections
concurrently. Replay is rebuild-and-swap: build a fresh database from position 0, seal it (fold its
WAL back into the file and drop to rollback mode so it is self-contained), then rename it in, so state
and position move together atomically and a reader that opens the file mid-swap never sees a torn one.

**Read model access** is only ever through the generated read API (section 10), never by opening the
SQLite file directly. Each read opens its own read-only connection and reads the projector position in
the same snapshot as the rows. The table layout stays private.

**Read-your-writes** is opt-in per read: a client passes `?after=<pos>` (the `positions.last` a command
returned) and the read blocks until that projector's committed position reaches it, then serves the
normal snapshot read. This is safe because the projector publishes its in-memory position only after the
batch and checkpoint commit, so once the wait observes it, a fresh read-only connection is guaranteed to
see the write. The wait is bounded by `timeout_ms` (default 5s, capped at 30s); on timeout it fails
closed with 503 and `Retry-After` rather than silently serving stale data, so a client that asked for a
position and did not get it knows so.

## 7. Effects (durable execution)

Effects are the crown jewel and the biggest departure from umari. They perform side effects (HTTP,
and via commands, writes) in reaction to events, and they are durable: an effect that crashes
mid-way resumes without re-firing side effects it already performed.

**Model**: `handle(event)` is straight-line blocking code that calls injected impure builtins.
Determinism under replay comes from a journal. Each builtin call looks itself up in the journal
first: if a result is recorded, it returns that; otherwise it performs the real call and appends the
result. After a crash, `handle` re-runs from the top, replays journaled calls until it passes the
end of the journal, then resumes making live calls. There are no step functions and no yielding:
blocking a thread keeps evaluation state on that thread's stack, and crashes are handled by replay.

**Journal key is the content hash of the call** (for HTTP, `(method, url, body)`), plus an optional
disambiguator for legitimately-identical repeated calls. It is not a sequence number, so editing or
reordering the script does not corrupt replay, which is what makes live editing safe later. The
script hash is recorded in the journal. v1 does not pin to it, but on restart, if an in-flight
invocation's recorded hash differs from the on-disk code, the runtime logs a warning naming the
effect and invocation. That makes an otherwise invisible situation visible, and it is exactly the
field the pinning implementation needs later, so writing it now avoids a journal-format migration.

**Journal storage** is the shared operational DB, never the event log. HTTP responses are
operational scratch (tokens, PII), not domain facts; putting them in an immutable log would make
them permanent and would couple the log to the effect implementation. A background sweeper deletes
completed invocations older than a configurable retention window (`kiln.toml`), and the same sweeper
ages out command idempotency keys. Sweeping is lazy GC, not transactionally required: because the
terminal record step is journaled, a crash between the append and the delete replays the terminal
step rather than double-applying, so the sweeper only reclaims space and never affects correctness.

**Builtins (v1)**: journaled `http.{get,post,...}`; `invoke_command(name, input)` targeting a public
or internal command; `now()`; `log()`; and a journaled `read(projector, entity, key)` plus
filter/scan variant, so effects query read models instead of keeping a local database. There is no
effect-local SQLite: durable state is the journal plus events written through commands. Whether
`read()` is journaled is a deliberate tradeoff. Journaling it (the chosen default) gives full replay
determinism, so a replayed effect makes the same decisions it originally made, at the cost of seeing
point-in-time-stale data. Not journaling it would give fresh data at replay time but let replay
diverge from the original run. kiln journals `read()` for consistency with the rest of the model.

**Writing outcomes**: effects do not append events; they `invoke_command`, and that invoke is a
journaled, idempotent side effect, so durable domain facts (tracking numbers, external ids) land
exactly once across replays. The idempotency key an effect passes is deterministic, so the target
command tags every event it emits with that key and guards the append against the tag. A replay (or a
crash between the command's append and the effect's journal write) finds the prior commit by that tag
and returns its recovered outcome without re-running the command: exactly-once is enforced by the event
log itself, not by any op-DB reservation. This is the same mechanism, and the same guarantee, as for
HTTP commands. A command rejection is a normal terminal outcome, not a retryable
failure: if a completion command rejects because state moved on (the claim was already cancelled, the
order already fulfilled), the runtime records the rejection in the journal and completes the
invocation. Treating rejection as retryable would loop forever on legitimately-stale completions.

**Retry split**: the runtime absorbs transport errors and 5xx with backoff, and those never reach
the script. A result that reaches Starlark is therefore always terminal, so `status >= 400` in a
handler is a real, decide-what-to-do failure rather than something every effect re-implements.

**Wedging and the skip hatch**: transport errors, 5xx, and a raised handler all wedge the invocation.
The runtime retries the whole invocation with capped exponential backoff, forever, replaying journaled
calls each attempt so completed side effects never re-fire, and never skipping. Because a wedge is not
the same as ordinary lag, the status endpoint reports each effect's consecutive-failure count and last
error alongside its position. The only way past a genuinely unprocessable event is fixing the code and
restarting (which replays the running invocation) or an explicit, manual operator skip
(`POST /effects/{name}/skip/{position}`); nothing is skipped automatically. The durable resume point is
a per-effect watermark advanced only once a batch's invocations are all terminal, so a crash re-scans
from the last completed batch and never skips an event; the journal rows and the terminal record commit
call-by-call in autocommit, never in one per-invocation transaction, which is what lets journaled side
effects survive a crash and be replayed.

**Concurrency (v1)**: sequential per effect: one in-flight invocation, strict position order, no
cross-lane watermark. v1 runs one dedicated thread per effect, which already bounds concurrency at the
number of effects; the configured blocking-pool size (`kiln.toml`) is validated but reserved for a real
shared pool once partition-key parallel lanes land (the watermark-plus-completed-set format enables
them). Because processing is sequential but events are at-least-once, an effect whose handler is slower
than its event arrival rate falls behind. Falling behind, visible as lag, is the correct behaviour, not
an unbounded pending queue or unbounded thread growth.

**Redeploy**: content-hash keying limits the blast radius. Unchanged calls replay from the journal
regardless of edits elsewhere in the file, so the failure mode of editing during a deploy is "a
different path was taken", not "a side effect fired twice"; duplicates require editing the URL or body
of a call that already fired, which is narrow and visible. Graceful-shutdown draining (stop
dispatching new invocations, wait for in-flight ones with a timeout, then exit) makes the common
deploy have nothing in flight, so "drain before deploying changed effect code" is automatic rather
than a user chore.

**Observability**: effect lag (current position vs log head) is surfaced in the status endpoint,
since a sequential effect on a slow API will fall behind and that should be visible rather than
silent.

## 8. Runtime and concurrency

- Lightweight tokio tasks with bespoke supervision. No actor framework in v1.
- Commands run on `spawn_blocking`, because Starlark evaluation is synchronous.
- One sequential task per projector.
- One dedicated thread per effect (the projector model), each running its invocations synchronously in
  strict position order. The configured blocking-pool size is validated but reserved for a real shared
  pool once partition-key parallel lanes land (section 7).
- starlark-rust specifics: `FrozenModule` is `Send + Sync`, so a module is parsed and frozen once at
  load and shared across tasks. A fresh `Evaluator` per invocation with a tick budget bounds runaway
  scripts.

## 9. Storage layout

```
data/
  events/                # tephra segments (immutable source of truth)
  projectors/{name}.db   # read-model tables + checkpoint (one transaction)
  kiln.db                # shared operational DB: effect journals, idempotency keys, module metadata
```

Backup is "copy the directory". Projector databases are rebuildable from the log regardless, so a
consistent copy is not required for them.

## 10. HTTP API surface (v1)

- `POST /commands/{name}` executes a command (public commands only), accepting idempotency-key and
  correlation-id headers, and echoes correlation and causation. The outcome maps to status: committed
  to 200 (with the appended positions), `reject` to 422, `invalid_input` to 400, and a DCB
  concurrency conflict that survives retries to 409. A replayed idempotency key returns the original
  outcome and status.
- **Read API generated from entity schemas**: `GET /read/{projector}/{entity}/{key}` and an indexed
  filter/scan endpoint. Only declared indexes are filterable; an unindexed filter is a 400 telling
  the author to declare the index, never a table scan. Pagination is cursor-based, not offset. Every
  read response includes the projector's log position, and an optional `?after=<pos>` waits for the
  projector to reach that position before reading (read-your-writes), failing closed with 503 on
  timeout (section 6).
- `POST /projectors/{name}/replay`.
- `POST /effects/{name}/skip/{position}`: an explicit, manual operator action to advance a wedged effect
  past a genuinely unprocessable event. Never automatic.
- `GET /status` and health: per-module positions and lag (position vs log head), plus each effect's
  consecutive-failure count and last error, so a wedge is distinguishable from ordinary lag.
- An admin-only, read-only SQL endpoint behind a flag, off in production, for debugging.
- Direct SQLite file access is not a supported surface. The table layout stays private behind the
  generated read API.

## 11. CLI and dev loop

- `kiln serve <dir>`: run the runtime and HTTP API from a project directory, loading at startup.
- `kiln check <dir>`: the only static analysis Starlark gets, so it is thorough. It parses, resolves
  the load graph, verifies every query filters on tags the event type actually declares, verifies
  emit constructors match field schemas, and verifies projector indexes reference declared fields.
  For CI and pre-commit.
- `kiln test <dir>`: events in, assert events out, for commands. Pure functions with declared inputs
  make the harness small, and it is what earns trust in an untyped language.
- `kiln fmt`: starlark-rust ships a formatter, and indentation is syntactically meaningful.

## 12. Why Starlark (determinism and purity)

umari pins the wall clock and zeroes the monotonic clock to make commands deterministic, and polices
nondeterminism with static analysis and linters. Starlark makes it structural: no clock, no
randomness, and no I/O except injected builtins, so every nondeterministic input is either absent
(commands and projectors) or journaled by construction (effects). Deployment is source text: no
toolchain, no compile cache, parse-and-freeze in milliseconds.

## 13. Non-goals

**Deferred** (see the roadmap, each with a trigger): encryption and crypto-shredding; metrics and
Prometheus; partition-key parallel effect lanes; an upload API with versioning, pinning, and
retention; hot reload; a fold library; a workspace crate split.

**Permanent commitments** (not deferrals, and not to be reopened): Starlark is the only authoring
surface. There is no Rust, TypeScript, or WASM SDK path now or later. This is deliberate: a single
pure, sandboxed authoring language is what makes determinism structural, deployment source text, and
the durable-effect journal sound. Multi-language authoring is permanently out of scope.

## 14. Code layering

kiln is a single crate. The dependency direction is documented and enforced by discipline, revisited
only when a seam proves real (embeddability, or compile times that actually hurt): `starlark_builtins`
and `schema` depend on nothing internal; `dispatch` depends on those; `runtime` (projectors, effects,
journal, storage) depends on `dispatch`; `api` and `cli` sit on top.
