# Authoring hekla modules

A complete reference for writing the Starlark files that make up a hekla project. If you are
building an application on this runtime, everything you need to write is described here.

hekla is a single-app event-sourcing / DCB (Dynamic Consistency Boundary) runtime. Business logic is
Starlark over an immutable event log:

- **Commands** validate input, check invariants against replayed history, and append events. They
  are the only writers.
- **Projectors** consume events and build queryable SQLite read models.
- **Effects** react to events with durable, replay-safe side effects (HTTP, invoking commands).

Starlark is pure and sandboxed: no clock, no randomness, no I/O except the builtins hekla injects.
Determinism is structural, not policed. Deployment is source text; there is no build step.

---

## 1. Project layout

Kind comes from the directory, name from the file stem, and one file is one unit of behaviour.

```
project/
  events/              # shared event definitions, importable
  lib/                 # shared pure helpers, importable
  commands/            # public commands: HTTP-routed and invokable by effects
  commands/internal/   # internal commands: invokable by effects, NOT HTTP-routed
  projectors/
  effects/
  tests/               # hekla test scenarios
  hekla.toml            # operational config, optional
  data/                # created by the runtime: event log, read models, operational DB
```

Rules:

- **`load()` may only import from `events/` and `lib/`.** A command can never import another
  command. Import paths are project-relative: `load("events/user.star", "user_registered")`.
- **A module's name is its file stem**, and must be a slug: lowercase ASCII letters, digits, and
  single interior hyphens (`send-welcome`, `place-order`). No leading, trailing, or doubled hyphens.
- **Public vs internal is structural.** Only the literal `commands/internal/` prefix marks a command
  internal. Nesting elsewhere is free and stays public: `commands/billing/refund.star` is public.
- Names must be unique per kind. Two commands named `place-order` in different subdirectories is an
  error.
- **Deploy is restart.** Modules load at startup; there is no hot reload.
- Files outside these directories, and non-`.star` files, are ignored.

## 2. What each module kind declares

Every module kind requires `handle`. Everything else depends on the kind.

| Global | Command | Projector | Effect | Test file |
|---|---|---|---|---|
| `input = schema(...)` | **required** | - | - | - |
| `query(...)` | optional, takes `input` | - | optional, takes `event` | - |
| `initial` | optional | - | optional | - |
| `fold = {clause: fn}` | optional | - | optional | - |
| `handle` | **required**, one function `(input, state)` | **required**, clause-keyed map of `(event)` | **required**, clause-keyed map of `(event, state)` | - |
| `entity(...)` bindings | - | collected implicitly from module scope | - | - |
| `cases = [...]` | - | - | - | **required** |

There are no registration calls. hekla reads named top-level values off the frozen module.

## 3. Field types

One field type system, shared by event schemas, command `input = schema(...)`, and entity schemas.

| Constructor | Meaning |
|---|---|
| `str(max_length = N)` | text |
| `int()` | signed 64-bit integer |
| `uint()` | unsigned integer, `0..=i64::MAX` |
| `bool()` | boolean |
| `uuid()` | UUID, stored as its canonical string form |
| `timestamp()` | ISO-8601 / RFC 3339 string, sorts lexicographically |
| `money()` | fixed-scale decimal carried as a string (`"10.50"`), integer minor units in storage |
| `json()` | arbitrary JSON, validated as nothing, stored as text |
| `one_of(["a", "b"])` | a string constrained to a fixed set |
| `optional(inner)` | nullable, inheriting `inner`'s policy |

**There is no float field type and there never will be.** Binary-float rounding in an append-only
log is permanent. Use `money` for currency and scaled integers for everything else.

### Per-field options

Named-only arguments accepted by every field constructor:

- `indexed` (default `True`): whether the field becomes a store tag, and so whether it is filterable
  in queries and read-model reads. Set `indexed = False` for large blobs or free text nobody queries.
- `subject = "sibling_field"`: encrypt this field under a key scoped to that sibling field's value.
  See section 11.
- `unique = True`: additionally emit a global-key tag for a uniqueness check that survives erasure.
  Requires `subject`, and cannot be combined with `indexed = False`.
- `max_length = N` (text only): reject longer values. Required on a text field that is `subject` or
  `unique`, so its ciphertext tag stays bounded.

`json` cannot be subject-encrypted or unique. `schema(...)` rejects `subject` and `unique`: command
input is plaintext at the boundary.

### The shadowing rule

`str`, `int` and `bool` deliberately shadow Starlark's builtins. One rule keeps both reachable:

> **A positional argument means Starlark's conversion. No positional argument means a field
> declaration.**

So `str(response.status)` converts and `str(max_length = 200)` declares. This works because every
standard conversion is positional-only and every field option is named-only. Passing both is an
error, never a silent drop.

The one cost: `int()` and `bool()` no longer produce `0` and `False`. Write the literals.

`uint` shadows nothing. `one_of` keeps a distinct name because a variant list and starlark-rust's
`enum(...)` are both positional, so the rule cannot separate them. `float` and `bytes` remain plain
Starlark conversions.

## 4. Events

Event definitions live in `events/` and are imported with `load()`. An event declares its type and
its typed fields. **There is no `tags = [...]` list**: every field is automatically indexed as a
store tag unless it opts out with `indexed = False`.

```starlark
# events/order.star

order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "shop_id": uint(),
        "email": str(subject = "customer_id", unique = True, max_length = 200),
        "shipping_address": str(subject = "customer_id", max_length = 200),
        "order_total": money(subject = "shop_id"),
        "notes": str(indexed = False),
    },
)

order_shipped = event(
    type = "order.shipped",
    fields = {"order_id": uuid(), "carrier": str(), "tracking": str()},
)

order_cancelled = event(type = "order.cancelled", fields = {"order_id": uuid()})
```

One file holds as many definitions as you like; the later sections load these three.

- Field names may not start with `_hekla_`, which the host reserves for its own tags.
- **The binding is callable, and that call is the one dispatch form.** With field values it
  constructs an event to emit. In a query position (a command's `query`, or the keys of a `fold` or
  `handle` map) it builds a filter clause instead.
- **Only the registered definition may be emitted.** Each `event(...)` call mints a process-unique
  id and constructed events carry it, so a handler that builds its own `event(type = "order.placed",
  ...)` inside a function body is rejected at append. Declaring an event at module scope outside
  `events/` is a `hekla check` error. Rebinding a loaded definition to a second name is fine.
- Constructing an event validates the payload against the field schema. Missing or extra fields fail
  fast.

**Envelope.** hekla wraps each payload with an `event_id`, `correlation_id`, `causation_id`, an
optional `triggering_event_id`, and the append `timestamp`. Starlark never sets these. Do not restate
the append time in your payload; use `timestamp()` fields only for domain time (`due_at`,
`expires_at`).

