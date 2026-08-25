# hekla roadmap

Phased delivery of the design in [ARCHITECTURE.md](./ARCHITECTURE.md). Each phase builds on the code
that already exists, and each is shippable on its own except where noted. A cross-cutting rule: every
phase keeps `hekla check` honest for whatever it introduces, so the static analysis never falls behind
the language.

## Phase 0: command and projector core (done)

The current single-crate code is the baseline:

- `src/starlark_builtins.rs`: field types (`str`, `int`, `uint`, `bool`, `uuid`, `timestamp`,
  `money`, `json`, `one_of`, `optional`), `schema()` for command input, `entity()` for implicitly
  collected read-model tables with `key`, fields, and `index(...)`, `event()` for typed event
  definitions with tagged fields, `events()` / `all_events()` with list OR-ing, `put` / `patch` /
  `delete`, `reject`, `load_script(filename, src, kind)` deriving the module name from the file stem,
  plus dispatch helpers (`alloc_input`, `alloc_event`, fold and handle plumbing, `parse_event_specs`,
  `parse_tags`).
- `src/dispatch.rs`: `run_command` (allocate input, boundary from `query`, fold, `handle`, append
  guarded by `AppendCondition`) and `run_projector` (read `source`, `handle` per event, apply ops).
- `src/read_model.rs`: `ReadModel` over bundled rusqlite, typed column binding both ways, DDL
  generated from `EntityDef`.
- `src/main.rs`: a throwaway demo wiring one command and one projector against a temp tephra store.

Everything below layers the decided design on top of this.

## Phase 1: project shape, shared events, validation (done)

Toolchain only. This phase lands the loader, validation, and CLI checks, but nothing serves until
Phase 2, so it is scoped honestly as "toolchain, no server" rather than a runnable milestone.

- Directory-convention loader (`events/`, `lib/`, `commands/`, `commands/internal/`, `projectors/`,
  `effects/`); kind from directory, name from file stem.
- `load()` resolver restricted to `events/` and `lib/`, with a load graph (dependency-ordered
  evaluation and cycle detection) and an evaluated-module cache.
- Event definitions in `events/`; the event-def constructor validates each payload against the field
  schema and derives tags; structured tag queries.
- Effects evaluate against effect-scoped globals (`http.*`, `invoke_command`, `now`, `log`, stubbed
  until Phase 4), so command and projector purity is structural: they never see a clock or the
  network.
- Deploy-time validation: `query` tag fields and projector/effect `source` types checked against the
  event registry, projector indexes against declared fields. Emit payloads are validated by the event
  constructor itself, at the point of emit.
- Operational DB (`hekla.db`) skeleton: idempotency table, effect journal table, effect invocation
  table, module metadata, under a versioned migration.
- `hekla check` (thorough, collects every finding in one pass) and `hekla fmt` (a conservative
  whitespace normaliser; AST-level reflow is deferred, since starlark-rust 0.14 exposes no
  pretty-printer).

## Phase 2: command runtime and HTTP API (done)

The first runnable server: execute commands over HTTP. `hekla serve` opens the tephra store and the
operational DB, loads the project (refusing to serve if `hekla check` would fail), and runs the
decision cycle behind Axum.

- Command context: correlation id (from the `x-correlation-id` header or generated), a fresh causation
  id, optional triggering event; every response echoes correlation and causation. Pinned `now()` is
  in scope only during `handle` (carried on the evaluator via `eval.extra`); `query` and `fold` run
  without it, so calling `now()` there is an error.
