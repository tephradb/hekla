# hekla architecture

hekla is a rewrite of [umari](https://github.com/tqwewe/umari), a Rust event-sourcing / DCB
(Dynamic Consistency Boundary) runtime where you author **commands**, **projectors**, and
**effects**. umari runs those as WASM component modules (Wasmtime, WIT, Rust/TS SDKs) on the
[tephra](https://crates.io/crates/tephra) event log. hekla keeps umari's conceptual model and its
tephra + SQLite substrate but replaces WASM with **embedded Starlark**.

The swap is not cosmetic. Starlark is pure and sandboxed: no clock, no randomness, no I/O except
host-injected builtins. That makes command and projector determinism structural rather than
policed, makes deployment source text (no build step, no compile cache), and enables a
Temporal-style durable-execution model for effects that umari could not express.

This document describes the system as designed. Delivery is phased in [ROADMAP.md](./ROADMAP.md).

## 1. What hekla is

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
- **Envelope**: host-stamped per-event metadata: an `event_id`, `correlation_id`, `causation_id`, an
  optional `triggering_event_id`, and an append `timestamp`. The `position` is tephra's; the id is
  the envelope's, and a handler reads it as `event.id` (section 4). The client's idempotency key is
  not on the event; a reserved tag derived from it is (section 5).

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
  hekla.toml            # operational config (optional)
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
- **Configuration (`hekla.toml`)**: a small project-level file for operational knobs that are not
  code: the effect blocking-pool size, the retention window for effect journals, and whether the
  continuous invariant checks run. Defaults are sensible, so a project runs with no config.
- **One process per data directory.** A runtime takes an exclusive lock on its data directory for
  its lifetime, because tephra locks nothing itself and two writers on one segment set corrupt the
  log. The lock is an open `BEGIN EXCLUSIVE` on a dedicated SQLite file, so it needs no dependency
  and is released by process death however it arrives. It is also what keeps `hekla verify` off a
  directory a server is using (see section 11.2).

## 4. Events and schema

Event definitions live in `events/` and are imported with `load()`. An event declares its type and
its typed fields; there is no `tags = [...]` list. **Every field is automatically indexed as a store
tag** unless it opts out with `indexed = False`. Auto-tagging removes the old failure where a field
forgotten from the tag list was unqueryable forever (and adding it later missed all prior events).

**Per-field policy** (named arguments on any field constructor):

- `indexed = False` opts a field out of tagging (a large blob, or free text where a tag is useless).
- `subject = "sibling_field"` encrypts the field under a key scoped to that sibling's value; see
  section 15. `unique = True` (which requires `subject`) additionally emits a global-key tag for a
  uniqueness check that survives erasure.

**Field type system** (shared across event schemas, command input `schema()`, and entity schemas):
`str`, `int`, `uint`, `bool`, `uuid`, `timestamp`, `money`, `json`, `one_of`, `optional`.

The scalar types deliberately reuse Starlark's builtin names, shadowing the standard `str`, `int`
and `bool` globals. One rule keeps both meanings reachable: **a positional argument means Starlark's
conversion, and no positional argument means a field declaration.** So `str(response.status)`
converts and `str(max_length = 200)` declares. This works because every standard conversion is
positional-only while every field option (`indexed`, `subject`, `unique`, `max_length`) is
named-only, and passing both at once is an error rather than a silent drop. The cost is one idiom:
`int()` and `bool()` no longer produce `0` and `False`, so write the literals. (`str()` costs
nothing, since the standard `str` requires its argument.) `uint` shadows nothing, and `one_of` keeps
a distinct name because the rule cannot reach it: a variant list and starlark-rust's `enum(...)` are
both positional, leaving nothing to tell them apart.

`float` and `bytes` are intentionally left as plain Starlark conversions, because **there is no float
field type and there will not be one.** Binary-float rounding in an append-only log is permanent;
auto-tagging a float needs an encoding that sorts lexicographically, the same problem that stops
`money` from keying an ordered scan; and float equality under a `unique` index is a trap. Use
`money` for currency and scaled integers for everything else. A float reaching a typed field is
rejected at the write boundary. The one door left open is `json`, which validates nothing by design
and so will store one.

**Two host tags sit in a reserved `_hekla_` namespace** an author can neither emit nor query: a keyed
command's idempotency tag, and the correlation tag every event carries. The correlation id lives in
the envelope payload, but a store query filters on type and tags only, so without the tag a causal
chain could be found only by decoding every event in the log. Both are stripped from command
responses, and `hekla check` rejects the prefix on both sides.

Three representations are pinned so they are not decided inconsistently in two places:

- **`money`**: a decimal string on the wire (JSON event payloads and read-API responses), an integer
  count of minor units in storage.
- **`one_of`**: the runtime validates value membership only (a written value must be in the declared
  set). It does not validate that a transition between values is legal; transition rules, if ever
  needed, are the author's job in `handle`.
- **`int` and `uint`**: both land in a SQLite `INTEGER` column, which is signed 64-bit, so the
  storable range for either is `i64::MIN..=i64::MAX`. A `uint` above `i64::MAX` is rejected at the
  write boundary (command input and event construction), not silently stored. Reinterpreting the
  bits would round-trip the value but sort it below zero, which would quietly break `ORDER BY` and
  the `key > ?` cursor for those rows: the same failure that keeps `money` from keying an ordered
  scan. So `uint` means "non-negative, up to `i64::MAX`"; widening it would need a storage form that
  still orders correctly.

**Construct an event via its definition**: `user_registered(user_id = ..., email = ...)`. The
runtime validates the payload against the field schema and derives a tag from every indexed field.
Missing or extra fields fail fast. A command's `handle` returns the constructed event, or a list of
them, to append. The same constructor called in query position (a command's `query`, or the keys of a
`fold` or `handle` map) instead builds a filter clause; see section 5.

**Only the registered definition may be emitted.** Each `event(...)` call mints a process-unique id,
and a constructed event carries the id of the definition that built it, so the append seam can check
identity rather than the type name. That closes the case where a handler builds its own
`event(type = "user.registered", ...)` inside a function body: the name matches a declared type, so
the event would be lowered against the registry's schema, and any field the real definition does not
declare would ride into the immutable log verbatim, never validated and never encrypted. Referring
to a loaded definition by a second name is the same definition and keeps working. The same identity
check runs at load time, so a module-scope redeclaration outside `events/` is a `hekla check` error
rather than a runtime one.

**Payload access**: a host-built value with a fixed shape is read with **dot access**, and everything
else is a dict read by subscript.

Dot: `input` and `event.data`, built from a declared field schema (`input.email`,
`event.data.email`), where a field the schema does not declare is a shape error and one the payload
omits reads as `None`. `event.id` sits beside them, so an event that declares its own `id` field
keeps it at `event.data.id`. Also the two fixed-shape wrappers an effect gets back: `http.*` returns
`{status, body, headers}` and `invoke_command` returns `{status, body}`.

Subscript: values a handler builds itself, a folded `state` (in a command or an effect) and a
`put()` row, because there is no declared shape to check them against. Also the *contents* of the
wrappers above, which the host cannot promise a shape for: a response `body` is parsed JSON when the
bytes parse and a string otherwise, and `headers` is keyed by arbitrary header names.

A read-model row from `get()` stays a dict for a second reason: `put()` takes a dict, so
read-modify-write has to round-trip without a conversion in between.

**Envelope**: the tephra payload is a JSON envelope wrapping `data` with an `event_id`,
`correlation_id`, `causation_id`, an optional `triggering_event_id`, and the append `timestamp`. The
host stamps these at append; Starlark never sets them.

Two envelope fields are exposed to handlers, beside `event.type` and `event.data`: **`event.id`** and
**`event.timestamp`**. Both are stamped once at append and never move, so a projector rebuild and an
effect replay see what the original append wrote. That stability is what makes `id` the input to
derive from, and what lets `timestamp` be the source for a `created_at`-style read-model column.

**Prefer `event.timestamp` over restating the clock.** A command using `now()` for a field that only
records when the event was appended duplicates what the envelope already holds; the rule in section 5
stands, and this is what makes it followable. `now()` remains right for time that is genuinely domain
data and not the append instant (`expires_at`, `due_date`, a `purchased_at` an upstream system
reported).

The rest of the envelope (`correlation_id`, `causation_id`, `triggering_event_id`) stays host-side:
each would need its own argument for why a handler should branch on it.

**Deriving ids**: no module may mint a random one. Commands take new-entity ids from their input
(see section 5), and a handler that needs an id with no such source derives one with
`uuid5(namespace, name)`, RFC 4122 version 5, usually over `event.id`:

```starlark
invoke_command("record-notified", {
    "notification_id": uuid5(event.id, "confirmation"),
    "order_id": event.data.order_id,
})
```

The `name` argument is what lets one handler derive several distinct ids from one event. Randomness
here would not merely be unavailable, it would be wrong: a command retry and an effect replay both
re-run the code that mints the id, so a fresh id per attempt would turn one intent into several
entities, which is the same failure host-minted ids have. Deriving is the third choice, not the
first: prefer an identity that already exists (the entity the fact is about) or one an external
system returned in a journaled response.

**Deploy-time validation** is the reason event definitions are shared. A query that filters an event
type on a field that type does not declare (or declares `indexed = False`) is a hard error, never a
silent empty result. Value types are checked against the field's kind, event constructors against the
field schema, and projector indexes against declared fields.

## 5. Commands

A command validates input, checks invariants against replayed state, and appends events. It is the
only writer.

**Shape** (everything but `handle` is optional):

- `query(input)` returns the boundary as **typed clauses**: an event definition called with the
  fields to match, e.g. `user_registered(email = input.email)`, or a list of clauses OR-ed together.
  Within a clause, fields AND; a bare `TaskCreated()` matches every event of that type; `all_events()`
  matches everything. Constraining a field is a subset match, so over-constraining silently matches
  nothing (which `hekla check` warns about). A subject-encrypted field can only be filtered when its
  subject is also constrained (scoped) or it is `unique` (global); see section 15.
- `initial` is a literal value producing the fold's starting state, never a function: it sees no
  input, no clock and no randomness, so it can only be a constant, and a module-level expression
  already covers everything a function could compute.
- `fold` reduces the boundary's events into decision state, and **returns the new state rather than
  mutating the one it was handed**. It is a dict mapping query clauses to functions, which dispatches
  per clause instead of branching on `event.type`:
  ```starlark
  fold = {
      order_placed(): lambda state, event: dict(state, taken = True),
      shop_suspended(): lambda state, event: dict(state, suspended = True),
  }
  ```
  Keys are clauses built from the loaded definitions, not type strings, so a typo fails at `load()`
  and the only-the-registered-definition rule reaches dispatch too. The key language is the same one
  `query` uses and the same one a projector's or effect's `handle` uses: always a call, never a bare
  definition, and a constraint is allowed (`order_placed(status = "cancelled")`). A `fold` key can
  only filter on constants, since it is module-level, so this is worth reaching for on enum-shaped
  fields and not much else. `all_events()` is the clause that folds every boundary event.
  An event type with no entry leaves state
  unchanged, but is still read into the boundary and still counts toward the append condition. That
  is a normal shape rather than an oversight: **the boundary and the fold answer different
  questions.** The boundary is the append condition, so a type belongs there whenever a concurrent
  write of it should make this command fail; the fold is the decision state, so a type belongs there
  only when `handle` needs to know about it. `commands/rename-user.star` in `examples/users` is the
  case: renames are in the boundary so two concurrent renames conflict, and the fold has no arm for
  them because `exists` is already settled by the registration. `hekla check` therefore does not
  report a boundary type with no entry. It does report the other direction, an entry for a type the
  boundary never returns, which is dead code. That cross-check is by type only: `query` is evaluated
  against a placeholder input, so an arm made dead by its *constraint* rather than its type is not
  visible to it.

  Build the new state with `dict(state, taken = True)`, or `dict(state, **{key: value})` when the key
  is computed. `initial` is a frozen module global, so a fold that assigns into `state` fails on the
  first event it sees; once an arm has returned a dict it built, mutating that one *between arms* is
  its own business, because the value is discarded at the end of the fold either way. What the fold
  hands out is frozen before `handle` sees it. starlark-rust exposes no freezer mid-evaluation
  (`Freezer::new` is crate-private and freezing rewrites the source heap), so freezing an arm's
  intermediate result is not available; the contract between arms is carried by the first-event
  failure, the `must return the updated state` error on a `None` return, and this paragraph.
- `handle(input, state)` decides and returns one of three terminal outcomes. It is always a single
  function: it decides from input and folded state rather than from one event, so per-type dispatch
  belongs on `fold`.
  - an event, or a list of events, appends them (an empty list means "nothing to append", valid for
    an idempotent command).
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
concurrent write inside the boundary fails the append, and the attempt is retried in place.

**A retry costs the delta, not the boundary.** Each attempt keeps the state it folded and the last
position that state covers; the next one reads strictly after that position and folds what landed
onto the state it already has, rather than replaying the boundary from zero. The fold is a left fold
over an append-only log, so folding `[0, a]` and then `(a, b]` gives the state folding `[0, b]`
would. The attempt loop lives in `dispatch` rather than in the runtime so the work that does not vary
between attempts (the input struct, the boundary, the lowered `fold` plan, whose clauses cost a
keystore lookup and a deterministic encryption each when a field is subject-scoped) is done once per
request. The runtime still owns the *policy*: how many attempts, and how long to wait between them.

**The carried state is frozen, so `handle` cannot mutate what the next attempt folds onto.** Each
attempt folds in a scratch heap and freezes the result: an assignment into `state` inside `handle`
fails with `Immutable` and a message naming the reason, exactly as it already did when the boundary
was empty and `state` was the frozen `initial`. Without the freeze a mutating `handle` would corrupt
every later attempt and commit straight past the boundary, silently and with a 200.

**A fold's live heap does not grow with the boundary's depth, so per-event cost stays flat.**
Starlark collects only when executing a statement at the root of a module, and a fold loop never
executes one, so *nothing a fold allocates is released until its heap is dropped*: every event
struct, every string, and every superseded state from `dict(state, ...)` survives to the end. One
heap for the whole boundary therefore costs memory linear in its depth, and once that working set
outgrows the cache the cost per event stops being flat, which turns a linear fold into a
quadratic-looking one. So a fold is not one pass over one heap: it runs in chunks, freezing the
state and dropping the scratch heap every `HEKLA_FOLD_HEAP_BUDGET` bytes (1 MiB by default), then
thawing that state into the next chunk. The events die with each chunk. The seam is sound for the
same reason the retry carry is, and the read is planned once before the first chunk, so the whole
fold still runs against a single pinned watermark and reports one position for the append condition.

The per-chunk states are not free, and it is worth being exact rather than claiming a flat bound.
Thawing the carry adds a reference to the previous chunk's frozen heap, and freezing keeps every
referenced heap alive, so the states form a chain released only when the fold ends. What bounds it is
a ratio rather than a constant: a chunk must be at least eight times the size of the state it
carries, so the chain can never exceed an eighth of what folding the whole boundary in one heap would
have held. A fold over a few scalars chunks at the configured budget; one accumulating a large dict
chunks less often, which is the right trade, since that is exactly the fold whose per-chunk copy is
expensive. Tuning the budget *down* to save memory therefore backfires, and it has a floor for that
reason.

**Built-in idempotency key** is distinct from id-based dedupe. It exists for commands where nothing
in the input distinguishes intent (approving a claim twice with identical input could be one retry
or two deliberate approvals, and no domain check can resolve that). `execute` accepts an idempotency
key; the runtime hashes it together with the command name into a reserved `_hekla_idem` tag, stamps
every emitted event with that tag, and guards the append against the tag existing anywhere in the
log. The guard is whole-log rather than scoped to the boundary, so a duplicate that committed
anywhere is caught even once the boundary's `after` has moved past it, and it is asserted by the
append itself, so there is no read-then-write window. When it fires, the runtime re-reads by the tag
and returns the original commit's events and identity verbatim instead of re-running `handle`. A
first attempt that rejected appended nothing and so left no tag, so a retry re-decides and returns
the same rejection unless state moved; a reject that folded state is still checked against the tag,
so a duplicate racing an in-flight commit recovers that commit rather than reporting a spurious
refusal. Hashing the command name in keeps the same key on two commands from colliding, and keeps
the tag fixed-length whatever the client sent. Nothing is stored outside the log, so nothing has to
be swept: exactly-once is a property of the append. The client's raw key never reaches an event; the
derived tag lives in the reserved `_hekla_` namespace, which no event definition can emit and no
`query()` can name, so request plumbing never becomes domain vocabulary.

**Commands never invoke commands.** Sharing a boundary would make the callee's query a lie, and
separate appends give partial failure with no rollback. Chaining goes through the log: a command
emits an event, an effect reacts, and the effect invokes the next command. That path is durable and
independently retryable.

## 6. Projectors

A projector consumes events and builds a queryable read model.

**Shape**: entities are declared with `entity(key, fields, indexes)` and collected implicitly from
module scope. `handle` returns `put` / `patch` / `delete` ops, and may call `get(entity, key)` to read
the current row first. It is a dict mapping query clauses to functions, and **the keys are the
subscription**: they say which events to read and what to do with each, so there is no second list
beside them to keep in step. Several clauses may name one event type, and **every arm whose clause
matches runs, in declaration order**. No arm can be shadowed by an earlier one, so order fixes only
the sequence of ops, never which arms run.

```starlark
handle = {
    order_placed(): lambda event: [put(orders, {...})],
    order_placed(shop_id = 1): lambda event: [put(shop_one_orders, {...})],
    order_cancelled(): lambda event: [delete(orders, event.data.order_id)],
}
```

A key is always a call, never a bare definition: one spelling covers the unconstrained and the
constrained arm, and it is the spelling `query` and `fold` already use. `all_events()` is the clause
that selects everything, so `{all_events(): on_any}` is how one body handles every event. A
multi-statement handler is a named `def` referenced from the map, which puts the subscription and the
handler name on one line and costs no more than a separate subscription list did.

An arm's clause is matched by tephra's own `Matches` predicate, over the very `QueryItem` the
subscription lowered it to, so an arm's filter and the subscription's filter are the same code and
cannot drift apart. Matching happens before the payload is decoded, so an event no arm selects costs
nothing.

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
head even when the latest events are ones its subscription does not select, so a selective projector reads
as caught up (honest `/status` lag, and read-your-writes resolves against it) rather than stalling at
its last matching event. That empty-batch advance persists on its own, outside any op transaction.

**Storage**: one SQLite database per projector, holding both the read-model tables and the
checkpoint. Co-location is what makes the single-transaction commit possible. The projector thread is
the only writer, running the database in WAL so the read API can open read-only connections
concurrently. Replay is rebuild-and-swap: build a fresh database from position 0, seal it (fold its
WAL back into the file and drop to rollback mode so it is self-contained), then rename it in, so state
and position move together atomically and a reader that opens the file mid-swap never sees a torn one.

**Definition reconcile and readiness**: a projector records the hash of its *definition* (its
subscription and entity schema, not its handler bodies) inside its read model. At startup the recorded hash is
compared with the current one before the projector's handle is published, because the read API builds
its `SELECT` from the current entity definitions while the database on disk still has the previous
shape, and `CREATE TABLE IF NOT EXISTS` will not add a column to an existing table. Comparing is one
small read; only the replay it may imply is slow, and that stays on the projector thread, so boot
never blocks on log length. Each projector therefore carries a readiness:

- `ready`: the on-disk model matches the current definition, and reads are served normally.
- `rebuilding`: the definition changed and a rebuild is in flight. Reads of that projector answer
  `503` with a `Retry-After`; every other projector keeps serving.
- `stale`: the same mismatch with `[projectors] auto_rebuild = false`, so only an operator resolves
  it. Reads answer `503` naming `POST /projectors/{name}/replay`, and the thread idles rather than
  applying batches, since a batch built from the current entities would fail on a missing column.
  The definition hash is deliberately left unrecorded, so the mismatch stays visible until a replay
  actually rebuilds the model.
- `rebuild_failed`: a rebuild ran and failed. Like `stale` it needs an operator, but the cause is an
  error rather than a setting, so `last_error` names it and reads answer `503` pointing there. The
  thread survives the failure and idles, so `POST /projectors/{name}/replay` retries in place once
  the cause is fixed; a rebuild that took the thread down instead left the read API promising a
  `rebuilding` retry that nothing would ever satisfy, recoverable only by restarting. A replay is
  attempted against whatever survived on disk, since the atomic swap is the rebuild's last step and
  may well have landed.

Readiness is reported per projector in `/status` alongside `position`, `lag`, `running` and
`failed`. `running` is what separates a projector that idles awaiting an operator from one whose
thread is gone: a replay posted to the latter is refused with `503` rather than accepted and
dropped.

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

**Model**: `handle` is straight-line blocking code that calls injected impure builtins, and takes the
same shape a projector's does (section 6): a clause-keyed dict, whose keys are the subscription and
where every matching arm runs in declaration order. Declaration order is what makes fan-out replay-safe: several arms in one invocation journal
their calls in a fixed sequence, so a replay reproduces it exactly.
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
completed invocations older than a configurable retention window (`hekla.toml`), in bounded chunks
so one sweep never holds the connection across a long scan.

Sweeping is lazy GC, but it is not unconditionally safe, so the delete is bounded by the effect's own
cursor: it reclaims only positions at or below the persisted watermark. The driver completes
invocations one position at a time and advances the watermark only once the whole batch is terminal
(and an interrupted pass does not advance it at all), so a crash or shutdown mid-batch leaves terminal
positions *above* the watermark. Those are exactly the positions the next boot replays; reclaiming
them would drop the journal that makes the replay a no-op, and every side effect in them would fire a
second time. Within the bound the original reasoning holds: because the terminal record step is
journaled, a crash between the append and the delete replays the terminal step rather than
double-applying, so the sweeper only reclaims space. An effect that has never persisted a cursor is
never swept, matching the resume path, which treats a missing cursor as position 0 and replays from
the start of the log. The cost of the bound is retention: rows above the watermark, and everything
belonging to a permanently wedged or removed effect, are kept past the window until the cursor moves
over them.

**Builtins (v1)**: journaled `http.{get,post,...}`; `invoke_command(name, input)` targeting a public
or internal command; `now()`; `log()`; `reveal()` and `erase()` (section 15). Every one is a real
side effect or an unrepeatable observation, which is what earns it a journal entry. There is no
effect-local SQLite and no way to read a projector.

**State**: an effect declares `query` / `initial` / `fold` exactly as a command does, with `query`
taking the triggering event where a command's takes `input`, and each `handle` arm receiving
`(event, state)`. The fold is bounded at the effect's own position, inclusive, so `state` is a pure
function of the log prefix and that position.

That bound is the whole design. Because the state is derived rather than observed, it cannot race a
projector, it is identical on every attempt and every replay, and it needs no journal entry: there
is nothing to record that re-folding would not reproduce. An earlier design gave effects a journaled
`read(projector, entity, key)`, which forced a choice between replay determinism and fresh data and
resolved it badly: a read that missed because a projector was behind journaled `null`, and every
retry then replayed that null, so a transient lag became a permanent wedge only an operator skip
could clear. Folding the log has no such failure mode. Its cost is that a wide boundary is re-folded
per invocation, and effects are sequential, so that comes off throughput; a boundary keyed on an
entity id is as important here as it is for a command.

**Writing outcomes**: effects do not append events; they `invoke_command`, and that invoke is a
journaled, idempotent side effect, so durable domain facts (tracking numbers, external ids) land
exactly once across replays. The idempotency key an effect passes is deterministic, so the target
command tags every event it emits with the tag derived from it and guards the append against that
tag. A replay (or a crash between the command's append and the effect's journal write) finds the
prior commit by that tag and returns its recovered outcome without re-running the command:
exactly-once is enforced by the event log itself, not by any op-DB reservation. This is the same
mechanism, and the same guarantee, as for HTTP commands. A command rejection is a normal terminal
outcome, not a retryable failure: if a completion command rejects because state moved on (the claim
was already cancelled, the order already fulfilled), the runtime records the rejection in the
journal and completes the invocation. Treating rejection as retryable would loop forever on
legitimately-stale completions.

**Retry split**: the runtime absorbs transport errors and every retryable status (408, 425, 429 and
any 5xx) with backoff, and those never reach the script. A result that reaches Starlark is therefore
always terminal, so `status >= 400` in a handler is a real, decide-what-to-do failure rather than
something every effect re-implements. The split has to fall there rather than in the script, because
every response that reaches a handler is journaled: an effect that raised on a 429 would replay the
recorded 429 on every retry, never re-send, and wedge until an operator skipped it and dropped the
work. A `Retry-After` on a retryable response raises that attempt's backoff (delta-seconds only,
capped at five minutes) so a limiter's own window is waited out rather than hammered; it never
lowers the backoff, so a limiter repeating `Retry-After: 1` still gets exponentially rarer attempts.

**Wedging and the skip hatch**: transport errors, retryable statuses, and a raised handler all wedge
the invocation.
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
number of effects; the configured blocking-pool size (`hekla.toml`) is validated but reserved for a real
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
  hekla.db                # shared operational DB: effect journals, subject keys, module metadata
```

Backup is "copy the directory". Projector databases are rebuildable from the log regardless, so a
consistent copy is not required for them.

## 10. HTTP API surface (v1)

- `POST /commands/{name}` executes a command (public commands only), accepting idempotency-key and
  correlation-id headers, and echoes correlation and causation. The outcome maps to status: committed
  to 200 (with the appended positions), `reject` to 422, `invalid_input` to 400, and a DCB
  concurrency conflict that survives retries to 409. An idempotency key whose first attempt
  committed replays that commit's response, positions and original correlation and causation
  included; a key whose first attempt rejected has nothing in the log to replay, so it re-decides.
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
- **`GET /admin/*`: read-only introspection.** Page and filter the event log, follow a
  `correlation_id` through the whole causal chain it set off, read an effect invocation's journaled
  calls and their recorded results, inspect a projector's entities and definition hash, and read back
  the loaded project and effective configuration. Every route is a `GET`; the mutating operator
  routes stay outside the prefix. Always served, because the bind address is already the boundary for
  a surface that appends events without authentication, and one prefix is what a proxy can deny.
  Subject-scoped fields decrypt by default: the same kind of boundary the read API already crosses,
  over a wider surface (every field of every event, rather than the columns one projector chose to
  materialise), which is why a decrypting request is audited. A journaled call's *arguments* are never
  stored, only hashed, so introspection cannot resurrect plaintext an erasure was meant to shred.
- An admin-only, read-only SQL endpoint behind a flag, off in production, for debugging.
- Direct SQLite file access is not a supported surface. The table layout stays private behind the
  generated read API.
- `GET /openapi.json` and a Scalar reference over it at `GET /docs`. The document is generated from
  the loaded project rather than maintained by hand, and covers every route above: one concrete path
  per public command, two per projector entity (with the key typed from the key column and one query
  parameter per filterable field), and the operator endpoints. Internal commands are absent because
  they are not routed.

  Everything is generated through one function pair, `openapi::Surface::from_project` and
  `openapi::build`, which the runtime calls at startup and `hekla openapi` calls without opening a
  data directory. Two generators would be two things to keep true; one means the served document and
  a spec committed from the CLI cannot disagree, and a test asserts they are the same value.

  Event schemas are the one part of the document that is not a request or response body. An event's
  fields never reach the wire (a command's 200 reports each emitted event as its type and its
  plaintext tags), so `components/schemas/event.*` documents what the log holds and says so in its
  own description. The declared event set does become load-bearing in one place, as the `enum` of
  `EmittedEvent.type`.

## 11. CLI and dev loop

- `hekla serve <dir>`: run the runtime and HTTP API from a project directory, loading at startup.
- `hekla check <dir>`: the only static analysis Starlark gets, so it is thorough. It parses,
  resolves the load graph, verifies every query filters on tags the event type actually declares,
  verifies event constructors match field schemas, and verifies projector indexes reference declared
  fields. For CI and pre-commit.
- `hekla test <dir>`: events in, assert what the module did, for all three kinds. Every case seeds a
  throwaway store with `given` and then runs one module against it: a **command** produces events, a
  rejection or invalid input; a **projector** produces the rows the read API reads back (subject
  columns decrypted, as `GET /read/...` would return them); an **effect** produces the ordered
  sequence of `http_call(...)`, `command_call(...)` and `erase_call(...)` it made, with `responds`
  stubbing the HTTP replies (its state comes from folding the seeded `given` log, section 7).
  Pure functions with declared inputs make the harness small, and it is what earns trust in an
  untyped language. Everything a handler can observe is pinned so a case is reproducible: the clock,
  the master key, each `given` event's `event.id` (counting from
  `00000000-0000-0000-0000-000000000001`, so an id derived with `uuid5` is assertable), and its
  `event.timestamp`, which is the same fixed clock. A case tests
  the author's logic, not the runtime around it: batching, checkpoints, retry, the journal and
  replay are covered elsewhere.
- `hekla verify <dir>`: the runtime invariant sweep over a data directory. Section 11.2.
- `hekla fmt`: starlark-rust ships a formatter, and indentation is syntactically meaningful.
- `hekla lsp`: the language server, over stdio. Section 11.1.

### 11.1 The language server

Hekla modules are Starlark, but not *generic* Starlark, and that is precisely why hekla has to serve
them itself. Two things a general-purpose Starlark server cannot know: **which builtins are in scope
depends on the directory** (a projector has `get` and no clock, an effect has `http` and a journaled
one, a test file has `case`), and **`load()` resolves against the project root** under the
`events/`-or-`lib/` restriction. Point a Bazel-flavoured server at a hekla project and every builtin
reads as undefined and every import resolves to the wrong place. `hekla lsp` is built on
[`starlark_lsp`](https://github.com/facebook/starlark-rust), which supplies the protocol; hekla
supplies the language knowledge.

What it does:

- **Diagnostics**, in three tiers. Parse errors; then hekla's `load()` rules and name resolution
  against the directory's own builtins; then, unless `--no-project-checks`, the file evaluated
  against the project's `events/` and `lib/` modules with the same shape and clause checks
  `hekla check` runs. The governing rule is that it never reports a problem `hekla check` would not,
  which a test asserts against the shipped examples.
- **Hover** on any builtin, from the same doc comments the runtime carries.
- **Goto-definition** into a generated stub for a builtin, and into the real file for a `load()`.
- **Completion** of the directory's builtins, and of `load()` paths, which offers exactly the
  loadable modules, turning the restriction into a list rather than a rule to trip over.

It does **not** do formatting, rename, document symbols, semantic tokens or code actions: the crate
advertises none of them. Use `hekla fmt` (or buildifier) as an editor format-on-save task. It also
does not re-diagnose a file's dependents when a shared module changes, and it has no file watching;
on-disk changes are picked up by a short poll. Whole-project diagnostics remain `hekla check`'s job,
since the protocol only publishes for open documents.

Editor setup. The server takes no project directory: each open file is placed in its own project, so
one session can span several (this repository's `examples/` holds two).

- **Helix**: this repository carries `.helix/languages.toml`; copy it into a project to use it there.
- **Neovim**: `vim.lsp.start({ name = "hekla", cmd = { "hekla", "lsp" }, root_dir = ... })`, or a
  `configs.hekla` entry for `lspconfig` with `filetypes = { "starlark" }`.
- **VS Code / Zed**: any generic LSP bridge extension, with the command `hekla lsp` for `.star` files.


### 11.2 Invariant checks

`hekla check` is static analysis; this is its runtime counterpart. The log is append-only, so the
faults worth spending verification on are the ones nothing can undo: an event that should never have
been appended, an effect that fired twice. A wrong read model, by contrast, is a rebuild. The checks
follow that asymmetry.

Four invariants. Three are reported as a `verify::Violation`; fold determinism is not,
because there is no safe way to continue from it.

- **Rebuild equivalence**: a projector rebuilt from position 0 matches the live one row for row.
  Compared exactly rather than approximately, because subject encryption is deterministic AES-SIV and
  a projector stores the event's ciphertext verbatim. The rebuild is **bounded at the live model's
  own checkpoint**: building to head instead would compare a shadow that absorbed the whole log
  against a projector that was merely lagging when the server stopped, and report the gap as
  corruption.
- **Replay equivalence**: a recorded invocation re-run from its journal reaches the same calls, in
  the same order, and performs none of them. It runs against a **sealed** host, which serves journal
  hits and turns a journal miss into a violation rather than a call. That is load-bearing rather than
  tidy: the divergence being hunted is exactly the case where a naive replay would fire a real side
  effect, so the check must be incapable of causing it. The sequence is compared as an ordered list,
  which is the part the content-keyed journal cannot see for itself.
- **Fold determinism**: the same boundary at the same position folds to the same state. Section 7's
  claim that state can be derived rather than stored rests entirely on this. The second fold is
  bounded at the position the first one reached, so a concurrent append reads as ordinary DCB
  contention rather than as nondeterminism.
- **Checkpoint monotonicity**: no position reached by *tailing* moves backwards. A rebuild replaces
  the model, so it publishes its checkpoint without that guard: a bounded rebuild legitimately lands
  behind, and treating that as a violation stopped the projector while leaving it readable.

Two entry points over one set of checks. `hekla verify <dir>` sweeps offline and exits non-zero on a
violation, for CI or a nightly job; it takes the data-directory lock, so the documented shape is to
verify a copy of the directory, which exercises the backup at the same time. `serve --verify` (or
`[verify] enabled` in `hekla.toml`) runs the per-operation half continuously. `hekla test` always
checks folds: a scenario is cheap, and it is where a nondeterministic fold should surface first.

A violation **quarantines the component**: it stops advancing, `/status` names what broke, and the
rest of the runtime keeps serving. A quarantined projector's reads return 503 rather than its rows,
because what a failed check calls into question is precisely the rows and the position, and a
read-your-writes wait against a position that moved backwards would resolve on a lie.

Rebuild equivalence is offline only: it costs a full log replay, and against a live projector the
shadow model would race the one it is comparing to.
## 12. Why Starlark (determinism and purity)

umari pins the wall clock and zeroes the monotonic clock to make commands deterministic, and polices
nondeterminism with static analysis and linters. Starlark makes it structural: no clock, no
randomness, and no I/O except injected builtins, so every nondeterministic input is either absent
(commands and projectors) or journaled by construction (effects). Deployment is source text: no
toolchain, no compile cache, parse-and-freeze in milliseconds.

## 13. Non-goals

**Deferred** (see the roadmap, each with a trigger): metrics and Prometheus; partition-key parallel
effect lanes; an upload API with versioning, pinning, and retention, plus hot reload; a fold
library; a workspace crate split.

**Permanent commitments** (not deferrals, and not to be reopened): Starlark is the only authoring
surface. There is no Rust, TypeScript, or WASM SDK path now or later. This is deliberate: a single
pure, sandboxed authoring language is what makes determinism structural, deployment source text, and
the durable-effect journal sound. Multi-language authoring is permanently out of scope.

## 14. Code layering

hekla is a single crate. The dependency direction is documented and enforced by discipline,
revisited only when a seam proves real (embeddability, or compile times that actually hurt):
`starlark_builtins` and `schema` depend on nothing internal; `dispatch` depends on those; `runtime`
(projectors, effects, journal, storage) depends on `dispatch`; `verify` and `introspect` sit above
the runtime, reaching into the projector, effect and storage paths they read; `api` and `cli` sit on
top. `lock` depends on nothing internal.

## 15. Subject-scoped encryption and erasure

A field marked `subject = "sibling_field"` is encrypted under a key scoped to that subject's identity
`(subject_field, subject_value)`, in the tag index, the event payload, and any read-model column, all
before it reaches tephra. **Erasing a subject is deleting its key**, one O(1) operation that makes
every value scoped to it unmatchable and unreadable across the log and every read model at once, with
no rewrite, compaction, or index rebuild.

**Two ways to erase**, the same key delete either way. `hekla erase <field> <value>` is the operator
path, for a one-off request handled by hand. `erase(subject_field, subject_value)` is the effect
builtin, for erasure driven by an event: a provider webhook, a retention deadline, an
`account.closed` your own command emitted. It is journaled like every other effect side effect, and
idempotent besides, so a replay neither repeats the deletion nor reports a different answer than the
first run saw.

Erasure from a handler has two ordering rules, both consequences of `reveal()` deliberately not being
journaled (it re-decrypts every attempt, which is what makes an erased subject fail rather than
replay stale plaintext):

- **Erase last.** An invocation that reveals a subject and then erases it cannot be replayed: the
  replay re-runs `reveal` against a key that is gone. That is a terminal skip, not a wedge, so the
  position is completed and the effect advances, and journaled calls made before the erase stay done
  rather than re-firing. But the work after the reveal does not run on that replay.
- **Do not read a subject to decide whether to erase it.** A handler needs the subject ids it erases
  in plaintext, from a declared field or a read model, not from a value scoped to the key it is about
  to destroy. Otherwise a second erasure request for an already-erased subject cannot be read at all.

**Erasure is a point-in-time shred, not a tombstone.** A later event writing a subject-scoped field
for the same subject mints a fresh key (`encrypt_subject` creates on first use), so values written
after the erase are readable while everything before it stays shredded. A read path never resurrects
a key: `encrypt_subject_existing` returns `None` for a missing subject and the clause is lowered to
match nothing.

**Mechanism.** Encryption is deterministic (AES-SIV): the same plaintext under the same key and field
yields the same ciphertext, so it works as an equality-matchable tag while staying decryptable. Each
per-subject key is a random secret stored in `hekla.db`, wrapped with AES-256-GCM under a master key
from `HEKLA_MASTER_KEY` and tagged with the wrapping master's id so masters can rotate online:
`hekla rotate` rewraps every row under a new `HEKLA_MASTER_KEY`, unwrapping with
`HEKLA_MASTER_KEY_PREVIOUS` as needed, without touching any ciphertext. The global uniqueness key
behind `unique = True` is a wrapped reserved secret, so rotation never changes global tags.

**Information flow.** Plaintext of a subject field exists only at the HTTP command input (the client
supplied it) and at read-API output or an effect's `reveal()` (the runtime decrypted it). Everywhere
between (log, tag index, read-model columns, and every `fold`/projector `handle` body) it is
ciphertext. A handler reads a subject field as an opaque handle: it can store it (`put`/`patch` keep
the ciphertext) and compare it for equality, but not concatenate, slice, or otherwise derive a
plaintext string from it. Because read models store ciphertext and the read API decrypts on the way
out, deleting the key shreds the log and every read model together. A derivation a handler wants must
be computed by the command and emitted as its own subject field. An effect crosses the boundary
explicitly with `reveal(handle)`; a `reveal` of an already-erased subject fails terminally (no retry
can recover the data).

**Per-field, not per-event.** An `order.placed` has both a customer and a shop, so scoping the whole
event to one destroys the other's record on erasure. Per-field puts `email` under the customer key
and `order_total` under the shop key, and leaves the ids plaintext.

**Accepted limitations** (documented, not discovered):

- **Subject id fields survive erasure.** A subject cannot be encrypted under its own key (you need it
  to find the key), so after erasure the log still shows `customer_id:42` with the personal fields
  unreadable. Standard crypto-shredding.
- **Deterministic encryption is searchable encryption.** It leaks equality and frequency: an observer
  with index access learns which events share a value. Fine for high-cardinality ids; do not give a
  low-cardinality field (a status enum) a subject.
- **`unique = True` keeps a global token past erasure.** After erasing a subject, a global-key tag
  still proves "some subject once used this value" without revealing it, and the global index is
  dictionary-attackable if the master key leaks. It is strictly weaker than the per-subject case, so
  it is opt-in per field.
- **A field appended without a subject cannot be erased** until a segment-rewrite tool exists (out of
  scope): its plaintext is already in the log payload and tag index, and replaying projectors just
  re-reads it. `hekla check` warns when a personal-looking field name has no subject.
- **Range predicates over encrypted tags are foreclosed** (tags are equality-only anyway).
- **One subject per field**; genuinely joint data (a message between two people) is deferred.
- **Effect external sinks are outside the boundary.** Erasure shreds hekla's own store; it cannot
  un-send an email an effect already delivered. The effect journal holds revealed plaintext only
  transiently, until the retention sweeper reclaims the completed invocation.
- **Losing `HEKLA_MASTER_KEY` is total, unrecoverable loss** of every subject-scoped value. Boot fails
  fast with a subject-specific message when a project uses subjects and the key is absent or wrong.