Two envelope fields are readable from a handler, alongside `event.type` and `event.data`:
**`event.id`** and **`event.timestamp`** (the append time, RFC 3339). Both are stamped once at append
and never move, so a projector rebuild and an effect replay both see the value the original append
wrote. They sit beside `data` rather than in it, so an event declaring its own `id` or `timestamp`
field still reads that at `event.data.id` / `event.data.timestamp`.

**Use `event.timestamp` for a `created_at` column, not a `now()` field.** A command that stamps
`now()` into a payload just to record when the event was appended duplicates the envelope. Keep
`now()` for time that is genuinely domain data: `expires_at`, `due_date`, or a `purchased_at` an
upstream system reported.

**Deriving ids.** Nothing in a hekla module may mint a random id: a command retry and an effect replay
both re-run the code that would mint it, so a fresh id per attempt turns one intent into several
entities. `uuid5(namespace, name)` derives a stable one instead (RFC 4122 version 5), usually over
`event.id`:

```starlark
invoke_command("record-notified", {
    "notification_id": uuid5(event.id, "confirmation"),
    "order_id": event.data.order_id,
})
```

`name` is what lets one handler derive several distinct ids from one event. The namespace must be a
canonical UUID, which `event.id` and every `uuid()` field carry.

Reach for it third, not first:

1. **Use an identity that already exists.** If the fact is "this order was notified", its identity is
   the order id. A second id is one more thing to keep unique for no gain.
2. **Take an external id from a journaled response** (`response.body["id"]`). It replays identically
   and it is the id you actually want recorded.
3. **Derive with `uuid5`** when the id has no anchor outside this handler.

## 5. Query clauses: the one key language

A **clause** is an event definition called with the fields to match.

```starlark
order_placed()                      # every order.placed
order_placed(shop_id = 1)           # subset match: shop_id must equal 1
order_placed(shop_id = 1, email = x)  # fields AND together
all_events()                        # every event, regardless of type
```

Clauses appear in exactly three positions, and mean the same thing in all of them:

1. what a `query` returns (a clause, or a list of clauses OR-ed together): a command's
   `query(input)`, or an effect's `query(event)`,
2. the keys of a `fold` map, on a command or an effect,
3. the keys of a projector's or effect's `handle` map.

**A key is always a call, never a bare definition.** `{order_placed: f}` is a load error; write
`{order_placed(): f}`. One spelling covers both the unconstrained and the constrained arm.

Semantics:

- Constraining a field is a **subset match**, so over-constraining silently matches nothing.
  `hekla check` warns about clauses that look like copied emit calls.
- A field must be declared by that event type and must be indexed, or it is a hard error at check
  time, never a silent empty result.
- A subject-encrypted field can only be filtered when its subject is also constrained (scoped) or
  the field is `unique` (global). In a `handle` key it cannot be filtered at all: a subscription is
  lowered without a keystore, so filter by the plaintext subject id instead.
- Dispatch is **fan-out**: every arm whose clause matches runs, in declaration order. No arm can be
  shadowed by an earlier one, so order fixes only the sequence of ops, never which arms run.
- Two identical clauses in one map are rejected by Starlark itself as a repeated dictionary key.

## 6. Commands

A command validates input, checks invariants against replayed state, and appends events. It is the
only writer in the system.

```starlark
# commands/place-order.star

load("events/order.star", "order_placed")
load("lib/validation.star", "is_blank")

input = schema(
    order_id = uuid(),
    customer_id = uint(),
    shop_id = uint(),
    email = str(),
    shipping_address = str(),
    order_total = money(),
    notes = str(),
)

def query(input):
    return order_placed(email = input.email)

initial = {"taken": False}

fold = {order_placed(): lambda state, event: dict(state, taken = True)}

def handle(input, state):
    if is_blank(input.email):
        return invalid_input("email must not be blank")
    if state["taken"]:
        return reject("email_taken", "that email has already placed an order")
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        shop_id = input.shop_id,
        email = input.email,
        shipping_address = input.shipping_address,
        order_total = input.order_total,
        notes = input.notes,
    )
```

### `input = schema(...)` (required)

The host validates the request body against it before `handle` runs, so a command never sees a
malformed input. Read it with dot access: `input.email`.

### `query(input)` (optional)

Returns the **consistency boundary**: the set of events this command reads and appends under. A
concurrent write inside the boundary fails the append with a 409 and the caller retries.

Omit it entirely for a command with no invariants.

### `initial` (optional)

A literal value producing the fold's starting state, **never a function**. It sees no input, no
clock and no randomness, so a module-level expression already covers everything a function could
compute. When absent, `handle` receives `None` as `state`.

### `fold = {clause: fn(state, event)}` (optional)

Reduces the boundary's events into decision state. **Each arm returns the new state rather than
mutating the one it was handed.** `initial` is a frozen module global, so a fold that assigns into
`state` fails on the first event it sees. Returning `None` (falling off the end) is an error.

```starlark
fold = {
    order_placed(): lambda state, event: dict(state, taken = True),
    shop_suspended(): lambda state, event: dict(state, suspended = True),
}
```

Build new state with `dict(state, taken = True)`, or `dict(state, **{key: value})` when the key is
computed.

**The boundary and the fold answer different questions.** A type belongs in `query` whenever a
concurrent write of it should make this command fail. It belongs in `fold` only when `handle` needs
to know about it. An event type in the boundary with no fold arm is a normal shape, not an
oversight: it still counts toward the append condition. The reverse (a fold arm for a type the
boundary never returns) is dead code, and `hekla check` reports it.

A `fold` key can only filter on constants, because it is module-level and cannot see `input`. Reach
for a constrained key on enum-shaped fields (`order_placed(status = "cancelled")`) and not much
else. `all_events()` folds every boundary event.

### `handle(input, state)` (required)

Always a single function: it decides from input and folded state rather than from one event, so
per-clause dispatch belongs on `fold`. Returns exactly one of:

| Return | Meaning | HTTP |
|---|---|---|
| an event, or a list of events | append them (an empty list is valid: nothing to append) | 200 |
| `reject(code, message)` | well-formed input the current state forbids | 422 |
| `invalid_input(message)` | malformed input, regardless of state | 400 |

A DCB concurrency conflict that survives retries is a 409, kept distinct from 422 so the status
alone tells a client whether a retry can help.