- Client-supplied ids need no new code (the schema's `uuid()` fields already carry them). Each emitted
  event is wrapped at the append seam in a host-stamped envelope (event id, timestamp, correlation,
  causation, optional triggering event); tags stay outside the envelope as tephra tags, and every
  store read unwraps the payload.
- Built-in per-command idempotency lives in the event log, not the operational DB: a keyed command
  hashes its key into a reserved `_hekla_idem` tag on every event it emits and guards the append against
  that tag. A replay (or a crash between append and responding) finds the prior commit by that tag and
  rebuilds the original response from those events, so exactly-once is enforced by the log itself with
  nothing to reconcile at startup. A rejected or empty-emit command anchors nothing on the log, so a
  replay re-runs the pure `handle` and reproduces the same terminal outcome.
- Outcome to status: committed to 200 (with positions and emitted events), `reject` to 422,
  `invalid_input` (and host-side input validation) to 400, a DCB conflict that survives bounded retry
  to 409, unknown or internal command to 404. The runtime re-runs the whole cycle on a conflict so a
  fresh read rebuilds the decision model.
- Axum HTTP API: `POST /commands/{name}` (public commands only), `GET /status` (log head and the loaded
  module inventory, no fabricated projector or effect lag yet), `GET /health`, a generated
  `GET /openapi.json`, and a Scalar reference UI over it at `GET /docs`. Graceful shutdown drains
  in-flight work, then joins the writer.
- Public vs internal commands: `commands/internal/` are invokable by effects (a later phase) but return
  404 over HTTP and are absent from the generated OpenAPI.
- `hekla test`: `tests/*.star` scenarios seed a throwaway store through the same append path and run the
  real command, asserting emitted events (type, data, tags) or the rejection.

## Phase 3: projectors and generated read API (done)

Read models and the query surface over them. `hekla serve` now runs one thread per projector and serves
a generated read API over the materialised state.

- One sequential thread per projector, subscribing to its `source` from a persisted checkpoint. Each
  batch's ops and the checkpoint it advances to commit in one SQLite transaction, so state and position
  can never disagree and a crash resumes without skipping events. The checkpoint is a watermark plus a
  completed-set; the set is always empty under the sequential model, reserved so parallel lanes need no
  migration.
- `get(entity, key)` reads the current row through the batch's own uncommitted writes (every read and
  write runs on the projector's one connection), so read-modify-write stays in Starlark; `put` /
  `patch` / `delete` unchanged. Projectors stay pure otherwise: no clock, no randomness, no network.
- One SQLite database per projector at `data/projectors/{name}.db`, holding the read-model tables and
  the checkpoint together. The read API opens it read-only per request (WAL), reading the position in
  the same snapshot as the rows.
- Generated read API: `GET /read/{projector}/{entity}/{key}`, and `GET /read/{projector}/{entity}` with
  an indexed filter and cursor pagination. A filter on anything but the key or a declared index is a
  400, never a table scan. Every response carries the projector's log position.
- Projector replay is rebuild-and-swap: `POST /projectors/{name}/replay` builds a fresh database from
  position 0 and renames it in, so a crash mid-rebuild leaves the live model untouched. It returns 202;
  progress shows as lag in `GET /status`, which now reports each projector's position and lag.

Honest scope for this phase:

- The admin-only read-only SQL endpoint is deferred to a later phase.
- Read-API `money` output is the raw stored integer minor units: `money` carries no scale, and no
  entity uses it yet, so the decimal-string wire form is deferred with the scale decision.
- A scan supports a single indexed filter field; multi-field (composite-prefix) filters are deferred.
- The checkpoint's completed-set is always empty under the sequential model; the format is built for
  parallel lanes, but no lane runs yet.
- Reads return the projector position; blocking on it (read-your-writes) is delivered in Phase 5.

## Phase 4: effects (durable execution) (done)

The durable-execution model. `hekla serve` now runs one thread per effect: it subscribes to the effect's
`source`, and for each event runs the straight-line `handle` whose impure builtins are journaled, so a
crash mid-handler resumes by replaying journaled calls and running only the unjournaled tail live.

- One sequential thread per effect, strict position order, one invocation per event. The durable resume
  point is a per-effect watermark (a new `effect_cursor` table) advanced only once a batch's events are
  all terminal; the `effect_invocation` rows are the completed-set (the watermark-plus-completed-set
  format the design calls for).
- Journal in the operational DB, keyed by the content hash of the call plus a per-run disambiguator.
  Each call's journal row and the terminal record commit call-by-call in autocommit (never one
  per-invocation transaction), so journaled side effects survive a crash and replay skips them; a
  failed invocation replays completed calls and fails at the same point without re-firing. The script
  hash is recorded on each invocation, and a restart warns when in-flight code changed under it.
