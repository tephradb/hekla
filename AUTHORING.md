# Authoring hekla projects

A hekla project is a heklang program plus a directory convention and an HTTP surface. **The language
is documented in [heklang's own `docs/`](../heklang/docs/), and this document does not repeat it.**
That split is deliberate: the rules about commands, projectors, effects, folds, sealed content and
tests belong to the language and are versioned with it, so a second copy here could only drift.

What this document covers is everything hekla adds around the language: where a declaration has to
live, what the runtime stamps on an event, what `hekla check` sees that the compiler does not, the
generated HTTP surface, the CLI, and how subject encryption really works underneath the seal the
language models.

hekla is a single-app event-sourcing / DCB (Dynamic Consistency Boundary) runtime:

- **Commands** validate input, check invariants against replayed history, and append events. They
  are the only writers.
- **Projectors** consume events and build queryable SQLite read models.
- **Effects** react to events with durable, replay-safe side effects (HTTP, invoking commands).

Determinism is structural, not policed: a command cannot call out, a projector cannot decrypt, and
only an effect journals, each because of what kind of declaration it is rather than because
something checks. Deployment is source text; there is no build step.

### Where to read about the language

| For | Read |
|---|---|
| Commands, `state`, slices, the append condition | `heklang/docs/commands.md` |
| Projectors, `put` / `patch` / `update` / `delete`, subject propagation | `heklang/docs/projectors.md` |
| Effects, arms, the journal, `reveal` and `erase`, the 14 numbered rules | `heklang/docs/effects.md` |
| `test` scenarios: `given`, `run`, `project`, `deliver`, `expect` | `heklang/docs/testing.md` |
| Types, optionals, money, strings, containers, module `fn`s | `heklang/docs/types.md` and its siblings |
| What a host must provide, and what it may not | `heklang/docs/host.md` |

---

## 1. Project layout

**The directory decides what a declaration is allowed to be; the declaration decides its name.**

```
project/
  events/              # event declarations
  lib/                 # shared pure `fn`s and constants
  commands/            # public commands: HTTP-routed and invokable by effects
  commands/internal/   # internal commands: invokable by effects, NOT HTTP-routed
  projectors/
  effects/
  tests/               # hekla test scenarios
  hekla.toml           # operational config, optional
  data/                # created by the runtime: event log, read models, operational DB
```

Rules:

- **There is no import.** Every `.hk` file in the project is compiled together, so a command names an
  event declared three directories away without saying so, and file order is irrelevant.
- **A declaration is named by its declaration.** `commands/place-order.hk` declaring
  `command PlaceOrder(...)` is routed at `POST /commands/PlaceOrder`. The file name is a convention
  this repository follows and the runtime does not read; kebab-case files with PascalCase
  declarations is what the examples do.
- **The directory is hekla's rule, not the language's.** heklang would accept a `projector` in
  `commands/`; `hekla check` refuses it, because the directory is what makes a command routable.
- **Public vs internal is structural.** Only the literal `commands/internal/` prefix marks a command
  internal. Nesting elsewhere is free and stays public: `commands/billing/refund.hk` is public.
- Names must be unique within a kind, which the compiler enforces for the whole program.
- **Deploy is restart.** The project loads at startup; there is no hot reload.
- Files outside these directories, and non-`.hk` files, are ignored.

## 2. What the runtime adds to an event

An event's fields are yours. Around them the runtime stamps an **envelope**: an `event_id`, a
`correlation_id`, a `causation_id`, an optional `triggering_event_id`, and the append `timestamp`.
Three of those are readable through a projector's or an effect's trigger binding:

```
on @order.placed as e {
  // e.order_id and the rest of the event's own fields, plus:
  // e.id        the envelope's event id, stable across a rebuild and a replay
  // e.at        the append timestamp
  // e.position  the log position
}
```

A command's fold cannot reach them: a fold arm binds the event's declared fields and nothing else.

**Prefer `e.at` over `now()` for the append instant.** A command that stamps the current time into a
field only recording when the event was appended duplicates what the envelope already holds. `now()`
is right for time that is genuinely domain data (`expires_at`, `due_date`, a `purchased_at` an
upstream system reported).

**Two reserved tags** live in a `_hekla_` namespace no event field may occupy (`hekla check` refuses
one that tries): a keyed command's idempotency tag, and the correlation tag every event carries. Both
are stripped from command responses. The correlation id is in the envelope payload, but a store query
filters on tags only, so without the tag a causal chain could be found only by decoding every event
in the log.

**Every field is a tag** unless it opts out with `@no_index`. That is what makes a slice answerable
by the store, and it is why a filter on a `@no_index` field is a compile error rather than a query
that silently matches nothing.

## 3. Idempotency

Two mechanisms, for two different problems.

**Id-based dedupe is the default and needs nothing.** New-entity ids are parameters, so a retried
request carries the same id, the command's own boundary sees the existing event, and the duplicate
rejects. Idempotency for creation falls out of DCB.

**A key is for when nothing in the input distinguishes intent.** Approving a claim twice with
identical input could be one retry or two deliberate approvals, and no domain check can resolve that.
Pass `Idempotency-Key`; the runtime hashes it with the command name into a `_hekla_idem` tag, stamps
every emitted event with it, and guards the append against that tag existing **anywhere in the log**.
When it fires, the original commit's events, positions and identity come back verbatim rather than
the command re-running.

The guard is whole-log rather than boundary-scoped, so a duplicate that committed anywhere is caught
even once the boundary has moved past it, and it is asserted by the append itself, so there is no
read-then-write window. Nothing is stored outside the log, so nothing has to be swept.

A first attempt that *rejected* appended nothing and so left no tag: a retry re-decides, and returns
the same rejection unless state moved.

**An effect's `invoke` gets one automatically**, derived from the journal identity of the call (the
effect, the position, the call and its ordinal). It is the same on every replay and different for
every call, so a crash between the append and the journal write replays into the existence clause
rather than appending the fact twice.