**`state` is read-only in `handle`.** Assigning into it fails with `Immutable`, the same error a fold
arm gets for mutating rather than returning. The reason is the retry: a conflict is retried by
folding what landed since onto the state the previous attempt already built, rather than re-reading
the boundary from the start, so a mutation would decide the *next* attempt instead of being discarded
with this one. There was never anything to gain from it: `handle` returns its decision, and the state
is thrown away with the request. The same holds for an effect's `handle`, whose state is folded from
the log: writing into it changes nothing durable, so it fails rather than being silently lost.

**A fold arm must return the new state even when it is mutating a value it built itself.** Returning
a fresh value on the first event and then writing into that value on later events looks like it
works, and on a shallow boundary it does. It is still wrong, and it fails in two places: on any
retry, and on any boundary deep enough that the fold chunks (hekla freezes the accumulated state
every megabyte or so, to keep a deep fold's memory flat). The failure is therefore *depth-dependent*,
which is a bad thing to discover in production, so write the arm the way the contract says:

```starlark
# Wrong: works until the boundary grows, then fails with `Immutable`.
def tally(state, event):
    if state == None:
        return {"seen": 1}
    state["seen"] = state["seen"] + 1
    return state

# Right: a new value every time.
initial = {"seen": 0}

def tally(state, event):
    return dict(state, seen = state["seen"] + 1)
```

### Determinism and ids

- `query` and `fold` are pure and clock-free. `now()` is available **only in `handle`**, pinned once
  per request so repeated calls agree.
- **New-entity ids are client-supplied in the input.** Starlark mints no ids and has no randomness. A
  retried request carries the same id, so the command's own boundary rejects the duplicate, and
  idempotency for creation falls out of DCB with no extra layer.
- **Commands never invoke commands.** Chaining goes through the log: a command emits an event, an
  effect reacts, the effect invokes the next command.

### Idempotency keys

Separate from id-based dedupe, for commands where nothing in the input distinguishes intent. The
client passes an `Idempotency-Key` header; the runtime hashes it with the command name into a
reserved `_hekla_idem` tag, stamps every emitted event with it, and guards the append against that
tag existing anywhere in the log. On a hit it returns the original commit's events and identity
verbatim rather than re-running `handle`. A first attempt that rejected appended nothing and left no
tag, so a retry re-decides.

## 7. Projectors

A projector consumes events and builds a queryable read model. One SQLite database per projector.

```starlark
# projectors/orders.star

load("events/order.star", "order_placed", "order_shipped", "order_cancelled")

orders = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 200),
        "status": one_of(["placed", "shipped"]),
    },
    indexes = [index("by_customer", ["customer_id"])],
)

def on_placed(event):
    return [put(orders, {
        "order_id": event.data.order_id,
        "customer_id": event.data.customer_id,
        "email": event.data.email,
        "status": "placed",
    })]

handle = {
    order_placed(): on_placed,
    order_shipped(): lambda event: [patch(orders, event.data.order_id, {"status": "shipped"})],
    order_cancelled(): lambda event: [delete(orders, event.data.order_id)],
}
```

### Entities

`entity(key = ..., fields = {...}, indexes = [...], name = ...)`.

- Entities are **collected implicitly from module scope**: every global bound to an `entity(...)` is
  a table. There is no `entities = [...]` list.
- The table is named after the binding (`orders = entity(...)` gives `orders`). Pass `name =` only
  to override it.
- `key` names the primary-key field, which must appear in `fields`.
- `indexes = [index("by_customer", ["customer_id"])]` declares secondary indexes. **Only declared
  indexes are filterable** through the read API; an unindexed filter is a 400, never a table scan.
- A subject-encrypted column cannot be indexed, because a filter arrives as plaintext. Filter by the
  plaintext subject id instead.

### `handle` (required)

A clause-keyed map, and **the keys are the subscription**: they say which events to read and what to
do with each, so there is no second list beside them to keep in step. Each arm takes `(event)` and
**must return a list of ops** (possibly empty).

| Op | Meaning |
|---|---|
| `put(entity, row)` | replace a whole row. `row` is a dict carrying every non-`optional` field, including the key. Columns absent from `row` are dropped. |
| `patch(entity, key, changes)` | set the fields in `changes`, clear those set to `None` (must be `optional`), leave others alone. A no-op if the row does not exist. Cannot change the key field, and `changes` may not be empty. |
| `delete(entity, key)` | delete the row. |

The first argument is the **entity value itself** (`put(orders, ...)`), not its name.

### `get(entity, key)`

Reads one row from this projector's own read model, returning `None` when there is no such row.
Available only inside a projector's `handle`.

**It reads through the current batch's uncommitted writes**, so a read-modify-write within one batch
sees its own earlier `put`. That is what makes running totals correct under batching:

```starlark
def count_registration(event):
    row = get(totals, "all")
    count = (row["count"] if row else 0) + 1
    return [put(totals, {"id": "all", "count": count})]
```

Rows are **dicts**, read by subscript, because `put()` takes a dict and read-modify-write has to
round-trip without a conversion. Subject columns come back as opaque handles, so the row round-trips
through `put` unchanged.

### Rebuilds

A projector records the hash of its **definition** (its subscription and entity schema, not its
handler bodies) inside its read model. Change either and the projector rebuilds from position 0 at
startup, answering `503` with a `Retry-After` in the meantime. Editing only a handler body does not
trigger a rebuild; use `POST /projectors/{name}/replay` when you want one.

Read models are a rebuildable cache, never a source of truth.

## 8. Effects

Effects perform side effects in reaction to events, and they are **durable**: an effect that crashes
mid-way resumes without re-firing side effects it already performed.

```starlark
# effects/notify-customer.star

load("events/order.star", "order_placed")

def query(event):
    return [order_placed(customer_id = event.data.customer_id)]

initial = {"orders": 0}

fold = {order_placed(): lambda state, event: {"orders": state["orders"] + 1}}

def notify(event, state):
    response = http.post(
        url = "https://mail.example/confirm",
        body = {
            "to": reveal(event.data.email),
            "order_id": event.data.order_id,
            "first_order": state["orders"] == 1,
        },
    )
    if response.status >= 400:
        log("confirmation rejected with status " + str(response.status))
        return
    invoke_command("record-notified", {
        "order_id": event.data.order_id,
        "notification_id": uuid5(event.id, "confirmation"),
    })

handle = {order_placed(): notify}
```

`handle` takes the same shape a projector's does: a clause-keyed map whose keys are the
subscription, where every matching arm runs in declaration order. Each arm takes `(event, state)`;
its return value is ignored.

### `query` / `initial` / `fold` (optional)

An effect reads state exactly the way a command does, with the same three globals and the same
meanings (section 6). The one difference is that `query` takes the **triggering event** where a
command's takes `input`, because an effect's boundary is scoped by what it is reacting to.

An effect that needs no state declares none of the three; its arms still take `(event, state)` and
receive `initial`, or `None` when that is undeclared too.

**The fold stops at the effect's own position, inclusive.** `state` is the boundary folded over the
log up to and including the triggering event, so it is a pure function of the log prefix and that
position. Three things follow, and they are the reason effects have no `read` of a projector:

- **It cannot race.** There is no other module to be behind. State written one position earlier is
  always visible.
- **It is not journaled.** Every attempt and every replay re-folds and gets the same answer, so
  there is nothing to record. Contrast `now()`, which is journaled precisely because it cannot
  reproduce itself.
- **It counts the trigger.** An effect that folds its own trigger type sees itself, so a first
  order leaves a count of one, not zero.

Every arm selecting one event gets the same `state`: the fold is of the log, not of what an earlier
arm did.

A `query` or `fold` key may filter a subject-encrypted field when its subject is also constrained,
because both are lowered per invocation with the key store. A `handle` key may not: it is lowered
once for the whole subscription, so filter by the plaintext subject id there.

The cost is that the boundary is re-folded on every invocation, and effects are sequential, so a
wide boundary is throughput. Keep `query` keyed on an entity id, exactly as for a command.

### How durability works

`handle` is straight-line blocking code. Each impure builtin call looks itself up in a **journal**
first: if a result is recorded it returns that, otherwise it performs the real call and appends the
result. After a crash, `handle` re-runs from the top, replays journaled calls until it passes the
end of the journal, then resumes making live calls. There are no step functions and no yielding.

The journal key is the **content hash of the call**, not a sequence number, so editing or reordering
the script does not corrupt replay. The journal lives in the operational DB, never in the event log,
and a background sweeper reclaims completed invocations after the retention window.

**Write your handlers so a replay is safe.** Anything not journaled (a `log()` line) may happen
twice.

### The retry split

**The runtime absorbs transport errors and every retryable status with backoff, and those never
reach your script.** A retryable status is **408, 425, 429, or any 5xx**: each names a condition
that clears on its own, with the same request. A result that does reach Starlark is therefore
always terminal, so `status >= 400` in a handler is a real, decide-what-to-do failure rather than
something every effect re-implements.

That split is not a convenience, it is the only place the decision can live. **Every response that
reaches your handler is journaled.** A handler that raised on a 429 would replay the recorded 429 on
every retry: the request would never be re-sent, and the invocation would wedge until an operator
skipped it and dropped the work. Re-sending a call is something only the runtime can do, because
only the runtime decides what goes into the journal.

A `Retry-After` on a retryable response is honored: the next attempt waits at least the window the
server named, up to a five-minute ceiling. It raises the backoff and never lowers it, so a limiter
answering `Retry-After: 1` forever still gets exponentially rarer attempts rather than one a second.
Only the delta-seconds form is read; the HTTP-date form reads as absent.

Transport errors, retryable statuses, and a raised handler all **wedge** the invocation: the runtime
retries the whole invocation with capped exponential backoff, forever, replaying journaled calls
each attempt, and never skipping. `/status` reports each effect's consecutive-failure count and last
error, so ordinary rate limiting surfaces there like any other wedge. That is what it is (the
invocation is not progressing), and unlike most wedges it clears itself once the window opens. The
only way past a genuinely unprocessable event is fixing the code and restarting, or an explicit
operator `POST /effects/{name}/skip/{position}`.

### Writing back to the log

**Effects never append events.** They call `invoke_command(name, input)`, which is journaled and
passed a deterministic idempotency key, so the domain fact lands exactly once across replays. Point
it at a command under `commands/internal/` so nobody can POST a fabricated completion event.

A command **rejection is a normal terminal outcome**, not a retryable failure: the runtime records
it in the journal and completes the invocation. Treating it as retryable would loop forever on
legitimately-stale completions.

### Effect builtins

| Builtin | Returns | Journaled |
|---|---|---|
| `http.get(url = , headers = )` | `{status, body, headers}` struct | yes |
| `http.post(url = , body = , headers = )` | same | yes |
| `http.put` / `http.patch` / `http.delete` | same | yes |
| `invoke_command(name, input)` | `{status, body}` struct | yes |
| `now()` | RFC 3339 string | yes |
| `log(message)` | `None` | **no** |
| `reveal(handle)` | plaintext string | no, re-decrypts every attempt |
| `erase(subject_field, subject_value)` | `True` if a key was deleted | yes |

Every builtin here is a real side effect or an unrepeatable observation, which is what earns it a
journal entry. **There is no way to read a projector from an effect, deliberately.** State comes from
the boundary above, which folds the log and so cannot be stale, cannot race, and needs no journal.
Durable state is that fold, the journal, and events written through commands.

`reveal()` is the explicit boundary an effect crosses to act on personal data. A `reveal` of an
already-erased subject fails terminally, because no retry can recover the data.

`erase()` is crypto-shredding from a handler: the same key delete `hekla erase` performs, for
erasure driven by an event (a provider redact webhook, a retention deadline, your own
`account.closed`). It is **irreversible**. Two rules follow from `reveal` not being journaled:

- **Erase last.** An invocation that reveals a subject and then erases it cannot be replayed: the
  replay re-runs `reveal` against a key that is gone and skips terminally. Calls journaled before the
  erase stay done, so nothing re-fires, but work after the reveal does not run on that replay.
- **Do not read a subject to decide whether to erase it.** Take the subject ids from a plaintext
  field or a read model, never from a value scoped to the key you are destroying. Otherwise a repeat
  request for an already-erased subject cannot be read at all.

Erasure is a point-in-time shred, not a tombstone: an event that writes a subject-scoped field for
that subject afterwards mints a fresh key, so later values are readable while earlier ones stay
shredded.

### Concurrency

Sequential per effect: one in-flight invocation, strict position order, one dedicated thread. An
effect whose handler is slower than its event arrival rate falls behind, and that lag is visible in
`/status`. That is the correct behaviour, not an unbounded queue.

## 9. Tests

`tests/*.star` files declare `cases = [...]`. Each case seeds a throwaway store with `given` and
runs **one** module against it. `hekla test` covers all three module kinds.

```starlark
# tests/orders.star

load("events/order.star", "order_placed", "order_shipped")

A = "11111111-1111-1111-1111-111111111111"
B = "22222222-2222-2222-2222-222222222222"

# One dict serves as both the command's input and the event's fields, since they
# line up here. Splice it with `dict(ORDER, ...)` where a case needs a variation.
ORDER = {
    "order_id": A,
    "customer_id": 1,
    "shop_id": 7,
    "email": "a@example.com",
    "shipping_address": "1 High St",
    "order_total": "19.99",
    "notes": "leave at door",
}
PLACED = order_placed(**ORDER)

cases = [
    case(
        name = "places a first order",
        command = "place-order",
        input = ORDER,
        expect = PLACED,
    ),
    case(
        name = "rejects a repeat email",
        command = "place-order",
        given = [PLACED],
        input = dict(ORDER, order_id = B, customer_id = 2, order_total = "5.00"),
        expect = reject("email_taken", "that email has already placed an order"),
    ),
    case(
        name = "projects the order with its personal columns readable",
        projector = "orders",
        given = [PLACED],
        expect = {"orders": [{
            "order_id": A,
            "customer_id": 1,
            "email": "a@example.com",
            "status": "placed",
        }]},
    ),
    case(
        name = "confirms to the revealed address",
        effect = "notify-customer",
        given = [PLACED],
        responds = [http_response(status = 200)],
        expect = [
            http_call(
                method = "POST",
                url = "https://mail.example/confirm",
                body = {"to": "a@example.com", "order_id": A, "first_order": True},
            ),
            command_call("record-notified", {
                "order_id": A,
                "notification_id": uuid5("00000000-0000-0000-0000-000000000001", "confirmation"),
            }),
        ],
    ),
]
```

`case(...)` takes exactly one of `command`, `projector` or `effect`, naming the module by its file
stem. `given` is a list of events, constructed from the event definitions. `name` labels the case.

| Kind | Extra inputs | `expect` |
|---|---|---|
| `command` | `input` (a dict) | an event, a list of events, `reject(...)` or `invalid_input(...)` |
| `projector` | - | a dict of entity name to the rows the read API should return |
| `effect` | `responds` (a list of `http_response(...)`) | the ordered list of `http_call(...)`, `command_call(...)` and `erase_call(...)` the handler should have made |

- `http_response(status = , body = , headers = )` stubs a reply. `responds` serves them to the
  handler's `http.*` calls in the order it makes them. Running past the end is a case failure. The
  status has to be one a handler can actually see: the runtime retries 408, 425, 429 and every 5xx
  itself, so a case declaring one would describe a path that cannot happen.
- `http_call(url = , method = , body = )` is an expected request. **Only the arguments you give are
  compared**, so a case need not restate headers it does not care about.
- `command_call(name, input)` is an expected `invoke_command`.
- `erase_call(subject_field, subject_value)` is an expected `erase`. The erase really runs against
  the case's own key store, so a `reveal` after it fails exactly as it would live.
- An effect case needs nothing extra for state: its boundary folds the same seeded log, so `given`
  is both the trigger and the state. An effect that folds its own trigger type counts the trigger,
  so a single `given` event leaves that count at one.
- A projector case asserts through the read API, so subject-scoped columns read back **decrypted**.
  Write the assertion in plaintext.
- `expect = []` means "no events" for a command case and "no external calls" for an effect case; the
  target decides.
- Everything a handler can observe is pinned, so a case is reproducible: `now()` and
  `event.timestamp` are both `1970-01-01T00:00:00Z`, and the nth `given` event has id
  `00000000-0000-0000-0000-00000000000n`, counting from 1. That is what makes an id a handler
  derived with `uuid5(event.id, ...)` assertable.

A case tests **your logic**, not the runtime around it. Batching, checkpoints, retry, the journal and
replay are covered by hekla's own test suite.

## 10. Builtins by directory

Which builtins are in scope depends on the directory. This is why hekla ships its own language
server (`hekla lsp`): a generic Starlark server has one environment for the language, and
hekla has five.

**Everywhere** (`events/`, `lib/`, `commands/`, `projectors/`, `effects/`, `tests/`), on top of
standard Starlark:

```
event  schema  entity  index  all_events
str  int  uint  bool  uuid  timestamp  money  json  one_of  optional
put  patch  delete
reject  invalid_input
uuid5
```

Note that the shared set is deliberately loose: `put` exists in a command's environment, it just has
nothing to do there. What actually constrains you is the per-kind shape in section 2.

**Commands** add: `now()`, valid only inside `handle`.

**Projectors** add: `get(entity, key)`.

**Effects** add: `http` (`.get`, `.post`, `.put`, `.patch`, `.delete`), `invoke_command`, `now`,
`log`, `reveal`, `erase`.

**Test files** add: `case`, `http_response`, `http_call`, `command_call`, `erase_call`.

Standard Starlark is otherwise available: `dict`, `list`, `len`, `sorted`, `any`, `all`, `enumerate`,
`zip`, `range`, `min`, `max`, `fail`, comprehensions, and so on. There is no clock, no randomness, no
filesystem and no network beyond the builtins above.

## 11. Subject-scoped encryption and erasure

A field marked `subject = "sibling_field"` is encrypted under a key scoped to that subject's identity
`(subject_field, subject_value)`, in the tag index, the event payload, and any read-model column, all
before it reaches storage.

**Erasing a subject is deleting its key**: `hekla erase customer_id 42`. One O(1) operation makes
every value scoped to that subject unmatchable and unreadable across the log and every read model at
once, with no rewrite, compaction, or index rebuild.

**Information flow.** Plaintext exists only at the HTTP command input (the client supplied it) and at
read-API output or an effect's `reveal()` (the runtime decrypted it). Everywhere in between (the log,
the tag index, read-model columns, and every `fold` and every `handle` body) it is ciphertext. That
includes an effect's folded `state`: a subject field carried through a fold is a handle there too, so
an effect that needs the plaintext calls `reveal()` on it exactly as it would on `event.data`.