- Builtins: journaled `http.*`, `invoke_command` (public or internal, deterministic idempotency key plus
  the target command's DCB boundary), `now()`, and `log()` (not journaled). A journaled
  `read(projector, entity, key)` plus `scan` shipped here and was removed in Phase 13.
- Retry split: the runtime absorbs transport errors and 5xx (they never reach the script) by wedging the
  invocation and retrying with capped backoff; a 2xx/3xx/4xx result reaches the script, so `status >= 400`
  is a real decide-what-to-do outcome. A handler error wedges the same way (retry forever, never skip).
- Graceful-shutdown draining (effects first, then projectors, then the writer), with a bounded join so a
  wedged effect cannot hang shutdown. `/status` reports each effect's position, lag, consecutive-failure
  count, and last error, so a wedge reads as broken rather than merely slow.
- `POST /effects/{name}/skip/{position}`: an explicit, manual operator action to advance a wedged effect
  past a genuinely unprocessable event. Never automatic.
- Retention sweeper task (lazy GC) for effect journals and command idempotency keys, with configurable
  windows in `hekla.toml`, sweeping in bounded chunks.

Honest scope for this phase:

- `effects.pool_size` is validated but not enforced: v1 runs one thread per effect, which already bounds
  concurrency. A real shared blocking pool is reserved for partition-key parallel lanes (a later phase),
  which the watermark-plus-completed-set format already supports.
- `invoke_command` lands the domain fact exactly-once: it passes a deterministic idempotency key, so the
  target command tags every emitted event with that key and guards the append against the tag. A replay
  (including across the append-then-journal crash window) finds the prior commit by the tag and returns
  its recovered outcome, so dedupe lives in the event log, exactly as for HTTP commands. Raw `http.*` is
  at-least-once (a crash between a successful request and its journal write re-fires on replay).
- `read()`/`scan()` are journaled, so a replayed effect sees point-in-time-stale data by design; at cold
  start an effect can also outrun a projector and journal an empty read that then replays empty forever.
  *(Both closed by Phase 13, which removed the builtins in favour of a folded boundary.)*
- One explicit skip endpoint; no automatic dead-lettering, and no per-event retry endpoint beyond
  fix-the-code-and-restart (which replays the running invocation). The script hash is recorded and
  mismatch-warned but not pinned.

## Phase 5: read-your-writes (done)

A per-read consistency knob over the machinery Phase 3 already built. Command responses return the
appended positions and every read returns its projector's position; this lets a read *wait* for a
projector to catch up before serving, so a client can observe its own write.

- `GET /read/...` accepts an optional `?after=<pos>` (typically the `positions.last` a command
  returned). The async handler polls the target projector's in-memory position, published only after
  the batch and checkpoint commit, then runs the normal single-snapshot read, so a satisfied wait is
  guaranteed to see the write.
- The wait is bounded by an optional `?timeout_ms=` (default 5s, capped at 30s). On timeout the read
  fails closed with `503` and a `Retry-After` header, rather than silently serving stale data, so a
  client that asked for a position and did not get it knows so and can retry.
- Backward compatible: with no `after`, reads behave exactly as before. The `after` and `timeout_ms`
  params are reserved, so neither is mistaken for an indexed filter on the scan endpoint.

Honest scope for this phase:

- The wait targets the single projector named in the read path; there is no cross-projector "wait for
  all projectors" barrier.
- The wait is a fixed server-side poll bounded by `timeout_ms`; there is no long-poll or streaming, and
  no client-tunable poll interval.
- The read endpoints are still absent from the generated OpenAPI (which documents only commands), so
  `after`/`timeout_ms` are undocumented there for now.

## Phase 6: subscription-keyed dispatch and the fold contract (done)

Language ergonomics over the machinery every earlier phase built. Once a boundary spans more than one
event type, `fold(state, event)` becomes a chain of `if event.type == ...` branches, and the same
chain appears in every projector and effect. This phase gives all three one structural dispatch,
folds a projector's and effect's `source` into it, and settles the contract `fold` had left open.

- **Clause-keyed dispatch** for a projector's or effect's `handle`: a dict mapping query clauses to
  functions, alongside the single-function form which stays valid (and is the only option over an
  `all_events()` subscription). **The keys are the subscription**, so `source` is derived from them
  and declaring it beside a map is an error: the two-lists-to-keep-in-step shape this codebase avoids
  for entities and for tags is now avoided here too.
- **Every arm whose clause matches runs, in declaration order.** Several clauses may name one event
  type, so an arm can select a subset (`order_placed(shop_id = 1)`) without the general arm losing
  it. No arm can be shadowed by an earlier one, so order fixes only the sequence of ops or journaled
  calls, which determinism needs, and never which arms run.
- **The match is tephra's own predicate.** Each arm is lowered to the `QueryItem` the subscription
  already builds and matched with `tephra::Matches`, the single definition of "does this event
  match" that tephra's index is itself differential-tested against. hekla writes no matcher, so an
  arm's filter and the subscription's filter cannot drift apart, and a subject-scoped constraint
  works because the same lowering encrypted both the tag and the filter.
- **A command's `fold` keeps bare-definition keys.** Its boundary is `query(input)`, computed per
  request, so a constraint on a key would be a filter the boundary never applied; a clause key there
  is a load error saying so. The dispatch rule is the same for all three, `fold` simply cannot
  express overlapping keys. *(Reversed in Phase 8: the reasoning conflated the evaluation mode with
  the semantics. Reading a subset of what the boundary locked is safe.)*
- **An event no arm selects is skipped before its envelope is decoded**, so a map over a wide
  boundary pays nothing per irrelevant event. For a projector or effect it is not even read, since
  the keys are the subscription.
- **`fold` returns the new state and never mutates the one it was handed.** `initial` is now a
  literal value and never a function (a zero-arg, clock-free function can only produce a constant),
  so it stays the frozen module global it already was: the per-request JSON round-trip that existed
  solely to hand `fold` something mutable is gone, and a fold that assigns into `state` fails on the
  first event it sees, with a message that names the contract rather than starlark's bare `Immutable`.
- **`hekla check` reports the one thing neither the loader nor the subscription check catches**: a key
  built by calling `event(...)` inline. The loader's module-scope scan only sees definitions bound to
  a name, so an inline one inside a dict literal would reach dispatch unregistered and quietly work.
  A command's `fold` is additionally checked for entries its boundary never returns, which is dead
  code.

Honest scope for this phase:

- **The no-mutation rule is enforced at the first folded event, not at every one.** starlark-rust
  0.14 makes `Freezer::new` crate-private, so an arbitrary value cannot be frozen mid-evaluation:
  once an arm returns a dict it built, mutating that one is undetectable. And `AstModule`'s
  statements are crate-private too, so there is no static lint of handler bodies to fall back on.
- **A dead `fold` entry is a warning, not an error.** A command's `query` is evaluated with a
  placeholder input, so a branch the placeholder did not take could legitimately name a type the map
  covers. A projector's and effect's keys need no such check at all, being the subscription itself.
- **A boundary type with no fold entry is not reported.** It was, briefly, on the reasoning that
  ignoring a boundary event narrows what a command observes. That reasoning was wrong: the boundary
  and the fold answer different questions, so the check fired on correct code (the shipped
  `rename-user` example, which had to carry a no-op arm to satisfy it), and it penalised the map form
  for being explicit where a `def fold` that ignores a type says nothing at all.
- **Two `handle` forms remain.** Folding `source` into the map removed one list, not one form. The
  single-function form stays because any multi-statement handler needs a `def` anyway, and forcing
  the map form on a single-type projector reintroduces a named-function indirection that reads as a
  naming convention. Four of the five example subscriptions are single-type, so that is the common
  case rather than an edge. *(Reversed in Phase 8, on evidence: rewriting those four subscriptions
  showed the map form is the same line count, because the `source` line becomes the `handle` line.)*
- **State is still read by subscript** (`state["taken"]`). Unifying field access on dot syntax is its
  own item, delivered for `event.data` in Phase 7; state deliberately stays a dict, for the reason
  recorded there.

## Phase 7: dot access on event payloads (done)

Commands read `input.email` but every handler read its payload as `event.data["email"]`. This phase
makes `event.data` a struct, so both read the same way, and settles where the dot stops.

- **`event.data` is a struct, read as `event.data.email`.** All 90 payload reads in the examples and
  tests moved with it; nothing used `event.data` as a whole value, so nothing else had to change.
- **It is built from the event definition's fields, not from the stored payload.** That is the part
  worth more than the syntax: a field the payload omits now reads as `None` instead of raising,
  exactly as an absent optional does on `input`. An unregistered event type has no field list, so it
  still falls back to whatever the payload carries.
- **The dot marks a declared shape.** `input` and `event.data` are host-built from a field schema, so
  a misspelled field is a shape error rather than a silent miss. Handler-built values keep subscript:
  a command's folded `state` and a `put()` row are the author's own dicts, with nothing to check
  against.

Honest scope for this phase:

- **Folded state stays a dict, and this is a decision rather than a deferral.** starlark-rust's struct
  implements attribute reads and nothing else: no `+`, no `|`, no merge. So a struct-shaped state
  could not express "same state, but with `taken = True`" except by restating every field, and
  `dict(state, taken = True)` (the idiom two error messages recommend) would stop working. Making it
  pleasant needs a hekla-owned record type with an update operation, which is new vocabulary and wants
  arguing on its own merits, not smuggling in under "unify dot syntax".
- **Projector rows stay dicts too.** `get()` returns a row that `put()` takes straight back, and
  `put()` takes a dict, so read-modify-write round-trips without a conversion. The subject-handle
  wrapping is shared between the two paths and now hangs off one helper, so an event payload and a
  row still wrap identically.
- **Optional event fields are unexercised in the examples.** No `.star` file declares one, so the
  absent-reads-as-`None` behaviour is pinned by an integration test rather than by a worked example.

## Phase 8: one way to handle events, and tests for all three kinds (done)

Phases 6 and 7 each left a second way to do something. This phase removes them, on the principle that
one spelling for one meaning is worth more than the convenience of either alternative.

- **One dispatch form.** A projector's or effect's `source` plus `def handle(event)` is gone, as is a
  command's `def fold(state, event)`. Every event-driven handler is a clause-keyed map. Phase 6 kept
  the function form on the reasoning that a multi-statement handler needs a `def` anyway, so the map
  form would reintroduce a named-function indirection. Rewriting all four example subscriptions
  showed that was wrong: the map form is the **same line count**, because the `source` line becomes
  the `handle` line and the generic name `handle` becomes a real one. What the function form did cost
  was real: a second list that could drift from the body, and the `if event.type == ...` chain that
  Phase 6 existed to kill, still reachable for any multi-type subscription.
- **`all_events()` is what replaced it**, in all three kinds. `{all_events(): f}` means exactly what
  `def fold(state, event)` and `source = [all_events()]` plus `def handle(event)` meant, so the
  collapse loses nothing. Under fan-out it also composes: an `all_events()` arm beside typed arms is
  a prologue rather than a replacement for them.
- **One key language: a key is a query clause.** `fold` took bare definitions, `handle` took clauses
  and quietly accepted bare ones too, and nothing taught the rule. The split was not a design
  decision; it fell out of only projectors and effects evaluating their module body in query mode.
  Now every kind does, so `fold = {order_placed(): ...}` works and a bare key is a load error naming
  the fix. The spelling now matches `query`, which has only ever accepted clauses.
- **The old failure was invisible.** A clause key in a `fold` used to report ``missing required field
  `user_id` `` (it was building an *event*), or starlark's ``Value of type `event` is not hashable``
  if every field was supplied. Neither named `fold`, dispatch, or keys, and the carefully-worded
  error written for the case was nearly dead code: it could only fire for `all_events()`.
- **Constrained `fold` keys follow, and are safe.** An arm reads a subset of what the boundary
  already locked, and reading less than you locked never breaks DCB. `validate_specs` now runs over
  dispatch keys in every position, so a `fold` arm filtering an unindexed or subject-encrypted field
  is caught at check time. The guardrails differ by position because the lowering does: a `fold` key
  is lowered with the command's keystore like `query`, a `handle` key with none, so only the first
  can filter a subject-scoped field.
- **The dot now covers every fixed-shape host value.** Phase 7 gave `event.data` dot access on the
  rule that a host-built value with a declared shape earns it, but left three wrappers behind as
  dicts: `http.*`'s `{status, body, headers}`, `invoke_command`'s `{status, body}`, and `scan`'s
  `{items, next_cursor}`. All three are structs now. Their *contents* stay subscripted, which is the
  same rule rather than an exception: a response `body` is parsed JSON or a string depending on what
  arrived, `headers` is keyed by arbitrary names, and `items` is a list of rows. A read-model row
  stays a dict for the reason Phase 7 recorded, that `put()` takes one.
- **`hekla check` says each thing once.** With the keys doubling as the subscription, an
  unregistered type used to be reported twice, once by the clause validation and once by the
  dispatch check. The dispatch check now keeps only what is genuinely its own: a key built by
  calling `event(...)` inline, which the loader's module-scope scan cannot see.
- **`hekla test` covers all three kinds.** `case()` was command-only, which left the two kinds this
  phase and Phase 6 changed most with no in-language way to check their routing. It now takes
  `projector = ...` (project `given`, assert the rows the read API reads back, subject columns
  decrypted) and `effect = ...` (run `handle` over `given`, stub the replies with `responds`, assert
  the ordered `http_call(...)` / `command_call(...)` sequence). A projector case needed no new
  machinery: `project_to_head` was already the non-runtime entry point. An effect case needed one
  extraction, splitting `try_invocation` into the durable wrapper and a `run_handle` that takes the
  host as a trait object, which is a better shape regardless.
- **`expect` is read against the case's kind, not its own type.** An empty list means "no events"
  for a command and "no calls" for an effect, and nothing about `[]` says which. Reading it against
  the target also lets every mismatch name the form that kind actually takes.

Honest scope for this phase:

- **A constraint-level dead arm is not detected.** A `fold` key whose filter cannot overlap the
  boundary (`query` returns `order_placed(shop_id = 1)`, the arm keys `order_placed(shop_id = 2)`) is
  silent dead code. The cross-check is by type only, because `query` is evaluated against a
  placeholder input, so constraint *values* are not statically known.
- **`fold` keys can only filter on constants**, being module-level. That makes them worth reaching
  for on enum-shaped fields and little else; the docs say so rather than advertising the capability.
- **Naming a handler is now mandatory** for any multi-statement body. That is the one real cost, and
  it buys a name better than `handle`: the map puts the subscription and the handler on one line.
- **A test case runs the handler, not the runtime.** Batching, checkpoints, retry, the journal and
  replay are not exercised by a case, deliberately: they belong to the runtime and are covered by the
  integration tests, which keeps a case a statement about the author's own logic.
- **An effect case stubs `read` and `scan` rather than asserting them.** They return nothing, the
  same answer a live effect gets for a row its projector has not built, and `invoke_command` is
  recorded rather than executed: a command's behaviour belongs in a command case. *(The first half
  was reversed in Phase 11: `rows = {...}` now seeds them. They are still not asserted.)*