## 4. Read models, and what the read API needs from them

A projector's entities become SQLite tables, and the read API is generated from them, so a few
constraints come from the reader rather than from the language. `hekla check` reports each:

- **The key must be a present, orderable, plaintext scalar.** The read API paginates by the key as an
  opaque cursor and binds it as a typed filter. An optional key has no cursor; a `Bool` key cannot
  page; a `Money` key is stored as its decimal string, so `ORDER BY` and `key > ?` would compare
  lexicographically and sort `"2"` after `"10"`. (heklang refuses most of these itself, for the same
  reasons.)
- **An index over a sealed column is refused.** A filter arrives as plaintext and, without the
  subject, cannot derive the key to compare against the ciphertext, so the index could never match.
  Filter by the plaintext subject id instead.
- **A filterable column may not be named `limit`, `cursor`, `after` or `timeout_ms`.** Those are the
  read API's own query parameters, so such a column could never be filtered.

**A column's subject is propagated, not declared.** You never write `@subject` on an entity column: a
column that receives sealed content becomes sealed, and the read API decrypts it on the way out. That
is `heklang/docs/projectors.md` rule 9, and it is what lets a projector store a credential it may
never read.

**A sealed column must be optional.** An erased subject's column reads back *absent*, and a type that
cannot be absent could not say so. This is the one place the port changed an example's declaration
rather than its syntax.

## 5. Subject-scoped encryption and erasure

The language models a **seal**: a value carries the field, subject and id its key is filed under, and
only `reveal` reads it out. hekla is what makes that real. A field marked `@subject(sibling_field)`
is encrypted under a key scoped to `(subject_field, subject_value)` in the tag index, the event
payload and any read-model column, all before it reaches storage.

**Erasing a subject is deleting its key**: `hekla erase customer_id 42`, or `erase(customer_id)` from
an effect arm. One O(1) operation makes every value scoped to that subject unmatchable and unreadable
across the log and every read model at once, with no rewrite, compaction or index rebuild.

**Information flow.** Plaintext exists only at the HTTP command input (the client supplied it) and at
read-API output, an effect's `reveal(...)`, or `GET /admin/events...` (the runtime decrypted it).
Everywhere in between it is ciphertext, and the language never sees one: the host decrypts on the way
in and encrypts on the way out, so a program holds sealed *plaintext* it is not allowed to read and
the store holds ciphertext nobody interprets.