A handler reads a subject field as an **opaque handle**. It can store it (`put`/`patch` keep the
ciphertext) and compare it for equality, but it cannot concatenate, slice, or otherwise derive a
plaintext string from it. **A derivation you want must be computed by the command and emitted as its
own subject field.**

**Per-field, not per-event.** An `order.placed` has both a customer and a shop, so scoping the whole
event to one would destroy the other's record on erasure. Put `email` under the customer key and
`order_total` under the shop key, and leave the ids plaintext.

Practical rules:

- Subject id fields stay plaintext: they are how the runtime finds the key. After erasure the log
  still shows `customer_id:42` with the personal fields unreadable. That is standard
  crypto-shredding.
- Encryption is deterministic (AES-SIV), so it leaks equality and frequency. Fine for
  high-cardinality ids. **Do not give a low-cardinality field (a status enum) a subject.**
- `unique = True` keeps a global token past erasure: it still proves "some subject once used this
  value" without revealing it. Opt in per field, and only when you need a uniqueness rule that
  survives erasure.
- **A field appended without a subject can never be erased.** `hekla check` warns when a
  personal-looking field name (`email`, `phone`, `address`, `name`, `dob`, `ssn`, `postcode`, `zip`,
  `birth`) has no subject.
- `HEKLA_MASTER_KEY` (32 bytes, base64-encoded) wraps every per-subject key. **Losing it is total, unrecoverable loss**
  of every subject-scoped value. `hekla rotate` rewraps under a new master, unwrapping with
  `HEKLA_MASTER_KEY_PREVIOUS`, without touching any ciphertext.