- **`response.body` is still a union**, dict or string depending on whether the bytes parse as JSON.
  A struct field makes the shape look more declared than it is.

## Phase 9: derived identity (done)

A handler had no way to produce an id. Commands take new-entity ids from their input, which is right
for a command (a retry carries the same id, so DCB dedupes it), but an effect is the caller in
`invoke_command` and had nothing to pass. Randomness is not the missing piece and never was: a replay
re-runs the code that would mint it, so a random id would make each attempt a different domain fact
and break the exactly-once guarantee `invoke_command` exists to give.

Two small additions close it, and they compose:

- **`event.id`**, beside `event.type` and `event.data`. It is the envelope's `event_id`, not the
  tephra position: stamped once at append and stable across a projector rebuild and an effect replay.
  All three dispatch sites already decoded the envelope and discarded it, so this is a field on
  `alloc_event`, not a new plumbing path. It sits beside `data`, so an event declaring its own `id`
  field keeps it at `event.data.id`.
- **`uuid5(namespace, name)`**, RFC 4122 version 5. Deterministic by construction, which is the
  whole requirement. `name` is what lets one handler derive several distinct ids from one event.

`build_event` now takes the event id from its caller rather than minting one, which is what lets
`hekla test` pin it: the nth `given` event gets `00000000-0000-0000-0000-00000000000n`, alongside
the clock and master key it already pinned. Without that, a case asserting a derived id would be
flaky rather than failing, and the feature would be untestable in the language it ships for.

