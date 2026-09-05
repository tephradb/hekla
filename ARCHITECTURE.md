# hekla architecture

hekla is a rewrite of [umari](https://github.com/tqwewe/umari), a Rust event-sourcing / DCB
(Dynamic Consistency Boundary) runtime where you author **commands**, **projectors**, and
**effects**. umari runs those as WASM component modules (Wasmtime, WIT, Rust/TS SDKs) on the
[tephra](https://crates.io/crates/tephra) event log. hekla keeps umari's conceptual model and its
tephra + SQLite substrate but replaces WASM with **[heklang](../heklang)**, a language built for
this shape of program and nothing else.

The swap is not cosmetic. heklang has no clock, no randomness and no I/O except through a host seam
it declares, and its three declaration kinds are the three module kinds: a command may not call out,
a projector may not decrypt, only an effect journals. That makes determinism structural rather than
policed, makes deployment source text (no build step, no compile cache), and enables a
Temporal-style durable-execution model for effects that umari could not express.

hekla was written against embedded Starlark first, and the port to heklang is finished. Where this
document says what changed, it is because the difference is worth knowing rather than for history.

This document describes the system as designed. Delivery is phased in [ROADMAP.md](./ROADMAP.md).

## 1. What hekla is

A single-app, event-sourced runtime. Business logic is written as small heklang files (`.hk`),
declaring commands, projectors and effects over an immutable tephra event log. Commands are the only
writers. Projectors build queryable SQLite read models. Effects react to events with durable,
replay-safe side effects. Determinism and sandboxing come from the language, not from runtime
policing: what a declaration may do is a property of what kind of declaration it is.

## 2. Concepts and vocabulary

One word per concept, no synonyms.

- **Event**: an immutable fact in the tephra log. Carries a type, tags, and a JSON payload, plus a
  host-stamped envelope (below). Events are the single source of truth.
- **Tag**: the routing vocabulary everywhere, matching DCB and tephra. Tags are `key:value` pairs
  stored and indexed on events. Authors never write tag strings: event definitions declare which
  fields are tags, emitting auto-tags them, and queries pass structured `tags = {...}` that the
  runtime encodes. There is no separate "domain ID" term; a tag is a tag.
- **Slice**: an event type plus the filters that narrow it, resolved to literal values. A slice is
  what a `fold` declares and what an append conditions on.
- **Consistency boundary**: the set of events a command reads, which is exactly the slices its
  `fold` declarations name. Optimistic concurrency is enforced by appending under a tephra
  `AppendCondition` over the same slices: **what you folded is what you conflict on**.
- **Command, Projector, Effect**: the three declaration kinds (sections 5 to 7).
- **Fold**: one keyword for both halves, because they are one construct: a `fold` declares the slices
  it reads *and* the reduction it performs over them to recover decision state. A boundary and the
  state derived from it therefore cannot drift apart. It was spelled `state` until heklang collapsed
  the two words, which is why the state a fold produces has no keyword of its own.
- **Read model**: a projector's SQLite database. A rebuildable cache, never a source of truth.
- **Journal**: an effect's durable record of its side-effect calls, keyed by content hash, used to
  replay the effect deterministically after a crash. Lives in the operational DB, never the log.
- **Envelope**: host-stamped per-event metadata: an `event_id`, `correlation_id`, `causation_id`, an
  optional `triggering_event_id`, and an append `timestamp`. The `position` is tephra's; the id is
  the envelope's, and a projector or effect arm reads both through its trigger binding (section 4).
  The client's idempotency key is not on the event; a reserved tag derived from it is (section 5).

## 3. Module layout and deployment

A project is one heklang program spread over files. **Three directories decide what a declaration is
allowed to be; the declaration decides its name; everywhere else is the author's.**

```
project/
  commands/            # enforced: public commands (HTTP-routed and invokable by effects)
  commands/internal/   # enforced: invokable by effects, NOT HTTP-routed
  projectors/          # enforced
  effects/             # enforced
  events/              # convention
  lib/                 # convention: guards, refusals, `fn`s, constants
  tests/               # convention: `hekla test` scenarios
  hekla.toml            # operational config (optional)
```

- **There is no import.** Every `.hk` file in the project is compiled together, so a command names
  an event declared three directories away without saying so, and file order is irrelevant. The
  Starlark version had a `load()` graph with a restriction (only `events/` and `lib/` were
  importable), a resolver, a module cache and a cycle check; all of it is gone, along with the
  class of error where a file was valid but unreachable.
- **A declaration is named by its declaration, not by its file.** `commands/place-order.hk`
  declaring `command PlaceOrder(...)` is routed at `POST /commands/PlaceOrder`. The file name is a
  convention this repository follows and the runtime does not read. Under Starlark the file stem
  *was* the name, so this changed every URL the port touched.
- **A directory is a rule only where being in the wrong one would change what the runtime does.**
  That is true of three: a command's directory routes it and decides whether anything can POST it,
  and a projector's and an effect's is what they are. It is not true of an event, a guard, a refusal
  or a `fn`, so hekla has nothing to say about where those live, and `events/` and `lib/` are what
  the examples do rather than what the loader checks. The earlier design had a directory whitelist
  deciding which files were *read*, which enforced none of this and could only fail one way: a file
  in an unlisted directory vanished, and what the author saw was every use of what it declared
  failing to resolve in files that were themselves correct. The whole tree is read now, and the
  convention is checked per declaration, where the diagnostic can name the file at fault.
- **`hek` and hekla agree on what the program is.** heklang compiles every `.hk` file under a path
  with no convention at all, and hekla now discovers the same set (less `.`-directories, `target/`
  and the runtime's `data/`). The only thing the two can disagree about is the three placement
  rules, which is the disagreement hekla means to have.
- **Public vs internal is structural, not a flag.** `commands/internal/` keeps effect-completion
  commands (for example `RecordShippingLabel`) off the HTTP surface, so nobody can POST a
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

An event declares its type and its typed fields; there is no `tags = [...]` list. **Every field is
automatically indexed as a store tag** unless it opts out with `@no_index`. Auto-tagging removes the
old failure where a field forgotten from the tag list was unqueryable forever (and adding it later
missed all prior events).

```
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
  email: String? @subject(customer_id) @max(200),
  // Free text nobody queries: opt out of tagging, and of being a huge tag.
  notes: String @max(2000) @no_index,
}
```

**Per-field annotations**:

- `@no_index` opts a field out of tagging (a large blob, or free text where a tag is useless).
- `@max(n)` bounds a string. It is checked at the write, and an over-length value is a command's
  `Invalid` rather than a crash.
- `@subject(sibling_field)` encrypts the field under a key scoped to that sibling's value; see
  section 15.

**Field types** are heklang's, and the same set describes an event field, a command parameter and an
entity column: `Bool`, `Int`, `Decimal(n)`, `String`, `Uuid`, `Timestamp`, `Money(n)`, an enum, a
record, `Json`, `List(T)`, `Map(K, V)`, and `T?` for an optional.

Two of these changed shape in the port and are worth naming:

- **`Money(n)` carries its scale in the type.** `Money(2)` and `Money(3)` are different types that
  read different values out of the same string, and the type is what says which. Money plus money is
  fine, money plus a bare decimal is not.
- **There is no unsigned type.** The Starlark version had `uint`, which landed in the same signed
  SQLite `INTEGER` as `int` and so had to reject anything above `i64::MAX` at the write boundary.
  `Int` is that range, with nothing left to fall off.

**There is no float type and there will not be one.** Binary-float rounding in an append-only log is
permanent, and auto-tagging a float needs an encoding that sorts lexicographically, the same problem
that stops `Money` from keying an ordered scan. Use `Money` for currency and `Decimal(n)` or scaled
integers for everything else. The one door left open is `Json`, which validates nothing by design.

**`emit` writes an event whole.** Every field the event declares is given, each of them once. There
is no partial event and no default: an event is a fact, and a fact with a hole in it is a different
fact. The same rule holds for a `put` into a read model and for an `invoke`'s arguments, and it is
checked in the same place each time, at the write against the declaration. Under Starlark the
*runtime* held it, which meant a command that omitted a field checked clean and failed at the
append.

**A field is read by name, against its declaration.** A handler binds the fields it wants out of the
triggering event (`on @order.placed { order_id, email }`), or binds the whole record with `as e` and
reads `e.order_id`. Either way the name is checked at compile time against the declaration, so a
typo is a parse error rather than a `None` that flows onward. There is no dynamic field access and
no subscript: the Starlark version needed a rule about which values read with a dot and which with a
subscript precisely because half of them had no declared shape.

The one genuinely shapeless value is an HTTP response body, which is a `Json` read through fallible
one-step accessors (`body.string("id")`, `body.int("count")`). Every read of an untyped body is a
branch anyway, and making that visible is the point.

**Envelope**: the tephra payload is a JSON envelope wrapping `data` with an `event_id`,
`correlation_id`, `causation_id`, an optional `triggering_event_id`, and the append `timestamp`. The
host stamps these at append; a program never sets them.

Three envelope fields are exposed through a trigger binding, beside the event's own fields: **`e.id`**,
**`e.at`** and **`e.position`**. All three are stamped once at append and never move, so a projector
rebuild and an effect replay see what the original append wrote. That stability is what makes `id`
the input to derive from, and what lets `at` be the source for a `created_at`-style read-model column.

**A command fold cannot reach them**, and that is deliberate rather than an omission: a fold arm
binds the event's declared fields and nothing else. A projector or effect arm has a trigger, so it
has a record; a fold has a stream of events.

**Prefer `e.at` over restating the clock.** A command using `now()` for a field that only records
when the event was appended duplicates what the envelope already holds. `now()` remains right for
time that is genuinely domain data and not the append instant (`expires_at`, `due_date`, a
`purchased_at` an upstream system reported).

The rest of the envelope (`correlation_id`, `causation_id`, `triggering_event_id`) stays host-side:
each would need its own argument for why a handler should branch on it.

**Deriving ids**: no declaration may mint a random one. Commands take new-entity ids from their
parameters (see section 5), and an arm that needs an id with no such source derives one with
`Uuid.derive(seed, name)`, RFC 4122 version 5, usually over `e.id`:

```
invoke RecordNotified {
  order_id: e.order_id,
  notification_id: Uuid.derive(e.id, "confirmation"),
}
```

The `name` argument is what lets one arm derive several distinct ids from one event. Randomness here
would not merely be unavailable, it would be wrong: a command retry and an effect replay both re-run
the code that mints the id, so a fresh id per attempt would turn one intent into several entities,
which is the same failure host-minted ids have. Deriving is the third choice, not the first: prefer
an identity that already exists (the entity the fact is about) or one an external system returned in
a journaled response.

**Compile-time validation is the reason event declarations are shared, and it is now the language's
rather than hekla's.** A slice that filters an event type on a field that type does not declare, or
declares `@no_index`, is a parse error with the field's own span. So is a filter on sealed content,
an `emit` missing a field, an `invoke` with an unknown argument, and an index over a column the
entity does not have. The Starlark version re-derived each of these in a validation pass over a
`query()` evaluated against a stub input, which could only see the branch the stub happened to take.

**Two host tags sit in a reserved `_hekla_` namespace** an author can neither emit nor query: a keyed
command's idempotency tag, and the correlation tag every event carries. The correlation id lives in
the envelope payload, but a store query filters on type and tags only, so without the tag a causal
chain could be found only by decoding every event in the log. Both are stripped from command
responses, and `hekla check` rejects the prefix on both sides.

Two representations are pinned so they are not decided inconsistently in two places:

- **`Money(n)` and `Decimal(n)`**: a string at scale `n` on the wire (JSON event payloads, request
  bodies and read-API responses), an integer count of minor units in storage. A string rather than a
  number so no precision is lost to a float on the far side, which is the same reason they are
  scaled integers here.
- **`Timestamp`**: epoch microseconds on the wire, RFC 3339 text in a SQLite column and in the
  envelope. The column form is what the read API serves and what sorts lexicographically, and the
  two are converted at exactly one seam. A command's request body takes **either**, converted at
  that same seam before heklang sees it: the form a client has in hand is the one a read handed it,
  and a body it could not post back would make the generated document's `date-time` a lie. A sealed
  timestamp column holds the *column* form too, so whether a field is personal never changes the
  shape a reader sees.

## 5. Commands

A command validates input, checks invariants against replayed state, and appends events. It is the
only writer.

**Shape**:

```
refusal TooManyOpen "too many open orders"

command PlaceOrder(order_id: Uuid, customer_id: Int, email: String?, total: Money(2)) {
  fold open_orders: Int = 0
    on @order.placed(customer_id) => open_orders + 1
    on @order.cancelled(customer_id) => open_orders - 1

  if open_orders >= 10 {
    return reject TooManyOpen
  }

  emit @order.placed { order_id, customer_id, email, total }
}
```

**`fold` is a read declaration, not a binding**, and it is the one thing about a command worth
understanding before anything else. A `fold` declares a **slice** of the log (an event type plus
the filters that narrow it) and folds it into a value. The slices are what the append conditions on,
so **what you folded is what you conflict on**. A `let` produces no slice and contributes nothing.

That collapses four Starlark constructs into one. `query` declared the boundary, `initial` seeded the
fold, `fold` was a dict of clauses to reducer functions, and the three could disagree: a fold arm for
a type the query never returned was dead code, a query with no fold read events nobody looked at, and
`hekla check` had a rule for each. None of those states is representable now, so none of those rules
exists.

- **A slice comes back resolved.** `@order.placed(customer_id)` leaves as `@order.placed` narrowed to
  `customer_id = 7`, because a filter is an expression the command evaluated and "which slice" means
  nothing to a host that did not compile the program. Resolving is what makes the condition
  answerable: it is the same shape a tag query has, so the host that appends against it can also
  index on it.
- **Filters are sorted by field name**, so one slice is one predicate however it was written.
- **`guard` is a `fold` that binds nothing**, for a decision that depends on a slice being empty
  when there is no value to keep. It is rarely what you want: a `fold` already contributes its
  slice, so guarding what a fold already covers adds a duplicate predicate and no safety.
- **A fold arm may not read the clock or call out**, because a fold is not journaled: every attempt
  re-folds and must get the same answer.
- **Execution order is fixed**: parameters bind, hoisted `let`s run, filters evaluate, seeds
  evaluate, `after` is taken, the fold runs in **one pass over the log** applying every matching
  slice per record, then the body runs. Ten folds over a million events read the log once.

**Three outcomes.** `return` with no value, or falling off the end, is `Ok` with whatever was
emitted; `reject <Refusal>` is a state-dependent refusal (422); `invalid(message)` means the input
is malformed regardless of state (400). The distinction is the caller's: a blank address is
`invalid` whoever sends it and whenever, a blocked customer is `reject` because the same request
would have succeeded yesterday. `reject` carries a code because there is something to branch on;
`invalid` does not, because there is nothing to say when the answer is "you sent nonsense".

**The 422 code is derived from the refusal's name**, not written at the throw site: `TooManyOpen`
becomes `too_many_open`. It is the one name in a heklang program whose spelling leaves the process,
so declaring it once is what stops two sites disagreeing about a code a client switches on.

`Conflict` and `Unavailable` have **no variant in the type at all**. Being beaten to the log is the
runtime's to retry and an author who saw it could only retry worse, so "retryable outcomes never
reach the handler" is unrepresentable rather than filtered. hekla still has both, because hekla is
the thing that retries.

**The append condition is returned with all three outcomes**, including the two refusals: a refusal
still read the log, and a host that wants to cache or trace the decision needs to know what it
depended on.

**Determinism**: a filter and a fold see no clock and no network. `now()` is available in the body,
pinned once per request so repeated calls agree. It is for time as domain data (`expires_at`,
`due_date`), not for restating the host-stamped append timestamp.

**Ids**: new-entity ids are client-supplied parameters. A retried request carries the same id, so the
command's own boundary rejects the duplicate, and idempotency for creation falls out of DCB with no
extra layer. The language mints no ids and has no randomness.

**Append and DCB**: emitted events are appended under an `AppendCondition` over the boundary. A
concurrent write inside the boundary fails the append, and the attempt is retried in place. The
attempt budget and the backoff are hekla's and reach the language as a callback; the decision, and
the attempt loop that carries state between attempts, are the language's.

**A retry costs the delta, not the boundary.** Each attempt keeps the state it folded and the
position that state covers, and the next one reads strictly after it, folding what landed onto what
it already has: a fold is a left fold over an append-only log, so folding `[0, a)` then `[a, b)` is
the state folding `[0, b)` would have given. Being beaten to the log on a boundary a hundred thousand
events deep therefore costs the handful of events that beat you. The carry lives in heklang's frame
because that is the only place folded state exists; hekla could not do it from outside, and for
one phase after the port it did not, so every retry re-read the whole boundary from position zero.

What the port *did* remove for good is the machinery that made the carry safe under Starlark: a
frozen scratch heap per attempt and the `Immutable` error that policed a `handle` writing into
folded state. heklang has no mutable binding, and the carry is taken before the body runs, so there is no
longer a way to write the bug they caught.

**A fold's cost is flat in the boundary's depth**, which under Starlark it was not. Starlark collects
only when executing a statement at the root of a module, and a fold loop never executes one, so
nothing a fold allocated was released until its heap was dropped: every event struct, every string
and every superseded state survived to the end, and once that working set outgrew the cache a linear
fold started looking quadratic. hekla answered that with a chunked fold that froze and thawed its
state every megabyte, four tuning constants and an environment variable. All of it is deleted.

**Termination is structural.** heklang has no `while`, rejects recursion, and iterates only finite
containers, so there is nothing to meter. The per-handler instruction budget went with the chunking.

**Built-in idempotency key** is distinct from id-based dedupe. It exists for commands where nothing
in the input distinguishes intent (approving a claim twice with identical input could be one retry
or two deliberate approvals, and no domain check can resolve that). `execute` accepts an idempotency
key; the runtime hashes it together with the command name into a reserved `_hekla_idem` tag, stamps
every emitted event with that tag, and guards the append against the tag existing anywhere in the
log. The guard is whole-log rather than scoped to the boundary, so a duplicate that committed
anywhere is caught even once the boundary's `after` has moved past it, and it is asserted by the
append itself, so there is no read-then-write window. When it fires, the runtime re-reads by the tag
and returns the original commit's events and identity verbatim instead of re-running the command. A
first attempt that rejected appended nothing and so left no tag, so a retry re-decides and returns
the same rejection unless state moved; a reject that folded state is still checked against the tag,
so a duplicate racing an in-flight commit recovers that commit rather than reporting a spurious
refusal. Hashing the command name in keeps the same key on two commands from colliding, and keeps
the tag fixed-length whatever the client sent. Nothing is stored outside the log, so nothing has to
be swept: exactly-once is a property of the append. The client's raw key never reaches an event; the
derived tag lives in the reserved `_hekla_` namespace, which no event declaration can emit and no
slice can name, so request plumbing never becomes domain vocabulary.

**The clause is hekla's alone.** heklang has no idea a request has a key, so the append condition it
returns carries only the slices; hekla adds the existence clause beside them. That split is why the
tag survived the port at all: it is not a property of the program, and nothing in the language would
have carried it.

**Commands never invoke commands**, and that is now enforced by the grammar rather than by a rule:
`invoke` is an effect construct and a command that writes one does not parse. Sharing a boundary
would make the callee's slices a lie, and separate appends give partial failure with no rollback.
Chaining goes through the log: a command emits an event, an effect reacts, and the effect invokes the
next command. That path is durable and independently retryable.

## 6. Projectors

A projector consumes events and builds a queryable read model.

**Shape**: a projector declares its entities and one handler per event it consumes. **The handlers
are the subscription**: they say which events to read and what to do with each, so there is no
second list beside them to keep in step.

```
projector CustomerOrders {
  entity Order {
    order_id: Uuid @key,
    customer_id: Int @index,
    email: String? @max(200),
  }

  on @order.placed { order_id, customer_id, email } {
    put Order { order_id, customer_id, email }
  }

  on @order.cancelled { order_id } {
    delete Order[order_id]
  }
}
```

Four write statements: `put` writes a row whole, `patch` materializes one from zeros if it is
absent, `update` skips if it is absent, and `delete` removes it. `patch` and `update` read the row
they write, and a stored `.field` load is filled before any value expression runs, so
`patch Totals["all"] { count: .count + 1 }` is the running-total idiom.

**Several handlers may name one event type, and every one of them runs**, in declaration order.
Fanning one event out to several read models is a real pattern, and a projector has no journal, so
nothing about ordering is dangerous: a rebuild replays every handler in the same order and reaches
the same rows. **Effects take the opposite rule** (section 7), and the difference is deliberate.

**There is no general read.** A handler cannot ask for an arbitrary row; the only read is the one
`patch` and `update` do of the row they are about to write. That is what makes rebuild determinism
structural rather than a property an author has to preserve: a projector that could read anything
could read something a rebuild has not written yet.

The Starlark version had `get(entity, key)` for exactly the read-modify-write cases `patch` now
covers, and it read through uncommitted writes in the current batch so that batching did not change
behaviour. `patch` reads through the same open transaction, so that property survives; what does not
survive is the ability to read a row this event is not about.

**A column's subject is propagation, not declaration.** No column is authored `@subject`; a column
that receives sealed content becomes sealed, which is why a projector can store a credential it may
never `reveal`. That closes a whole family of errors the Starlark version had to catch at runtime: a
handle filed under the wrong subject id, into a plaintext column, or into a column scoped to a
different subject. None of the three is representable when the column's scope is computed from what
is written into it.

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

**Definition reconcile and readiness**: a projector records the hash of its *definition* inside its
read model. The definition is heklang's digest entry for that projector, which covers its
subscription, its entity shapes **and** its handler bodies. At startup the recorded hash is
compared with the current one before the projector is published, because the read API builds
its `SELECT` from the current entity definitions while the database on disk still has the previous
shape, and `CREATE TABLE IF NOT EXISTS` will not add a column to an existing table. Comparing is one
small read; only the replay it may imply is slow, and that stays on the projector thread, so boot
never blocks on log length.

**The handler bodies are in it, and that is new.** hekla used to canonicalize the subscription and
the entity shapes by hand and leave the bodies out, because the only way to include them was to hash
source text and then every comment forced a full replay. The cost was that a *corrected* projector
changed nothing: the model kept serving rows the old logic had built while applying the new logic to
everything after the checkpoint, until an operator remembered `POST /projectors/{Name}/replay`. The
digest hashes what runs rather than how it is written, so both halves are now right: a reformat
rebuilds nothing and a fixed handler rebuilds. Because `const`, `refusal` and `guard` are inlined
before a program exists, a projector's hash also covers every one it reaches, so editing a shared
`const` rebuilds each projector that uses it. That is the correct blast radius and a wider one than
before.

Each projector therefore carries a readiness:

- `ready`: the on-disk model matches the current definition, and reads are served normally.
- `rebuilding`: the definition changed and a rebuild is in flight. Reads of that projector answer
  `503` with a `Retry-After`; every other projector keeps serving.
- `stale`: the same mismatch with `[projectors] auto_rebuild = false`, so only an operator resolves
  it. Reads answer `503` naming `POST /projectors/{Name}/replay`, and the thread idles rather than
  applying batches, since a batch built from the current entities would fail on a missing column.
  The definition hash is deliberately left unrecorded, so the mismatch stays visible until a replay
  actually rebuilds the model.
- `rebuild_failed`: a rebuild ran and failed. Like `stale` it needs an operator, but the cause is an
  error rather than a setting, so `last_error` names it and reads answer `503` pointing there. The
  thread survives the failure and idles, so `POST /projectors/{Name}/replay` retries in place once
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

**Model**: an effect is one or more **arms**, each straight-line blocking code over one event type,
and **the arms are the subscription**. Determinism under replay comes from a journal. Each impure
call looks itself up in the journal first: if a result is recorded, it returns that; otherwise it
performs the real call and appends the result. After a crash the arm re-runs from the top, replays
journaled calls until it passes the end of the journal, then resumes making live calls. There are no
step functions and no yielding: blocking a thread keeps evaluation state on that thread's stack, and
crashes are handled by replay.

```
effect NotifyCustomer {
  on @order.placed as e {
    fold orders: Int = 0
      on @order.placed(customer_id: e.customer_id) => orders + 1

    let response = http.post("https://mail.example/confirm", {
      "to": reveal(e.email),
      "order_id": e.order_id,
      "first_order": orders == 1,
    })

    if response.status >= 400 {
      fail("confirmation rejected")
    }

    invoke RecordNotified {
      order_id: e.order_id,
      notification_id: Uuid.derive(e.id, "confirmation"),
    }
  }
}
```

**One event selects exactly one arm**, and two arms naming one event type is a parse error. This is
the deliberate opposite of the projector rule, and of what hekla did under Starlark, where every arm
whose clause matched ran in declaration order. Three things went wrong with that:

- **Declaration order became load-bearing for replay.** Arms ran in the order they were written, so
  moving one changed which side effects were journaled before which.
- **The trigger binding became polymorphic.** An arm matched by two event types could only name
  fields common to both. An arm may still list several types explicitly, and then the restriction is
  visible where it is chosen rather than falling out of which clauses happened to match.
- **Every cross-arm static rule had to reason about the matched set** rather than about one arm.

A projector legitimately wants the other rule and keeps it: it has no journal, so nothing about
ordering is dangerous there.

**State lives inside the arm.** `as e` binds the trigger, and it is in scope for the arm's `fold`
filters and its body. There is no effect-level trigger and no effect-level state, which is what lets
two arms of one effect fold different slices of the log.

**Journal key is the call itself** (for HTTP, `http.post <url> <body>`), plus an ordinal separating
legitimately-identical repeated calls. It is not a sequence number, so editing or reordering the
arm does not corrupt replay, which is what makes live editing safe later. The language hands the
host a readable key and an ordinal; hekla stores the sha256 of the key, which is what keeps the hash
a host concern and the key the language's. The effect's **digest hash** is recorded on the
invocation as its `script_hash`. v1 does not pin to it, but on restart, if an in-flight invocation's
recorded hash names a *different known version* of the effect, the runtime logs a warning naming the
effect and invocation. That makes an otherwise invisible situation visible, and it is exactly the
field the pinning implementation needs later, so writing it now avoids a journal-format migration.

Being a digest hash rather than a hash of the file is what makes both halves of that useful. A
reformat no longer reads as a redeploy, so the warning fires on behaviour and not on layout, and
`hekla verify`'s replay check keeps its coverage across one. And because the `declaration` table
retains every version of every declaration, a recorded hash resolves three ways rather than two:
equal to the current one, a known earlier version (whose packed form is on hand to show), or absent
from the table entirely, which means it was written under some other scheme and nothing can be
concluded by comparing it.

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

**What an arm may do**: journaled `http.{get,post,put,patch,delete}`; `invoke Command { ... }`
targeting a public or internal command; `now()`; `erase(...)` (section 15). `log(...)` and
`reveal(...)` are not journaled, and each has a reason: a duplicated log line is the least harmful
thing that can be duplicated, and a `reveal` that replayed stale plaintext against a destroyed key
would defeat the erasure it is meant to respect. There is no effect-local SQLite and no way to read
a projector.

**`invoke` takes a typed struct**, checked at compile time against the target command's declared
parameters, and its literals are parsed with the parameter's type as their hint. The runtime check
stays as well, because a journaled value can be read back by a build other than the one that wrote
it: an invocation straddling a deploy hits its first not-yet-journaled `invoke` against a command
that may have changed.

**State**: an arm declares `fold` exactly as a command does, with the trigger binding in scope for
its filters. The fold is bounded at the arm's own position, inclusive, so its state is a pure function
of the log prefix and that position.

That bound is the whole design. Because the state is derived rather than observed, it cannot race a
projector, it is identical on every attempt and every replay, and it needs no journal entry: there
is nothing to record that re-folding would not reproduce. An earlier design gave effects a journaled
`read(projector, entity, key)`, which forced a choice between replay determinism and fresh data and
resolved it badly: a read that missed because a projector was behind journaled `null`, and every
retry then replayed that null, so a transient lag became a permanent wedge only an operator skip
could clear. Folding the log has no such failure mode. Its cost is that a wide boundary is re-folded
per invocation, and effects are sequential, so that comes off throughput; a boundary keyed on an
entity id is as important here as it is for a command.

**Writing outcomes**: effects do not append events; they `invoke`, and that invoke is a journaled,
idempotent side effect, so durable domain facts (tracking numbers, external ids) land exactly once
across replays. The idempotency key is derived from the journal identity of the call itself (the
effect, the position, the call key and its ordinal), so it is the same on every replay and different
for every call; the target command tags every event it emits with it and guards the append against
that tag. A replay (or a crash between the command's append and the effect's journal write) finds the
prior commit by that tag and returns its recovered outcome without re-running the command:
exactly-once is enforced by the event log itself, not by any op-DB reservation. This is the same
mechanism, and the same guarantee, as for HTTP commands. A command rejection is a normal terminal
outcome, not a retryable failure: if a completion command rejects because state moved on (the claim
was already cancelled, the order already fulfilled), the runtime records the rejection in the
journal and completes the invocation. Treating rejection as retryable would loop forever on
legitimately-stale completions.

**Retry split**: transport errors and every retryable status (408, 425, 429 and any 5xx) are
absorbed and re-sent, and never reach an arm. A result that reaches one is therefore always
terminal, so `status >= 400` is a real, decide-what-to-do failure rather than something every effect
re-implements. The split has to fall there rather than in the arm, because every response that
reaches one is journaled: an effect that failed on a 429 would replay the recorded 429 on every
retry, never re-send, and wedge until an operator skipped it and dropped the work.

**The re-send loop is the language's and the attempt is the host's.** heklang decides whether a
status is retryable and how many times to try; hekla makes the request. Two hosts answering one
program differently is exactly what that split rules out. When every attempt is absorbed, the call
comes back as "the URL did not answer" and the invocation wedges.

A `Retry-After` on a retryable response raises the *invocation's* backoff (delta-seconds only,
capped at five minutes) so a limiter's own window is waited out rather than hammered; it never
lowers the backoff, so a limiter repeating `Retry-After: 1` still gets exponentially rarer attempts.
The header never reaches a program: rule 5 makes it the host's business precisely so an arm cannot
see it, so hekla reads it off the response and hands it to its own driver. Where the wait happens
moved with the port. It used to be the only wait there was, since a 429 wedged the invocation
immediately; now the language re-sends a few times first, so a limiter that refuses once and then
relents is absorbed inside the invocation and no wait is owed.

**Wedging and the skip hatch**: an exhausted re-send loop and a host error both wedge the
invocation. An author's own `fail(...)` does not: it is a terminal outcome that completes the
position and advances, because an author saying "this cannot be processed" is a decision rather than
a fault.
The runtime retries the whole invocation with capped exponential backoff, forever, replaying journaled
calls each attempt so completed side effects never re-fire, and never skipping. Because a wedge is not
the same as ordinary lag, the status endpoint reports each effect's consecutive-failure count and last
error alongside its position. The only way past a genuinely unprocessable event is fixing the code and
restarting (which replays the running invocation) or an explicit, manual operator skip
(`POST /effects/{Name}/skip/{position}`); nothing is skipped automatically. The durable resume point is
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
- Commands run on `spawn_blocking`, because evaluation is synchronous.
- One sequential task per projector.
- One dedicated thread per effect (the projector model), each running its invocations synchronously in
  strict position order. The configured blocking-pool size is validated but reserved for a real shared
  pool once partition-key parallel lanes land (section 7).
- **The program is compiled once at load and shared.** A `heklang::Program` is `Send + Sync` and is
  held behind an `Arc`, so every command attempt, projector batch and effect invocation runs against
  the same compiled artefact. What is per-run is the *host*: one `HeklaHost` per request or
  invocation, carrying that run's causation, its pinned append time and its idempotency identity,
  which is exactly the scope tephra's writer is not.
- **Nothing bounds a runaway program, because nothing can run away.** The tick budget the Starlark
  evaluator needed has no counterpart: heklang has no `while`, rejects recursion, and iterates only
  finite containers.

## 9. Storage layout

```
data/
  events/                # tephra segments (immutable source of truth)
  projectors/{Name}.db   # read-model tables + checkpoint (one transaction)
  hekla.db               # shared operational DB: effect journals, subject keys, declarations
```

Backup is "copy the directory". Projector databases are rebuildable from the log regardless, so a
consistent copy is not required for them.

## 10. HTTP API surface (v1)

- `POST /commands/{Name}` executes a command (public commands only), accepting idempotency-key and
  correlation-id headers, and echoes correlation and causation. `{Name}` is the declared name, so a
  `command PlaceOrder` is at `/commands/PlaceOrder` whatever its file is called. The outcome maps to
  status: committed to 200 (with the appended positions), `reject` to 422, `invalid` to 400, and a
  DCB concurrency conflict that survives retries to 409. An idempotency key whose first attempt
  committed replays that commit's response, positions and original correlation and causation
  included; a key whose first attempt rejected has nothing in the log to replay, so it re-decides.
- **Read API generated from entity schemas**: `GET /read/{Projector}/{Entity}/{key}` and an indexed
  filter/scan endpoint. Both names are declared names, so `GET /read/CustomerOrders/Order/{id}`. Only declared indexes are filterable; an unindexed filter is a 400 telling
  the author to declare the index, never a table scan. Pagination is cursor-based, not offset. Every
  read response includes the projector's log position, and an optional `?after=<pos>` waits for the
  projector to reach that position before reading (read-your-writes), failing closed with 503 on
  timeout (section 6).
- `POST /projectors/{Name}/replay`.
- `POST /effects/{Name}/skip/{position}`: an explicit, manual operator action to advance a wedged effect
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
- **The admin console is served from those same URLs**, chosen by `Accept`: `text/html` gets the
  console's shell, everything else gets the JSON unchanged. `*/*` (curl's default, and a bare
  `fetch()`'s) counts as everything else, so no existing client changes behaviour. Deep links fall
  out of this for free, since every view's URL is already a real endpoint. The negotiation is a
  layer attached per route from the same table the router folds, not to the whole router: a path
  outside `/admin` is untouched, and an unrouted `/admin/...` still 404s rather than becoming a 200
  page. Responses carry `Vary: Accept`, because one URL with two representations behind a proxy is
  otherwise a poisoned cache. `GET /admin/assets/{file}` serves the console's own files from a table
  compiled into the binary, and is the one path under the prefix that is not negotiated.
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
- `hekla check <dir>`: parse the project and report every finding. Most of what this used to do is
  the language's now (section 4), so what is left is what hekla alone knows: that a declaration sits
  in the directory its kind requires, that a read model can be keyed and indexed the way the read API
  needs, that no event field occupies the reserved `_hekla_` tag namespace, that a sealed column can
  say it is absent, and two lints. For CI and pre-commit.

  The lints are warnings, never errors, because each is a judgement call and an error would stop a
  valid project deploying over one: a boundary with no filter on a high-cardinality field (it
  defeats the append's fast reject), and a boundary pinning nearly every field of an event (a slice
  is a subset match, so over-constraining matches nothing).
- `hekla test <dir>`: events in, assert what the declaration did, for all three kinds. Every case
  seeds a throwaway world with `given` and then runs one declaration against it: `run` a **command**
  and expect its events or its refusal, `project` a **projector** and expect its rows, `deliver` an
  **effect** and expect the ordered calls it made, with `respond` stubbing the HTTP replies and
  `erased` destroying a subject key up front.

  **The runner is the language's and the world is hekla's.** heklang defines what an expectation
  means; hekla supplies real tephra, a real SQLite read model, a real `KeyStore` and a stubbed
  network. One definition of `expect`, two worlds. That split is what makes an erasure case worth
  running: in heklang's own harness a seal carries plaintext and "erased" is a flag, so
  `erased customer_id "7"` then `project CustomerOrders` then `expect Order[...] { email: none }`
  could not fail. Here the column holds AES-SIV ciphertext and the key is really deleted.

  It also holds the rule that **a test cannot see anything a program cannot**. The only
  world-dependent assertion is a row, and it is read through the same seam `patch` reads through, so
  a case cannot assert on folded state, on an append condition, or on how many times something was
  retried. Everything a handler can observe is pinned so a case is reproducible: the clock, the
  master key, and each `given` event's id (counting from `…-000000000001`, so an id derived with
  `Uuid.derive` is assertable) and timestamp.
- `hekla verify <dir>`: the runtime invariant sweep over a data directory. Section 11.2.
- `hekla plan <dir>`: what deploying this project over a data directory would change. It reads the
  `declaration` table rather than the log, so it needs no lock and runs against a live directory.

**`hekla fmt` and `hekla lsp` are gone.** Both were Starlark tooling: starlark-rust ships a formatter
and a language server, and hekla wrapped them with its own project knowledge (which builtins are in
scope depends on the directory, and `load()` resolves against the project root). heklang has a
tree-sitter grammar and neither of those yet, so the subcommands were dropped rather than stubbed.
The ~1,300 lines behind them, and the `starlark_lsp` / `lsp-server` / `lsp-types` dependencies that
existed only to match it, went with them.

### 11.2 Invariant checks

`hekla check` is static analysis; this is its runtime counterpart. The log is append-only, so the
faults worth spending verification on are the ones nothing can undo: an event that should never have
been appended, an effect that fired twice. A wrong read model, by contrast, is a rebuild. The checks
follow that asymmetry.

Three invariants, each reported as a `verify::Violation`.

There were four. **Fold determinism** checked that the same boundary at the same position folds to
the same state, because section 7's claim that state can be derived rather than stored rested on it.
It is gone, because heklang removes its sources by construction: ordered maps, a clock a fold cannot
reach, no randomness, and no read of anything but the log. The check itself lived inside the chunked
fold, comparing two Starlark states, so it had nowhere left to stand once the chunking went.

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
- **Checkpoint monotonicity**: no position reached by *tailing* moves backwards. A rebuild replaces
  the model, so it publishes its checkpoint without that guard: a bounded rebuild legitimately lands
  behind, and treating that as a violation stopped the projector while leaving it readable.

Two entry points over one set of checks. `hekla verify <dir>` sweeps offline and exits non-zero on a
violation, for CI or a nightly job; it takes the data-directory lock, so the documented shape is to
verify a copy of the directory, which exercises the backup at the same time. `serve --verify` (or
`[verify] enabled` in `hekla.toml`) runs the per-operation half continuously.

A violation **quarantines the component**: it stops advancing, `/status` names what broke, and the
rest of the runtime keeps serving. A quarantined projector's reads return 503 rather than its rows,
because what a failed check calls into question is precisely the rows and the position, and a
read-your-writes wait against a position that moved backwards would resolve on a lie.

Rebuild equivalence is offline only: it costs a full log replay, and against a live projector the
shadow model would race the one it is comparing to.
## 12. Why heklang (determinism and purity)

umari pins the wall clock and zeroes the monotonic clock to make commands deterministic, and polices
nondeterminism with static analysis and linters. A pure sandboxed language makes it structural: no
clock, no randomness, and no I/O except through a declared host seam, so every nondeterministic
input is either absent (commands and projectors) or journaled by construction (effects). Deployment
is source text: no toolchain, no compile cache, parse in milliseconds.

Starlark supplied that much, and hekla was built on it first. What it could not supply is the second
half: **the rules of this domain are not expressible in a general-purpose language, so they had to be
enforced by a validation pass instead.** That pass grew to sixteen checks over 4,039 lines of
hand-built globals, value types and marshalling, and it could only ever be as good as its own
approximations, most visibly in evaluating a boundary against a stubbed input and so seeing one
branch of it.

heklang moves each of those into the grammar or the type system. A command cannot call out because
`invoke` does not parse in one. A projector cannot decrypt because `reveal` does not parse in one. A
fold cannot read the clock. Sealed content cannot be compared, interpolated or sent. A read model
column's subject is computed from what is written into it, so it cannot disagree with its content.
An event is written whole or not at all. Each of those replaces a runtime failure, a validation rule,
or in several cases a class of bug that had no check at all.

The mechanical results are worth naming, because they are the argument in numbers: the four constructs
a command's boundary needed became one, the chunked fold and its four tuning constants went, the
instruction budget went, `load()` and its resolver went, and `hekla check`'s sixteen rules became
three lints plus what a directory means.

## 13. Non-goals

**Deferred** (see the roadmap, each with a trigger): metrics and Prometheus; partition-key parallel
effect lanes; an upload API with versioning, pinning, and retention, plus hot reload; a fold
library; a workspace crate split.

**Permanent commitments** (not deferrals, and not to be reopened): **there is exactly one authoring
surface, and it is heklang.** There is no Rust, TypeScript, or WASM SDK path now or later. This is
deliberate: a single pure, sandboxed authoring language is what makes determinism structural,
deployment source text, and the durable-effect journal sound. Multi-language authoring is permanently
out of scope.

The language behind that surface was Starlark and is now heklang, which is the one thing this
document has changed its mind about. The commitment was never to Starlark specifically; it was to
there being exactly one authoring language, chosen for what it makes impossible rather than for what
it makes convenient. That still holds, and heklang holds it harder.

## 14. Code layering

hekla is a single crate. The dependency direction is documented and enforced by discipline,
revisited only when a seam proves real (embeddability, or compile times that actually hurt):
`schema`, `tags` and `http` depend on nothing internal; `heklang_host` depends on those and is the
one file that knows both models, so every conversion between the language and the store lives in it
and nowhere else; `dispatch` depends on `heklang_host`; `runtime` (projectors, effects, journal,
storage) depends on `dispatch`; `verify` and `introspect` sit above the runtime, reaching into the
projector, effect and storage paths they read; `api` and `cli` sit on top. `lock` and `ui` depend on
nothing internal: `ui` is the console's bytes plus the content negotiation over them, so the server
depends on it and it depends on nothing.

**`heklang_host` is the seam and is meant to be the only one.** It implements heklang's five host
traits (`Log`, `Clock`, `Keys`, `Http`, plus `Calls` per invocation and `Rows` per projector) against
tephra, the key store, `ureq` and the operational DB. Everything in it is a conversion: positions
(heklang counts from zero, tephra from one), JSON in both directions against a declared type, and
crypto, which lives entirely *below* the seam because heklang models a seal logically and hekla
really encrypts. The language never sees a ciphertext and the store never sees a plaintext.

## 15. Subject-scoped encryption and erasure

A field marked `@subject(sibling_field)` is encrypted under a key scoped to that subject's identity
`(subject_field, subject_value)`, in the tag index, the event payload, and any read-model column, all
before it reaches tephra. **Erasing a subject is deleting its key**, one O(1) operation that makes
every value scoped to it unmatchable and unreadable across the log and every read model at once, with
no rewrite, compaction, or index rebuild.

**Two ways to erase**, the same key delete either way. `hekla erase <field> <value>` is the operator
path, for a one-off request handled by hand. `erase(customer_id)` is the effect statement, for
erasure driven by an event: a provider webhook, a retention deadline, an `account.closed` your own
command emitted. It recovers the subject from the value, which must be a field of the triggering
event and may not itself be sealed; `erase(subject, value)` names the subject explicitly where the
inference does not apply. It is journaled like every other side effect, so a replay skips it.

**`erase` returns nothing.** hekla's used to return whether a key was really deleted, and an author
reading that was branching on whether someone else got there first: a race that is always already
lost, since the key is gone by the time they read it. Dropping the result costs nothing and keeps
`erase` out of expression position, which is what makes the erase-last analysis below exact.

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
after the erase are readable while everything before it stays shredded.

**No read path ever resurrects a key**, and there are two of them. An append mints on first use; a
*projection* must not, because re-projecting a log whose subject has been erased would otherwise
create the very key the erasure destroyed and write readable content under it, undoing a shred by
rebuilding a read model. So a projector writes through `encrypt_subject_existing`, which answers
`None` for a missing subject, and the column is written NULL: absent, which is the same answer the
read API gives a reader.

**Mechanism.** Encryption is deterministic (AES-SIV): the same plaintext under the same key and field
yields the same ciphertext, so it works as an equality-matchable tag while staying decryptable. Each
per-subject key is a random secret stored in `hekla.db`, wrapped with AES-256-GCM under a master key
from `HEKLA_MASTER_KEY` and tagged with the wrapping master's id so masters can rotate online:
`hekla rotate` rewraps every row under a new `HEKLA_MASTER_KEY`, unwrapping with
`HEKLA_MASTER_KEY_PREVIOUS` as needed, without touching any ciphertext. The global uniqueness key
The reserved uniqueness secret that used to sit beside these went with `unique` (below).

**Information flow.** Plaintext of a subject field exists only at the HTTP command input (the client
supplied it) and at read-API output or an effect's `reveal(...)` (the runtime decrypted it).
Everywhere between (log, tag index, read-model columns) it is ciphertext.

**A program sees sealed content, which is a type rather than an opaque value.** Exactly three things
may be done to it, and everything else is a compile error:

| | Why it is safe |
| --- | --- |
| **Move it** into a position sealed under the same subject: a `let`, a `fold`, an entity column, another event field | the content is never read |
| **Ask if it is there**: `.is_some()` / `.is_none()` | presence is not content |
| **`reveal` it** | the boundary itself |

So `log(email)`, `"{email}"`, `http.post(url, { "to": email })`, `invoke C { note: email }`,
`email.trim()`, `email.unwrap_or("")` and `if email == "x"` are each refused where they are written.
Writing *plain* content into a seal is free, because that is the encrypting direction: a command
holding an ordinary `String` may emit it into a `@subject(...)` field with no ceremony.

That is stricter than the Starlark handle in one direction and looser in another, and both matter.
Stricter: a handle could be compared for equality, and an equality over two ciphertexts leaks whether
they hold the same value, so it is gone. Looser: a handle could not be carried into an `emit` at all,
so a command could not move a customer's own address forward; moving sealed content into a field
sealed under the *same* subject is legal, because moving is not reading.

Because read models store ciphertext and the read API decrypts on the way out, deleting the key
shreds the log and every read model together. A derivation an author wants must be computed by the
command and emitted as its own subject field. An effect crosses the boundary explicitly with
`reveal(...)`, which is an optional in and an optional out: absent stays `none` **without consulting
the key store at all**, because a value that was never set was never encrypted. Only a *present*
value under a shredded key fails, and it fails terminally, because no retry can recover it. "Never
set" and "key destroyed" are different facts and must not collapse.

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
- **There is no cross-subject uniqueness on a sealed field.** `unique = True` used to mint a tag
  under a never-erased global key, so one email could be matched across every account and stayed
  matched after erasure. It required an equality on sealed content, which is now refused for the
  reason above, so the feature and its reserved secret are deleted. What replaces it is an ordinary
  boundary on a plaintext field beside the sealed one: `examples/orders` keeps a per-shop allocation
  cap, and `ACCOUNT_EVENTS` in the test suite keeps a plaintext `handle` beside a sealed `email`.
  Erasing a subject does not reopen a handle it claimed, which is the property `unique` existed for,
  and it holds with no key at all.
- **A field appended without a subject cannot be erased** until a segment-rewrite tool exists (out of
  scope): its plaintext is already in the log payload and tag index, and replaying projectors just
  re-reads it. Which fields are personal is a judgement about meaning rather than about a name, so
  nothing warns about one that has no subject.
- **A fold decrypts eagerly**, which under Starlark it did not. heklang's seal holds plaintext and
  its key seam answers only "is this subject erased", so the adapter decrypts every subject-scoped
  field of every record a fold reads, where a handle used to keep ciphertext opaque all the way
  through. Measured at 3.5µs a record, which makes a fold over an encrypted boundary about four times
  the cost of the same fold over plaintext (`tests/measure.rs`). It is forced rather than chosen, and
  it is the motivating number for closing heklang's ciphertext gap.
- **Range predicates over encrypted tags are foreclosed** (tags are equality-only anyway).
- **One subject per field**, and **one subject per variable**: two fold arms writing values sealed
  under different subjects into one variable is an error naming both, because `reveal` names the key
  by the subject and a runtime answer would make the terminal message unpredictable. Genuinely joint
  data (a message between two people) is deferred.
- **Effect external sinks are outside the boundary.** Erasure shreds hekla's own store; it cannot
  un-send an email an effect already delivered. The effect journal holds revealed plaintext only
  transiently, until the retention sweeper reclaims the completed invocation.
- **Losing `HEKLA_MASTER_KEY` is total, unrecoverable loss** of every subject-scoped value. Boot fails
  fast with a subject-specific message when a project uses subjects and the key is absent or wrong.