- Erasure cannot un-send an email an effect already delivered. External sinks are outside the
  boundary.
- **Three places decrypt**: the read API (a projector's subject columns, on every `GET /read/...`),
  an effect's `reveal()`, and `GET /admin/events...` (section 13). All three are the same boundary
  and all three fail the same way once a key is gone. Introspection renders an unreadable field as
  its stored ciphertext with an explicit `erased` marker rather than dropping it, which is the one
  way it differs from the read API: a read model has to look like an ordinary row, and an operator
  should not have to infer that a field ever existed.

## 12. Reading values: dot vs subscript

> **A host-built value with a fixed shape is read with dot access. Everything else is a dict read by
> subscript.**

**Dot access:**

- `input`, built from the command's declared `schema`: `input.email`.
- `event.data`, built from the event's declared field schema: `event.data.order_id`. Also
  `event.type`, `event.id` and `event.timestamp`.
- The two fixed-shape wrappers an effect gets back: `http.*` returns `{status, body, headers}` and
  `invoke_command` returns `{status, body}`.

A field the schema does not declare is a shape error; one the payload omits reads as `None`.

**Subscript:**

- A folded `state`, in a command or an effect, and any dict you build yourself, including a `put()`
  row.
- A read-model row from `get()`. Rows stay dicts because `put()` takes a dict.
- The **contents** of the wrappers above, whose shape the host cannot promise: a response `body` is
  parsed JSON when the bytes parse and a string otherwise, and `headers` is keyed by arbitrary
  header names.

## 13. HTTP surface

- `POST /commands/{name}` executes a **public** command. Body is the input JSON. Accepts
  `Idempotency-Key` and `X-Correlation-Id` headers. Success returns
  `{correlation_id, causation_id, positions: {first, last}, events: [...]}`. Status mapping:
  committed 200, `reject` 422, `invalid_input` 400, unresolved concurrency conflict 409.
- `GET /read/{projector}/{entity}/{key}` returns `{item, position}`, or 404.
- `GET /read/{projector}/{entity}?<field>=<value>&limit=&cursor=` returns
  `{items, next_cursor, position}`. Only the key and declared indexes are filterable; anything else
  is a 400. Pagination is cursor-based, never offset.
- **Read-your-writes** is opt-in per read: pass `?after=<pos>` (the `positions.last` a command
  returned) and the read blocks until that projector reaches the position, then serves the normal
  snapshot. Bounded by `timeout_ms` (default 5s, capped at 30s); on timeout it fails closed with 503
  rather than silently serving stale data.
- `POST /projectors/{name}/replay` schedules a rebuild-and-swap.
- `POST /effects/{name}/skip/{position}` is an explicit, manual operator action for a wedged effect.
  Never automatic.
- `GET /status` gives per-module positions and lag, plus each effect's consecutive-failure count and
  last error, so a wedge is distinguishable from ordinary lag.
- `GET /health` is a liveness check, with none of `/status`'s per-module detail.
- `GET /openapi.json` and `GET /docs` (a Scalar reference over it) are generated from your project,
  and describe every endpoint above. Commands get their real request body from your `input` schema.
  Each projector entity gets two paths, with the key typed from its key column and one query
  parameter per field you can actually filter on. The operator endpoints list your projector and
  effect names. Responses carry real schemas, so a client generator has something to work with.

  `components/schemas` also holds a schema per declared event and per entity. The entity schemas are
  read responses; the **event schemas are documentation of the log, not wire shapes**. An event's
  fields never appear in an HTTP response, so `event.*` is there to describe the vocabulary your
  commands append and your projectors and effects subscribe to. The one place the event set is
  load-bearing is `EmittedEvent.type`, the enum a command's 200 reports.
- Opening the SQLite files directly is not a supported surface. The table layout is private.

### Introspection (`/admin`)

A read-only surface for looking at a running system. Every route is a `GET` and none of them writes;
`replay` and `skip` stay where they are.

| Endpoint | What it answers |
|---|---|
| `GET /admin` | An index of everything below. `hekla serve` prints this URL at startup. |
| `GET /admin/events` | Page the log, newest first. `?type=` and `?tag=` may each repeat: types OR together, tags AND together. `?cursor=` is a log position, `?direction=forward` walks the other way. |
| `GET /admin/events/{position}` | One event: its envelope ids, its payload, its tags including the host's own. |
| `GET /admin/traces/{correlation_id}` | Every event of one causal chain: the command's own events, plus anything an effect appended in reaction, transitively. Pages, so a chain longer than one page reports `complete: false` and a cursor to finish it. |
| `GET /admin/effects` and `/{name}` | Per effect: position, lag, durable watermark, failure count, last error, quarantine record. |
| `GET /admin/effects/{name}/invocations[/{position}]` | An effect's invocations, and for one of them the calls it journaled with what each returned. This is how you diagnose a wedge. The call list pages (`?cursor=` is the previous page's `next_cursor`), so a truncated list never reads as the whole sequence. |
| `GET /admin/projectors` and `/{name}` | Readiness, lag, entity shapes, and the definition hash the read model was built under. `?counts=true` adds row counts (a full scan, so opt-in). |
| `GET /admin/schema` | The project this process loaded: events, commands (including internal ones), projectors, effects, and each module's source hash. |
| `GET /admin/system` | Version, uptime, data directory, operational-DB schema version, keystore state, and the effective `hekla.toml`. |
| `GET /admin/subjects` and `/{field}/{value}` | Which subjects still hold key material. Never the key material. |