Practical rules:

- **Subject id fields stay plaintext**: they are how the runtime finds the key. After erasure the log
  still shows `customer_id:42` with the personal fields unreadable. That is standard
  crypto-shredding.
- **Encryption is deterministic (AES-SIV)**, so it leaks equality and frequency. Fine for
  high-cardinality ids. **Do not give a low-cardinality field (a status enum) a subject.**
- **Erasure is a point-in-time shred, not a tombstone.** A later event writing the same subject's
  field mints a fresh key, so values written after the erase are readable while everything before it
  stays shredded. No *read* path ever mints: re-projecting a log whose subject was erased writes the
  column NULL rather than creating the key the erasure destroyed.
- **A field appended without a subject can never be erased**, and nothing warns about it: which
  fields are personal is a judgement about meaning, and hekla cannot make it from a name.
- **`HEKLA_MASTER_KEY`** (32 bytes, base64) wraps every per-subject key. **Losing it is total,
  unrecoverable loss** of every subject-scoped value. `hekla rotate` rewraps under a new master,
  unwrapping with `HEKLA_MASTER_KEY_PREVIOUS`, without touching any ciphertext. Boot fails fast with
  a subject-specific message when a project uses subjects and the key is absent or wrong.
- **Per-field, not per-event.** An `order.placed` has both a customer and a shop, so scoping the whole
  event to one would destroy the other's record on erasure. Put `email` under the customer key and
  `order_total` under the shop key, and leave the ids plaintext.
- **Erasure cannot un-send an email** an effect already delivered. External sinks are outside the
  boundary.
- **Three places decrypt**: the read API (a projector's subject columns, on every `GET /read/...`),
  an effect's `reveal(...)`, and `GET /admin/events...` (section 7). All three are the same boundary
  and all three fail the same way once a key is gone. Introspection renders an unreadable field as
  its stored ciphertext with an explicit `erased` marker rather than dropping it, which is the one
  way it differs from the read API: a read model has to look like an ordinary row, and an operator
  should not have to infer that a field ever existed.

**There is no cross-subject uniqueness on a sealed field.** Enforcing "one account per email address"
across accounts would need an equality over two ciphertexts, which leaks whether they hold the same
value and is refused. Keep a plaintext handle beside the sealed address and fold on that:
`examples/orders` and the test suite's `ACCOUNT_EVENTS` both do. Erasing a subject does not reopen a
handle it claimed, which is the property that rule exists for, and it holds with no key at all.

## 6. What `hekla check` catches

Most static analysis is the compiler's now, and its diagnostics carry a span, a code and a hint. What
`hekla check` adds is what only hekla knows.

Errors:

- a declaration outside the directory its kind requires (a `command` not under `commands/`, and so on)
- an entity key the read API cannot paginate by, or an index it could never match (section 4)
- a filterable column colliding with a reserved read query parameter
- an event field in the reserved `_hekla_` tag namespace
- a projector column sealed under a subject whose type cannot be absent: an erasure blanks it, so a
  column that cannot say "absent" stalls the projector and breaks the read API's own schema
- a project directory that cannot be walked, so a silently missing command cannot deploy

Warnings, which never fail the check, because each is a judgement call:

- a boundary with no filter on a high-cardinality field, so it defeats the append's fast reject
- a boundary pinning most of an event's fields, which looks like a copied `emit`: a slice is a subset
  match, so over-constraining can match nothing

Everything else the old validation pass did is now a parse error with the offending field's own span:
a filter on an undeclared, `@no_index` or sealed field; an ill-typed filter value; an `emit` missing a
field; an `invoke` with an unknown argument; an index over a column the entity does not have.

## 7. HTTP surface

Every `{Name}` below is a *declared* name, not a file stem: `command PlaceOrder` is at
`/commands/PlaceOrder` whatever file it lives in.

- `POST /commands/{Name}` executes a **public** command. Body is the parameters as JSON. Accepts
  `Idempotency-Key` and `X-Correlation-Id` headers. Success returns
  `{correlation_id, causation_id, positions: {first, last}, events: [...]}`. Status mapping:
  committed 200, `reject` 422, `invalid` 400, unresolved concurrency conflict 409. A parameter the
  command does not declare is a 400 naming it, because a key binding by name cannot notice on its
  own is a typo far more often than it is spare data.
- `GET /read/{Projector}/{Entity}/{key}` returns `{item, position}`, or 404.
- `GET /read/{Projector}/{Entity}?<field>=<value>&limit=&cursor=` returns
  `{items, next_cursor, position}`. Only the key and declared indexes are filterable; anything else
  is a 400. Pagination is cursor-based, never offset.
- **Read-your-writes** is opt-in per read: pass `?after=<pos>` (the `positions.last` a command
  returned) and the read blocks until that projector reaches the position, then serves the normal
  snapshot. Bounded by `timeout_ms` (default 5s, capped at 30s); on timeout it fails closed with 503
  rather than silently serving stale data.
- `POST /projectors/{Name}/replay` schedules a rebuild-and-swap.
- `POST /effects/{Name}/skip/{position}` is an explicit, manual operator action for a wedged effect.
  Never automatic.
- `GET /status` gives per-module positions and lag, plus each effect's consecutive-failure count and
  last error, so a wedge is distinguishable from ordinary lag.
- `GET /health` is a liveness check, with none of `/status`'s per-module detail.
- `GET /openapi.json` and `GET /docs` (a Scalar reference over it) are generated from your project,
  and describe every endpoint above. Commands get their real request body from their parameters.
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
| `GET /admin/effects` and `/{Name}` | Per effect: position, lag, durable watermark, failure count, last error, quarantine record. |
| `GET /admin/effects/{Name}/invocations[/{position}]` | An effect's invocations, and for one of them the calls it journaled with what each returned. This is how you diagnose a wedge. The call list pages (`?cursor=` is the previous page's `next_cursor`), so a truncated list never reads as the whole sequence. |
| `GET /admin/projectors` and `/{Name}` | Readiness, lag, entity shapes, and the definition hash the read model was built under. `?counts=true` adds row counts (a full scan, so opt-in). |
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