**Honest scope:**

- **Deriving is documented as the third choice, not the first.** Prefer an identity that already
  exists (the entity the fact is about), then one an external system returned in a journaled
  response, then a derivation. The docs say so in both places the question comes up.
- **Only `id` is exposed, not the rest of the envelope.** `correlation_id`, `causation_id` and the
  append `timestamp` stay host-side; each would need its own argument for why a handler should branch
  on it, and none has one yet. Adding one later is a one-line change to `alloc_event`. *(Phase 12
  added `timestamp` on exactly that basis: a port needed `created_at` read-model columns and the
  alternative was six commands restating the clock. The other two stay host-side.)*
- **The namespace must be a canonical UUID.** Passing `event.type` or a bare string is the likely
  mistake, so it errors naming what it got rather than deriving from garbage. That does mean a
  project wanting a fixed namespace constant has to write a UUID literal.
- **A derived id is not secret.** Version 5 is a hash, not a MAC, so anyone holding the namespace and
  the name can recompute it. Fine for entity ids, wrong for anything used as a capability token.
- **`ARCHITECTURE.md` §2 was wrong about this** and is corrected here: it said "the `id` and
  `position` are tephra's", but tephra's `Event` carries only type, tags and payload. The id was
  always the envelope's.

## Phase 10: erasure from an effect (done)

Erasure was operator-only (`hekla erase`), so an app that receives erasure requests as events (a
provider redact webhook, a retention deadline, an `account.closed`) had no way to act on one. Its
handler could see the request and do everything except the one thing that matters.