**Payloads are shown, and subject-scoped fields are decrypted by default.** That is the same boundary
`GET /read/...` already crosses for a projector's subject columns; pass `?decrypt=false` to see the
stored ciphertext instead. It is not a way around erasure: see section 11. It is a **wider** surface
than the read API though, not merely an equal one: a read model exposes the columns a projector chose
to materialise, while this reaches every field of every event. A decrypting request logs an audit line
for that reason.

Each subject field reports its own state, so one unreadable value marks one field rather than failing
the request:

| State | Meaning |
|---|---|
| `decrypted` | The value in `data` is plaintext. |
| `encrypted` | Nothing was attempted: `?decrypt=false`, or no master key is configured. |
| `erased` | The subject has no key. Irreversible, and `data` holds ciphertext forever. |
| `stale` | The subject *has* a key, but this value was written under a superseded one (erased, then recreated by a later event) or is corrupt. Unreadable, but not the total loss `erased` reports. |
| `unreadable` | The key could not be obtained at all: a corrupt wrapping, or a master that is not configured. The server log names it. |

**A `sources` of `null` means `all_events()`**, and an empty list means a module subscribed to
nothing. The two are different answers and are reported differently.

**A journaled call's arguments are not stored, only hashed**, so an invocation view reports what came
back and not what was sent. Storing the arguments would let plaintext that came out of `reveal()`
outlive the erasure of the subject it belonged to.