**A `sources` is the event types a declaration's arms name**, and it is always a list. There is no
way to say "every event": a projector and an effect both select by named arm, so an empty `sources`
is a module subscribed to nothing rather than one subscribed to everything.

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
curl localhost:8080/admin/effects/SendWelcome            # the effect, as JSON
open http://localhost:8080/admin/effects/SendWelcome     # the console, on that effect
```

The console is compiled into the binary. There is no build step, no npm, and no CDN: it is plain ES
modules plus one vendored 13KB runtime (`ui/VENDOR.md`), served from `/admin/assets/{file}`, so it
works with no network at all. `HEKLA_UI_DIR=./ui` serves it from disk instead, for editing it without
a recompile.

Two things it does that the raw API does not:

- **It can act.** `POST /projectors/{Name}/replay` and `POST /effects/{Name}/skip/{position}` are
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

## 8. CLI and config

| Command | Purpose |
|---|---|
| `hekla check <dir>` | Parse the project and report every finding: the compiler's diagnostics, plus what only hekla knows (section 6). For CI and pre-commit. |
| `hekla test <dir>` | Run the scenarios under `tests/`, against real tephra, a real read model and a real key store. |
| `hekla serve <dir>` (`--addr`, `--data-dir`) | Run the runtime and HTTP API. |
| `hekla openapi <dir>` | Print the generated OpenAPI 3.1 document to stdout. Reads the project only (no data directory, no master key), so a committed `openapi.json` can be diffed in CI. Findings go to stderr, so redirecting stdout gives you pure JSON. |
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


## 9. Rules of thumb

- **`state` is a read declaration, not a binding.** What you fold is what you conflict on, so put an
  event type in a `state` when a concurrent write of it should fail this command, and use `guard`
  when you need a slice in the boundary that nothing folds.
- **Client-supplied ids.** There is no random uuid, and there will not be: a retry re-runs the code
  that would mint one. Have the caller send the id, take one from a journaled response, or derive one
  with `Uuid.derive(e.id, "...")`.
- **Use `now()` for domain time only.** The envelope already carries the append instant, readable as
  `e.at`.
- **Reach for `commands/internal/` for anything an effect completes**, so a completion fact cannot be
  forged over HTTP.
- **Never reach for a float.** `Money(n)` for currency, `Decimal(n)` or scaled integers otherwise.
- **Give personal fields a `@subject` from day one**, and make them optional. A field appended
  without a subject can never be erased, and a sealed column that cannot be absent cannot report an
  erasure.
- **Long free text gets `@no_index`**, so it does not become a huge tag.
- **An effect gets state by folding the log, never by reading a projector.** That is why a fold
  cannot be stale and needs no journal entry, and it is why there is no read.
- **Write the test.** `hekla test` runs heklang's own runner against hekla's real world: real tephra,
  a real SQLite read model and a real key store. An erasure case there is worth running precisely
  because the ciphertext and the deleted key are real.

## 10. A complete project

`examples/orders` is this, in full and checked by CI. It is eleven files; here is its spine.

```hek
// events/order.hk
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
  shop_id: Int,
  // Optional because an erased subject's column reads back absent, and a type that
  // cannot be absent could not say so.
  email: String? @subject(customer_id) @max(200),
  shipping_address: String? @subject(customer_id) @max(200),
  // A different subject on the same event: erasing the customer leaves this readable.
  order_total: Money(2) @subject(shop_id),
  // Free text nobody queries: opt out of tagging, and of being a huge tag.
  notes: String @max(2000) @no_index,
}
```

```hek
// commands/place-order.hk
command PlaceOrder(
  order_id: Uuid,
  customer_id: Int,
  shop_id: Int,
  email: String?,
  shipping_address: String?,
  order_total: Money(2),
  notes: String,
) {
  // Narrow: this one order. A caller retrying the same `order_id` is a no-op rather
  // than a second order.
  state placed: Bool = fold false
    on @order.placed(order_id) => true

  // Wide on purpose: an allocation is a rule about every order in the shop, so every
  // order in a shop conflicts with every other. That is what a hard cap costs, and the
  // retry loop is what absorbs it.
  state sold: Int = fold 0
    on @order.placed(shop_id) => sold + 1

  if placed {
    return
  }
  if sold >= LAUNCH_ALLOCATION {
    return reject("sold_out", "this shop's launch allocation is gone")
  }

  emit @order.placed {
    order_id, customer_id, shop_id, email, shipping_address, order_total, notes,
  }
}
```

```hek
// projectors/customer-orders.hk
projector CustomerOrders {
  entity Order {
    order_id: Uuid @key,
    customer_id: Int @index,
    // No `@subject` here: the seal propagates from the event fields written into them.
    email: String? @max(200),
    shipping_address: String? @max(200),
  }

  on @order.placed { order_id, customer_id, email, shipping_address } {
    put Order { order_id, customer_id, email, shipping_address }
  }
}
```

```hek
// effects/notify-customer.hk
effect NotifyCustomer {
  on @order.placed as e {
    // The fold stops at this event's own position, so the count is a function of the
    // log prefix rather than of when the handler happened to run.
    state orders: Int = fold 0
      on @order.placed(customer_id: e.customer_id) => orders + 1

    // `reveal` is the explicit decrypt boundary; only an effect has it.
    let response = http.post("https://mail.example/confirm", {
      "to": reveal(e.email),
      "order_id": e.order_id,
      "first_order": orders == 1,
    })

    if response.status >= 400 {
      log("confirmation rejected with status {response.status}")
      return
    }
  }
}
```

```hek
// tests/erasure.hk
test "erasing the customer takes their columns and leaves the order" {
  given @order.placed {
    order_id: "22222222-2222-2222-2222-222222222222",
    customer_id: 7,
    shop_id: 1,
    email: "ada@example.com",
    shipping_address: "1 High St",
    order_total: 25.99,
    notes: "",
  }
  erased customer_id "7"
  project CustomerOrders
  expect Order["22222222-2222-2222-2222-222222222222"] {
    customer_id: 7,
    email: none,
    shipping_address: none,
  }
}
```

Run it:

```sh
hekla check examples/orders
hekla test examples/orders
HEKLA_MASTER_KEY=$(head -c 32 /dev/urandom | base64) hekla serve examples/orders
```