`erase(subject_field, subject_value)` is an effect builtin, journaled like every other side effect
and idempotent besides, returning whether a key was there to delete. `EffectHost` gains one method,
`TestEffectHost` performs it against the case's own key store, and `erase_call(field, value)` joins
`http_call` and `command_call` as an assertable expectation.

**Why a builtin rather than a declarative subscription.** The obvious alternative was a marker on an
event definition ("this event erases this subject"), which is safer: no handler code could erase by
accident. It was rejected on evidence from a real production handler, which needs two things it
cannot express: the subject id read out of an untyped webhook payload rather than a declared field,
and a fan-out that erases every customer of a shop, a set computed by scanning a read model. A
declarative marker covers only the case where the subject is a single declared field of the
triggering event.

**Honest scope:**

- **`reveal` is still not journaled, and that constrains ordering.** An invocation that reveals a
  subject and then erases it cannot be replayed: the replay re-runs `reveal` against a deleted key
  and takes the existing terminal-skip path, so the position completes and journaled calls do not
  re-fire, but work after the reveal does not run. Documented as "erase last" rather than fixed.
  Fixing it properly means either journaling `reveal` (which would put plaintext in the journal
  systematically) or deferring erases to the invocation's terminal record (which risks losing an
  erasure if the process dies between the handler finishing and the flush). Neither trade is worth
  making before a real handler needs it.
- **Erasing the global uniqueness subject wedges rather than failing fast.** `crypto::erase_subject`
  refuses it, and a refusal from a handler is an ordinary wedge, retried forever until the code is
  fixed. That matches how every other handler bug behaves, but the subject field is a runtime string,
  so `hekla check` cannot catch it.
- **The effect journal is not shredded by an erase.** Revealed plaintext that flowed into a journaled
  request body outlives the key until the retention sweeper reclaims the invocation. Already an
  accepted limitation of the model; automating erasure makes it easier to hit.