**Correlation tracing only covers events appended by a version of hekla that stamps the correlation
tag.** The id has always been in the envelope, but a query filters on tags, so events older than that
carry nothing to find them by.

**None of this is authenticated, and neither is the rest of the API.** A caller who can reach the port
can already append events and skip an effect's work; the bind address is the boundary and defaults to
`127.0.0.1`. One prefix is what lets a deployment that binds wider deny `/admin` in a proxy.

### The console

`hekla serve` prints a URL. Open it and you get an admin console over everything above: the log with
filters and a payload viewer, a correlation chain drawn as the causal tree it is, an effect's
journaled calls, projector shapes, the loaded project, and the subject-key inventory.

**It is the same URL as the API, not a second one.** A request that names `text/html` in its `Accept`
header gets the console; everything else gets the JSON above, byte for byte unchanged. `curl` sends
`*/*` and so does a bare `fetch()`, so neither is affected. That is also where deep links come from:

```sh
curl localhost:8080/admin/effects/send-welcome            # the effect, as JSON
open http://localhost:8080/admin/effects/send-welcome     # the console, on that effect
```

The console is compiled into the binary. There is no build step, no npm, and no CDN: it is plain ES
modules plus one vendored 13KB runtime (`ui/VENDOR.md`), served from `/admin/assets/{file}`, so it
works with no network at all. `HEKLA_UI_DIR=./ui` serves it from disk instead, for editing it without
a recompile.

Two things it does that the raw API does not:

- **It can act.** `POST /projectors/{name}/replay` and `POST /effects/{name}/skip/{position}` are
  reachable from the projector and effect views, each behind a confirmation that makes you type the
  module's name. `/admin` itself stays read-only; these are the same operator endpoints that have
  always lived outside it.
- **It decrypts one event at a time.** A list is fetched with `?decrypt=false` and a payload with
  `?decrypt=true`, so one audit line in the server log means one operator read one event, rather than
  one page having decrypted a hundred fields nobody looked at.

| Key | Does |
|---|---|
| `⌘K` / `Ctrl-K` | Jump to a position, a correlation id, an effect, a projector, or a view |
| `j` / `k` | Move the row cursor |
| `Enter` | Open the row |
| `Esc` | Close the drawer or dialog |
| `/` | Focus the filter |

## 14. CLI and config

| Command | Purpose |
|---|---|
| `hekla check <dir>` | Static analysis: parse, resolve the load graph, verify every clause filters on fields the event type declares and indexes, verify event constructors match field schemas, verify projector indexes reference declared fields. For CI and pre-commit. |
| `hekla fmt <dir>` (`--check`) | Format `.star` files. Indentation is syntactically meaningful. |
| `hekla test <dir>` | Run the scenarios under `tests/`. |
| `hekla serve <dir>` (`--addr`, `--data-dir`) | Run the runtime and HTTP API. |
| `hekla openapi <dir>` | Print the generated OpenAPI 3.1 document to stdout. Reads the project only (no data directory, no master key), so a committed `openapi.json` can be diffed in CI. Findings go to stderr, so redirecting stdout gives you pure JSON. |
| `hekla lsp` | Language server over stdio, for editor integration. |
| `hekla erase <field> <value> <dir>` | Delete a subject's key. Irreversible. |
| `hekla rotate <dir>` | Rewrap every subject key under the current `HEKLA_MASTER_KEY`. |

`hekla.toml` is optional; every value has a default.

```toml
[effects]
pool_size = 16              # validated, reserved for parallel lanes

[retention]
effect_journal_days = 7     # completed invocation journals are swept after this

[projectors]
auto_rebuild = true         # rebuild on a definition change, or leave it to an operator
```

Environment: `HEKLA_MASTER_KEY`, a base64-encoded 32-byte key, required only if any field declares a
`subject`. `HEKLA_MASTER_KEY_PREVIOUS` is a comma-separated list of prior masters, read during
rotation.

## 15. What `hekla check` catches

`hekla check` is the only static analysis Starlark gets, so it is thorough. Errors:

- a clause filtering an event type on a field it does not declare, or that is `indexed = False`
- a constraint value that is not a valid instance of the field's type
- a `handle` key filtering a subject-encrypted field at all (a subscription is plaintext-only)
- a `query` or `fold` key filtering a subject-encrypted field without its subject constrained, and
  without `unique`