- **A `scan`-driven fan-out is not testable in `hekla test`.** *(Closed by Phase 11: `rows = {...}`
  seeds an effect's projector reads, so the per-row erase loop is now covered by a case.)*

## Phase 11: an effect case can seed the projectors it reads (done, retired by Phase 13)

`TestEffectHost` served `read` and `scan` as empty, so every effect that reads a projector was
untestable in the language it ships for: the interesting branch never ran, and the case could only
assert the do-nothing path. That is most non-trivial effects, and it was found by planning a real
port whose riskiest handlers are exactly the read-dependent ones.

`rows = {projector: {entity: [row, ...]}}` on an effect case seeds them. The implementation is not a
stub: `seed_models` opens one real `ReadModel` per projector in the project and applies the declared
rows through `apply_one`, and the host serves `read`/`scan` through `read_api::get_one` and
`read_api::scan`. So key lookup, index validation, ordering, cursor paging and subject decryption all
behave as they do live, by construction rather than by imitation.

Subject columns are written as plaintext in the case and stored encrypted under the case's key store
(`encrypt_row`), so a `read` decrypts them back. A scenario never contains ciphertext, and the
plaintext an effect sees is the plaintext the real read path would hand it.

**Honest scope:**

- **`read` and `scan` are still not assertable.** A case declares what they find; it cannot assert
  that the handler asked. The contract of an effect is the calls it makes outward, and a read is not
  one of those.
- **Every projector in the project gets a temp read model per effect case**, not only the declared
  ones. That is what lets an undeclared projector read as empty (the live answer when a projector has
  not caught up) while a projector name the case got wrong still fails by name. It costs one SQLite
  open per projector per effect case.
- **The rows are a snapshot, with no relationship to `given`.** A case can declare rows that the
  seeded events would never have produced. That is deliberate (it is how you set up a precondition),
  but it means a case can describe an impossible world.
- **Filterability is checked in the host rather than shared with the runtime.** `scan_projector` and
  the test host now both call `read_api::is_filterable` in the same order, but they are two call
  sites that must stay in step. A test asserts the unindexed-filter rejection so a drift shows up.

**Retired by Phase 13.** `rows` was the right answer to "how does a case stub an effect's projector
reads", and it stopped being needed because the question did: an effect's state now comes from
folding the seeded log, so `given` is both the trigger and the state. The honest-scope note above
about a snapshot with no relationship to `given` is what the fold removes.

## Phase 12: event.timestamp (done)

A read model that wants a `created_at` column had nowhere to get one. The envelope has held the
append `timestamp` since Phase 1, but only `event.id` was exposed, so the only route was a command
stamping `now()` into its payload, which section 5 explicitly warns against: it duplicates what the
envelope already holds. The architecture said don't, and offered no alternative.

`event.timestamp` sits beside `event.id`, `event.type` and `event.data`, threaded from the envelope
each dispatch site already decodes. Same stability argument as `event.id`: stamped once at append, so
a projector rebuild and an effect replay both reproduce it. `hekla test` pins it to the same fixed
clock `now()` uses, so a column built from it is assertable.

The rule this settles, now stated in both §4 and the authoring guide: **`event.timestamp` for when
the event was appended, `now()` for time that is genuinely domain data** (`expires_at`, `due_date`, a
`purchased_at` an upstream system reported).

**Honest scope:**

- **`correlation_id`, `causation_id` and `triggering_event_id` stay host-side.** Phase 9's reasoning
  is unchanged for them: no handler has yet needed to branch on one. The seam is now proven, so
  adding one is a parameter and a struct field.
- **A handler can build a non-deterministic value from a deterministic one.** `event.timestamp` is
  stable, but nothing stops a projector deriving a column that a later code change would compute
  differently, which a rebuild would then silently rewrite. That is true of every handler body and is
  not specific to this field.
- **No formatting or arithmetic on it.** It is an RFC 3339 string; a projector that wants a date
  bucket does its own string slicing, and a fold that wants a duration parses both ends by hand.
  Starlark has no date library and hekla adds none.

## Phase 13: effects fold the log instead of reading projectors (done)

An effect got state from `read(projector, entity, key)` and `scan(...)`, reaching another projector's
read model by string name. That was the only cross-module coupling in hekla, and it was unsound in a
way Phase 4's honest scope already recorded as the "cold-start empty read": an effect could outrun a
projector, and because reads were journaled, the miss recorded `null` and **every retry replayed that
null**. The row could never be observed, even once the projector caught up. The retry loop that
looked like waiting was not waiting; it burned attempts at the 60s cap until an operator skipped the
position by hand.

Journaling exists to make *side effects* exactly-once. A read has no side effect. Phase 4 journaled
it "for consistency with the rest of the model", and that consistency bought an unrecoverable failure
mode.

An effect now declares `query` / `initial` / `fold`, the same three globals as a command with the
same meanings, and each `handle` arm takes `(event, state)`. `query` takes the triggering event where
a command's takes `input`, because an effect's boundary is scoped by what it is reacting to. **The
fold is bounded at the effect's own position, inclusive**, so `state` is a pure function of the log
prefix and that position.

That bound is the whole point. Reading a projector is nondeterministic: the answer depends on where
another thread happens to be. Folding the log at your own position is deterministic. Same
information, and every problem above dissolves at once: no race, no frozen miss, and no journal entry
needed, because re-folding reproduces the answer exactly. The dispatch and effect paths share one
`fold_boundary`, so the two cannot drift on the parts that are not obvious (match before decode,
lower once outside the loop).

`hekla test` got simpler rather than harder: `rows = {...}` is gone, because `given` is already the
state. The boundary folds the same seeded log the case built.

Two smaller consequences fell out. `check_state_shape` is now shared by commands and effects, so
`initial` and `fold` fail identically in both. And an effect's `query` is validated against a
placeholder event per subscribed type, mirroring the command path's placeholder input.

**Honest scope:**

- **The fold runs per invocation, from position zero.** The same cost model commands already pay per
  request, but effects are sequential and single-lane, so it comes off throughput rather than off one
  request's latency. No snapshotting and no state carried between invocations. Incremental folding is
  the optimisation to reach for if this hurts; it is not here because it cannot be scoped by the
  triggering event, so it would trade the throughput problem for a memory ceiling.
- **`run_handle` still re-parses and re-lowers `handle` on every event**, where `run_command` hoists
  it. Hoisting across events means restructuring the per-call `Module::with_temp_heap`. It is a
  smaller cost than the boundary read this phase adds, so it is noted rather than fixed.
- **`query` is not validated for an `all_events()` subscription**, because there is no event type to
  build a placeholder from. That effect's `query` is checked at runtime only.
- **A `query` that branches on the event's values is only validated along the placeholder's branch**,
  inherited verbatim from the command path's documented blind spot.
- **Large fan-outs must fit in memory.** A `scan` paged with a cursor; a fold does not. Scoping via
  `query(event)` keeps this to one entity's worth at a time, but an effect that folds an unbounded
  set has no backstop.
- **Removing `read` is a load error, not a guided migration.** `read(...)` in an effect now fails
  with starlark's own "Variable `read` not found". `hekla check` catches it, which is the important
  part, but the message does not name `query`/`fold` as the replacement. Registering a stub that
  errors with guidance would have deferred the failure from check time to runtime, which is worse.
- **Settled reference data now costs a fold.** State that is genuinely old (an access token, a plan's
  SKU) used to be an O(1) row read. If that proves too coarse, the successor is a log-query path that
  returns a value without emitting, not the reinstatement of `read`.

## Deferred, with triggers

Each item is placed with the condition that would pull it forward, so nothing is built before it is
warranted.

- **Upload API with versioning, pinning, and retention, plus hot reload** (load-graph incremental
  invalidation): when inline or live editing becomes a goal. The effect journal already records the
  script hash for this.
- **Partition-key parallel effect lanes**: when a single effect's throughput on slow APIs hurts. The
  checkpoint format (watermark plus completed-set) already supports it.
- **Encryption and crypto-shredding**: when PII-at-rest requirements land.
- **Metrics and Prometheus**: when there is something to operate at scale.
- **Fold library** (`event_counter`, `latest_event`, `toggle`): only after roughly fifteen real
  commands exist, and only if it compiles down to the existing `query` / `fold` shape rather than
  becoming a second execution path.
- **Workspace crate split**: when hekla must be embeddable as a library, or when compile times
  actually hurt.

### Carried-forward gaps from earlier phases

Deferrals recorded in the "honest scope" of a completed phase that no trigger above already pulls
forward. Collected here so they are not lost in the prose of the phase that introduced them.

- **Admin read-only SQL endpoint** (Phase 3): a read-only query surface over the projector databases,
  deferred with no successor phase named.
- **`money` scale and decimal wire form** (Phase 3): read-API `money` is the raw stored integer minor
  units; the decimal-string wire form waits on the scale decision, and no entity uses `money` yet.
- **Multi-field (composite-prefix) scan filters** (Phase 3): a scan supports a single indexed filter
  field only.
- **Automatic dead-lettering** (Phase 4): the manual `POST /effects/{name}/skip/{position}` is the only
  escape hatch; a wedged effect is never advanced automatically.
- **Integers above `i64::MAX`** (Phase 3): both `i64` and `u64` land in a signed SQLite `INTEGER`, so
  a `u64` past `i64::MAX` is rejected at the write boundary rather than stored. Widening it needs a
  storage form that still orders correctly, since the read API's `ORDER BY` and `key > ?` cursor
  depend on numeric order; a bit-reinterpretation would round-trip but sort those rows below zero.

Inherent design properties, listed for completeness (not future work): `invoke_command` is exactly-once
only when the target is idempotent under replay, and raw `http.*` is at-least-once.

## Design brainstorm: language and runtime ergonomics (unscheduled)

Raw considerations from design review, captured so they are not lost. None is scheduled, and none is a
committed shape: each names the tension it addresses and the open question. Several interact with
features that already shipped (auto-tagging, effect retry, projector auto-rebuild), so they are
refinements to revisit once real projects exercise them. Three have shipped: per-type folds and the
mutate-or-return decision as Phase 6 (which also folded a projector's and effect's `source` into the
dispatch map), dot access on event payloads as Phase 7, and the collapse to one dispatch form and one
key language as Phase 8. The rest stand as written.

- **Meaningful effect outcomes, idempotency keys, and delivery events.** An effect `handle` return value
  is currently ignored, so `http.post` then a log on failure is at-least-once with silent duplicates and
  no script-controlled backoff. A meaningful return (`ok()`, `retry(after = 30)`, `dead_letter(reason)`)
  would move the decision into the script. Pair it with a stable idempotency key derived from the event
  position (so the receiver can dedupe) and the ability to emit an event back (`email_sent`) so a
  projector can observe delivery. Connects to the deferred "automatic dead-lettering" gap and the
  terminal-skip reporting just added.
- **Event versioning and upcast hooks.** `type = "order.placed"` carries no `version` and no upcast
  hook, so the first schema change becomes a hand-written migration. A version tag plus an upcast
  function (old payload in, current shape out, applied on read) would keep the log append-only while
  letting the schema evolve.
- **Derive command input from the event schema.** `input = schema(...)` restates the event's field
  types and will drift from them. Let it derive, e.g. `input = schema(**order_placed.fields)` or a
  partial selection, so the two cannot disagree.
- **A record type for folded state.** The dot-syntax item shipped for `event.data` in Phase 7, which
  left `state["taken"]` as the only subscript read of a host-threaded value. Closing that gap is not a
  syntax change: starlark's struct supports attribute reads and nothing else, so state would need a
  hekla-owned record with an update operation (`state.with(taken = True)`, or a `replace()` builtin)
  before `state.taken` could work at all. Worth revisiting only if real commands accumulate enough
  state for `dict(state, ...)` to feel heavy; a two-flag decision state does not.
- **Type-shaped default tagging.** Auto-tagging currently indexes every field unless `indexed=False`. A
  better default could key off the field type: identity-shaped fields (`uuid()`, integers, short
  `str()`) are worth tagging, while `money()` almost never is. Refines the shipped auto-tagging default.
- **Projector rename detection.** Store the projector's source file path in its checkpoint record.
  Moving `customer-orders.star` then produces an explicit "rename or new projector?" error instead of
  silently rebuilding from position zero. Refines the shipped definition-change auto-rebuild.

### Suggested sequencing

An opinionated order for the above, by value-to-effort. Not a commitment, a starting point. Per-type
dispatch and the fold contract, which used to head this list, shipped as Phase 6.

1. **First, and cheap: reserve an event `version` slot in the envelope.** The log is empty today, so
   add the field now even if unused; the upcast hooks can wait, but retrofitting a version onto
   historical events is the exact migration this item exists to avoid.
2. **Effect outcomes.** The `ok()` / `retry(after)` / `dead_letter(reason)` return is the largest
   remaining operational win, and it closes the deferred automatic-dead-lettering gap. Two
   refinements to the item as written: derive the idempotency key from the event id rather than the
   raw log position (Phase 9 exposed it as `event.id` for the same stability reason, so the value is
   already threaded to where this needs it), and route "emit an event back" through the existing
   `invoke_command` path so effects stay out of the event-producer role.
3. **Ergonomics:** projector rename detection, and a record type for folded state if `dict(state, ...)`
   ever feels heavy. Each is small and independent.
4. **Only if a real project asks:** deriving `input` from the event schema, and type-shaped default
   tagging. Both are double-edged. Command input legitimately diverges from event fields (plaintext vs
   subject-encrypted, server-derived ids), so at most make derivation opt-in sugar for the 1:1 case.
   And prefer "do not auto-tag unbounded `str()` / `json()`" (or warn on it) over an allowlist of
   taggable types, keeping the explicit `indexed=` opt-out predictable.