- a clause naming an unregistered event type
- a bare definition used as a dispatch key
- a `def handle` / `def fold` where a clause-keyed map is required
- a leftover `source = [...]` global (the map's keys are the subscription now)
- a `load()` outside `events/` and `lib/`
- an event declared at module scope outside `events/`
- a projector index naming a field the entity does not declare
- a file stem that is not a valid slug

Warnings:

- a personal-looking field name with no `subject`, so it could never be erased
- a `fold` entry for a type `query` never returns, so it never runs
- `fold` declared with no `query`, so nothing folds and `handle` only sees `initial`
- an effect's `query` declared with no `fold`, so the boundary is read and discarded. An effect
  never appends, so unlike a command's, a bare `query` guards nothing
- a query clause with no constraint on a high-cardinality field, so it guards broadly
- a query clause constraining most of an event's fields, which looks like a copied emit call

Not caught:

- a `fold` or `handle` arm made dead by its **constraint** rather than its type. A `query` is
  evaluated against a placeholder (an input for a command, one event per subscribed type for an
  effect), so constraint values are not statically known.
- an effect's `query`, when its `handle` subscribes to `all_events()`. There is no event type to
  build a placeholder from, so that one is checked at runtime only.

## 16. Rules of thumb

- **One rule to remember about dispatch: a key is a query clause.** Always a call, never a bare
  definition. `all_events()` is the clause that selects everything.
- **In a command, put an event type in `query` if a concurrent write of it should fail this command.
  Put it in `fold` only if `handle` needs to know about it.** They are different questions.
- **In an effect, `query` asks only "what state do I need".** There is no append to guard, so the two
  questions collapse into one and a `query` with no `fold` is always a mistake.
- **A fold arm returns the new state.** Never mutate `state`, in a fold arm or in `handle`.
- **A projector arm returns a list of ops**, possibly empty.
- **An effect arm's return value is ignored.** Its contract is the calls it makes.
- **An effect gets state by folding the log, never by reading a projector.** That is why a fold
  cannot be stale and needs no journal entry, and it is why there is no `read()`.
- **Client-supplied ids.** There is no `uuid4()`, and there will not be: a retry re-runs the code
  that would mint one. Have the caller send the id, take one from a journaled response, or derive
  one with `uuid5(event.id, "...")`.
- **Use `now()` for domain time only**, and only in a command's `handle` or an effect. The envelope
  already carries the append timestamp.
- **Reach for `commands/internal/` for anything an effect completes**, so a completion fact cannot be
  forged over HTTP.
- **Never use a float.** `money()` for currency, scaled integers for everything else.
- **Give personal fields a `subject` from day one.** A field appended without one can never be
  erased.
- **Long free text gets `indexed = False`**, so it does not become a huge tag.
- **A multi-statement handler is a named `def` referenced from the map.** A one-liner is a `lambda`
  inline. Both are the same form.

## 17. A complete project

```starlark
# events/order.star

order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", unique = True, max_length = 200),
        "address": str(subject = "customer_id", max_length = 200),
        "total": money(),
        "notes": str(indexed = False),
    },
)

order_shipped = event(
    type = "order.shipped",
    fields = {"order_id": uuid(), "carrier": str(), "tracking": str()},
)

order_notified = event(
    type = "order.notified",
    fields = {"order_id": uuid(), "notification_id": uuid()},
)
```

```starlark
# lib/validation.star

def is_blank(value):
    return value == None or value.strip() == ""
```

```starlark
# commands/place-order.star

load("events/order.star", "order_placed")
load("lib/validation.star", "is_blank")

input = schema(
    order_id = uuid(),
    customer_id = uint(),
    email = str(),
    address = str(),
    total = money(),
    notes = str(),
)

def query(input):
    return order_placed(email = input.email)

initial = {"taken": False}

fold = {order_placed(): lambda state, event: dict(state, taken = True)}

def handle(input, state):
    if is_blank(input.address):
        return invalid_input("address must not be blank")
    if state["taken"]:
        return reject("email_taken", "that email has already placed an order")
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
        address = input.address,
        total = input.total,
        notes = input.notes,
    )
```

```starlark
# commands/internal/record-notified.star

load("events/order.star", "order_notified")

input = schema(order_id = uuid(), notification_id = uuid())

def query(input):
    return order_notified(order_id = input.order_id)

initial = False

fold = {order_notified(): lambda state, event: True}

def handle(input, state):
    if state:
        return reject("already_notified", "this order was already notified")
    return order_notified(order_id = input.order_id, notification_id = input.notification_id)
```

```starlark
# projectors/orders.star

load("events/order.star", "order_placed", "order_shipped")

orders = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 200),
        "status": one_of(["placed", "shipped"]),
        "tracking": optional(str()),
    },
    indexes = [index("by_customer", ["customer_id"])],
)

handle = {
    order_placed(): lambda event: [put(orders, {
        "order_id": event.data.order_id,
        "customer_id": event.data.customer_id,
        "email": event.data.email,
        "status": "placed",
    })],
    order_shipped(): lambda event: [patch(orders, event.data.order_id, {
        "status": "shipped",
        "tracking": event.data.tracking,
    })],
}
```

```starlark
# effects/notify-customer.star

load("events/order.star", "order_placed")

def query(event):
    return [order_placed(customer_id = event.data.customer_id)]

initial = {"orders": 0}

fold = {order_placed(): lambda state, event: {"orders": state["orders"] + 1}}

# The fold is inclusive of this event, so a customer's first order counts one.
def notify(event, state):
    response = http.post(
        url = "https://mail.example/confirm",
        body = {
            "to": reveal(event.data.email),
            "order_id": event.data.order_id,
            "first_order": state["orders"] == 1,
        },
    )
    if response.status >= 400:
        log("confirmation rejected with status " + str(response.status))
        return
    invoke_command("record-notified", {
        "order_id": event.data.order_id,
        "notification_id": uuid5(event.id, "confirmation"),
    })

handle = {order_placed(): notify}
```

```starlark
# tests/orders.star

load("events/order.star", "order_placed", "order_shipped")

A = "11111111-1111-1111-1111-111111111111"
B = "22222222-2222-2222-2222-222222222222"

PLACED = order_placed(
    order_id = A,
    customer_id = 1,
    email = "a@example.com",
    address = "1 High St",
    total = "19.99",
    notes = "",
)

cases = [
    case(
        name = "places a first order",
        command = "place-order",
        input = {
            "order_id": A,
            "customer_id": 1,
            "email": "a@example.com",
            "address": "1 High St",
            "total": "19.99",
            "notes": "",
        },
        expect = PLACED,
    ),
    case(
        name = "rejects a repeat email",
        command = "place-order",
        given = [PLACED],
        input = {
            "order_id": B,
            "customer_id": 2,
            "email": "a@example.com",
            "address": "2 Low Rd",
            "total": "5.00",
            "notes": "",
        },
        expect = reject("email_taken", "that email has already placed an order"),
    ),
    case(
        name = "projects a placed order, then ships it",
        projector = "orders",
        given = [PLACED, order_shipped(order_id = A, carrier = "dhl", tracking = "TRK1")],
        expect = {"orders": [{
            "order_id": A,
            "customer_id": 1,
            "email": "a@example.com",
            "status": "shipped",
            "tracking": "TRK1",
        }]},
    ),
    case(
        name = "confirms to the revealed address",
        effect = "notify-customer",
        given = [PLACED],
        responds = [http_response(status = 200)],
        expect = [
            http_call(
                method = "POST",
                url = "https://mail.example/confirm",
                body = {"to": "a@example.com", "order_id": A, "first_order": True},
            ),
            command_call("record-notified", {
                "order_id": A,
                "notification_id": uuid5("00000000-0000-0000-0000-000000000001", "confirmation"),
            }),
        ],
    ),
]
```

Run it:

```
export HEKLA_MASTER_KEY=$(openssl rand -base64 32)
hekla check .
hekla test .
hekla serve . --addr 127.0.0.1:8080
```

```
curl -X POST localhost:8080/commands/place-order \
  -H 'content-type: application/json' \
  -H 'idempotency-key: req-1' \
  -d '{"order_id":"1111...","customer_id":1,"email":"a@example.com",
       "address":"1 High St","total":"19.99","notes":""}'

curl 'localhost:8080/read/orders/orders/1111...'
curl 'localhost:8080/read/orders/orders?customer_id=1&limit=20'
```
