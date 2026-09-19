# hekla roadmap

Phased delivery of the design in [ARCHITECTURE.md](./ARCHITECTURE.md). Each phase builds on the code
that already exists, and each is shippable on its own except where noted. A cross-cutting rule: every
phase keeps `hekla check` honest for whatever it introduces, so the static analysis never falls behind
the language.

**Phases 0 to 20 were built against embedded Starlark, and Phase 21 replaced it with heklang.** They
are left as written, because a roadmap is a record of what was decided and why, not a description of
the current code. Several describe machinery the port deleted outright: the `load()` graph (Phase 1),
the per-type fold map and the clause key language (Phases 6 and 8), the incremental conflict carry
(Phase 15), the chunked fold and its heap budget (Phase 16). Where a phase says "`fold`", "`query`",
"`handle`" or "a clause", read it as the Starlark construct of the time. Phase 21 says what each
became.

## Phase 0: command and projector core (done)

The single-crate code the phases below build on:

- `src/schema.rs`: the language-agnostic schema model (field kinds, entity and event definitions,
  input schemas), built from the compiled program and depended on by the read model, the read API,
  OpenAPI and introspection alike.
- `src/dispatch.rs`: binding a request body to a command's parameters, running it, and retrying a
  conflict.
- `src/read_model.rs`, `src/read_api.rs`: SQLite read models and the generated reads over them.
- `src/crypto.rs`, `src/envelope.rs`, `src/opdb.rs`: subject keys, the host-stamped envelope, and
  the operational database.

## Phase 1: loader, validation and CLI (done)

Toolchain only. This phase lands the loader, validation and the CLI checks, but nothing serves until
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
  Phase 18 widened the absorbed set to every retryable status.
- Graceful-shutdown draining (effects first, then projectors, then the writer), with a bounded join so a
  wedged effect cannot hang shutdown. `/status` reports each effect's position, lag, consecutive-failure
  count, and last error, so a wedge reads as broken rather than merely slow.
- `POST /effects/{name}/skip/{position}`: an explicit, manual operator action to advance a wedged effect
  past a genuinely unprocessable event. Never automatic.
- Retention sweeper task (lazy GC) for effect journals and command idempotency keys, with configurable
  windows in `hekla.toml`, sweeping in bounded chunks.

Honest scope for this phase:

- `effects.pool_size` is validated but not enforced here: this phase ran one thread per effect, which
  already bounded concurrency. Phase 26 makes it live, as the bound on partition-key lanes.
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
`hekla test` pin it, alongside the clock and master key it already pinned. Without that, a case
asserting a derived id would be flaky rather than failing, and the feature would be untestable in
the language it ships for. *(The id it pinned was hekla's own, `…-00000000000n`, and Phase 32
replaced it with heklang's: a pinned id that disagrees with `hek test` is reproducible and still
unassertable, because no one case can satisfy both runners.)*

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
a projector rebuild and an effect replay both reproduce it. `hekla test` pins it, so a column built
from it is assertable. *(It pinned it to a frozen instant, which turned out to be neither what
`now()` reads nor what `hek test` writes; Phase 32 is the correction.)*

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

## Phase 14: the invariant harness and verify mode (done)

The suite covered cases someone thought of. The properties the design rests on were asserted only by
example, which is the wrong shape for a system whose worst bugs are the ones you cannot undo: a
wrong event is permanent, a double-fired effect is money already spent, while a wrong read model is
only a rebuild. So the verification budget belongs on the append and effect paths, and it belongs in
checks that run against whatever state a deployment actually reached.

`src/verify.rs` holds the checks; two entry points wrap them. `hekla verify` sweeps a data directory
offline (CI, or a nightly job over a copy of the backup); `serve --verify` runs the per-operation
half continuously, and `[verify] enabled` in `hekla.toml` makes that permanent. `hekla test` always
checks folds, since a scenario is cheap and is where a nondeterministic fold should surface first.

- **Rebuild equivalence** builds a shadow model with `project_to` and compares `ReadModel::rows` per
  entity, as a merge join over the two key-ordered row sets. The comparison is exact rather than
  approximate because subject encryption is deterministic AES-SIV and a projector stores the event's
  ciphertext verbatim, so a rebuild copies bytes. The shadow is bounded at the live model's own
  checkpoint: an unbounded rebuild would report every event a lagging projector had not reached as
  corruption.
- **Replay equivalence** re-runs a completed invocation against a **sealed** host: a journal hit
  replays, a journal miss records a violation and returns, and nothing is ever performed. That
  property is the design, not an optimisation. The divergence being hunted is precisely the case
  where a naive replay would fire a real side effect, so the check must be structurally incapable of
  causing it. The visited call sequence is compared against the journal's as an ordered list, which
  is the shape nothing else could catch: the journal is keyed by call *content*, so a handler that
  merely reordered its calls hits every entry and a set comparison would call it faithful.
  The invocation is completed *before* the check runs, so a violation cannot leave a `running` row
  for the next boot to re-enter live, and the quarantine is recorded durably so a restart honours it.
- **Fold determinism** folds twice and compares, with the second fold bounded at the position the
  first reached (an unbounded re-fold would read a concurrent append as nondeterminism). States are
  compared with Starlark equality rather than through JSON, since `check_fold_result` admits values
  that `to_json_value` cannot represent. It is an error rather than a typed violation because there
  is no safe way to continue: a command fails the request, an effect wedges.
- **Checkpoint monotonicity** routes every position reached by *tailing* through one helper that
  refuses to go backwards. A rebuild publishes through a separate, unguarded path: it replaces the
  model, so its checkpoint is authoritative even when it lands behind.

A violation quarantines the component: it stops advancing, `/status` names what broke, and the rest
of the runtime keeps serving. A quarantined projector's reads return 503 rather than its rows, since
what a failed check calls into question is exactly the rows and the position.

**A data-directory lock came with it**, and closes a hole that predates this phase: tephra locks
nothing, so two `hekla serve` processes on one data directory would have corrupted the log with
nothing to stop them. `Runtime::open` now takes an exclusive lock (an open `BEGIN EXCLUSIVE` on a
dedicated SQLite file, so it needs no dependency and dies with the process however it dies), which is
also what keeps `verify` off a directory a server is using.

**Honest scope:**

- **Rebuild equivalence is offline only.** It costs a full log replay, and against a live projector
  the shadow would race the model it is comparing to: the bound it needs (`project_to` now takes
  one) would be moving while the comparison runs.
- **A terminal `reveal`-after-`erase` is exempt, not checked.** The `erase last` rule the authoring
  guide recommends produces an invocation that deliberately cannot replay. Found by testing rather
  than by design: the first cut reported it, which would have quarantined every effect written the
  recommended way.
- **Fold determinism is positive-only end to end.** Starlark's purity means a genuinely
  nondeterministic fold cannot be written from the language, so the planted-violation test for it is
  a unit test of the comparator rather than a scenario.
- **`hekla verify` needs the log open for writes.** tephra exposes no read-only handle (`ReadHandle`
  is reachable only through a `WriteCoordinator`), which is why the sweep takes the lock and why
  verifying a copy is the documented shape rather than a suggestion.
- **The replay sweep only covers what the journal still holds.** Retention reclaims completed
  invocations, and an edited effect's recorded runs are skipped by script hash, so the sweep audits a
  rolling recent window rather than all history. Both are counted and reported separately from the
  checked ones, so a sweep cannot read as thorough when it was not.
- **A code review after the fact found seven correctness bugs in the first cut**, all of them in the
  checking machinery rather than in what it checks, and all now covered by tests that fail without
  their fix. Worth recording because they share a shape: a checker is code too, and its failure mode
  is confident wrongness. The two that mattered most inverted the feature's purpose. Treating a
  rebuilt projector's checkpoint as a regression stopped the projector while `readiness` still read
  `ready`, so the read API kept serving a model rebuilt from nothing and read-your-writes resolved
  against it, which is exactly the lie the check exists to prevent. And quarantining *before*
  completing the invocation left its row `running`, so the next boot re-entered the handler in live
  mode and performed for real the call the sealed replay had refused: detection turning into the
  double-fire. The rest were narrower: an unbounded rebuild reporting a merely-lagging projector as
  corrupt, a missing master-key guard reporting a healthy directory as diverged, an unbounded second
  fold turning DCB contention into a 500, audit lines emitted for erasures that did not happen, and a
  subset comparison that could not see the call reordering three documents claimed it caught.
- **A live replay divergence cannot be provoked from Starlark**, which is a good property and an
  awkward one. The language is pure and every impure call is journaled, so a healthy handler's replay
  always agrees with its first run. The continuous check is therefore exercised by planting state
  (deleting a journal row, recording a quarantine) rather than by writing a misbehaving effect. The
  same purity is why fold determinism has no end-to-end negative test.
- **Boundary safety is still unchecked.** "At most one of a set of concurrent commands appends" is a
  linearizability property, so it needs the deterministic simulator rather than an in-process
  assertion. It is the headline invariant for that work, and this phase exists partly to give it an
  oracle.

## Phase 15: a conflict costs the delta, not the boundary (done)

A DCB benchmark against a deliberately hot boundary (one course, ~30k events deep, 32 concurrent
subscribers) put hekla about 4x behind a plain tephra client under contention, while it was ~20%
ahead uncontended. Both numbers came from the same fact and neither was about storage, since hekla
embeds the same tephra: uncontended, hekla folds in-process where the client ships the whole boundary
over a socket, and it wins; contended, hekla re-read and re-folded the entire boundary on every
retry where the client applied only the delta, and it lost that back several times over.

The fold is a left fold over an append-only log, so folding `[0, a]` and then `(a, b]` is the state
folding `[0, b]` would give. A retry never needed to start over.

`run_command` now owns the attempt loop, and each attempt keeps the state it folded and the last
position that state covers; the next one reads strictly after that position and folds what landed
onto the state it already has. The loop lives in `dispatch` rather than the runtime so the work that
is invariant across attempts happens once per request: the input struct, the boundary, and the
lowered `fold` plan, whose clauses cost a keystore lookup and a deterministic encryption *each* when
a field is subject-scoped. The retry *policy* stays with the caller as a `Retry { max_attempts,
backoff }`, so the timing decision is still the runtime's and `hekla test` can ask for a single
attempt.

**Each attempt folds in a scratch heap and freezes the result**, which is what makes the carry safe
rather than merely fast:

- **`handle` cannot mutate what the next attempt folds onto.** This is not a style rule. Measured on
  a capacity-three boundary with six racers and a `handle` that reset the count: before the freeze,
  all six committed and the log ran to seven events, every caller getting a 200. The DCB boundary
  exists to make exactly that impossible, and the incremental carry had quietly handed a handler the
  ability to defeat it. Frozen, the assignment fails with `Immutable` at the offending line and a
  message saying why, which is what already happened when the boundary was empty and `state` was the
  frozen `initial`. The freeze makes the rule uniform instead of conditional on how much history
  there was.
- **An attempt's allocations die with it.** A fold over a deep boundary allocates a Starlark event
  per matched position, and starlark collects only at module top-level statements, which a handler
  call is not. One heap across the whole retry sequence would have pinned every attempt's events
  through every backoff sleep; the freeze copies out only what the state reaches and drops the rest.

The same measurement exposed three per-event allocations in the fold loop: the arm's error label was
`format!`ed eagerly, the list of selecting arms allocated a `Vec` per event, and `event.id` went
through `Uuid::to_string`. Alongside those, every JSON field of every event was deep-cloned before
allocation, because `heap.alloc(value.clone())` satisfies a trait `heap.alloc(value)` satisfies just
as well. The labels are built once per fold, the arm buffer is reused, the id formats into a stack
buffer, and the clones are gone.

Measured on a 20k-deep boundary with padded events, same machine, three runs each:

| | before | after |
|---|---|---|
| one command, no contention (warm) | 37.8-38.8 ms | 33.4-34.3 ms |
| 4 concurrent | 146-190 ms, 4/4 committed | 52-59 ms, 4/4 committed |
| 16 concurrent | 315-325 ms, **7/16 committed** | 86-101 ms, **10-16/16 committed** |
| shallow boundary, no append (per request) | 2.2-2.5 us | 2.9 us |

The committed counts matter more than the times: under 16-way contention the old path burned its
retry budget re-folding and answered 409 to nine of sixteen callers. That count is timing-dependent
(one boundary, one winner per round, a five-attempt budget), which is why it is a range. The last row
is the price of the freeze on the cheapest possible command, about 0.6 us per attempt, against a
~2 ms fsync on any command that actually appends; on the deep fold it is about 2 ms in 32.

**Honest scope:**

- **The incremental fold cannot be observed from outside.** Folding the delta and re-folding from
  zero are the same state by construction, so no black-box test can tell them apart, and the
  contention test here passes against the pre-change implementation too. What it guards is that the
  carry stays *correct*: folding the delta onto `initial` instead of onto the carried state makes it
  commit five seats against a capacity of two. Named for the property it holds, not the optimisation
  it accompanies.
- **The mutation guard is a runtime error, not a `hekla check` rule.** It fires on the first request
  that reaches the assignment with a non-empty boundary, which a `hekla test` scenario will do.
  `AstModule::statement()` is public, so a static rule rejecting an assignment into `state` inside
  `handle` is buildable and would move this to deploy time; the loader currently consumes the AST
  during evaluation, so it would need to hold onto it.
- **The first fold is untouched.** A boundary 30k events deep still costs 30k Starlark calls on the
  first attempt, and that, not the retries, is what puts a floor under the contended latency. The fix
  for *that* is caching folded state across requests, keyed by boundary. The freeze this phase added
  is most of what such a cache needs (a shareable immutable state), but the invalidation story is not
  written. Worth doing when a real workload has a boundary that deep.
- **The backoff policy is unchanged**, deliberately: measuring an incremental fold and a new retry
  cadence at once would attribute neither. tephra distinguishes a durable conflict from a
  conservative same-batch rejection (`ConflictSite`), and hekla treats both identically; now that a
  retry is cheap, retrying a durable conflict immediately and backing off only for the advisory one
  is the obvious next experiment.
- **`event.data` is still materialised in full.** Every declared field of a folded event becomes a
  Starlark value whether the fold arm reads it or not, so a padded or wide event pays for fields
  nobody touches. Making it lazy means a hekla-owned value type with `get_attr`, which is a real
  change to what handlers see (`dir()`, equality, `to_json_value`) rather than an allocation tweak.
- **A code review caught the mutation hole before it shipped**, which is worth recording because the
  first cut had reasoned its way to leaving it open. The argument for not freezing was that a module
  per attempt would give up a pooled temp heap; `Module::with_temp_heap` calls `Heap::temp`, which
  builds a fresh `OwnedHeap` every call and pools nothing. A wrong premise, a plausible-sounding
  conclusion, and a silent-wrong-commit left in the tree behind it.

## Phase 16: a fold's live heap is bounded, not linear in the boundary (done)

A wider DCB benchmark (1M events, 1000 courses, 32 concurrent, hekla against tephra, umadb and Axon
Server) put hekla first uncontended (3815 ops/s against tephra's 3081, on the round trips it saves by
folding in-process) and last but one contended (145 against 537). The harness reports mean read
amplification, so per-event throughput can be computed rather than guessed: tephra folds 3.40M
events/s at skew 0 and 3.23M at skew 0.99, flat on the same corpus and the same queries, while hekla
goes 4.21M to 0.87M. Per-event cost rising 4.85x when mean depth rises 5.45x is a slope, and no fixed
per-event cost can produce a slope, so the constants were not the explanation.

What is proportional to depth is the live heap. From starlark's own source, in `possible_gc`: "For
the moment we only GC when executing a statement at the root of the module". The instruction that
calls it is emitted only at module top level, and a fold loop calls `eval_function` repeatedly
without ever reaching one, so **nothing a fold allocates is released until its heap is dropped**:
every event struct, every string, and every superseded state from `dict(state, ...)` survives to the
end of the boundary. A fold over 24k events with 4 KB payloads holds ~100 MB live, and 8 of those at
once hold most of a gigabyte. tephra's client decodes an event, applies it, and drops it, at O(1)
live for any depth.

So a fold is no longer one pass over one heap. It runs in chunks: fold until the scratch heap has
grown `HEKLA_FOLD_HEAP_BUDGET` bytes (1 MiB by default), freeze the state out, drop the heap with
everything the chunk allocated, thaw the state into the next one. The seam is sound for the reason
the retry carry already was, a left fold over an append-only log, and the read is planned once before
the first chunk so the whole fold still runs against a single pinned watermark and reports one
position for the append condition. Freezing between chunks is cheap because a freeze copies only what
the state reaches, which is the state, not the boundary.

Two constants came out with it, both free:

- **`envelope::decode` no longer goes through `#[serde(flatten)]`.** On the deserialize side flatten
  buffers the whole event into an intermediate map and deserialises a second time out of it, and
  every store read pays it: both fold paths, plus projectors, effects, `hekla verify` and
  `hekla test`. A hand-written `MapAccess` visitor does one pass. Unknown keys are still skipped
  rather than rejected, which forward compatibility depends on.
- **`alloc_event` borrows its field names.** It cloned a `String` per declared field per event while
  the envelope fields beside it already used `&str`.

Measured on this machine (20 cores), boundary folded through a rejecting command so nothing appends,
best of 7 solo and best of 5 for the concurrent batch. `us/event` is wall time over boundary depth,
so the concurrent column is latency under pressure, not throughput:

400-byte payloads, 16 concurrent:

| depth | solo before | solo after | x16 before | x16 after |
|---|---|---|---|---|
| 1,000 | 1.261 | 1.143 | 3.312 | 3.097 |
| 6,000 | 1.284 | 1.141 | 3.092 | 2.395 |
| 24,000 | 1.312 | 1.179 | 3.230 | 2.446 |

4 KB payloads, 8 concurrent, where retention bites harder:

| depth | solo before | solo after | x8 before | x8 after |
|---|---|---|---|---|
| 1,000 | 2.411 | 2.094 | 5.659 | 4.431 |
| 6,000 | 2.631 | 2.221 | 5.614 | 3.663 |
| 12,000 | 3.038 | 2.214 | 5.969 | 3.536 |

Per-event cost is now flat in depth: 26% drift across a 12x depth range becomes 6%, and 4% becomes
3% at the smaller payload. Under concurrency the deep boundaries gain 22% to 38%.

The retry backoff was also uncapped, `thread::sleep(1 << attempt)` milliseconds with no jitter, which
is why the benchmark measured `HEKLA_MAX_ATTEMPTS=10` as *worse* than 5: the last wait alone is about
a second, held on a request thread and its blocking slot, with 32 lockstep clients waiting through
it. It is now full jitter over a capped exponential, uniform in `[0, min(2^attempt, 16 ms)]`. The
jitter is the load-bearing half: requests that conflict are by definition synchronised, so an
undithered backoff sleeps them identically and lines them up to collide again.

**Confirmed on the benchmark**, three samples at the shipped `HEKLA_MAX_ATTEMPTS=5`, same corpus,
seed and machine:

| skew 0.99 | before | after |
|---|---|---|
| sustained at 250 ms | 145 / 145 / 702 | 757 / 830 / 830 |
| sustained at 500 ms | 150 / 150 / 739 | 977 / 830 / 830 |
| p99 at 25 ops/s (the cold deep fold) | 62-121 ms | 51.3 / 52.3 / 52.6 ms |
| events folded/sec | 0.87 M | 4.99 M |

Against 4.21 M events/sec at skew 0, the contended cell is now at parity: **the depth penalty is
gone**, which is what the retention hypothesis predicted and the sharpest confirmation this harness
can give. hekla now leads tephra at every deadline it reaches (830 against 537 at 250 ms) and returns
zero 409s, absorbing every conflict inside its five attempts. The oracle re-folded 1.1 M events per
cell at every prefix: no violations.

**Honest scope:**

- **The 5.5x median is mostly variance, not mean.** Before, two of three runs tipped into congestive
  collapse at the same ramp step and were trapped near 150 while the third reached 739; after, all
  three take an almost identical path (156 ops/s at 54.3 / 54.7 / 54.8 ms). Best case against best
  case the gain is about 12%, in line with the local probe. What the fix removed is the *coupling*:
  boundary depth is heavy-tailed here (p50 1757 events, p99 30031), so with memory linear in depth,
  whether 32 concurrent requests happened to land on deep boundaries together decided whether the run
  collapsed. A bounded heap decouples them, so the good outcome stops being a coin flip. Both readings
  are true and the distinction matters for where to look next: service time improved by tens of
  percent, and throughput improved 5.5x because the system had been sitting on a cliff edge.
- **The local probe under-reproduced the effect by about 15x, on the same machine.** It measured mean
  latency at a fixed depth; the benchmark measures p99 over a heavy-tailed depth distribution in an
  open-loop ramp near saturation. Right hardware, wrong statistic, and it nearly argued this change
  out of the tree. A probe has to reproduce the *metric*, not just the workload.
- **The two changes are still confounded.** The chunked fold and the jittered backoff shipped
  together. At `attempts=5` the old backoff could sleep at most 1+2+4+8 = 15 ms in total across a
  request, which cannot account for 150 to 830, so the fold work is almost certainly the driver; the
  backoff's own effect was measured at `attempts=10`, where the old code's last wait alone was about a
  second. An `attempts=1` run separates them outright, since hekla's backoff never fires there.
- **The 50 ms deadline is still unreachable**, and it is the one place tephra still wins outright. One
  cold fold of the deepest boundary costs about 52 ms at 25 ops/s, where nothing is contending, so no
  rate can get under it. That is roughly 1.7 us per event over a 30k-event boundary, and lowering it
  is now a constants problem rather than a scaling one.
- **`/status` now reports `folds`**, as `events_folded` and `chunk_seams` since boot. The first is
  read amplification, the number the benchmark had to proxy from tephra because
  `events_returned_p99` is zero in every hekla sample; divided by commands run it is the mean
  boundary depth a deployment is paying for. The second exists because chunking is otherwise
  unobservable: a chunked fold and a single-pass fold produce the same state by construction, so
  without a seam count nothing distinguishes "the budget worked" from "the budget was never reached",
  and the test that guards the seam would pass either way. A verify-mode re-fold is deliberately not
  counted, being the check's cost rather than the request's.
- **Verify compares with starlark equality, which has two false-positive cases.** A state holding a
  NaN, or a value whose type does not implement `equals` (a constructed event), is not equal to
  itself, so a deterministic fold producing either is reported as nondeterministic: a failed command
  or a wedged effect. Comparing rendered forms instead would close both and open something worse,
  since several types render lossily (a `CipherHandle` prints as `<encrypted:field>` with the
  ciphertext dropped), so two states differing only in a subject-encrypted value would look
  identical and verify would call a real divergence reproducible. A checker that can say "fine" when
  it is not is worse than no checker, so the false positive stands.
- **The chunk seam is invisible from outside.** Folding in chunks and folding in one pass give the
  same state by construction, so no black-box test distinguishes them. The test that guards it seeds
  600 padded events, which crosses the default budget five times, and asserts the count *and* the
  first and last ids the fold saw, so a carry dropped at a seam shows up as a wrong id rather than
  only a wrong total.
- **The budget is bytes as bumpalo reports them**, which is chunk capacity rather than bytes used,
  and it over-reports by up to a doubling. So a 1 MiB budget holds more like 500 KB of live data.
  Conservative in the right direction, and worth knowing before anyone tunes it.
- **Chunking is not free in memory, and tuning the budget down makes it worse.** Thawing the carry
  references the previous chunk's frozen heap and freezing keeps referenced heaps alive, so the
  per-chunk states form a chain released only when the fold ends. A code review caught the first cut
  claiming a flat bound it does not have. Two guards now hold it: a floor under the knob (a review
  measurement had a 4 KiB budget costing 76 MB peak where 64 KiB cost 34 MB, since a chunk per event
  pays every seam and saves nothing), and a rule that a chunk must be at least eight times the state
  it carries, so the chain can never exceed an eighth of the unchunked footprint. Verified after the
  fix: a 4 KiB budget now behaves exactly like the default rather than degenerating.
- **A fold that mutates the state a previous arm call built now fails once the boundary chunks.**
  Contract-breaking already (`AUTHORING.md` has always said to return the new state) and already
  broken on any retry since Phase 15, but the failure is depth-dependent, which is a bad way to find
  out. Documented with the fix alongside it, and pinned by a test that asserts both halves: it
  succeeds shallow and fails deep. An effect's `handle` likewise receives a frozen state now, which
  makes it agree with a command's.
- **`event.data` is still materialised in full.** The benchmark's events carry a `_pad` of 100 to 400
  bytes plus `eventId`, `title` and `name`, and no fold arm reads any of them, yet every one is
  parsed and allocated per event. That is the largest remaining constant on that workload, and with
  the scaling problem solved it is now the thing standing between hekla and the 50 ms deadline.
  Making it lazy means a hekla-owned value type with `get_attr`, which changes documented author
  surface (`type()`, `dir()`, unknown-field errors, and serialising `event.data` whole).
- **Caching folded state across requests is still the only thing that changes the asymptote**, and it
  is still the wrong thing to ship for a benchmark: it would drive read amplification to zero and
  stop the comparison being a comparison. If built, it belongs off by default and disclosed, the way
  the harness already treats umadb's page cache.

## Phase 17: the generated OpenAPI describes the whole surface (done)

The generator covered one of the router's nine routes. It emitted `paths` and nothing else: no
`tags`, no `components`, and responses whose only content was a `description` string. In the Scalar
reference at `/docs` that rendered as a flat list of `execute the ... command` with no request or
response shapes attached.

Three gaps, closed together because they share one generator:

- **The read API had no spec at all.** `GET /read/{projector}/{entity}` and its by-key sibling are
  public surface whose path params, query params, response shapes and 400/404/503 codes existed only
  in `server.rs` and in `AUTHORING.md` prose. Everything needed to generate them was already in
  `EntityDef`.
- **No response or error schemas.** The command 200 body and the two error envelopes (commands carry
  correlation ids, read and operator endpoints do not) are shared by every path and were documented
  nowhere, so nothing could be generated from the document.
- **No grouping and no domain vocabulary.** No tags, and the event and entity schemas the system is
  built around never appeared.

What shipped:

- `openapi::Surface`, a borrowed view of a `LoadedProject`, plus `openapi::build` over it. The
  runtime builds the document from this before it takes the project apart, and `hekla openapi <dir>`
  builds it from the same two calls with no data directory, no lock and no master key. One code path
  rather than two, and a test asserts the CLI dump and the served document are the same value.
- Paths for every route: one per public command (plus the `Idempotency-Key` and `X-Correlation-Id`
  headers), two per projector entity, and the operator endpoints, whose `name` params carry an
  `enum` of the project's own projector and effect names.
- Tags in render order: `commands`, one `read: <projector>` per projector, then `operations`.
- `components/schemas`: `ErrorDetail`, `Error`, `CommandError`, `CommandAccepted`, `EmittedEvent`,
  `Status`, `ProjectorStatus`, `EffectStatus`, plus one schema per declared event and per entity,
  with each field's policy as prose and as `x-hekla-*`.
- `read_api::filterable_fields`, with `is_filterable` and `EntityDef::validate`'s reserved-param gate
  both reimplemented on top of it. Three open-coded copies of "the key plus each index's leading
  column" became one, which matters because that load-time gate is the only thing stopping the
  generator from emitting a duplicate query parameter: widening filterability without it would let
  an entity whose index leads on a column named `limit` load, and shadow the page-size control.
- `server::route_table`, one list that `app()` folds into a `Router` and `server::routes()` projects
  the paths out of. The drift test reads `routes()`, so a route the process serves and a route the
  document is checked against are the same list by construction, rather than two lists and an
  instruction to keep them in step.

Eight details worth recording, because each is a place the obvious generated answer would have been
wrong. The last five came out of code review, and two of those were shipping broken output:

- **An entity's `required` is not simply "non-optional".** A subject-encrypted column whose key was
  erased is removed from the row rather than nulled, so declaring it required would describe a body
  the server does not always send.
- **`indexed` and `unique` are event-field policy and mean nothing on a read-model column**, which
  defaults to `indexed: true` regardless. Emitting them there produced a column annotated
  `x-hekla-indexed: true` next to a description saying it was not filterable. Entity columns carry
  `x-hekla-filterable` instead, derived from the key and the declared indexes.
- **Every documented filter parameter is guaranteed plaintext**, so the generator needs no
  ciphertext caveat. A filter arrives as plaintext and a subject column holds ciphertext, so such a
  filter could only ever match nothing, and `EntityDef::validate` already rejects both possible
  routes to one at load: a subject-encrypted key, and a subject-encrypted column in any index. The
  generator's first draft carried a warning for a case the loader makes unreachable.
- **A field annotation appends to the kind's description, it does not replace it.** `field_schema`
  is the only place that states `money` is a decimal string and that `uint` spans 0 to 2^64-1 (no
  numeric `maximum` can carry that ceiling without misleading the many tools that parse bounds as
  f64). Assigning the per-field note over it left every `money` column indistinguishable from any
  other string, on every field of every event and entity.
- **An optional command input admits an explicit null, not just absence.** `check_value` returns
  early for a null on a nullable kind, so `{"note": null}` is a 200. Omission from `required` says
  only that the key may be missing, so the type has to be widened too, and a `one_of`'s `enum` needs
  null as well or it rejects what its own `type` now permits. Verified against a running server.
- **`limit` and `timeout_ms` are clamped, not rejected, so neither declares a `maximum`.** The
  handlers do `clamp` and `min`, and a bound would make a validating client refuse `limit=1000`
  locally rather than receive the page of 500 the server would return.
- **`LoadedProject::load` succeeds at finding nothing.** A root that does not exist yields zero
  findings, so `hekla openapi /typo` exited 0 and printed a valid document containing only the six
  operator paths, which exist whatever the project declares. Harmless for `check` (which says
  "checked 0 module(s)" and moves on) and disqualifying for a command whose output gets committed:
  a CI regeneration step run from the wrong working directory would replace a real spec with that
  stub and pass. It now refuses a non-directory, and a directory that declares no modules at all.
- **Component keys need the same structural uniqueness as operation ids.** An event type is an
  unvalidated author string, so `event(type = "order placed")` and `event(type = "order_placed")`
  both sanitise to `event.order_placed` and the second `insert` silently replaced the first, leaving
  one schema describing the wrong event's fields while `EmittedEvent.type` listed both. Keys are now
  assigned up front by `ComponentNames`, before anything emits a `$ref`, seeded with the fixed names
  so an event type cannot displace `Error`. The first draft argued from the character set that this
  could not happen; the argument held for module names and not for event types.

Deliberately not done: narrowing `EmittedEvent.type` per command (a `handle` returns arbitrary
Starlark, so the emit set is not statically knowable), and vendoring Scalar, which `/docs` still
loads from a CDN.

The `oas3` dev-dependency is pinned `default-features = false`: its default `preserve-order` feature
enables `serde_json/preserve_order`, which would flip `serde_json::Map` to insertion order across the
whole test build. `effect.rs`'s journaled call hash is a hash of canonical JSON and depends on
`serde_json::Value` sorting object keys, so that feature would have made tests exercise a different
hash than production.

## Phase 18: the retry split covers every status that clears on its own (done)

The runtime absorbed transport errors and 5xx and handed everything else to the script, on the
stated invariant that "a result that reaches Starlark is always terminal". **429 was the
counterexample**, and 408 and 425 with it: the canonical retryable status arrived in the handler as
an ordinary result.

The gap was not that authors had to handle it. It was that they **could not**. Every response that
reaches a handler is journaled, and the wedge retry deliberately never clears the journal, so the
obvious handler:

```python
res = http.post(url = url, body = payload)
if res.status == 429:
    fail("rate limited")
```

wedges the invocation and then replays the recorded 429 from the op-DB on every attempt, forever.
The request is never re-sent. Retention does not reclaim it either, since the sweeper only touches
terminal invocations. The only exit is an operator skip, which abandons the work. A
reasonable-looking handler turned a routine rate limit into permanent data loss needing a human.
The alternative available to an author, a bounded loop inside one handler run, does re-fire (the
disambiguator makes each repeat a distinct journal entry) but there is no `sleep` builtin, so it is
a hot loop against a rate limiter bounded only by the tick budget.

What shipped:

- `effect::is_retryable_status`: 408, 425 and 429 join every 5xx, checked before the journal write.
  That ordering is the whole mechanism. Bailing before `journal_put` is what leaves the next attempt
  free to re-send instead of replaying the refusal.
- `Retry-After` is honored, as a floor under the wedge backoff rather than in place of it:
  `retry_delay = max(backoff(attempt), min(retry_after, 5min))`. Honoring it alone would let a
  limiter repeating `Retry-After: 1` pin an effect at one attempt a second forever; capping it at
  `BACKOFF_CAP` instead would defeat the point, since a limiter naming a 300s window is naming
  something longer than any backoff we would pick. The hint rides out of the host on a
  `Cell<Option<Duration>>` next to `terminal`, for the same reason: the starlark boundary flattens a
  host error down to its message, so a value the driver needs cannot travel inside one.
- Only the delta-seconds form of the header is parsed. The HTTP-date form would mean taking on a
  date parser and turning the peer's clock into a duration against ours; it reads as absent, and the
  backoff stands unchanged.
- `hekla test`'s `http_response()` guard moved onto the same predicate. It already refused a 5xx
  ("the runtime retries it, so a handler never sees one") and would otherwise have let a case assert
  behaviour on a 429 that the runtime now makes unreachable.

Two judgment calls worth recording:

- **A rate limit is a wedge, and reads as one.** `consecutive_failures` and `last_error` move for a
  429 exactly as for a 5xx, so an alarm on those will fire on ordinary rate limiting. That is
  accurate rather than unfortunate: the invocation genuinely is not progressing. It differs from
  other wedges only in clearing itself, which no separate counter would have conveyed better than
  the counter returning to zero does.
- **The ceiling on `Retry-After` is a defence, not a policy.** Five minutes is far longer than any
  backoff the runtime chooses and far shorter than the day a stray or hostile header could ask for.

The `Retry-After` test asserts wall-clock: a 1s window against a 200ms first backoff. It was checked
against a build with the honoring removed, where the retry landed at 223ms, so it discriminates
rather than passing on timing slack.

## Phase 19: read-only introspection under `/admin` (done)

hekla is an event-sourcing runtime that had **no way to look at the event log**. `/status` reported
counters and that was the whole window into a running system: no `/events`, no correlation trace, no
way to see what a wedged effect had actually done. Debugging "why did this happen" meant writing a
Starlark effect or opening the SQLite files, which ARCHITECTURE.md declares unsupported.

Most of the primitives were already there and unused. `WriteHandle::read_back` had never been called
from hekla, though its own doc names this use case. `journal_keys` plus `journal_get` already
reconstruct an invocation's exact ordered call sequence, which is what `verify_replay` compares
against. `module_metadata` had been written at every boot since the first schema and **never read**:
it is the deployed-inventory table, and a projector's and effect's `source_hash` survive nowhere else
once their units move into their threads.

What shipped: fourteen `GET` routes under one prefix, a new `src/introspect.rs` sitting above the
runtime alongside `verify`, nine bounded readers on `OpDb`, and the paths and schemas to describe all
of it in the generated document.

- **The log.** `/admin/events` pages newest-first over `read_back`; `?type=` and `?tag=` repeat and
  lower to exactly one tephra query item (types OR, tags AND), so nothing is reinterpreted on the way
  through. Positions are dense and 1-based, so the cursor is a position and needs no opaque encoding.
- **Traces.** `/admin/traces/{correlation_id}` is the one feature that needed a write-path change.
  The correlation id has always been in the envelope, but a store query filters on type and tags
  only, so finding a chain meant decoding every event in the log. Every event now carries a reserved
  `_hekla_corr` tag, making a trace an indexed probe. `build_event` already threaded `extra` tags for
  idempotency, so it was three lines; both command-response paths already strip the `_hekla_` prefix,
  and `hekla check` already forbids an author from naming it.
- **Effects.** `/admin/effects/{name}/invocations/{position}` lists every journaled call with its
  recorded result, which turns "my effect is stuck" into one request: the calls already listed will
  replay rather than re-fire, and the first one missing is where it is wedged.
- **Projectors, schema, system, subjects.** Entity shapes and the definition hash read out of the
  read model itself (so it is what the rows were built from, not what the project declares); the
  loaded project with per-module source hashes; the effective configuration, which `Runtime` had been
  discarding at boot; and the subject-key inventory, never the key material.

Judgment calls worth recording:

- **Always on, no flag.** ARCHITECTURE.md had contemplated an admin surface "behind a flag, off in
  production", and the flag turned out to protect nothing the bind address does not. `DEFAULT_ADDR`
  is already loopback, and the surface already lets any caller who reaches it append events and skip
  an effect's work. What a flag would have cost is concrete: `openapi::Surface` grows a boolean, the
  route-drift test becomes a matrix, and the served document either lies about what is routed or
  disagrees with `hekla openapi` (the exact guarantee Phase 17 exists to provide). Auth, when it
  comes, is one layer over the whole surface; gating one prefix would imply the rest is protected.
  The escape hatch is the prefix itself, which a proxy can deny without hekla's cooperation.
- **Payloads are shown and subject fields decrypt by default.** Not a new boundary: `decrypt_row`
  already decrypts a projector's subject columns on every `GET /read/...` over the same port. It is a
  slightly *wider* one, since the log holds subject values no projector materialised, so the request
  emits one audit line the way `reveal()` does. `?decrypt=false` opts out.
- **An unreadable field keeps its ciphertext and says which kind of unreadable it is.**
  `decrypt_row` removes such a column, which is right for a read model that must look like an
  ordinary row and exactly wrong for an operator, who would have to already know the field existed to
  notice it was gone. The four failure states are kept apart rather than collapsed to "erased":
  the decryptor returns `Ok(None)` both when a key is gone and when a live key simply does not match
  this ciphertext, and calling the second one erased would report permanent loss that did not happen.
- **A journaled call's arguments stay unstored.** Only the result is recorded, so this reports what
  came back and not what was sent. Storing arguments would be far more useful and would let plaintext
  that came out of `reveal()` outlive the erasure of the subject it belonged to.
- **Schema v5 adds `effect_journal.kind`.** The kind lives only inside the call hash's pre-image, so
  without a column an invocation view could show what a call returned but not what it was. Nullable,
  because a pre-v5 row genuinely does not record it and an invented value would read as a real one.

Honest scope:

- **Only events appended from this version forward are traceable.** Older events carry no correlation
  tag. The tag costs roughly 50 bytes per event in the log and the tag index.
- **`terminal_skips` remains process-local**, and a skipped position's durable trace is a terminal
  invocation row indistinguishable from a completed one. Introspection reports this rather than
  papering over it.
- **A projector quarantine is still in-memory only** while an effect's is durable. The asymmetry is
  now visible rather than merely true.
- **Row counts are opt-in** (`?counts=true`) and gated on a `ready` projector: a count is a full table
  scan, and a model at a previous definition's shape has no table to count.
- **No live tail.** `Subscription` makes an SSE stream cheap and it is the obvious next step, but it
  is a different transport with its own backpressure and shutdown story.
- **The admin read-only SQL endpoint deferred in Phase 3 is still deferred.** This makes it less
  necessary rather than delivering it. (It was later closed rather than delivered: `hekla project`
  and `POST /admin/projections` answer the arbitrary-query case in heklang over the log, which is a
  better answer than SQL over the read models would have been.)
- Every reader over a table that grows with traffic takes a caller-supplied limit, because the
  operational database is one mutex shared with each effect's hot path. Three do not, and say so:
  the module inventory and the per-effect runtime state are bounded by the module count, fixed at
  boot, and the subject-key counts are an aggregate no limit can bound, so `/admin/subjects` takes
  them once per listing rather than once per page. Anti-vacuity: removing the correlation tag makes
  the command-effect-command trace test return an empty chain rather than a shorter one.

## Phase 20: an admin console, in the binary (done)

Phase 19 answered every operational question over HTTP and left all of it as JSON. The only rendered
surface hekla shipped was `/docs`, a Scalar page loaded from a CDN. So the data existed and nobody
could look at it: diagnosing a wedged effect meant `curl | jq` against an API you had to read the
generated document to discover, and a correlation trace, the feature the log format was changed for,
came back as an array you assembled into a causal tree in your head.

What shipped: a keyboard-driven console compiled into the binary, served from the same URLs as the
API, plus three small additions to that API which the console needed and every client benefits from.

- **One URL, two representations.** `Accept: text/html` gets the console's shell; everything else
  gets the JSON, byte for byte unchanged. Deep links then cost nothing, because every view's URL is
  already an endpoint: `/admin/effects/send-welcome` opens that effect in a browser and returns that
  effect to a client. `hekla serve` prints one URL and it is both.
- **No build step.** Plain ES modules and one vendored 13KB runtime (Preact plus htm, `ui/VENDOR.md`),
  in a flat asset table compiled in with `include_bytes!`. `cargo build` stays hekla's only build,
  the console works with no network, and `HEKLA_UI_DIR` serves it from disk for editing it.
- **The three API additions.** An effect's `state` in one word (`healthy` / `lagging` / `wedged` /
  `quarantined`), derived once on `EffectShared` so `/status`, `/admin/effects` and any dashboard
  cannot disagree; `retry_in_ms`, so a wedge can be counted down rather than polled blindly; and
  `invocations` on a trace, joining the journal so a chain says *which* effect produced an event
  rather than only that one did.
- **It can act.** The projector and effect views drive the existing `replay` and `skip` endpoints
  behind a type-the-name confirmation. `/admin` itself stays read-only; those two have always lived
  outside it.

Judgment calls worth recording:

- **Negotiation on the existing routes, not a `/admin/ui` prefix.** A second prefix would have meant
  a second route table for the console's own views, kept in step with the first by hand. Sharing the
  URLs makes the console's route list and the router's the same list, and makes every page shareable
  as a link that also answers `curl`. The cost is one subtlety that had to be got right: `*/*` is
  what curl and a bare `fetch()` send, so a "does the client accept HTML" check written the obvious
  way would have turned every existing client's JSON into a web page.
- **A layer per route, not on the `Router`.** `Router::layer` wraps the fallback too, so negotiating
  there would have turned every unrouted `/admin/typo` 404 into a 200 shell, and would have run the
  check on `/commands` and `/read`. Attaching it inside the existing fold, selected from the same
  table, means a future `/admin` route gets a deep link without anyone remembering to.
- **The asset table is the namespace, and the dev override is a content substitution.** axum
  percent-decodes a path parameter, so an override that joined the requested name onto a directory
  would be a traversal. Resolving against the compiled-in table first and joining only the table's
  own name makes that unrepresentable rather than defended against.
- **The trace join constrains both columns.** `effect_invocation` is keyed `(effect, position)` and
  has no index on `position` alone, so the obvious `WHERE position IN (...)` would have scanned a
  table that grows with traffic, behind the mutex every journaled call contends for. The query names
  both, and a test runs `EXPLAIN QUERY PLAN` over it, because the correct and the incorrect version
  return identical rows and differ only in cost.
- **`retry_in_ms`, not `retry_at`.** A deadline published as an instant has to be compared against
  the reader's clock, which is a different machine's. A remaining duration is immune to that and to
  a server clock step, and the value's only consumer is a countdown.
- **The document describes the negotiation in prose, not as a second media type on all fourteen
  200s.** Listing `text/html` beside each page schema would be literally accurate and would make
  every generated client model each call as a union with a string, for a representation no
  programmatic client asks for.
- **Decryption is per event, not per page.** A decrypting request emits an audit line, so the console
  lists with `?decrypt=false` and opens one event with `?decrypt=true`. One line in the log then
  means one operator read one event. A list renders no payload anyway.

Honest scope:

- **No live tail.** A shared 3s poll of `/status` backs the badges and the views, pausing when the
  tab is hidden. `Subscription` still makes SSE cheap, and the console is what will make it worth
  wanting, but it is a different transport with its own backpressure and shutdown story. (Phase 34
  added one streaming response and did not take that bet: a projection's progress is one request
  with no reconnection and no fan-out, which is the part of SSE that costs.)
- **The overview's sparkline is not a metric.** It is bucketed in the browser from the timestamps on
  one page of events, and is labelled as such. Phase 31 gave hekla a metrics endpoint and this still
  does not pretend to be one: a rate belongs in Prometheus, and what this draws is the shape of the
  log tail the page is already showing.
- **The schema graph does not draw effect-invokes-command, and cannot.** The targets are chosen at
  runtime inside Starlark, so reading the project does not reveal them, and the journal is no help
  either: `journaled` records each call's *result*, and an `invoke_command` result is the invoked
  command's `{status, body}`, which does not carry its name. The page says so rather than drawing an
  edge it would have to guess at. Recording the name in the journal result would fix it and is not a
  console change: the result is what a replaying effect receives back from `invoke_command`, so
  adding a key to it changes what authors' Starlark sees. That deserves its own decision.
- **`/docs` keeps its CDN.** An offline console arguably pulls "vendoring Scalar" forward, and this
  phase deliberately does not do it: the reference is a different artefact with its own visual
  language and about a megabyte of third-party JavaScript.
- **No auth, unchanged.** The console is served from the prefix a proxy can already deny, and adding
  a login to one prefix would imply the rest is protected.
- **The JavaScript has no test runner.** Its two failure modes that Rust can see are covered instead:
  every `/admin` URL the console builds is checked against `server::routes()`, and every asset it
  references is checked against the compiled-in table, both by scanning the shipped bytes. Rendering
  bugs are found by opening it. Anti-vacuity: dropping the `effect IN (...)` half of the trace join
  makes the plan test report `SCAN effect_invocation` rather than a slower pass, and both scanning
  tests assert they found a plausible number of things before checking any of them.

## Phase 21: port from Starlark to heklang (done)

Starlark made determinism structural, which was the whole reason it was chosen. What it could not
make structural is the *domain*: that a command may not call out, that a projector may not decrypt,
that a fold may not read a clock, that sealed content may not be compared or interpolated, that an
event is written whole. Every one of those was a rule in `src/validate.rs` or a check at the append
seam, and a rule can only be as good as its own approximation. The most visible one: a boundary was
validated by evaluating `query()` once against a stubbed input, so a branch the stub did not take
shipped unchecked, and `dispatch` had a fail-closed lowering path to catch it at runtime.

[heklang](../heklang) moves each into the grammar or the type system. `starlark`, `starlark_lsp` and
`allocative` are gone from `Cargo.toml`.

### What replaced what

| Starlark | heklang |
|---|---|
| `input = schema(...)` | the command's parameter list |
| `query(input)` + `initial` + `fold = {clause: fn}` | `state x: T = fold seed on @path(filters) => expr` |
| `def handle(input, state)` | the command body |
| `reject` / `invalid_input` | `reject` / `invalid` |
| a projector's `handle = {clause: fn}` returning ops | `on @path { fields } { put / patch / update / delete }` |
| `get(entity, key)` then `put` | `patch E[k] { n: .n + 1 }`, which reads the row it writes |
| an effect's `handle = {clause: fn}`, every match running | `on @path as e { ... }`, one arm per event |
| `invoke_command(name, dict)` | `invoke Command { field: value }`, checked against its parameters |
| `uuid5(event.id, name)` | `Uuid.derive(e.id, name)` |
| `str(subject = "x", max_length = 200)` | `String? @subject(x) @max(200)` |
| `indexed = False` | `@no_index` |
| an opaque subject handle | sealed content, a type with three legal operations |
| `case(...)` in a `tests/*.star` `cases` list | `test "..." { given / run / project / deliver / expect }` |

### Three seams added to heklang

The port needed three things the language did not have, designed together because they are one cut:

- **`Rows`**, so a projector can write into a *persistent* read model inside the transaction that
  also advances a checkpoint. It reads as well as writes, because a stored `.field` load is filled
  before any value expression runs. `Interpreter::project` became a wrapper over `project_into`.
- **`World`**, so `hek test`'s runner is generic over the world it runs in. hekla supplies real
  tephra, real SQLite, a real `KeyStore` and a stubbed network. One definition of `expect`, two
  worlds; `docs/testing.md` rule 8 holds either way, because the only world-dependent assertion is a
  row and it is read through `patch`'s own seam.
- **`Value::from_json(&json, &ty, defs)`**, the inbound half of rule 8's conversion table. Every
  stored record field, every read-model column and every command argument arrives as JSON and has to
  become a value of a declared type, and whether `Money(2)` and `Money(3)` read differently out of
  one string is a language question with one right answer rather than three host guesses.

### What got smaller

- **The chunked fold is gone**: about 180 lines, four tuning constants, `HEKLA_FOLD_HEAP_BUDGET`, the
  freeze/thaw seam and `tests/fold_chunking.rs`. It existed because Starlark collects only at a
  module's root and a fold loop never executes one, so a fold's live heap grew with the boundary.
  heklang folds with ordinary Rust ownership.
- **The instruction budget is gone.** heklang has no `while`, rejects recursion and iterates only
  finite containers, so termination is structural.
- **The incremental conflict carry is gone** (Phase 15). It was worth its complexity when a fold ran
  in a Starlark heap that could not be re-entered cheaply; an attempt is now a function call.
  *(Wrong, and Phase 22 puts it back. The heap was one of two costs a retry paid and the cheaper one:
  re-reading the whole boundary out of the store was the other, and dropping the carry restored it.)*
- **`load()` and its resolver are gone**, with the cycle check, the module cache and the alias-vs-
  redeclaration identity trick.
- **`src/starlark_builtins.rs` (4,039 lines) is gone**, replaced by `src/schema.rs` (plain data) and
  `src/heklang_host.rs` (the host seam).
- **`hekla check`'s sixteen rules became three lints** plus what a directory means. The rest are
  parse errors with the offending field's own span.
- **`fmt.rs`, `lsp.rs` and `lsp/` are gone** (~1,300 lines), with `tests/lsp.rs`.

### What the port removed on purpose

- **`unique = True`**, and the reserved global-uniqueness secret behind it. It required an equality
  on sealed content, which leaks whether two ciphertexts hold the same value. `examples/orders`
  replaces it with a per-shop launch allocation, which is a constraint that genuinely *must* be
  enforced at append time and so gives the retry loop something real to do; the accounts fixture
  keeps a plaintext handle beside the sealed address. Erasing a subject still does not reopen a
  handle it claimed.
- **`uint`**, which had no heklang counterpart and existed only to reject values above `i64::MAX`.
- **Fold determinism as a verify invariant**, whose sources heklang removes by construction.
- **Fan-out across effect arms.** Rule 1 makes an event select exactly one arm, because declaration
  order was load-bearing for replay. Projectors keep the opposite rule and the reason is in
  `docs/effects.md` rule 1.

### Ten defects the port surfaced

Each was found by a test that already existed, which is the argument for porting the suite rather
than rewriting it:

1. **The effect driver handed heklang a tephra position**, where heklang counts from zero. Every
   effect wedged on "no event at position N".
2. **An effect's `invoke` appended with no idempotency key.** A crash between the append and the
   journal write would have appended the fact twice. The journal's own call identity now keys it.
3. **The correlation chain broke across every effect**, because the invocation minted a fresh
   correlation id instead of taking the triggering event's.
4. **A sealed non-text field could not be read back.** Encryption takes a string, so a sealed `Int`,
   `Bool` or `Timestamp` came back as text and failed the record read.
5. **A sealed `Timestamp` column stored a different form than a plain one**, so the read API served
   micros or RFC 3339 depending only on whether the field was personal.
6. **`verify`'s replay check never passed**, because the sealed journal never recorded what it
   visited; and a genuine miss reported the wrong reason, because the miss was detected at the write
   rather than at the read.
7. **`Retry-After` was dropped entirely.** Rule 5 moved the per-request retry into the language and
   the header channel went with it.
8. **An unknown request field was silently ignored**, because heklang binds only the parameters it
   knows.
9. **The `_hekla_` tag namespace was no longer enforced**, so a program could declare a field that
   forges the tag an append condition is guarded against.
10. **An unreadable subject field on the log path read as `none`**, collapsing absent into erased,
    which is exactly what rule 12 exists to prevent. A placeholder keeps the value present so the
    program reaches `reveal`, which consults the key store and fails terminally.

### Two measurements, kept

`tests/measure.rs` (ignored by default) answers the two questions the plan owed, on 20,000 events:

- **A full projector rebuild takes 228ms**, over two entities and 40,000 writes of which 20,000 read
  the row first. `patch` producing a whole row is not what a rebuild spends its time on, so
  `apply_one`'s UPDATE arm stays dormant and adding a `Rows::update` fast path later is a seam
  change and nothing else.
- **A fold over an encrypted boundary takes 93ms, against 24ms over plaintext.** That is the eager
  decryption above, and it is the motivating number for the ciphertext gap.

## Phase 22: the conflict carry comes back (done)

Phase 21 dropped the incremental carry (Phase 15) on the reasoning that "an attempt is now a function
call". That is true and it was not the point. Two costs were being paid for a conflict and only one of
them was Starlark's: re-entering a heap the language could not re-enter cheaply, and **re-reading the
whole consistency boundary out of the event store**. The port removed the first and left the second
in place, so every retry went back to `Position::ZERO` and folded a boundary of any depth from the
start. The measurement in Phase 15 that put hekla 4x behind a plain tephra client under contention was
about the second cost, and it came back with it.

The fix is where the carry always belonged, which is not where Phase 15 put it. heklang's frame is the
only place a folded `state` exists, so the attempt loop moved into `Interpreter::run_retrying`, and
what stayed in `dispatch` is the policy: `Retry { max_attempts, backoff }` reaches heklang as an
`again(attempt) -> bool` callback, so the budget and the wait are still the runtime's decision. Each
attempt keeps the state it folded and the position that state covers; the next one reads strictly
after it and folds what landed onto what it already has.

Three things fell out of doing it there rather than in the runtime:

- **`Query` gained a `from`**, so the delta read is a seek rather than a filter. tephra pushes an
  exclusive `after` into planning, and heklang's inclusive `from` crosses as itself: heklang counts
  positions from zero and tephra from one, so the two off-by-ones cancel.
- **A command's fold now stops at the head it took `after` from**, where it used to read to whatever
  the head had become by the time the read got there. Nothing observable changes (a slice event past
  `after` was always going to be refused by the condition), but the carry needs an upper bound that
  something other than the store knows, and "what you folded is what you conflict on" stopped needing
  a footnote.
- **The freeze is gone.** Phase 15 needed a frozen scratch heap so a `handle` could not mutate the
  state the next attempt folded onto, and it cost about 0.6µs an attempt. heklang has no mutable
  binding and the carry is taken before the body runs, so the hole that guard existed to close cannot
  be written.

The invariant work is hoisted with it, and it is more than Phase 15 hoisted: the pinned `now()`, the
bound arguments, the hoisted prologue and the slices they resolve to are derived once per request,
because all four read the arguments and each other and nothing else. Only the fold and the body see a
log that moved.

Measured by `tests/measure.rs::contention_against_a_deep_boundary`: a boundary seeded to 20,000
events, 15 commands per writer, against a build with the carry ablated. Same machine, one run each.

| writers | with the carry | re-folding from zero |
| --- | --- | --- |
| 4 | 315ms, **60/60** committed | 940ms, **46/60** |
| 16 | 530ms, **238/240** committed | 2.07s, **93/240** |
| 32 | 1.02s, **427/480** committed | 4.36s, **106/480** |

The committed counts are the number that matters, exactly as in Phase 15. Under 32-way contention the
re-folding build spent its whole retry budget reading the boundary and answered 409 to 78% of its
callers; the carry answers 11%. The shallow measurement beside it (`contention_against_the_retry_budget`,
a boundary that never passes a few hundred events) shows almost nothing, which is the honest shape of
this: the carry buys nothing on a shallow boundary and buys everything on a deep one.

**Honest scope:**

- **The delta fold is now observable, and both halves are tested.** Phase 15 could only assert that
  the carry stayed correct, since folding the delta and folding from zero give the same state.
  `tests/host.rs` in heklang asserts the second attempt's read is `from = ` where the first stopped;
  `tests/dispatch.rs` asserts the count each concurrent commit folded is exactly `0..committed`, which
  fails loudly on a double-fold or a skipped one. The existing contention test passes either way,
  which is why it was not enough on its own.
- **A conflict inside an effect's `invoke` still gets one attempt.** Rule 4 replays a wedged
  invocation from the journal, so the retry that matters is one level up and also re-reads what the
  arm decided on. Giving `invoke` its own budget would nest two.
- **The first fold is still untouched**, exactly as in Phase 15. A boundary 100k events deep costs
  100k records on the first attempt, and caching folded state across requests is the fix for that.
  The eager subject decryption below the language seam (Phase 21) is the larger constant on that path.
- **The backoff policy is unchanged**, again deliberately, and for the same reason: measuring a
  restored carry and a new retry cadence at once would attribute neither.

## Phase 23: a meaningful change is one you can hash (done)

hekla answered "did this code change?" three different ways, and all three hashed **source text**, so
a reformat, a comment fix or a renamed local read as a rewrite. `loader.rs` hashed each `.hk` file's
raw bytes, which also meant two declarations sharing a file shared a hash. `projector.rs` hand-rolled
a `definition_hash` over the subscription and entity shapes, *deliberately excluding the handler
bodies*, because including them meant a full replay on every comment. And `effect_invocation.script_hash`
was the file hash, so `hekla verify` silently skipped every recorded invocation of an effect that had
merely been reindented, and said nothing about why its coverage went to zero.

heklang gained a digest: a deterministic per-declaration hash over the lowered IR, where sugar is gone
by construction, local binders are slot numbers, contract names are kept verbatim, sequences stay
ordered and sets are sorted. Adopting it deleted all three schemes.

What shipped: a `declaration` table replacing `module_metadata`, keyed `(kind, name, hash)` so a boot
that loads unchanged code writes no row and a restart loop costs nothing, retaining every version of
every declaration with its packed form. `module_metadata`, the per-file `hashes` map and `hash_of`,
and the forty lines of `definition_hash` all went.

- **The projector rebuild reclaims a capability.** The definition is now the projector's entry hash,
  which covers the handler bodies. The old binary choice was "rebuild on every comment" or "never
  rebuild on a logic fix", and hekla had picked the second, so a corrected projector kept serving rows
  the old logic built until an operator remembered to replay it. Both halves are now right. The blast
  radius is wider than before in one direction: `const`, `refusal` and `guard` are inlined before a
  program exists, so editing a shared `const` rebuilds every projector that reaches it. That is
  correct, and worth knowing.
- **`script_hash` resolves three ways, not two.** Because the table retains history, a recorded hash
  is the current one, a known earlier version (whose form is on hand), or absent entirely, which means
  it was written under some other scheme and nothing follows from comparing it. The restart warning
  excludes the third case rather than blaming the code for it.
- **The event registry exists now.** `Digest::entries` covers events, enums, records and `fn`s as well
  as the three module kinds, and all of them are recorded. hekla previously persisted nothing about an
  event's declared shape, so a `Money` scale change, a flipped `@no_index` or a newly added `@subject`
  was undetectable. It is the prerequisite for a deploy-time diff.
- **What it does not do.** `hekla plan` is not built here; Phase 24 builds it on this. The schema and
  signature half of that diff is now a join over `declaration`, but nothing consumes it yet, and the
  effect-replay half additionally needs a baseline that can be *executed*, which the packed form
  cannot be (it is a rendering, not a serialisation).

## Phase 24: what this deploy would change (done)

Phase 23 recorded every declaration and its packed form and stopped there, which left the question
that motivated it unanswered: this code is not what is running, so what is different, and what would
booting it do? `hekla plan` answers it. It loads a candidate project, reads what a data directory
records as deployed, and reports the difference.

The recorded side needs no source tree. `Entry::from_packed` reads a stored `form` column back into
a comparable entry, recomputing both hashes, so a deployment describes itself. heklang's own
`docs/digest.md` prescribes the join and says explicitly that it ships no differ; this is the differ.

- **Two axes, not one.** A declaration is `added`, `removed`, `behaviour` (its hash moved and its
  signature did not, so it does something different behind a contract nothing outside can tell has
  changed) or `contract` (the signature moved, so a caller could notice). The split is free: the
  digest already carries both hashes, and Phase 23 already stored both.
- **The fan-out is explained.** `const`, `refusal` and `guard` are spliced into what names them
  before a program exists, so none has an entry and editing one moves every hash that reaches it.
  The unit of evidence is the *edit*: declarations that changed by the same added and removed lines
  are grouped, and only then identified. A guard is named exactly, from the call graph heklang keeps
  after splicing for this purpose, and only when the group is its whole caller set: a guard some
  unchanged command also names cannot be the reason, because that command would have changed too. A
  `const` or `refusal` is inferred from what the edit touched and labelled as an inference, and
  names nothing when more than one candidate fits. The specific cause wins over the general one, so
  a reworded refusal is not reported as its enclosing guard.
- **It runs against production.** `verify` takes the data-directory lock because it opens the event
  log, and so refuses a directory a server has open. `plan` reads the `declaration` table and the
  read models and nothing else, so it needs no lock. It changes no database: the operational DB is
  opened only after its schema version is read over a separate read-only connection and found to
  match, because `OpDb::open` migrates and silently upgrading a live deployment is not a reader's
  business. Read models are opened read-only, which also stops a forecast from creating the model
  for a projector that has never run.
- **A change is not a fault.** It exits zero whether or not anything would change. `verify` exits
  non-zero on a violation because a violation is a fault; a change is the expected result of running
  `plan` at all, and a command that fails when it succeeds is no use in a pipeline. `--json` carries
  the whole plan, including the before and after forms, for a gate that wants one.
- **What it does not do.** No effect replay, so it cannot say an effect would now make different
  HTTP calls. That needs a baseline that can be *executed*, and a packed form is a rendering rather
  than a serialisation; the journal is where such a baseline lives, and reaching it means opening
  the log and taking the lock. Phase 25 closes this once tephra grows a read-only reader.
- **One honest limit.** Opening a WAL database read-only still maps a shared-memory index, and a
  read-only connection cannot remove it on close, so planning against a directory whose server is
  down can leave an empty `-wal` and `-shm` pair behind. No database's contents change, and the next
  boot reclaims them.

## Phase 25: what this deploy would do (done)

Phase 24 stopped at "an effect changed", which is the half a diff can answer. The half that decides
a deploy is whether the change would move a single call, and answering that needs a baseline that
can be *executed*. The journal is that baseline. Reaching it meant opening the event log, and until
tephra 0.4.0 opening the log meant becoming its writer: `ReadHandle` was reachable only through a
`WriteCoordinator`, and `SegmentSet::open` created directories and dropped unwritten segments. So
replay could only ever have run against a copy, which is not the question anyone was asking.

tephra 0.4.0 added `Follower`: read-only descriptors, nothing created, nothing deleted, no lock, and
what it sees is a committed prefix of the writer's log. `hekla plan --replay` spends it.

- **The journal is the mock, and it is real.** Every recorded invocation of every affected effect is
  re-run against the candidate code and the journal the original run left behind. The journal holds
  the responses that run actually received, so a candidate that branches differently on a response
  body reaches a call the journal has no entry for, and that miss *is* the finding. Nothing is sent,
  appended or erased: it is the same sealed replay `verify` runs, and the only difference is which
  program goes in. `verify` asks "did this reproduce itself"; `plan` asks "would this still do what
  happened".
- **The gate is inverted, deliberately.** `verify` skips an invocation whose recorded `script_hash`
  no longer matches the *candidate*, because for an audit that divergence is legitimate. For a plan
  that divergence is the answer, so the hash a row is kept for is the *deployed* one instead. It
  cannot be the candidate's: an effect pulled in by a changed helper has an unmoved `script_hash` on
  every row it owns, and gating on the candidate would drop the whole baseline. It cannot be nothing
  either: retention outlives an edit, so rows written by a version already replaced are still on
  disk, and replaying those reports a difference the running code already has.
- **An entry's hash covers what is written inside it, not what it names.** A module-level `fn` is a
  declaration of its own, so "its hash changed" alone would miss an edit to the helper that builds a
  URL. So is an `event`, and an arm binds a field by name (`Frame::trigger` emits the name and the
  slot, never the type), so it would also miss `@subject(...)` arriving on a field the handler posts.
  The affected set is therefore the transitive closure of "names something that changed", over the
  references the digest already spells out. Each of those is spelled two ways, and taking only one
  of each pair is how a false negative gets back in: an event is `(events @p …)` in an arm's trigger
  list and `(slice @p …)` in a fold, and a record or enum is `(Record N)`/`(Enum N)` in a type and
  `(new N …)`/`(variant N C)` in a value, so an effect that only folds over an event, or only ever
  constructs a record, names it exclusively through the second. Conservative on purpose, because an
  effect pulled in needlessly costs one replay that reports `matched` and one left out costs the
  finding.
- **Who is asking is a parameter, because two readings turn on it and neither is in the outcome.**
  An empty journal is unanswerable to a caller reading a row back and unambiguous to the driver that
  watched the run complete. A terminal `fail` is rule 4's *outcome* rather than an error, leaving the
  same `terminal` row a success leaves, so replaying the program that wrote the row learns nothing by
  noticing while a candidate that would newly give up on recorded events is the whole point. One
  `effect::Asked` (`Live`, `Sweep`, `Candidate`) covers both, and `replay` takes the program from the
  runtime alone so the interpreter and the host can no longer disagree about which schema decodes the
  log.
- **It still runs against production.** The follower takes no lock, so `--replay` runs against a
  directory a server has open, and `replay_changes_no_event_segment` pins that every byte under
  `events/` (segments and the index beside them) is where it was.
- **Coverage is reported, because a blind replay looks exactly like a clean one.** Four things the
  replay cannot see, and none of them is fixable. An erased subject: the plaintext the handler
  branched on is gone, and journaling it to make the invocation replayable would defeat the erasure
  it was destroyed for. An invocation that journaled no call at all, where the candidate now reaches
  one: an operator skip and a run that took a callless branch leave the same row, and
  `opdb::InvocationRow` says outright that nothing distinguishes them, so a call the replay reaches
  there is not evidence of a change. That last one turns on who is asking: the live check inside
  `run_invocation` watched the invocation complete and took the operator-skip branch elsewhere, so
  the same outcome there *is* a divergence, which `Replayed::violation` decides from a `Record` the
  caller passes rather than from the outcome alone. An error is never one of these: a candidate that
  crashes before it reaches any call would crash on this event whatever the record says. Retention:
  `sweep_effect_journal` deletes the
  `effect_invocation` row and the journal cascades off it, so a reclaimed invocation is *invisible*
  rather than skipped, and nothing can count what is gone. And `--replay-limit`, because a busy
  effect's week of history is unbounded in a way a deploy gate is not. The first two are counted,
  the third is named as a horizon (the candidate's window, which bounds it rather than measures it,
  since the deployed configuration is what actually swept and hekla does not record it), and the
  fourth names every effect it bit. That settles the deferred retention question rather than
  answering it with a longer default.
- **The follower pins a prefix, and the replay respects it.** A server appending while a plan runs
  completes invocations whose events are past the tip the reader pinned, so the invocation query is
  bounded by that tip. Without it a busy effect would produce findings that are really a race, and
  differ run to run.
- **A missing master key degrades rather than refuses, per effect.** `verify` bails without one,
  because a sweep without a key reports corruption that is not there. A plan without one cannot
  replay a handler that reveals, and demanding a production key before it will diff two declaration
  tables would be worse than saying so. Which effects that applies to is read off the form (`reveal`
  is a node in it), so one sealed field somewhere does not blind nine effects that never touch a
  key, and a `reveal` that fails for want of a key is never mistaken for a divergence. A key that is
  *present* and cannot unwrap what is stored (a rotation with the previous master forgotten) degrades
  identically and names the reason, because it costs exactly the same thing: a wrong key must not be
  worse than no key, and throwing away a diff that is already computed and still true would make it
  so.
- **`--replay` is opt-in.** Without it the command keeps Phase 24's promise verbatim: no log, no key,
  no cost beyond a declaration diff, and `coverage` is `None` rather than a zeroed struct, because a
  replay that was never asked for must not read as one that covered nothing. The converse holds too:
  a replay refused because the deployment was recorded under another digest version reports zero
  coverage rather than none, so a gate keying on the field cannot read a refusal as a question it
  never posed.
- **What it cost elsewhere.** `Runtime` now holds a `Store` rather than a `WriteHandle`: the same
  reads either way, and an `Option` for the two operations that are not reading. Appending needs a
  writer, which a follower is not; subscribing needs a watermark that advances, which a fixed prefix
  has not. Both ask rather than assume, and the writer is asked for before anything is minted so a
  refusal cannot leave the id counter advanced for a request that wrote nothing. `verify::Report`
  gained discriminated skip counters, since one number for four reasons said coverage was missing
  without saying how much.


## Phase 26: an effect arm names its lane, and says how much of history it wants (done)

heklang 0.3.0 landed rule 15: every effect arm declares a `@key` and may carry a delivery modifier.
This phase is the dispatcher half, without which the declaration means nothing. Two production
failures are the reason it exists: one oversized order event stalled a warranty effect for every
merchant on the platform for eight hours, because an effect had one global lane; and a new effect
replayed from position 0, firing its side effects across the whole history.

- **Lanes.** An event's `@key` values name the lane it runs in. One lane processes in log order;
  different lanes do not wait for each other. A lane is the key *alone*, across the effect's arms, so
  two arms touching one remote resource under one shop id share it. Documented as an ordering
  guarantee the author chooses rather than a parallelism hint.
- **A failure parks its lane rather than sleeping on a worker.** This, not the parallelism, is what
  fixes the stall: `pool_size` wedged lanes would otherwise hold every thread in the pool. `[effects]
  pool_size` becomes live as the process-wide bound, and `1` still gets the fix.
- **The watermark becomes a low-water mark** (the highest position every event at or below is
  terminal) with per-lane rows (`effect_lane`) above it so a restart skips what a lane finished.
  Those rows are an optimisation; `begin_invocation` remains the authority on what has run.
- **`/status` names the pinning key.** A partitioned effect can lag by thousands while every lane but
  one is healthy, and journal retention is bounded by the mark, so a lane wedged for a month makes a
  month of journal rows unsweepable for every lane.
- **The live boundary** is a second number per effect (`effect_activation`), resolved to the log head
  at first activation and kept. A position an `on live` arm declines gets no invocation row at all.
- **`hekla rewind <Effect> <position>`**, CLI only, against a stopped process, never an HTTP endpoint.
  It prints what it would discard, names the arms, and asks; `--yes` answers the prompt without
  silencing the summary. `--live` also lowers the boundary, and is off by default because an author
  who wrote `on live` said history must not fire.
- **A `@key` change is reported before it bites.** `hekla plan` names the event types whose lane moved
  and whether the effect has drained; at boot, an effect whose key moved with lanes outstanding
  reports `blocked` and does not start, while the rest of the runtime keeps serving.

Honest scope for this phase:

- **Batch collapse for `on latest` is not implemented, and a program declaring it is refused at
  load.** Running it as `on` would give one invocation per event where the author asked for one per
  key: a different guarantee, delivered silently. A program that checks but will not run beats one
  that runs differently from what it says. *(Phase 27 implements it and deletes the refusal.)*
- The key-change block is an **operator signal, not a correctness gate**. Reprocessing under a new key
  skips rather than re-fires, because rows above the mark are never swept. What stopping buys is that
  a repartition is noticed by whoever caused it.
- Schema v8 adds `effect_lane` and `effect_activation`. `hekla plan` refuses a directory this build
  has not migrated, so run `hekla serve` against it once first.


## Phase 27: an `on latest` arm runs once per key, not once per event (done)

Rule 15's fourth obligation, deferred out of phase 26 because it is the most complex of the four and
had one customer. It now has four: FlowWarranty's `EnableCartTransform`, `EnableWarrantyValidation`,
`RegisterShopifyWebhooks` and `SyncShopPlansMetafield` all declare `on latest`, and the load refusal
was the only thing standing between that port and a deploy.

- **Collapse, as the language defines it.** One invocation per key per dispatch batch, at the newest
  matching position in it. History is still processed: a fold stops at the trigger's own position
  inclusive, so the one surviving invocation has already seen every event before it. The group is
  `(arm, key)` and never the key alone, since two arms have two bodies.
- **The batch is what the lane has queued**, which is what makes the live behaviour real as well as
  the catch-up one: a merchant editing six plans in a minute gets one publish. A queue entry became a
  group, so the in-flight cap now counts work that will run rather than positions, and a shop's whole
  backlog collapses to one invocation however long it is.
- **The record is a range, not a list.** `effect_invocation.collapsed_from` plus the invocation's own
  position: the members are every position between them whose arm and lane match, so the two integers
  reconstruct the grouping exactly and the retirement stays one `UPDATE` at any scale. `hekla verify`
  re-derives membership rather than trusting a stored set, and counts what it found.
- **Nothing folds into an invocation that has begun**, because that one has a journal and abandoning
  it would discard the record of calls that really happened.
- `/admin/effects/{name}` reports `latest_collapsed` beside `live_suppressed`, so lag falling without
  a matching number of invocations reads as the arm doing what it declared.

Honest scope for this phase:

- **Two guards in other files are what make the range replayable**, and the record site says so:
  `effect_activation.lane_scheme` stops an effect whose `@key` repartitioned the lanes, and a key
  change also moves the effect's digest, which `hekla verify` compares before it reads the range.
- **A group's span is not capped.** If one is ever needed it belongs on `hi - lo` rather than on the
  global position count, and it has to say what it split: a silent cap reads as "collapsed
  everything" when it did not.
- Schema v9 adds `effect_invocation.collapsed_from`. Rows written before it read back as null, which
  is the same honest answer as a row that folded nothing.

## Phase 28: the console browses a projector's rows (done)

Phase 20 shipped a console that could describe every read model and show none of it.
`/admin/projectors/{name}` rendered each entity's fields, indexes, key kind and opt-in row count, then
stopped at a `GET /read/...` hyperlink you were expected to click into a raw JSON tab. The question an
operator opens a read model to ask, "what is in it", was the one the console did not answer.

- **No new route, and no new server code.** Every `/admin` view is also a JSON endpoint at the same
  URL, which makes a route expensive (a second representation, duplicating `/read`) and a query
  parameter free. The browse selection is therefore `?entity=&field=&value=&cursor=&row=` on the
  projector's existing URL, and a row someone found is a link they can send.
- **The rows come from the public read API, and that is the point rather than a shortcut.** What the
  page shows is what an application sees over the same port: the same key order, the same refusal to
  scan an unindexed column, the same columns missing where a subject key is gone. A privileged view of
  the same table would show more and mean less.
- **The filter select lists every column and disables the ones that are not indexed**, naming the
  reason. The read API's `unindexed_filter` 400 becomes an affordance that teaches the indexing model
  rather than an error found by hitting it.
- **`absent` is told apart from `null`.** `row_to_json` omits a SQL NULL and the read API omits a
  subject column whose key it cannot obtain, so absence carries two meanings and the response cannot
  separate them. Which of them are *possible* is a fact about the declaration, and that is what the
  cell reads: `null` where only null can occur, `absent` where a key may be gone. Drawing every
  absence as `null` would hide an erasure, which is the one thing here worth noticing.
- **A row links back to the events that built it**, filtered to the tag its key carries and to the
  source event types that can carry it. Offered only where the type declares that field `indexed` and
  unscoped: an unindexed field is not a tag at all, and a subject-scoped one is tagged with its
  ciphertext, so its tag can never be matched from a plaintext key. Either would be a confident link
  to an empty result, which is worse than no link, so a `Totals` keyed `"all"` gets a sentence saying
  why instead.
- **A 503 from `/read` is a state, not a failure.** A projector that is rebuilding, stale or
  quarantined cannot serve rows at the definition the page above is describing, and the server's own
  message already names the fix. Painting that red would say something broke.
- **`describe` was corrected while it was being read.** The `?` was suffixed to the whole rendering,
  so a nullable bounded string reported as `String @max(500)?` rather than the `String? @max(500)` its
  author wrote, and an optional enum as `Active | Claimed?`, which reads as though only the last
  variant were optional. Three introspection surfaces report through that function; the enum case now
  brackets its variants.

Honest scope for this phase:

- **Rows are not on the shared 3s poll.** A scan costs the process more than a `/status` read, and
  rows shifting under someone reading them is worse than rows three seconds old, so the refresh is a
  button. The shape and readiness above them keep ticking as before.
- **One indexed filter, key order, no total.** All three are the read API's own shape, and the page
  states each rather than papering over it: `older` is enabled by the server's `next_cursor` and not
  by a full page, `newer` walks a trail the console keeps because the cursor is forward-only, and a
  row count stays the opt-in full-table scan it already was.
- **The selection replaces history rather than pushing it**, as the events view's filters do, so
  `back` leaves the page rather than stepping through pages of it.
- **`OneOf` renders its variants, not the enum's name.** `FieldKind` does not carry the name, and the
  variants are the more useful of the two in a data browser: they say what a column may hold without
  opening the `.hk`. Carrying the name would mean threading it down from `Type::Enum` and then
  choosing between the two.
- **Still no JavaScript test runner.** The two scanning tests cover this without being touched, which
  is what they were built for: the prefixes they scan are derived from `server::routes()`, so the new
  `/read/{projector}/{entity}` and `/read/{projector}/{entity}/{key}` calls were checked against the
  router the moment they were written. `describe`'s two corrected forms are pinned by unit tests in
  `src/schema.rs`.

## Phase 29: the console can run a command (done)

After phase 28 the console could diagnose everything and cause nothing. The only writes it drove were
`replay` and `skip`, both operator surgery; to make the application actually do something you left for
`curl`, and the empty-log state said "run a command and it will appear here" with no way to do so.

Commands were also the one module kind with no per-name `/admin` endpoint. An effect and a projector
each had one; a command existed only as a row inside `/admin/schema`. This closes that gap and then
uses it: `GET /admin/commands` and `GET /admin/commands/{name}` answer JSON, and answer the console's
run form to a browser, on the same URL as everything else here.

- **The form is generated from the declaration, and that is worth more than a `curl` snippet for one
  reason: it makes the wire rules structural.** A `Money(n)` leaves as a decimal string and never as a
  JSON number, a `Timestamp` carries its offset because the `now` button emits `toISOString`, and an
  `Int` past 2^53 keeps its digits. Those are three of the easiest things to get wrong by hand and a
  generated body cannot get them wrong at all.
- **The body is built as JSON text, not as an object.** Two field kinds cannot survive a round trip
  through a JavaScript value: an `Int` past 2^53 loses digits and a `Money` turned into a number is a
  float. Emitting each field as an already-correct JSON fragment settles both where the rule is known,
  and it makes the `JSON` tab literally the bytes that get posted rather than a rendering of them. The
  same reasoning as `arbitrary_precision` in `Cargo.toml`, at the other end of the wire.
- **A plain Run button, not the typed confirmation `replay` and `skip` use.** A command is the
  application's front door rather than operator surgery, the console is exactly as powerful here as
  `curl` against the same port, and a dialog per run would make the loop this exists for unusable.
- **The result links onward.** A committed run shows each appended position as a link to that event,
  the emitted events with their tags, and the correlation id as a link to the trace. `positions: null`
  says plainly that the command decided to append nothing, which is a success and what an idempotent
  replay looks like.
- **A 422 is rendered as the command working.** It ran, folded its boundary and declined; the code is
  a declared `refusal` and nothing was appended. Painting it the same red as a 500 would teach the
  opposite of what a refusal is for. A 409 gets a Retry button, because retrying is what its message
  says to do.
- **One renderer for a command, not two.** `introspect::command_detail` is what `/admin/commands`,
  `/admin/commands/{name}` and `/admin/schema` all return, and the document `$ref`s it in both places.
  The inline copy that used to live in `schema_path` was the shape most likely to drift, because
  nothing failed when the two disagreed.

Honest scope for this phase:

- **An internal command gets the page and not the form.** `Runtime::execute` filters them, so `POST
  /commands/{name}` is a 404 for one; the page says so and still lists its parameters, because an
  operator reading an effect's journal needs the shape.
- **`input` gained `optional`.** The `?` is in the `kind` string already, but parsing it back out is
  not uniform (an optional enum is `(A | B)?` where an optional bounded string is `String? @max(200)`),
  and an entity's and an event's fields already report the flag. One more field, three surfaces
  consistent.
- **An empty optional is omitted rather than sent as an explicit null.** Both are accepted, and
  omission is the one a single control can express. The `JSON` tab is the escape hatch for the other,
  and for an empty string, and for anything else the form renders imperfectly.
- **A required enum starts on its first variant**, which is legal but is not necessarily the enum's
  `@default`: `FieldKind::OneOf` carries the variants and not which one that is. Starting empty was
  worse, since a required select has no empty option and would post `""`.
- **Still no JavaScript test runner.** The two scanning tests covered the new URLs and the two new
  assets without being touched, as designed. The interactive paths were driven in a real browser
  instead, through `HEKLA_UI_DIR`: the form building the right body for every kind, the JSON override,
  a refusal, and an idempotency key replaying a commit without appending a second one.

## Phase 30: a deployment credential is a declaration, not a constant (done)

A project had no way to hold a Discord webhook url or a Stripe key. `docs/language.md` documented
`const WEBHOOK: String = "https://..."` as the shape, and it is the wrong one three times over: the
value is in git; `digest.rs:1339` puts a `Literal::Str`'s text into the packed form, so rotating it
moves every hash that reaches it and costs replay coverage on every invocation already recorded
against that `script_hash`; and one constant cannot be three different things in dev, staging and
production.

The log was not the answer either, and saying why is most of this phase. heklang already stores
**per-tenant** credentials there: a shop's OAuth token arrives as a command, lands as a
`@subject`-sealed field, and an effect folds it out and reveals it. A **per-deployment** credential is
not a domain fact, is permanent once written, costs a fold per invocation, and leaves a fresh database
unable to do anything until an operator posts a command, which turns "deploy is restart" into "deploy
is restart plus a runbook". So the split is the design: log for per-tenant, environment for
per-deployment, and `AUTHORING.md` §5a says so where an author will hit it.

heklang shipped rule 16 (`secret NAME`, a `Secret` type that is a taint rather than a wall, and a
`Request` carrying `wire` and `shown`). This is hekla's half.

- **The refusal is at boot, and it names every missing credential at once.** `Runtime::open` and
  `open_quiescent` refuse; `open_following` deliberately does not, because a plan is asked from a
  laptop as well as from a pipeline and demanding production credentials before it will diff two
  declaration tables is worse than saying what it could not replay. That is the same three-way split
  the master key already has, and the third stance is why `Coverage` gained `no_secret`.
- **`hekla check` never reads the environment**, which is a deliberate departure from the plan this
  was written against. It is the CI gate; one that needed production credentials to pass would either
  be run with them, which is worse than the problem, or be skipped. It warns about a declaration
  nothing reads and a `[secrets]` entry naming no declaration, and that is all. Whether a credential
  is *set* is `hekla plan`'s question and `Runtime::open`'s refusal.
- **The unused-credential warning reads the digest rather than the IR.** A read site renders as
  `(secret NAME)` and the declaration has no entry of its own, so a substring search over the packed
  form answers "does anything read it" without walking the expression arena. Tests are excluded on
  purpose: a credential only a `test` reaches is one production never uses.
- **A fingerprint is domain-separated by the declared name and truncated to 8 hex.** The job is an
  operator telling staging from production, not identity, and the full digest of a credential is a
  stronger oracle than that needs.
- **The leak heklang could not close.** rule 16 redacts the journal key, `ErrorKind::Unreachable` and
  every `Display`. It cannot redact `ureq`'s error text, which is written below the seam, and hekla
  concatenates that onto the wedge message: a webhook whose whole address is the credential could
  reach `/status`, `/admin` and `tracing` on the first DNS failure. `HeklaHost::redact_transport`
  makes two passes. Substituting `request.shown.url` for `request.wire.url` is exact and handles a
  transport that echoed the url verbatim; the store's own scrubber then scans for the credential
  *values*, which survives a rendering that normalised the url around them (a default port dropped, a
  reserved character percent-encoded). Both, because measuring settled it: `ureq` 3 renders a DNS
  failure with **no url at all**, so the substitution alone could not be shown to be doing anything,
  and a rendering that included a parsed uri would have slipped past it. The scrubber skips a value
  under 8 characters, since one that short occurs inside ordinary words.
- **A source that is present and unreadable is reported, not raised.** `secrets::resolve` is
  infallible and records the reason on the `Resolution`. `refusal` counts it as missing and names why,
  so `serve` still refuses and says "Permission denied" rather than "not set"; but `hekla plan`
  against a deployment whose `/run/secrets` mount the pipeline user cannot read still produces a plan.
  Raising would have made the one case an operator most needs a diff for the one case that yields
  none.
- **The refusal happens before the declaration table is written.** A boot that refuses must leave no
  trace: recording the candidate and then bailing would make the next `hekla plan` compare the
  candidate against itself and report that nothing would change, for a deploy that has never run.
- **The sealed replay gets the real store.** A read is unjournaled, so it re-runs the way `reveal`
  does; a replay answering nothing would wedge on `MissingSecret` and report a divergence for every
  invocation of every effect that reads one. That is the fault the check exists to find, not to cause.
- **A relative `file` path resolves against the project root**, not the working directory, so
  `hekla serve ./app` and `cd app && hekla serve` read the same credential and a container image can
  ship one.

**Honest scope:**

- **`verify` refuses rather than degrading per effect, and that is deliberate.** `plan` counts an
  effect it cannot replay and carries on, because a plan is asked from a laptop. A sweep is the
  opposite: its whole output is an assertion that the invariants hold, and one that quietly checked a
  third of the history reports `ok` having established very little. That is the same reasoning the
  master-key guard in `open_quiescent` already carries, and splitting the two would make one of them
  wrong. The cost is real: `hekla verify` needs the credential set for the project, even for effects
  that read none. If a deployment turns up where that is the difference between sweeping nightly and
  not, the fix is `reads_unset_secret` applied per effect, exactly as `plan` does it.
- **Journaled response bodies are still stored and served in the clear.** `Recorded::Response { body }`
  lands in `effect_journal.result` and `admin_invocation` serves it, so an OAuth token-exchange effect
  writes its access token there for the retention window. Untouched here because it is a different
  problem with a different fix: either put the credential in the log as a `@subject` field, which is
  the per-tenant story working as designed, or put the body behind the `?decrypt=`-shaped opt-in
  `/admin/events` already establishes. Nothing in this phase makes it worse.
- **Nothing is zeroized past the source.** `Value::Secret` holds `Arc<str>`, and a shared buffer
  cannot be wiped, so heklang holds plaintext for the life of the invocation and hekla's store holds
  it for the life of the process. `SecretStore` is not `Debug`, which stops the obvious accident and
  not a determined one. A partial guarantee would be worse than an honest absence, and both repos say
  so rather than implying otherwise.
- **A revealed value is still untainted.** `reveal` hands back an ordinary string, so an author can
  put a customer's email in a log line exactly as before. `Sealed` protects content at rest and after
  erasure and `Secret` protects it in observable output; these are different threats and unifying
  them would be a breaking change to every effect in the corpus.
- **No `_PREVIOUS` list, deliberately.** A master key needs one because stored data is wrapped under
  it. A credential wraps nothing, so create-new, deploy, revoke-old is handled by a restart, and the
  docs say it out loud because someone will otherwise copy the master-key shape.
- **`[secrets]` has two source forms and no provider.** Vault, AWS Secrets Manager and a SOPS- or
  age-encrypted file are all new variants of the same enum rather than a redesign, which is why the
  table is keyed by source rather than by value. None is built because none is asked for.

## Phase 31: metrics (done)

`/status` answers "how is it right now" and can answer nothing else. It cannot say when a projector
fell behind, how often a refusal fires, or whether an effect's lag is falling or stuck, and it cannot
wake anyone up. The deferral said "when there is something to operate at scale"; running hekla beside
umari, whose metrics have repeatedly caught a module that was down, is that.

`GET /metrics` serves the Prometheus text format on the same port, described in the generated
OpenAPI like every other route. `metrics` and `metrics-exporter-prometheus` with
`default-features = false`, matching umari, so the facade and the naming carry across both runtimes
and the exporter never opens a socket of its own.

- **The gauges are read at scrape time, and there is no collector task.** umari runs a 15s ticker
  because its state lives behind actors and reaching it is an async, fallible ask. Every gauge here
  is an atomic load on a `ProjectorShared`/`EffectShared` handle or `Store::head`, which `/status`
  already does synchronously in an async handler. So there is no interval to configure, no missed
  tick, and no staleness between a number and the scrape carrying it. Nor any `idle_timeout`, and so
  none of umari's "the expiry silently does nothing without `run_upkeep`" trap: hekla's module set is
  fixed at load, so no series ever needs expiring.
- **A lane key is never a label, and the reason is erasure rather than cardinality.** `/status` and
  `/admin/effects` name the pinning lane, and may: that JSON is a live view, so an erased subject
  stops appearing in it. A scrape is a *copy*, taken into a time-series database that replicates and
  retains it, and `hekla erase` cannot reach there. So `hekla_effect_wedged_lanes` is a count and the
  operator follows the alert to `/admin/effects/{name}` for the key. `tests/metrics.rs` asserts the
  absence against the rendered bytes rather than trusting the intent, because that is the change
  someone will make later for the best of reasons.
- **Every other label is a declaration too**: a command, projector, entity, effect or event name, a
  refusal code, a fixed outcome or state word, or a digest hash. Nothing is computed from a request.
  That is what makes the series count a property of the project rather than of traffic, and it is why
  `hekla_reads_total` is recorded past `resolve_entity` and not at the two 404s above it: before
  those, `projector` and `entity` are whatever the URL said.
- **umari's lesson about query-aware lag does not apply, and copying it would have been waste.** A
  narrowly-subscribed umari module legitimately trails the global head, so umari pays an event-store
  read per module per tick to find its own head. hekla already advances both watermarks past
  non-matching events (`projector.rs` publishes the subscription watermark when caught up;
  `LaneState::low_water` falls back to `scanned`), so `head - position` is honest and lag is a
  subtraction.
- **What umari has no equivalent for is the half worth having.** Wedged lanes, consecutive failures
  on the pinning lane, terminal skips, quarantine and blocked as distinct states, projector
  readiness as a state set, DCB conflict retries as distinct from the 409 a caller sees, and rule
  15's `live_suppressed` and `latest_collapsed`. umari cannot tell one wedged partition from a whole
  effect being down; that distinction is most of what an operator needs.
- **A counter is primed to zero from the declarations at every scrape**, so a command that has never
  run reads `0` rather than being absent and `rate()` works from the first scrape.
- **`docs/monitoring/hekla-alerts.yml` is the deliverable that pays for the rest.** Its thresholds
  carry their reasoning, and two are worth naming: a projector does not auto-restart so its failure
  is a permanent `up == 0`, while an effect's driver re-subscribes with backoff capped at 60s, so a
  naive `up == 0` there would flap and the real signal is a wedge outliving several windows. And a
  stall is `min_over_time(lag[15m]) > 0` rather than a threshold nobody can pick: a healthy module
  drains to zero between events and a stuck one never does.

Honest scope for this phase:

- **No latency histograms.** Command execution, projector batch apply and effect invocation are where
  a duration would earn its place, and `projector.rs` still computes a rebuild's elapsed time and
  throws it into a log line. Left out because the chosen scope is liveness, progress and rates, and
  because a histogram needs per-deployment bucket tuning to be worth its series.
  `PrometheusBuilder::set_buckets_for_metric` is the hook when a slowness question arrives that a lag
  gauge and a rate cannot answer, and no name above has to change for it.
- **`ignored` is not an effect outcome.** heklang's `Done` and `Ignored` both settle an invocation and
  `try_invocation` folds them into one `Ok(())`, so telling them apart would mean widening that
  return to carry a label. `completed`, `failed`, `terminal` and `skipped` are the four that exist.
- **The lane pool's queue depth is unmeasured**, which is the natural saturation signal and is
  observable today. It belongs to `EffectRuntime` rather than to `Arc<Runtime>`, so a scrape cannot
  reach it without plumbing this phase did not need.
- **Nothing behind the op-DB mutex is a metric.** Subject-key counts and journal sizes are the
  obvious candidates, and scrape-time refresh is only correct because every source is an atomic load.
  A metric that took the process-wide mutex would put a scraper in contention with the effect hot
  path.
- **A refusal series appears on first use rather than at boot.** heklang inlines a `refusal` before a
  program exists, so it has no declaration row and `introspect::command_detail` reports no codes;
  hekla cannot enumerate them to prime them. Bounded by the source either way, but an alert over
  `hekla_command_refusals_total` needs `or vector(0)` where the other counters do not, and the docs
  say so. It is the **only** such counter: the read pair is primed from the entities on each
  projector handle, which are as enumerable as everything else and had no business being the
  exception.
- **A state set is the one shape that fails silently, so neither list lives here.**
  `Readiness::ALL` and `EFFECT_STATES` sit beside the functions that produce the words, with a test
  apiece, because a list that falls behind its `match` makes every series read 0 and every `== 1`
  alert quietly stop firing rather than making anything break.
- **`EffectShared` gained a `running` flag**, mirroring the one `ProjectorShared` already had, so
  `hekla_effect_up` can distinguish an idle effect from an absent one. The only struct change here.
  It is cleared by a `Drop` guard rather than by a call at each exit, for the reason the projector's
  is: a panic under `supervise` unwinds past every explicit clear, and an effect whose thread has
  died reporting `up 1` forever is precisely the reading an operator must be able to trust. It is
  `true` from the moment the handle is published rather than from the moment the thread runs, so a
  blocked effect reads `1` for as long as the OS takes to schedule the thread that clears it;
  `state()` reports `blocked` throughout that window, and starting at `false` would make every
  healthy effect read down at boot instead, which is the same disagreement far more often.
- **A completion that quarantines is counted `completed`.** The counter moved above the verify
  check rather than below it: by that point the row is written and the lane is clear, so the
  invocation is complete by every durable measure, and counting after the check would leave the one
  invocation an incident turned on filed under no outcome at all.
- **A sealed replay is not an outbound call.** `metrics::effect_http` sits at the `Http` trait
  boundary so a stubbed run counts like a real one, and a verify replay crosses that same boundary
  through `SealedHttp`. Its refusal is skipped on `HeklaHost::sealed`, because counting it would
  report a code divergence as the network being down, in the very series the outbound-failure alert
  divides by.
- **`AlreadyTerminal` is counted under no outcome, deliberately.** It is a position an earlier
  process already completed, recovered rather than invoked, so counting it would inflate an
  invocation rate with work that never ran a handler. The cost is real and worth naming: while a
  restart drains a backlog of them, `rate(hekla_effect_invocations_total)` reads zero for an effect
  that is plainly making progress. `hekla_effect_position` and `_lag` are the honest progress signal
  there, and they move.
- **`refresh` reallocates every label on every scrape**, plus the `Vec` and sort each `*_handles()`
  accessor does. For sixty modules at a 15s scrape that is on the order of a thousand short-lived
  allocations a minute, against a handler that already does a `Store::head` and a render. `SharedString`
  or `Arc<str>` names on the handles would make it free, and the trigger is a profile that shows it,
  not the arithmetic.
- **The integration tests serialize on a mutex.** One recorder is process-wide and a series is keyed
  by its labels, so two harnesses booting the same example project in parallel write
  `hekla_effect_state{name="SendWelcome"}` at each other. That is a property of running several
  runtimes in one process, which only a test does. Counters are asserted as deltas there and exactly
  in `src/metrics.rs`, against a recorder local to each case.
- **The console is unchanged.** Its sparkline stays a browser-side view of the log tail rather than
  becoming a metrics consumer, and the comments that justified it with "hekla has no metrics
  endpoint" now say the honest thing instead.

## Phase 32: `hekla test` synthesises the envelope `hek test` does (done)

A project whose projector wrote `created_at: e.at` passed under `hek test` and failed under
`hekla test`. Downstream, FlowWarranty's backend got `145 passed, 0 failed` from one runner and
`132 passed, 13 failed` from the other, every failure reading `expected 1577836800000000, got 0`.

Both runners share heklang's `run_tests_in` and its `World` trait, so `expect` already meant one
thing. What they did not share is the **synthesised envelope**: the id and the append time a world
invents for a `given` event and for whatever the action appends. Those are not facts about the
world, they are what a runner made up, and two runners making them up differently give one `test`
declaration two meanings. `World` has no clock hook, so only hekla could fix it.

There were three divergences, not the one the report named:

| | `hek test` | `hekla test`, before |
| --- | --- | --- |
| append time of record *n* | `2020-01-01T00:00:00Z` + *n* minutes | `1970-01-01T00:00:00Z`, frozen |
| `now()` | the epoch + one minute per record | the same frozen instant |
| id of record *n* | `0190d1a1-0000-7000-9000-` + *n* as twelve **decimal** digits | `00000000-…-` + *n*+1 as twelve **hex** digits |

So heklang's clock **advances**, and swapping hekla's constant would have fixed only the shallowest
row: a case asserting the second `given` event's timestamp would still have failed, and with a
number close enough to the right one to be read as a rounding. The id row had bitten nothing yet
only because no shipped project asserts a derived id.

`HeklaHost`'s `now: String` and `minted: Option<u32>` became one `Stamp`. `Stamp::Wall(String)` is a
live append: a v4 id and the wall clock read once per request. `Stamp::Pinned` derives **both**
halves from the log position, exactly as `heklang::Harness` does. One field rather than two because
the halves have to move together: a positional timestamp beside a counter-minted id is a world that
agrees about when an event happened and disagrees about which event it was.

- **Deriving from the log rather than setting a value at each seam is the point.** The alternative,
  stamping once per invocation and writing that value in `World::given` and `World::open`, produces
  the same answer for every case a `.hk` test can express, and needs every future seam to remember.
  Asking the log cannot be forgotten.
- **The counter in heklang's id is decimal, in a field that is read as hex.** `{position:012}` makes
  position 10 `…-000000000010`, not `…-00000000000a`. Building the id from a `u128` looks equivalent
  and diverges at the tenth event, which is deep enough into a suite to be found from the wrong end.
  hekla formats and parses back, and a case in the differential test pins position 10 for that
  reason alone.
- **The differential test asks heklang rather than restating it.** `PINNED_EPOCH_MICROS` and its step
  are copies of constants private to `heklang::Harness`, so hekla is now coupled to that crate's
  internals. `a_pinned_envelope_is_the_one_heklangs_own_harness_writes` pushes events into a real
  `Harness` and compares, so a heklang release that moves either number fails hekla's own suite
  instead of surfacing later as a downstream project whose two runners disagree by an amount nobody
  recognises.
- **`tests/pinned_clock.rs` runs every case twice**, through `testing::run` and through
  `heklang::run_tests`. Agreeing is the assertion. Nothing else in the tree could have caught this:
  `e.at` appears in exactly one other `.hk` source in the repo, and no shipped example reads it.

**Honest scope:**

- **This is breaking for `hekla test`.** A project asserting `1970-01-01T00:00:00Z` or a
  `00000000-…`-derived id starts failing. Those assertions never passed under `hek test`, which is
  the whole point, but they did pass here.
- **A `.hk` case cannot observe the stamps within one append.** heklang rejects a `run` and a
  `project` in the same test, and an effect fires on `given` events rather than on a command's
  output, so the only envelopes a case can read are the seeded ones, one event per append. Full
  parity therefore buys nothing a `.hk` test can see today; it is taken because deriving from the
  log has no seam to forget, and it is pinned from Rust instead.
- **It asserts a shape no `hekla serve` produces.** A live append stamps one instant across the whole
  request, and a pinned world puts a minute between two events of one append. That is heklang's
  harness fiction, and hekla reproduces it rather than improving on it: the fiction is heklang's to
  define, and a runtime that improved on it locally would be back to two dialects. If it is wrong it
  is wrong upstream.
- **`tests/support::seed_event` keeps a frozen clock.** It seeds hekla's Rust tests, which run under
  one runner and have no parity obligation. Its constant moved to the same epoch only so hekla tells
  one story about what a pinned clock reads.
- **The shipped examples still never read `e.at`**, which is why nothing here caught the bug.
  `tests/pinned_clock.rs` covers it; adding a column to `examples/users` would churn the read-API
  tests for coverage that file already gives.
- **`Clock::now`'s silent `unwrap_or(0)` stays** on the `Wall` arm. It is unreachable in practice,
  since `now_rfc3339` cannot produce an unparseable string, and narrowing it is a separate change.

## Phase 33: a deploy that cannot read its own log does not start (done)

Adding a non-optional field to an event that already had instances made every stored one
undecodable, and nothing said so. `record_of` fed `Json::Null` for a key the payload did not hold,
and the only arm of heklang's table that accepts null is `Type::Opt`, so a fold answered
`expected String, stored null` as an HTTP 500, a projector rebuild went `rebuild_failed` and retried
forever, and an effect lane wedged in `LaneId::unreadable` on capped backoff, pinning the watermark
and the journal retention behind it. `hekla check` reported `ok`, `hekla test` could not express an
old payload at all, and `hekla plan` said `contract` without saying history stops being readable.

heklang gained `@absent(<literal>)` first: what a field reads as in a payload written before it
existed, honoured by `value::stored_field` and by nothing else, so an inbound request body still has
to satisfy the declaration as it stands. This phase is hekla's half.

- **Two checks, with two different reaches, and the first draft had only the second.** The sampled
  check is `record_of` itself on the oldest stored event of each declared type: not a walk of its
  own, which is the whole argument for trusting what it catches, because a failure there is one of
  those three readers breaking one event early rather than a forecast that it would. One bounded,
  `limit`-1 read per type, with the cap pushed into tephra's planning.
- **"The oldest event is enough" was wrong, and a review caught it.** The claim rested on a field a
  declaration did not have being absent from *every* payload written under it, which is true, and on
  a field whose type changed being wrong on *every* event of that type, which is not: narrowing an
  enum breaks only the events that stored a variant it lost. The absence half had a second hole of
  the same shape, since a field removed in one deploy and put back in the next leaves events in
  between that the two ends do not resemble. Both were reproduced before anything was changed.
- **The complete half reads no event, and comes out of the declaration table.** Every deploy records
  every declaration and the rows are kept rather than replaced, so "which fields does this program
  have that some version it deployed did not" is answerable without opening the log; one existence
  read per affected type then says whether anything was written under such a version. Keeping every
  version rather than only the current one is exactly what closes the remove-and-re-add hole.
  `Entry::field_names` is heklang's, because the shape of an `(f ..)` node is the digest format's and
  a host picking it apart from outside would be a second copy free to drift.
- **The sampled half keeps its place, with its reach written down.** It is what catches a value that
  no longer fits its field, which no declaration diff can see, and it costs one read. What it misses
  is drift that is wrong on only some events of a type; `a_narrowed_enum_is_missed_when_the_oldest_event_kept_a_surviving_variant` pins that limit so closing it cannot happen silently. A complete
  answer means decoding every event at every boot, which is work proportional to history on a path
  that runs before the port is open.
- **Rejected: narrowing the refusal to absences.** The rule as first stated was about a field the
  payload has no key for, which is the case an annotation can answer. But the same decode also
  catches a stored value that no longer fits its field, at no cost, and that breaks the same three
  readers just as permanently. Shipping the narrow rule would have meant matching on the mismatch and
  *discarding* every kind but absence: code whose only purpose is to look at a stored event hekla
  knows it cannot read and boot anyway. The two faults keep separate guidance instead, because only
  one of them has an annotation that answers it, and the other's repair is a rule worth stating:
  **a field's type is part of the fact, so a new type is a new field.** Remove the old field and
  declare the new shape under a new name carrying `@absent`; a field nothing declares is a field
  nothing decodes.
- **Rejected: the `blocked` latch Phase 26 used for a lane repartition.** That one stops one effect
  and is recoverable by redeploying the previous key and letting it drain, so an operator signal is
  the right weight. This has no drain and no partial mode: every reader of the type is broken. It
  refuses the process, in the window the credential check already occupies, which is after the store
  and the op-DB are open and **before the declaration table is written**. A boot that refuses leaves
  no trace, so the repair is a source edit and nothing else.
- **`hekla verify` refuses too**, in the window where `open_quiescent` already repeats `open`'s
  master-key and credential guards, and with the same reasoning written there twice already: a sweep
  that replays a program which cannot read the log reports a failed rebuild and a divergence per
  invocation, so a healthy directory exits non-zero naming corruption that is not there.
- **Plan answers a narrower question than the boot, and the two are allowed to differ.** Plan asks
  what *this deploy* would newly break, off the diff it already computes; the boot asks whether the
  program can read the log at all, unconditionally, which is what catches a directory an earlier
  deploy already broke. That one plans clean and refuses to boot, and that is the right way round:
  plan reports changes, and there is no change.
- **`hekla plan` now opens the event log, conditionally, and that is a documented property moving.**
  It did so only under `--replay` before. The gate is the diff it already computes: when an `event`,
  `record` or `enum` moved, it reads the log through the same read-only follower `--replay` uses. A
  deploy that moves none of those three still opens nothing. `unreadable` is `null` when the question
  was never asked and `[]` when it was asked and the log was readable, following `divergences` rather
  than `secrets`, because a gate must not read one as the other.
- **`enum` is in that gate and is the reason it is not just `event` and `record`.** Narrowing an enum
  breaks history exactly as a type change does, and nothing about a field's *name* would have found
  it. `a_widened_enum_boots` beside `an_enum_that_loses_a_stored_variant_refuses` is what keeps the
  check empirical rather than a diff wearing a probe's clothes.
- **One warning, not an error: a boundary keyed on a field carrying `@absent`.** Every field is
  auto-tagged, so an event written before the field existed has no tag for it and the slice can only
  match events appended since. The absent value does not help, because it is read from the decoded
  payload and a slice never gets that far. A judgement about a design rather than something hekla can
  refuse, so it sits with the other two boundary warnings.

Honest scope:

- **A `.hk` test still cannot build a payload from before a field existed.** `given` writes an event
  whole, so the read-time fallback is reachable from hekla's own suite and not from a project's.
  Letting `given` omit a field that answers absence is a heklang change; deferred with a trigger
  rather than left open, because what it buys is testing a *handler* against old events, and no
  project has one of those yet.
- **The sampled half reads only the oldest event of each type**, so drift that is wrong on only some
  events of a type is caught only when it samples one of them. A value mismatch also masks an absence
  deeper in the same field, because the decode returns its first error; the deploy is refused either
  way, and the next boot after the first repair names the second.
- **The complete half is only as complete as the declaration table.** A data directory whose
  `hekla.db` was deleted, or one written under a digest version this build cannot reproduce, has no
  recorded shapes to compare against, and the sampled half is all that is left. A row whose form does
  not reproduce its own hash is skipped rather than guessed at.
- **The probe reads every declared type, every boot.** N indexed `limit`-1 reads, which is small
  beside opening the op-DB, and unconditional on purpose: it catches a directory an earlier deploy
  already broke, not only the deploy that would break one.

## Phase 34: a question the project never anticipated (done)

Every read surface hekla had answered a question somebody declared in advance. A read model exists
because an author wrote it down, and the page that browses it can only show what was written down.
The question an operator actually arrives with ("how many of those, grouped by that") had no answer
short of writing a projector, deploying it, and rebuilding.

An ad-hoc projection is that answer: heklang compiled against what the process booted with, folded
over the real log through a read-only handle, read back, and thrown away. It shipped on three
surfaces, and this phase is the third of them plus the transport the third needed.

- **`hekla project`** folds through a follower: read-only descriptors, no lock, a prefix pinned at
  open. It takes no data-directory lock and writes nothing to the data directory, which is what lets
  it run against a directory a server is serving from.
- **`POST /admin/projections`**, behind `[admin] projections = true`. The one route that runs code a
  caller supplied rather than code the project declared, which is exactly why a deployment has to say
  yes to it. What bounds it once on is that heklang is total and a projector holds no host: no clock,
  no network, no general read, no append. The residual cost is CPU and a temporary file, and both are
  clamped.
- **The console page**, which is what an operator actually uses: an editor with a gutter and heklang
  highlighting transcribed from the lexer, `⌘↵` to run, diagnostics whose line and column are a
  button that moves the caret, and saved projections in `localStorage`.

Decisions worth keeping:

- **Streaming is not the SSE bet phase 20 deferred.** Phase 20 said a live tail was "a different
  transport with its own backpressure and shutdown story" and that still holds for a subscription.
  This is not one: it is one request, with no reconnection, no fan-out, no long-lived connection, and
  a 16-line bounded channel that drops ticks rather than growing. The fold's progress callback
  already existed for the terminal ticker, so the data was there and only a body was missing.
- **Compiling is its own hop, before the body opens.** A status code is spent on the first byte, so
  a source that does not compile or a window that holds nothing has to be refused before that byte.
  `projection::check` is that refusal, one spelling shared with `run`.
- **The last line of the stream is the buffered body, byte for byte.** One shape reaches three
  transports, and a client that wants only the answer reads to the end.
- **Diagnostics became structured.** They were rendered strings, which a browser can only regex, and
  the console needs a line and a column to put a caret on. `validate::finding_json` sits beside
  `validate::render` so the two cannot drift. Fixing this surfaced that `render` had been adding one
  to a span heklang already counts from one, so every diagnostic hekla had ever printed pointed a
  line and a column past the problem.
- **Saved projections are client-side.** Storing them server-side would put a write surface on the
  one page whose whole claim is that it writes nothing, and a question an operator asked is theirs
  rather than the deployment's.

Honest scope:

- **`--upto` bounds the answer, not the work.** tephra's forward read has no upper bound, so a
  window ending early still reads to the tip and discards. `max_events` is what bounds the work.
- **A disconnected client does not stop a fold.** `spawn_blocking` is not cancellable, so the budget
  is what bounds an abandoned request, exactly as it is for the buffered shape.
- **Two at once, per deployment.** The third is refused with `Retry-After` rather than queued.
- **The console page is checked by opening it**, like the rest of the console: the JavaScript has no
  test runner, and the two failure modes Rust can see (a URL no route serves, an asset nothing
  references) are covered by scanning the shipped bytes.

## Phase 35: every wedged lane is nameable, not just the worst one (done)

An effect publishes exactly one stuck lane. `EffectShared.pinning` is `Mutex<Option<(String, u64)>>`,
surfaced as `pinning_key` and `pinning_position`, and `wedged_lanes` beside it is a bare count. So a
failure list built on hekla is structurally incomplete: it can show every `fail`, because those append
events, and not a single wedge, because those append nothing. Under lanes a wedge is the commonest
production failure, so the customer whose refund is stuck behind one sees an empty list.

**The fix is not the runtime appending events.** "Only commands append" stands, and
`heklang/docs/effects.md` rule 4 keeps a wedge invisible to the script on purpose: an author must not
be able to branch on a retry count. What this adds is an operator-facing surface that can enumerate
what is stuck, so a caller can join a lane back to an order.

Most of it is already in memory. `EffectShared.stuck` is `Mutex<BTreeMap<LaneId, StuckLane>>` and
`StuckLane` already holds the position, the attempt, the error and the retry deadline, because
`republish` derives every single-valued field from that map. Nothing new is recorded; what is missing
is a reader.

- An accessor beside `pinning()` returning one public view per lane, filtering out `DRIVER_LANE` the
  way the counters already do. `!driver` is not a key and must never be reported as one.
- `stuck_lanes` on `effect_detail`: the lane key, the position, the attempt count, the error and the
  time to the next retry. `pinning_key` stays, because it is the one `last_error` describes and the
  one an operator skips.
- The console lists them with a skip control per lane, which is the payoff: today the only skippable
  position is the pinning one, so clearing the second-worst lane means clearing the worst first.
- The OpenAPI `EffectDetail` schema grows the array and its `required` entry.

Decisions worth keeping:

- **The published deadline is a remaining duration, never an instant.** `StuckLane.retry_at_ms` is
  millis since `EffectShared::started`: monotonic, per-process, and meaningless on a reader's clock.
  It publishes through the same `retry_in_ms` shape the single-lane field already uses, for the
  reason written on that field.
- **Ordered by position ascending, so the pinning lane is first.** The same ordering `republish`
  picks the pinning lane by, so the head of the array and `pinning_key` cannot disagree.
- **Neither `/status` nor the `/admin/effects` listing grows an array.** `/status` is the summary a
  scrape reads and `wedged_lanes` is the number it wants; the listing is polled every few seconds
  across every effect at once, and during the outage where each one is wedged (the only time a person
  is watching) the arrays would be the whole body, mostly repeated error text. Per-lane detail belongs
  behind the route that names one effect. The listing's shape is `EffectSummary`, derived from
  `EffectDetail` by removing the one key rather than written out beside it, in the generated document
  and in `introspect` alike, so the twenty fields they share cannot drift.
- **The page and the count are read under one lock.** They are the only way to see that the page was
  capped, so a count from before a lane cleared beside a page from after it would read as a
  truncation that never happened.
- **Capped rather than assumed bounded.** `MAX_INFLIGHT` is 1024 positions, so the map is bounded in
  practice, but the array is capped and `wedged_lanes` carries the true count, so a reader can tell
  it was truncated.
- **The schema drift test grows two more bodies and learns to descend.**
  `a_described_body_carries_exactly_the_keys_its_schema_declares` (which guarded `/status` alone, as
  `the_status_body_matches_the_schema_that_describes_it`) now covers the effect detail and the
  listing, and walks into array items rather than stopping at the top level. Both halves matter: a
  `required` key the body stopped sending, and a key the body grew that
  `additionalProperties: false` forbids. It boots against a 503 stub so the effect actually wedges,
  because a body whose array is always empty leaves the item shape describing nothing the test looks
  at.

Honest scope:

- **`stuck` is process-local and a restart clears it.** The durable record is `effect_invocation`,
  which has no lane column. The map self-heals: the driver resubscribes from the watermark and every
  lane that was wedged re-wedges within a poll cycle, so the empty window is short. Enumerating
  stuck lanes durably is deferred rather than designed, and if it comes back it is resolve-on-read
  through `lane_of` (already `pub(crate)`) rather than a stored column, so there is no migration and
  the lane cannot drift from the code that computes it.
- **A blocked effect still reports nothing here**, and correctly: nothing is retrying and nothing is
  stuck. `state()` already reports `blocked` and `last_error` already outranks a wedge with the
  reason.
- **The page is capped and there is no cursor**, so past a hundred wedged lanes the rest cannot be
  named until the ones ahead of them clear. Deliberate rather than deferred: the cap keeps the worst
  lanes, and the worst are the ones holding the watermark and therefore the ones an operator clears
  first, so clearing forward is the order the work happens in anyway. `wedged_lanes` says how many
  are hidden. A cursor is what this grows if an effect ever routinely wedges in the hundreds.
- **The lane column can be wider than the screen.** A lane is an encoded `@key`, and a string key
  reaches 512 characters before `crate::lane::encode` elides it to a hash, so one long key pushes the
  error and the skip control off to the right. The table scrolls sideways, which is what the console's
  tables do and what `.table-wrap` is for, and the alternative was worse: clipping that column instead
  makes every column collapse on a phone rather than scroll, which was checked at both widths.

## Phase 36: a sealed record round-trips (written, gated on the heklang release)

heklang 0.8.0's `Value::from_sealed` has no arm for a record, a list, a map or a `Json`, and `reveal`
has no other decode path. So `ship_to: Address @subject(customer_ref)` does not merely fail in
heklang's test harness: it raises a `Mismatch` inside a deployed effect, which hekla classifies as
non-terminal and retries forever. heklang fixes the arm and moves the writing half to
`value::sealed_text` beside its inverse; this phase is hekla taking the fix and covering the path it
opens.

**hekla's own halves were already right**, which is why this is coverage rather than a fix.
`FieldKind::of` maps `Record`, `List`, `Map` and `Json` all to `FieldKind::Json`, `seal_text` writes
`json.to_string()` for that kind, and `unsealed_json` parses it back, falling back to the raw text
for a scalar that was flattened instead. That is the inverse pair heklang grew, and the two writers
agree case for case: a composite renders as its JSON document on both sides, a `Json` is written
whole on both sides, and a scalar renders bare on both. Object keys sort on both sides (heklang's
`Json::Obj` is a `BTreeMap`, and serde_json's `Map` is one with no `preserve_order` feature here),
and number text is exact on both (`Json::Num(String)` there, `arbitrary_precision` here). heklang's
reader is deliberately lenient about exactly what serde_json emits: `\u00XX`, surrogate pairs, and
the `\/`, `\b` and `\f` it never writes itself.

**None of that was tested.** No fixture seals a composite and no example uses one:
`examples/orders/events/order.hk` seals a `String?`, a `String?` and a `Money(2)`, all scalars. The
path is written correctly and unexercised, and authors will reach for the shape as soon as it works,
because it is the one `heklang/docs/declarations.md` now recommends: one address rather than nine
parallel optional sealed fields, where adding a tenth is a schema-evolution event on every event that
carries it.

- The heklang version bump, which is the gate. Nothing here can land against 0.8.0, and
  that is still true of the tree this is written in: the work is done and validated
  against a `[patch.crates-io]` path override, and it is not committed until heklang 0.9
  is published and the requirement moves with it. Removing the override without bumping
  the requirement resolves the released 0.8.0, which compiles and wedges, so the two
  edits are one edit.
- A fixture sealing a record, a list and a map, in `tests/fixtures/tickets`, which already seals a
  `Money(2)` and a `String?`.
- Coverage through the whole path: seal on append, store, read back, `reveal` in an effect, and a
  projector column that receives the sealed value and is decrypted by `read_api::decrypt_row`.
- The `Json`-that-is-a-string case, through a record containing a string field that looks like a
  number, which is the trap `seal_text`'s comment names and the one heklang's writer was falling
  into.
- A shredded key: a sealed record must come back absent from a row and terminal in an effect, never
  a partially parsed object.

**It turned out not to be coverage only.** Writing the fixture found a fault in `RowWriter::row`, and
it was hekla's rather than heklang's. A scoped column is a sealed one, so `Value::from_json` was
reading the decrypted plaintext back against a declaration with the seal still on it, and a sealed
position takes text and nothing else. Every kind whose stored form is not text failed there: an
`Int`, a `Bool`, a `Timestamp` and all three composites. Nothing caught it because the only sealed
columns anything projected were a `String` and a `Money`, and both of those read back as text anyway.
`row` now reads a decrypted column against `field.ty.unsealed()`, which is the declaration the column
would have had were the field not personal. The fixture grew a sealed `Timestamp` beside the three
composites to cover the rest of that class, and it earns a second keep of its own: it is the one
sealed kind whose column form differs from its payload form.

**Flattening it to text is the other way to satisfy that position, and it loses.** `put` cannot then
tell a document read back out of a column from a string a handler wrote, so an `update` touching one
column of a row carrying a sealed record re-seals that record quoted as a string: still readable,
still decryptable, and a different value. Reading it back at its declared shape has no such ambiguity,
and it is also what makes `expect Ticket[k] { reporter: filed_by() }` compare a record against a
record rather than against a document's text.

Decisions worth keeping:

- **The byte-stability test belongs on the projector column path**, not the append path. A moved seal
  in `stored_seal` is decrypted to text and re-encrypted as that same text, so a re-seal under a
  different field name is byte-stable by construction with no JSON round trip at all. The path that
  does parse and restringify a composite is the read-model column write, where a moved seal is
  opened, `unsealed_json` parses, `column_form` runs and `seal_text` restringifies.
- **A corrupt seal is a wedge, and that is the right answer.** `Json::parse` returning `None` raises
  a `Mismatch`, and any `Err` from `interpreter.deliver` is `terminal: false`, so the lane retries
  rather than skipping. A declaration that no longer matches stored data needs a person, unlike an
  erased subject, which is terminal by design. It is a new error path the arm creates, so it is
  asserted rather than left to be discovered in production.
- **The admin surface still shows the bad bytes.** `unsealed_json` falls back to the raw text where
  `from_sealed` hard-errors, so a corrupt composite renders as a string on `/admin/events/{id}` while
  the effect wedges. The asymmetry is deliberate and matches
  `a_json_column_that_does_not_parse_reads_back_as_its_raw_text`: the operator diagnosing the wedge
  is the one who needs to see them.
- **A nested timestamp stays micros in both seals.** `column_form` rewrites a top-level `Timestamp`
  only, so a timestamp inside a record is epoch microseconds in a payload seal and in a column seal
  alike. Consistent, and consistent for a reason nothing tested.
- **The byte-stability test is stated as one seal against the other**, not against a literal. A
  subject key is deterministic, so identical ciphertext is identical plaintext, and the payload seal
  and the column seal of one field are made by two different routes: one renders the request body,
  the other parses that rendering and renders it again. Comparing the two ciphertexts is the whole
  property in one assertion, and it is the only assertion that would fail: both halves still decrypt
  to a readable document, so everything about what a reader sees would go on passing. `due_at` is
  why it is stated per column rather than over every sealed one, since a sealed `Timestamp` column
  would hold RFC 3339 against the payload's micros on purpose.
- **A decrypted column is read as stored history, not as a body.** The first fix reached for
  `Value::from_json`, which is `Origin::Body` and so ignores `@absent` by design: a caller who
  omits a field must be told, not handed a default. A row is the other case. heklang states that
  the distinction holds through a seal, and `reveal` already keeps the promise on the payload copy
  of the same content through `Value::from_sealed`; keeping it on one copy and not the other is
  worse than not keeping it at all, because an author adding a field to a record would see every
  effect go on working and every row written before today fail. So the column goes through
  `from_sealed` too, which is the reading half of what `seal_text` wrote and cannot drift from it.
  Nothing triggers a rebuild to paper over it either: a projector's digest names a referenced
  record by name, so growing that record does not move the hash.
- **The regression test is an `update`, not a `put`.** `row` is only read by `update`, `patch` and a
  `.hk` `expect`, so a fixture that only `put`s never takes the path at all. The assertion sits on the
  retitle in `every_write_statement_reaches_the_read_model`, which reads the row back and puts it
  whole, so every sealed column on it makes the round trip whether or not the statement names it.
  A property test cannot stand in: the fault was in which type `row` hands `from_json`, and
  `scoped(ty).unsealed()` is `ty` by construction, so no property over those functions can see it.
- **The shadow world reads a seal back too.** `tests/support/shadow.rs` rendered a sealed column as
  the text it stores, which agreed with hekla only because every sealed column in the fixtures was
  text-shaped. It now reads the content back against the declaration through `Value::from_sealed`,
  which is heklang's half of the table `read_api::typed_from_string` is hekla's, so the model test
  keeps comparing two implementations rather than a copy.

Honest scope:

- **`@subject` on a field inside a `record` declaration stays refused in heklang**, and the refusal
  is correct: the annotation names a sibling field holding the id, and a record reached through a
  container has no sibling the parser can name. What is allowed is `@subject` on an event field whose
  type happens to be a record. Both arrive here as `FieldKind::Json` and hekla distinguishes nothing.
- **`Int.pad` and `Timestamp.add_seconds/minutes/hours/days` ride along in the same bump and cost
  nothing.** heklang's IR does not move, so no digest moves, no `signature_hash` moves, no
  `script_hash` is invalidated and no projector rebuilds. The console's editor highlights keywords
  only and its list still matches `lex.rs` exactly, so there is nothing to sync there either. The new
  `ErrorKind::PadWidth` is additive and hekla never matches `ErrorKind` exhaustively, so the whole
  bump is source-compatible.
- **`support::seed_event` cannot seed a sealed composite, and that is not a fault to fix here.** It
  reads an event from JSON, and a sealed field is read at its stored shape, which is text; a document
  passed as text is then sealed re-quoted. Seeding is a test facility and no production path builds
  an event this way, so the tests that need a hand-made seal go through `seed_event_value`
  instead, which takes the `Event` already built. It is also the only way to have a seal written
  under a declaration that has since moved, since the plaintext is behind a key and no rewrite is
  available: that is what the `@absent` test needs, and what the corrupt seal is.
- **An `expect` on a sealed column compares against what `row` read back**, which is now the declared
  value, so a record, a list and a map can each be named in one. A sealed `Timestamp` still cannot:
  a written moment is a string, and heklang's literal reader looks through a plain `Timestamp` column
  and not through a sealed one. That column is asserted in `tests/tickets.rs` instead, and the
  omission is written down where it sits.

## Phase 37: a scan filters on what was declared, not on its first column (done)

heklang has declared compound indexes for a while and hekla already creates them: `create_index_sql`
emits `CREATE INDEX ... ON t (a, b)` with every column in declared order. The index is in SQLite. It
is simply not reachable from the query surface: `read_api::is_filterable` admits the primary key and
the leftmost column of each declared index and nothing else, `read_model::scan` takes one
`(column, value)` pair, and the scan handler refuses a second filter outright.

What that costs an application is workaround columns. Six entities in one port grew a composite
string column whose only job was to be filterable (`shop_state`, `shop_open`, `shop_period` and
three more), each a stringly-typed key that has to be built byte-identically at every write site, and
each part of the projector's definition hash, so one mistake is a rebuild.

The safety property survives unchanged: **you may only ask what you declared, never a table scan.**

- ANDed equality across any prefix of a declared index's columns. `index (shop_id, status, month)`
  admits `shop_id`, `(shop_id, status)` and `(shop_id, status, month)`, and refuses
  `(status, month)`.
- A range on the column after the equality prefix, which is the standard index-prefix shape:
  equality on columns 1 to n-1, a range on column n.
- The index chosen explicitly in Rust from the declared set and pinned with `INDEXED BY`, so the
  planner cannot quietly choose a scan, with query-plan tests in the mould of
  `the_invocation_join_reads_the_primary_key_rather_than_scanning`. Both of them fail with
  `USE TEMP B-TREE FOR ORDER BY` when the key append is reverted, which is what makes them worth
  having rather than restatements of the code.

Two things turned up in the writing that the plan above does not say, and both changed what shipped.

**Pinning an index is a regression until the key is appended to it.** A scan orders by the key, and a
declared index ends at its declared columns, so `INDEXED BY` leaves SQLite sorting the whole match set
into key order on *every page* of a paginated scan. The `USE TEMP B-TREE FOR ORDER BY` that appears in
the plan is not a detail: it is per page, so it is worse under a cursor than without one. Appending
the key to every generated index removes it, which is why that half of Phase 38 moved here rather than
landing after the thing it pays for.

**Appending it only helps a filter that uses the whole index.** `index (a, b)` generates `(a, b, key)`,
so filtering `a` alone still leaves `b` between the filter and the key and still sorts. The answer is
in the chooser rather than the DDL: among the indexes that serve a filter, take the **narrowest**, so
an entity declaring both `index (a)` and `index (a, b)` gets the seek for either question. A prefix
shorter than every index serving it still sorts, and declaring the narrower index is the author's
lever.

Decisions worth keeping:

- **A range operator is a dotted suffix, so it adds no reserved query param.** `?month.gte=` and
  `?month.lt=` cannot collide with a declared field, because `is_sql_identifier` restricts a name to
  ascii letters, digits and underscores. That is what keeps `EntityDef::validate`'s reserved-param
  gate exactly as strict as it is, and it is why the range half of this phase carries no
  compatibility risk at all.
- **A range needs an order that means what the declaration says, which is a narrower question than a
  cursor's.** `EntityDef::validate` already decided it once for keys, so it became two predicates
  sharing a core: `FieldKind::is_keyable` (a stable total order, all a cursor needs, since it only has
  to walk every row once) and `is_comparable` (that order is the declared one). They differ on exactly
  one kind. An enum stores its variant's spelling, so it keys a cursor perfectly well and would answer
  `priority.gte=Low` by walking the alphabet rather than the severity; `Money` (a decimal string, so
  `"2" > "10"`), `Bool`, `Json` and optionals fail both. The OpenAPI generator emits range parameters
  only where `is_comparable` holds, which is also what stops the document growing four parameters per
  column for columns that could never take one.
- **The reconcile is `pragma_index_info`, not a schema version and not `sqlite_master.sql`.** A
  generated-DDL change is invisible to every rebuild trigger there is: `reconcile_from` compares
  heklang's digest, and the digest covers the columns an author declared rather than the key hekla
  appends to them. So `CREATE INDEX IF NOT EXISTS` would match the old index by name and adopt its
  shape forever. Comparing the live column list against the wanted one, and dropping on a mismatch,
  needs no version to bump and no text to match, and it reconciles an index written by any older hekla
  rather than only the one shape this release changed.
- **`filterable_fields` stays the one source.** The comment on that gate was explicit that it is the
  only thing stopping the OpenAPI generator from emitting a duplicate query parameter, and named this
  widening as what would expose it. The gate still derives from the same function.
- **Deduplication stopped being a nicety.** Every index now contributes every one of its columns, so
  a name repeating is ordinary rather than unusual. `scan_params` dedups and `introspect::self_entity`
  sorts and dedups; both now depend on it, and a test asserts no parameter is named twice.
- **The cursor did not move.** Ordering stays the primary key, so the cursor stays the plaintext key
  computed before decryption, and the property that decrypting a page cannot affect pagination is
  untouched.

Honest scope:

- **Widening filterability widened what `validate()` rejects at load.** An entity carrying a column
  named `limit`, `cursor`, `after` or `timeout_ms` anywhere in an index stops loading where it used
  to load if that column was not previously leftmost. That is the gate doing its job, and it is a
  clear error at boot rather than a silent shadowing at request time, but it is a real break and
  belongs in the release note.
- **A partial prefix still sorts.** Covered above; there is no way around it short of generating an
  index per prefix, and the cost is bounded by the match set rather than the table.
- **Ordering by anything but the key was Phase 38**, which landed straight after as purely `order_by`
  and the tuple cursor, the DDL it needed having come forward to here.
- **The console's filter UI still takes one field**, so it offers the key and each index's leading
  column, derived client-side from the `indexes` introspection already reports rather than from the
  widened `filterable`. Using the wider list would have enabled options that 400 on their own and lost
  the property that the console makes `unindexed_filter` visible before you can hit it.
- **No example declares a compound index.** `examples/orders` has only two plaintext columns (the key
  and `customer_id`; the rest receive sealed content, which `validate` refuses to index), so
  demonstrating a prefix there means adding a column to an example whose subject is erasure. The
  coverage is in `tests/fixtures/tickets`, which declares `index (org_id, priority, due_at)` and
  reaches every case including both refused ranges.

## Phase 38: an ordering is a declared index, and the cursor is its tuple (done)

After Phase 37 an entity can be filtered on what it declared and is still ordered only by its key. An
edit log that wants order plus status plus month, newest first, is three filters and a sort, and the
sort is the half still missing, so an application punts to paging and sorting in its own process.

- `order_by` naming a declared index, with the whole tuple reversed or none of it.
- The cursor over the index tuple plus the primary key as a final tiebreak, resumed with a row-value
  comparison the index serves.
- Nothing in the DDL: Phase 37 already appends the primary key to every generated index and
  reconciles an existing read model onto the new shape, because pinning an index needed it first.

Decisions worth keeping:

- **The direction rides in the value, so this costs one reserved name rather than two.**
  `?order_by=-by_shop_month` reverses, and `order_by` joins `RESERVED_QUERY_PARAMS` under the same
  load-time gate as the rest.
- **A tuple cursor needs the key as its last term.** The key is unique, so today's cursor can neither
  skip nor repeat a row. An index tuple is not unique, and without the tiebreak pagination silently
  drops rows at a page boundary where two rows share an index value.
- **One direction for the whole tuple, because a row-value comparison has one.**
  `(a, b, key) > (?, ?, ?)` is the shape the index serves, and mixed per-column directions are not
  expressible in it. An index is read as declared or reversed as a whole.
- **An index containing an optional column cannot back an ordering**, and nor can one containing a
  `Money` or enum column, which is `FieldKind::is_comparable` again: the predicate Phase 37 extracted
  for ranges turned out to be the same one an ordering needs, for the same reason. A row-value
  comparison against NULL yields NULL, so the row is excluded and pagination loses it at a page
  boundary; a decimal string sorts `"2"` above `"10"`; an enum sorts by its variant's spelling. Each
  index stays filterable and simply cannot sort. Request-time rather than load-time, because it
  depends on which index was asked for, and an index that cannot sort is still worth declaring.
- **The key is nameable as an ordering, so the default can be reversed.** `order_by` was specified as
  naming a declared index, and "newest first by key" then had no spelling at all while being the
  commonest thing to ask for. The key column's own name is accepted (`?order_by=-order_id`), which
  costs nothing: the primary key is an index, and the tuple cursor over it is the bare key the cursor
  used to be. An index is matched before the key, and they can only collide on an entity whose key is
  literally named `by_<something>`.
- **`order_by`'s OpenAPI parameter is an enumeration, and it leaves out what cannot sort.** The set is
  small, closed and knowable from the declaration, and it is generated from `ordering_problem`, which
  is what the runtime refuses on. Documenting an index and then rejecting it would be worse than not
  documenting it.
- **A cursor carries which ordering it was taken over, and a mismatch is a 400.** Today a cursor is a
  bare key and carrying one across queries is nearly harmless. A tuple cursor read under a different
  ordering is a correctness hazard, so it refuses rather than paginating wrongly.
- **An index over a subject-encrypted column still cannot back an ordering**, and nothing relaxes to
  allow it. `EntityDef::validate` already refuses such an index, which is what keeps every ordering
  column plaintext and keeps the cursor computable before decryption.
- **`ORDER BY` and the filter agree on one index by construction**, because the index is chosen in
  Rust and pinned. Letting the planner choose would reintroduce in memory the sort this API exists to
  refuse.

Honest scope:

- **Appending the key to a generated index needed a reconcile rather than a rebuild**, and that
  shipped with Phase 37: a projector's definition hash is heklang's digest and does not cover hekla's
  DDL choices, so `CREATE INDEX IF NOT EXISTS` is a no-op against an existing model and the old shape
  would survive forever. `ReadModel::open` compares `pragma_index_info` against the wanted columns and
  recreates on mismatch. Nothing here has to touch it again.
- **Nothing is asked of heklang, by either this phase or Phase 37.** `EntityDef.indexes` already
  carries every column in declared order and the digest already hashes them, so there is no language
  change and no version bump for the pair. Phase 37 did edit one sentence of
  `heklang/docs/projectors.md`, which said the runtime could filter only on an index's leftmost
  column; a doc correction in the sibling repo, not a language change.
- **`?order_by=` names the generated index name**, which is derived (`by_org_id_priority_due_at`)
  rather than authored, because heklang holds that index naming is a storage concern. So this is where
  a derived name became part of hekla's public request surface, which means renaming a column renames
  an ordering. Decided deliberately rather than inherited: the alternative is letting an author name
  an index, which is a heklang change and reopens a question it already answered. Both
  `/admin/projectors/{Name}` and the `order_by` enum in `/openapi.json` list the names, so nobody has
  to derive one by hand.
- **Every cursor issued before this is refused**, because the old one was the bare key base64'd and
  that is not the JSON a cursor now is. It fails as "not a cursor this server issued" rather than
  being read as a one-element tuple and compared against the wrong column, which is the failure worth
  paying a refusal for. Cursors are ephemeral, so the blast radius is a client holding one across the
  deploy, and its next page is a 400 rather than wrong rows.
- **A cursor's width is checked twice**, once in the handler with a message and once in
  `read_model::usable_cursor` without one, which drops a mismatched tuple and starts the page over.
  The second exists so that the statement and its binds cannot disagree: a row-value clause emitted
  with the wrong number of placeholders is a SQL error rather than a wrong answer, but it is one only
  the second check makes impossible to reach.
- **A partial prefix under an explicit ordering still sorts nothing extra**, but it does give up the
  narrowest-index rule from Phase 37: the ordering pins the index outright, so a filter on `a` under
  `?order_by=by_a_b` reads `(a, b, key)` rather than the narrower `(a, key)`. That is correct rather
  than unfortunate, since the caller asked for the rows in `b` order, and it is worth knowing when
  reading a plan.
- **The console still neither sends nor offers `order_by`.** It says "in `<key>` order" and that stays
  true of what it asks for. An order control is the obvious next console change and nothing here
  blocks it.

What review changed, after both phases were written:

- **A cursor's ordering token and its width move independently.** An index already containing the key
  is not widened by it, so moving `@key` onto another column turns `index (a, k)` from a two-column
  ordering into a three-column one while `by_a_k` still spells both. The token check passed, the
  model dropped the tuple, and every page came back as page one with the same `next_cursor`: a client
  following cursors in a circle with nothing reporting a problem. The handler now compares the width
  too. The same class is why `EntityDef::validate` refuses an index whose generated name is also the
  key column, rather than leaving `parse_order`'s index-before-key preference to adjudicate it.
- **A range over an optional column was refused, and should not have been.** `is_comparable` was
  read off the un-`base()`d kind, so `Optional(_)` matched none of its arms and every optional
  indexed column lost its `.gte`/`.lt` bounds, in the runtime and in the generated document alike.
  A range over a nullable column is well defined: `>=` does not match NULL, which is the answer
  someone asking for one wants. Only the *ordering* breaks, and `ordering_problem` refuses that
  separately, which is what made the bug invisible.
- **Two query-plan tests exercised no cursor**, despite comments insisting the cursor was the point:
  they passed a tuple as wide as the index against a key ordering, and `usable_cursor` silently
  dropped it. The tests still caught what they were written for, so nothing was wrong in the code
  they guarded; what they did not guard was the cursor's interaction with `INDEXED BY`. Their helper
  now asserts the width, and they assert the emitted statement as well as the plan. Same lesson as
  Phase 36: a test that would pass against the reverted fix is not the test it claims to be.
- **The index reconcile ran in two autocommits**, and pinning made that expensive. `ReadModel::open`
  runs at runtime, not only at boot, so a read landing between the `DROP INDEX` and the `CREATE`
  would fail on `INDEXED BY` with "no such index" rather than merely running slowly. It is one
  transaction now.
- **`cursor_at` returning `None` was reported as `next_cursor: null`**, which tells a caller the scan
  is complete. It is only reached once an over-fetched row has proved another page exists, so that
  was a silent truncation, and the one failure a paginating reader cannot detect. It errors.

Two findings from the same review are deliberately not acted on:

- **A sealed `Json` column whose plaintext does not parse wedges the projector while the read API
  serves the raw text.** The asymmetry is real, and the wedge is Phase 36's documented rule: a seal
  whose text is not the document its declaration promises wedges and retries, because the plaintext
  is intact behind a live key and one edit to one `.hk` line fixes it. Making the projector tolerant
  here would contradict that, so the right fix if it ever bites is to reconsider the rule, not this
  call site.
- **Two entities can generate the same index name.** `X_by_a` with `index (b)` and `X` with
  `index (a, by, b)` both produce `X_by_a_by_b`, and the reconcile would then drop and recreate one
  entity's index on the other's table on every open. It needs a column literally named `by`, and
  closing it needs a cross-entity validation pass that does not exist (`validate` is per-entity). The
  failure got louder with `INDEXED BY` (a 500 rather than an unindexed scan), which is the argument
  for a name check whenever something else needs that pass.

## Phase 39: a subject can be deleted with its tenant (done)

`shop/redact` is one erase of the shop plus one erase per customer that shop ever produced. `erase`
is journaled per call by design, so the row count is not something heklang can batch away: a
50,000-customer shop writes 50,000 journal rows in one invocation, and the application has to carry a
projector whose only purpose is to enumerate subjects for deletion. That is the mandatory-compliance
path, with a legal deadline on it.

Letting a subject declare a parent is what removes it. A customer's key is wrapped under its shop's
key, so deleting the shop's row makes every customer key beneath it unwrappable in one delete. It is
ordinary key-hierarchy cryptography, and "delete this tenant" is the shape of the multi-tenant
deployments hekla is most likely to land in.

**This is a decision before it is a task, and the sequence is hekla commits, then heklang declares,
then hekla implements.** A declaration the runtime does not act on is worse than no declaration,
because it reads as a guarantee.

### The decision: a subject becomes a declared type

This phase budgeted heklang "somewhere to say a subject has a parent". That is not buildable as
written, because **there is no subject to hang a parent on**. A subject today is an anonymous
namespace conjured by writing a field name, and the field name *is* the identity:

- `subject_field` (`heklang/src/parse.rs:9144`) resolves a subject by scanning every event in the
  project and taking the first that has a field of that name. So `owner_id` on two unrelated events
  is one key row and one erasure, and nothing says so.
- Spell it `customer_id` in one event and `cust_id` in another and they are two different people,
  erased independently. Nothing catches that either.
- Nothing checks that two events spelling one subject agree on its type. The first found wins.
- `erase(customer_id, some_other_int)` compiles, which `heklang/docs/effects.md` already records as
  accepted: "erasing the wrong namespace destroys the wrong subject's key."

A key hierarchy multiplies every one of those by the size of a tenant, so the namespace is fixed
first and the parent rides on the fix. **A subject becomes a declared type whose values are its
ids**, and the parent is a property of that type:

```hek
subject Shop(Int)
subject Customer(Int) under Shop
```

```hek
event @order.placed {
  order_id: Uuid,
  buyer: Customer,
  shop: Shop,
  email: String? @subject(buyer) @max(200),
  order_total: Money(2) @subject(shop),
}
```

Field names stop carrying identity: call it `buyer`, `cust` or `who` and it is still a `Customer`.
The key row is filed under the subject's declared name, which is an identifier carrying a digest
entry, so renaming one is a visible change rather than a silent re-partition of everybody's keys.

**Three things close together**, which is why the type is worth more than the parent alone:

- `erase(Customer, x)` accepts only a `Customer`, so the wrong-namespace hole becomes a type error.
  That earns the type kind on its own: this feature's whole job is irreversible destruction, and the
  thing selecting the target was an untyped string that doubled as a field name.
- `from: Customer, to: Customer` on one event becomes expressible. Today both fields would have to be
  literally named `customer_id`, so a transfer between two people of one kind cannot be written at
  all.
- "A `Customer` belongs to a `Shop`" is a statement about customers in general rather than about one
  event or one field, so it is stated once on the declaration with nowhere to write a contradicting
  copy.

Decisions worth keeping:

- **Nominal inside, positional at the edge.** `{"buyer": 7}` reads as a `Customer` because the
  declaration says that position is one. JSON carries no type tag, so nothing can verify that 7 is a
  customer id rather than a shop id, and a caller that swaps two ids is not caught and cannot be.
  What is caught is every mix-up past the boundary, `erase(Customer, ev.shop)` among them, which is
  where the damage lives. Written down because "subjects are types" otherwise reads later as "the API
  validates them".
- **`@subject(...)` keeps naming a field, not the type.** The type is reachable from the field, and
  naming the field is what keeps `from: Customer, to: Customer` disambiguable. The reference becomes
  local to the declaration, like an `index (a, b)` column list, rather than a key into a global
  namespace: that locality is the fix, not the annotation's spelling.
- **One parent per subject, forever.** A `Customer` under a `Shop` cannot also sit under a
  `Marketplace` in another flow. One hierarchy means one place a delete starts from, and a subject
  reachable by two routes would have two answers to "is this erased".
- **A subject id is an id and nothing else.** Equality within one subject type, yes. Use as an entity
  key, in an index and in a range, yes, reading through to the underlying scalar as Phases 37 and 38
  already do. Arithmetic, no. Mixing a `Customer` with a `Shop`, no, which is the point.
- **The subject graph is over type names, so it is finite and statically acyclic.** Depth is bounded
  by the number of declared subjects, which is what makes a recursive unwrap safe without a magic
  cap, and cycle detection a check on one small graph at load rather than a runtime guard.

### hekla's half

The write path already has what it needs. `lower` (`src/heklang_host.rs:1255`) builds an `ids` map of
every scalar field's plaintext in a complete pass before it seals anything, so the parent id is in
hand at the one moment a child key is minted. `stored_seal` is the only mint site on the append path,
and `RowWriter` goes through `encrypt_subject_existing` and never mints, so the projector write path
is unchanged.

- **`subject_key` grows a parent, and a row is wrapped under exactly one thing.** Schema v10 rebuilds
  the table with a nullable `master_key_id` beside `parent_field`/`parent_value` and a
  `CHECK ((master_key_id IS NULL) <> (parent_field IS NULL))`, so "a root or a child, never both and
  never neither" is a database rule rather than a Rust convention. A rebuild rather than two
  `ADD COLUMN`s, because SQLite cannot relax a `NOT NULL` in place and the check is worth the rebuild.
- **The wrapping key is derived, not reused.** `wrap_key` takes a 32-byte AES-GCM key and a subject
  secret is 64 bytes of AES-SIV, so a child wraps under a domain-separated SHA-256 of its parent's
  secret. SHA-256 rather than HKDF because the input is already uniformly random, which is the case
  HKDF-Extract exists to avoid needing. The child's own secret stays a fresh random 64 bytes, so **no
  ciphertext moves**: acquiring a parent is a rewrap, never a re-encrypt.
- **The chain walk falls out rather than being built.** `load_secret` recursing into its parent
  answers everything: an ancestor row that is gone means no secret, which is `Ok(None)`, which every
  one of the nine `None` consumers already treats as unreadable. "Presence checks that walk the
  chain" overstated the work. `KeyStore::erased` is dead code with zero callers, and `verify.rs:301`
  records why it was abandoned (it answers "does a key row exist" when the question is "do these
  bytes decrypt"), so it is deleted rather than taught to walk.
- **The two absences stay apart.** A missing ancestor row is permanent and reads as `Ok(None)`; a
  missing master is a misconfiguration and stays `Err`. That split already exists and generalises
  unchanged, and it is exactly what makes replacing an unwrappable child row safe, because only the
  first case can trigger the replacement.
- **Rotation gets simpler, not harder.** Only roots are wrapped under a master, so `all_subject_keys`
  filters on `master_key_id IS NOT NULL`. Children need no rewrap at all: their wrapping key derives
  from the parent's secret and a rotation leaves every secret untouched. This was called a walk over
  roots; it is one `WHERE` clause.
- **The sweeper gains a step.** `run_sweep` (`src/effect.rs:2101`) grows a bounded delete of child
  rows whose parent row is gone, in the `SWEEP_CHUNK` and `SWEEP_CHUNK_PAUSE` discipline the journal
  sweep already follows. A grandchild becomes an orphan only once its parent is swept, so the step
  repeats until a pass deletes nothing, bounded by the depth of the subject graph.
- **`hekla erase` grows the summary and the prompt `rewind` has.** Its no-prompt rationale is written
  down in two places (`src/cli.rs:504` and `reference/cli.md:474`) as "an erase carries its blast
  radius in its own arguments, because you named the subject". A cascading erase makes that false,
  and the asymmetry that justified the inconsistency goes with it. The summary can count descendants
  exactly from a recursive CTE over the parent columns, with no master key, which is what keeps the
  CLI master-free.
- **hekla checks the declaration at load, not at write.** "Every event carrying a child-scoped field
  also carries the parent id" was recorded as a check hekla cannot make. It can: it holds every
  `EventDef`. heklang's is the better diagnostic because it points at a source span, and hekla's is
  the one that refuses a deployment rather than a request. Both, for the same reason `check` and boot
  both refuse a sealed column that cannot be absent.

Honest scope:

- **O(1) deletes and O(1) journal rows, not O(1) storage.** Child rows survive unreadable until the
  sweep reclaims them. That is the growth cost, and the 50,000-row figure this is justified against
  is the journal rather than the disk.
- **Declaring a parent gives up the independence the current design advertises.** `ARCHITECTURE.md`
  and `reference/encryption.md` both sell per-field subjects on exactly this: erasing a shop provably
  cannot touch a customer's `email`. Under a parent it does, deliberately, and there is no way to have
  both for one pair of subjects. The docs have to say so beside the feature rather than keep the old
  claim two sections away.
- **A key row written before this does not follow its subject's rename.** Schema v10 carries every
  `subject_key` row across, but the namespace it is filed under changed meaning: it was the *field*
  the annotation named and is now the declared type's name, and nothing in the migration knows the
  mapping between the two. A carried row is reachable only where a project happened to name its
  subject exactly what the field was called. The symptom is the bad one, every pre-existing sealed
  value reading back absent and so indistinguishable from an erasure, which is why the migration
  logs a warning naming the count rather than carrying them silently. Nothing is deployed against
  this tree, so nothing more was built; a local data directory from before the change is the case
  that meets it.
- **A parent declared onto a project that is already running does not adopt the rows already there.**
  Those were wrapped under the master, so deleting the tenant misses them silently, which is the one
  failure a compliance feature cannot have. Left open here and closed in Phase 40, which does it at
  boot off a fold of the log rather than on write: the rows that need it are the dormant ones, and a
  write-triggered repair reaches exactly the subjects an erasure request is least likely to name.
- **`key_present` reads through the chain**, so introspection's `erased`-versus-`stale` split keeps
  meaning what it means, and `GET /admin/subjects/{field}/{value}` answering `absent` covers a row
  that exists but whose parent is gone. Both surfaces answer "unreadable", which is the guarantee,
  rather than "the row is there", which is not.
- **Rule 9's erase-then-reveal analysis in heklang is subject-blind**, so the wider blast radius costs
  nothing there.
- **Three stale things this phase passes and should fix on the way.** `KeyStore::erased` is dead
  (above); `schema.rs:405` names a `validate_subject_refs` that exists in neither repo; and
  `encrypt_global` has no production caller, so `_hekla_global` survives only as an exclusion filter
  and an erase refusal.


### What implementing it changed

Four things the design above did not have right, found by building it.

- **The two names had to come apart in hekla too, and one expression hid it.** `FieldDef::subject`
  on an *event* still holds the field name; `EntityField::subject` on an *entity* now holds the
  subject's name, because `propagate_subject` reads it off the type. hekla copied both into one
  `FieldMeta.subject` with the same line, so nothing failed to compile and instead
  `read_api::decrypt_row` looked up a column named `Customer`, found nothing, and dropped every
  sealed column as though erased. `FieldMeta` carries a `Seal` now, and the accessor is a method, so
  every old field access was a compile error rather than a silent one. That was luck rather than
  design, and it is the reason the adaptation was safe to do quickly.
- **"Presence checks that walk the chain" was not work.** `load_secret` recursing into its parent
  answers everything: a missing ancestor means no secret, which is `Ok(None)`, which all nine `None`
  consumers already treat as unreadable. `KeyStore::erased` turned out to be dead code with no
  callers at all (`verify.rs:301` records why it was abandoned), so it was deleted rather than taught
  to walk.
- **"A rotation that walks roots" is one `WHERE` clause, and the hierarchy makes rotation cheaper.**
  A child's wrapping key derives from its parent's *secret*, which a rotation never changes, so a
  child needs no rewrap at all. A tenant with 50,000 customers rotates one row.
- **"The one check hekla cannot make" is one hekla should make, and does.** hekla holds every
  `EventDef`, so it refuses at load an event that seals under a child without carrying its ancestor.
  heklang keeps its own copy because it can point at the annotation's span, which is what an author
  has to change; heklang's own comment says it keeps that copy *because* hekla would have one.

The open gap was written above and is closed by Phase 40: **a parent declared onto a project that
already has key rows does not adopt them.** The shape recorded here, a rewrap on write, turned out to
be the wrong half of it.


### What the hardening pass changed

Four correctness bugs, each found by writing the test before trusting the reasoning.

- **A child whose parent was erased and then written to again reported an error, not an
  absence.** The parent's new secret derives a different wrapping key, so the child's own
  key no longer opens: that failed at the *key* layer where a root's equivalent fails at
  the *data* layer, and the two have different error contracts. It meant a `500` on every
  read of a legitimately shredded row instead of an omitted column, and a write path wedged
  for that subject for good, since `get_or_create_secret_in` replaces a row that reads
  absent and propagates one that errors. Each row now records which **generation** of its
  parent it was wrapped under, so a superseded parent reads as a shred while a wrapping that
  fails against the live generation reads as tampering. Collapsing the two either way is
  wrong for the other.
- **The replacement compare-and-set matched the parent, which is identical for the stale row
  and the row replacing it.** Two writers racing to replace one orphan each matched the
  *other's* fresh row and deleted a key already sealed under, with no error anywhere. It
  matches the wrapped bytes now, which are unique per mint.
- **An erase landing between a writer's insert and its read-back failed the write.** A
  child's wrapping can only be opened through its parent's row, which is under a different
  lock, so the atomicity a root enjoys is not available. The mint retries, bounded, and the
  write lands on a freshly minted parent: the documented "a write after an erase gets a
  fresh key" rule, applied to a write that merely *finished* after one. Committing under the
  doomed key instead was the other option and is worse, because the command reports success
  over content that is already unreadable.
- **A cycle in the parent pointers hung the process.** Only a hand-edited store can produce
  one, and the reachability walk was an unbounded `UNION ALL` that never terminated: a hang
  is the worst of the three possible answers because nothing reports it. It deduplicates
  now, and the recursive unwrap has a depth cap.

Two smaller ones: the per-request decrypt cache did not cover the ancestors the new walk
visits, so a page of one tenant's members re-read and re-unwrapped the tenant key once per
row, defeating the only reason that cache exists; and the sweep was gated on whether the
project *currently* declares a parent, which would have stranded the rows of one that used
to.

Every fix above is pinned by a test that fails when the fix is reverted, which is the only
evidence worth having, and one of those checks silently passed at first because the revert
did not apply.

### Sequencing, as it actually went

This planned for heklang to cut 0.9 with the sealed-composite fix first and do subjects as types
afterwards, so Phase 36 would not wait behind a larger design. It did not go that way: both changes
sit in one unreleased set, so 0.9 carries the fix **and** `3f79a60`, and Phase 36 and Phase 39
unblock together. Nothing was lost by that, because hekla could not build against the new IR either
way; what it costs is that the two phases land on one heklang version rather than two, so a bisect
across the bump crosses both.

hekla's half is two commits. The first adapts to the new model and changes no behaviour: the key
namespace becomes the subject's declared name, `FieldMeta` carries the subject and the id field
apart, and every fixture is rewritten. The second is the feature this phase is named for, and lands
on top.


## Phase 40: a subject's key is where the declaration says it is (done)

Phase 39 left one gap: **a parent declared onto a project that already has key rows does not adopt
them.** A row's wrapping is chosen when it is first minted and never revisited, so adding
`under Shop` to a live `Customer` leaves every existing row under the master. Customers minted after
the edit hang from their shop; the ones from before do not, and erasing the shop reports success
having missed them.

It is the worst shape a failure in this area can take. Nothing looks wrong: reads work, `verify` is
clean, no error anywhere, and the only symptom is a deletion that quietly did less than it claimed,
discovered at the moment you are least able to check. It is also not a compatibility problem that
ages out; it bites current hekla on current data and would still be there at 1.0.

### What made it solvable

The first read said the parent was unrecoverable, because events written before the declaration
cannot carry a field that did not exist. That is wrong, and three refusals already in the tree are
why:

1. heklang's `check_ancestry` forces the ancestor's id onto every event that seals under a child, as
   a required, plaintext field. Not optional and not itself sealed.
2. hekla's `unanswered_history` refuses a boot that adds a required field to an event type with
   stored instances, unless the declaration answers for the payloads already written.
3. `value::stored_field` materialises that `@absent` value on read, so a fold sees it.

So declaring `under` onto a log is only *possible* after the author has written
`shop_id: Shop @absent(1)`, which **is** the statement of which shop the old events belong to. The
fold reads parentage rather than guessing it, and there is no unresolvable tail. None of the three
was built with this in mind; they compose into a guarantee none of them was aiming at.

The tie-break needed no invention either. `encryption.md` already says a key is minted once, "under
whichever arrived first", so adoption takes the parent from the earliest event that seals under the
subject and replays the decision the write path would have made.

### What the plan had wrong

**Rewrap-on-write, the shape Phase 39 recorded, is the wrong half.** It adopts a subject the next
time something writes to it, which covers active subjects and misses dormant ones. The people who
ask you to delete their data are almost always the ones who left and will never generate another
event, so a write-triggered repair reaches precisely the subjects an erasure request will not name.
The work belongs at boot, ahead of traffic, off a fold.

**A rewrap, never a re-mint.** The secret is what the content is encrypted with, so only the
container changes. The test that pins this compares ciphertext written before the move against a
read after it, and a mutation that mints a fresh secret fails that test and no other.

**One direction only.** A row already under a parent is left alone whichever way the declaration
moved. Re-parenting on sight would fight the "whichever arrived first" rule on every write, and
un-parenting would narrow a shred somebody may rely on. Widening the blast radius is the only change
safe to make unasked.

### The bug it turned up in Phase 39

A stress test added here failed about one run in three, but only under load and in a *different*
test: two writers racing to replace one unreachable row could each destroy the other's key.

The mint path read the row twice, once to judge it unreadable and once to get the bytes its
compare-and-set names. Between those two reads another writer could insert a fresh row, so the CAS
matched *that* and deleted a key already sealed under. This is the same failure the Phase 39 review
fixed by keying the CAS on the bytes rather than the parent, arriving through a different window, and
no ordering of two reads closes it: there has to be one. `load_secret_at` splits into a row read and
an `open_row`, and the mint path uses one read for both jobs.

It was found by reasoning, not by the flake, and then proved by injecting a delay into the window so
the old code failed deterministically and the fixed code passed. Chasing it by re-running would have
taken a long time: fifteen consecutive full runs did not reproduce it.

### Surfaces

- **Boot** adopts before starting projectors or effects, and refuses to serve if any row is left
  unaccounted for. A settled store pays one indexed count.
- **`hekla adopt`** runs the same pass ahead of a deploy, through a follower and without the lock,
  against the live directory a server is still using.
- **`hekla plan`** counts what would move, in prose and in `--json`, so a gate sees it first.
- **`hekla verify`** reports rows that have not moved and never moves them: an audit that advances
  state cannot re-run to check its own answer.
- **A disagreement is a warning**, not a refusal: two events naming different parents is legal,
  documented, and the expected shape of a migration where old rows take the `@absent` tenant and
  newer ones name a real shop. Reported because this fold is the only thing that ever looks, and
  honest about being what the pass noticed rather than an exhaustive audit, since the scan stops once
  every waiting key has a parent.

### What the review changed

Three more correctness bugs, and a claim that was written before it was true.

- **An event that sealed nothing decided where the key went.** Rule 12: an absent optional was
  never encrypted, so `lower` mints no key for it. The fold did not ask, so an event holding
  `email: null` while naming tenant 7 was read as the witness for a subject the write path had
  filed under tenant 9, and because that resolved the subject the scan stopped before ever reaching
  the event that minted it. Erasing the real tenant then left the content readable, silently, which
  is the precise failure this phase exists to remove. The fold now asks
  `heklang_host::seals_content`, so the witness is an event that actually minted a key.
- **`settled()` asked its own bookkeeping instead of the store.** `adopt_in` legitimately declines
  rows, and a declined subject is gone from `unresolved` while still sitting under the master, so a
  boot could serve on exactly the state it refuses. It re-asks the key store now, and a run makes up
  to three passes before reporting what is left, because a pass that moved less than it resolved is
  answered by looking again.
- **`hekla adopt` migrated the schema of the directory it was pointed at.** `Runtime::open_following`
  opens `hekla.db` through `OpDb::open`, which migrates; `plan` and `project` guard against exactly
  that and this did not, while advertising itself as the thing to run against a live directory.
- **"One indexed count per boot" was false.** The primary key seeks on `subject` and then filters
  `master_key_id IS NOT NULL` row by row, so a settled store with ten million children walked ten
  million entries to learn the answer was zero. Schema v11 adds a partial index over exactly the rows
  that are wrong, so on a healthy store it is empty and the count finds nothing rather than adding up
  to nothing. The test reads the query plan, because a comment cannot hold that claim up and this one
  did not.

Plus: the disagreement list grew with the log rather than with the subjects, on what the docs
themselves call the expected shape of a migration, and it is now one entry per subject and capped.

### What the second review changed

Three more, and the first two are the same mistake in two places.

- **The adoption's compare-and-set guarded on `master_key_id`, which an erase-and-recreate does not
  change.** The row that comes back holds a *different secret* under the *same* master, so the guard
  matched it and wrote the old secret's wrapping over the new one: everything sealed since the erase
  unreadable, everything the erase shredded readable again, silently, and reported as a successful
  move. This is the Phase 39 bug (a compare-and-set keyed on something two different rows share)
  arriving a third time, in code written to fix the second. It is keyed on the wrapped bytes now,
  like the replacement in `get_or_insert_subject_key`, because only the bytes name the row that was
  read.
- **Undoing an adoption could resurrect an erased row.** `load_secret` answering `None` covers both
  "the parent went underneath this" and "this subject was erased just after the write committed", and
  the undo could not tell them apart, so an `insert` put the subject's old secret back and un-shredded
  everything the erase destroyed. `OpDb::restore_root` is an **update**, never an insert: a row that
  is gone matches nothing and stays gone.
- **A subject that is only ever an ancestor was never resolved.** The fold read only the head of each
  seal chain. `Member under Tenant` already deployed leaves tenant rows minted as ancestors, so
  declaring `Tenant under Region` makes those the rows waiting while every event that could place
  them still seals under `Member`. The boot refused permanently, telling the author to add the
  `@absent` they had already added. Every link of the chain is registered now, except the last, which
  has no ancestors to be filed under.

Plus, from the same pass: `hekla adopt` made neither of the two refusals `serve` makes, so a wrong
master failed halfway through a half-migrated directory and a missing `@absent` surfaced as a raw
decode error; the boot could refuse *after* writing the declaration table, which is the "a boot that
refuses must leave no trace" rule the same function states forty lines earlier; the disagreement
sentence was duplicated between the boot and the CLI and had already drifted; a lost race still
logged that it had re-created a tenant; a stuck row cost three full-log scans instead of one; and the
boot fold said nothing for its whole duration while holding the directory lock.

### What the third review changed

Two that mattered, both of them in code written during the second round.

- **`hekla adopt` exited non-zero on exactly the directory it exists for.** A run re-asks the key
  store after each pass, and a live deployment serving the *old* declaration keeps minting roots the
  whole time, so `remaining > 0` always and the refusal always fired. The two endings needed to come
  apart: a subject no event accounts for is a declaration problem and is fatal anywhere, while a row
  that merely did not move is a race with a live writer, which is the **normal condition** of a
  pre-flight and a refusal only at boot, where nothing else should be writing. `Unfinished` names the
  two, and `settled()` is defined as it answering `None` so the two definitions cannot drift.
- **The boot's progress line underflowed on the second pass.** Every pass restarts its fold at the
  head of the log, so the position goes backwards, and `position - last` panics in a debug build and
  wraps in a release one, turning a line every 250,000 events into a line per event.

Plus: `hekla adopt` demanded a master key before the two answers that need none (a project with no
hierarchy, a directory with nothing deployed) and errored on a missing `hekla.db` rather than saying
so; the unresolved list was uncapped in a message that could name a million rows, when the
disagreement list had been capped for that reason; the tenant-resurrection notice asked whether the
parent's *row* existed rather than whether it was *reachable*, so the case where a whole erased
branch really is recreated was the one it stayed silent about; and the partial index's own comment
claimed it was empty on a healthy store, which it is not: its predicate covers every root row,
because which subjects declare a parent is a property of the program and a partial index is static.

And one the review did not raise, surfaced by the tests it prompted: **`MINT_ATTEMPTS` was four, and
four is too few.** A subject erased repeatedly while several threads write to it loses four races in
a row without anything being wrong, and Phase 39's own concurrency test failed about one run in
fifteen with `is being erased and recreated faster than a key can be minted`. That error is a refused
write on a request that should simply have taken longer. It is thirty-two now, which is still bounded
(a store erased in a tight loop for ever should say so rather than hang) but well clear of ordinary
contention: zero failures in thirty runs of that test and twenty of the whole suite.

### What the fourth review changed

The first round to reach past the new module into what Phase 39 had already committed, and three of
the five that mattered were there.

- **The reserved global secret was back on a public surface.** `/admin/subjects/{subject}/{value}`
  moved from `subject_key_exists` to `subject_key_reachable` when the hierarchy made reachability the
  right question, and the guard that hides `_hekla_global` did not move with it, so a point lookup
  reported `live` for a key the inventory deliberately hides. The rename had also merged the two
  functions' doc comments, leaving the guard *described* on the function that no longer had it, which
  is what let it through.
- **An entity's id column was checked by name where the code it backs assumes type.**
  `EntityDef::of` falls back to the subject's own name when it finds no column of that type, and its
  comment says `validate` refuses that "by *type*". `validate` compared names, so a column merely
  *called* `Org` satisfied the check, and `RowWriter::decrypt_field` then read that column as the
  subject id: the seal never opened and the column read back absent, which is the erasure-shaped
  answer the check exists to prevent.
- **The console's "events" link could never match.** A tephra tag is `<field>:<value>`, built from
  the field name, while a key row is filed under the subject's *type*. After the rename the console
  built `Org:7` and always found nothing. There is no single right answer when a program spells one
  subject's id two ways, so the API now reports the spelling when there is exactly one and the
  console links only then.
- **The tenant-resurrection notice fired on the happy path.** Changing it to ask reachability (the
  third review's finding) did not fix what it could never know: an erased row and one that never
  existed are the same absence, so the first member of every tenant claimed its tenant "had been
  erased". It is a count taken once per run now, saying what happened and leaving why to whoever
  knows whether they erased anything.
- **One row that could not be minted abandoned the whole run**, which contradicted the contract the
  third review had just established: `hekla adopt` calls contention ordinary, then treated one
  contended row as fatal for all of them.

Plus: the disagreement cap was applied per pass and not across them, so three passes could print
sixty lines from a constant whose point is twenty; the refusal cloned the entire unresolved
population to name twenty of it; and `placeholders` had been added beside a local closure of the same
name that shadowed it.

**And the memory bound, raised in every round and deferred in every round, is closed.** A pass takes
at most fifty thousand identities, so a backlog larger than that is several passes rather than one
allocation of the whole waiting population inside `Runtime::open`. The two reasons to go round again
needed separating to make that safe: a pass that *filled* its batch found more work and does not
count against the retry limit, while one that did not fill it saw everything and still left rows
behind, which is contention and gets three tries. Without that split, bounding the batch would have
turned a large migration into a refusal.

### What the fifth review changed

Back inside the adoption module, and the severity fell. One still mattered.

- **A run the key store could not act on reported as contention, and exited zero.** Per-row errors
  were dropped into a counter that never left the pass, so a corrupt wrapping or an ancestor under an
  unconfigured master left `remaining` high exactly as a race does. `hekla adopt` then printed
  "something else is writing to this data directory" and returned success, so a deploy gate went
  green over a store that could not be adopted at all, and the boot that followed refused with the
  same wrong explanation. The error text, meanwhile, only ever reached a `tracing::warn!` that this
  command installs no subscriber for, so it went nowhere. `Unfinished` has a third ending now, ahead
  of contention, and the two faults are fatal at both entry points.
- **`hekla adopt` reported success over a directory whose event log was gone**, without ever asking
  whether anything was waiting. The parent lives in the events, so a restored `hekla.db` with no
  `events/` beside it is a store nothing can place, and saying so is the only help available.
- **The unbounded orphan sweep never returned under continuous erasure.** Its doc says it converges
  with the depth of the hierarchy, which is true only on a store nobody is erasing: fresh direct
  orphans keep appearing, a pass always deletes something, and the sweeper thread stays there for
  ever while the journal retention pass beside it never runs again. Bounded now, which the same doc
  already argued for: what is left is unreadable whether or not it is still on disk.
- Plus: `minted_ancestors` counted ancestors of rows that never moved, on the one line an operator
  who erased a tenant is meant to be able to trust; `waiting` was latched to the first batch, so a
  four-pass run reported a quarter of the truth; the ancestor probe ran once per row rather than once
  per distinct ancestor, which on a full batch is fifty thousand round trips to learn one fact; and
  the spelling lookup behind the console's link rescanned every event declaration once per row.

**One finding was wrong, and checking it was the point.** The read API was reported as turning
`?status=open&status=closed` into two contradictory equality clauses. It does not: that handler
extracts `Query<HashMap<String, String>>`, which cannot hold two values for one key, and the comment
saying so is correct. Confirmed by request rather than by reading: with the pairs reversed the row
comes back every time, so one value reaches the filter and the last one wins.

### What the scoped review changed

Three reviews aimed at the previous round's diff alone, rather than at the whole phase. Two of the
three findings were the previous round's fix, wrong in the opposite direction, which is the pattern
this pass was run to look for.

- **Classifying every `adopt_in` error as fatal inverted the fix it was.** `adopt_in` raises when the
  mint retries run out, and that condition is *contention*: the chain is being erased and recreated
  faster than a key can be minted. So `hekla adopt` against the live directory it exists for exited
  non-zero and told the operator the store was corrupt, which is precisely the mirror of the bug
  being fixed, and the file said both things at once. `mint_secret_in` reports exhaustion rather than
  raising it, and `adopt_in` answers `Contended`; a *write* still fails loudly, because a write has
  content in hand and nowhere to put it.
- **A failure in one pass made a fully adopted store refuse to boot.** `failed` and `first_failure`
  were summed across passes and checked before `remaining`, so a row that errored in pass one and
  moved in pass two still reported a fault with nothing left under the master. `unfinished` asks the
  store first now: if nothing is waiting the run is done, however badly a pass along the way went.
  That is what `settled()`'s doc claimed all along and what reading the bookkeeping instead got wrong
  in both directions.
- **`minted_ancestors` was wrong in both directions, for the third time.** Neither half of the
  inference holds: `Contended` and `Undone` both return *after* the mint, so they can have created a
  key, and `Moved` does not imply one, because the mint stops at the first ancestor it can already
  open and never visits the links above it. The comment asserting otherwise was simply false. It is
  observed now rather than inferred: which ancestors were unreachable before the writes, and which of
  those are reachable after. It does not care which row did it, which is why it is finally right.
- Plus: the new no-log guard swallowed the errors it was written for behind two `unwrap_or(0)`s, so a
  corrupt `hekla.db` reported success; `waiting` double-counted a row that waited through two passes,
  having previously under-counted, and is now asked of the store once; a const was inserted inside a
  function's doc block and swallowed its summary, which is the same defect the same round had just
  fixed elsewhere; and two doc sentences named the wrong caller and the wrong tense.

The lesson is in the shape rather than any one item. Three of the last four rounds found that a fix
had introduced the failure it was fixing, mirrored. What broke the run of it here was not more care
on the same approach: it was replacing derived state with a question to the store, in both
`unfinished` and `minted_ancestors`. Bookkeeping that *describes* what happened can be wrong in a way
the store cannot.

### The scoped review, second pass

Run again against the previous pass's diff alone. Three findings, and the shape finally changed:
none of them was the fix inverted.

- **The contention fix only covered the first link.** `adopt_in` mints its direct ancestor through
  the tolerant path, but the recursion one level up still raised, so a chain of four links reproduced
  the whole failure: a healthy store reported as one that "will not resolve by looking again".
  Three levels hid it, because the tail of a three-link chain is a root and a root's mint cannot
  exhaust. The recursion reports losing the race the same way at every level now, and since it no
  longer carries back *which* level, the write path's refusal names the whole chain rather than
  asserting it was the leaf.
- **`Unaccounted` was the one verdict still decided without asking the store.** `run` broke on it
  before refreshing `remaining`, so an operator who erased the offending subject while
  `hekla adopt` was folding would be refused, and told to add an `@absent` for a key that no longer
  exists. The refresh moved above the break and `remaining == 0` now wins outright, which is the same
  "ask the store" that fixed the previous two.
- **A diagnostic count could throw away a completed run.** The after-loop probe for minted ancestors
  raises with `?`, *after* the writes have committed, so a transient database hiccup discarded
  `adopted`, `failed` and everything the next pass needed, and failed the boot over a log line.

Plus a doc that misattributed `/admin/subjects` to a wrapper with no production callers at all,
which is the same false trail that once put the reserved global key on a public surface, one layer
further up than where it was fixed.

**One test was written and then removed.** It raced an eraser against a four-link mint to pin the
first finding, and it passed against the reverted fix: with `MINT_ATTEMPTS` at thirty-two, losing
thirty-two consecutive races is effectively unreachable, which is the same reason raising that bound
stopped the Phase 39 stress test flaking. A green test that survives its own mutation is worse than
no test, so that fix stands on the trace through `try_mint_in` rather than on coverage, and this says
so rather than leaving a reassuring green line behind.

### Closing what had no test

Five paths were reachable and uncovered, found by looking rather than by being asked. Four are now
pinned by mutation-verified tests: adoption through a two-link chain, the disagreement cap and
deduplication, the v10-to-v11 migration, and the restore that puts a row back when its ancestor is
erased mid-adoption.

The last of those needed `adopt_in` to stop answering `bool`. Three of its four outcomes mean "did
not move" and are not interchangeable, and collapsing them left the restore **unreachable from a
test**: nothing could tell it from a lost compare-and-set. `Adopted` names them, and the test races
an eraser against a two-link chain so the window is the whole span between minting the root and
committing, rather than the tail of it.

The fifth, the pass loop, is pinned only at its contract: a run beside an eraser either settles the
store or says it did not, and a run with nothing else writing settles it, checked by erasing the
tenant afterwards and seeing it reach every member. Which branch a contended run takes is an
interleaving no test can fix, and the loop's failure mode is a refusal rather than a silent hole,
because the guarantee is held by `settled()` asking the store.

## Deferred, with triggers

Each item is placed with the condition that would pull it forward, so nothing is built before it is
warranted.

- **A `.hk` test that can omit a field younger than the log**: when a project has a handler whose
  behaviour differs for events written before a field existed and wants that difference under test.
  `given` writes an event whole, so today a project cannot build such a payload at all. The gap is
  not testing `@absent` itself, which heklang's suite and hekla's `tests/absent.rs` both pin: it is
  testing a fold arm or a projector arm *against old events*, which is project logic and has no
  other way in. Letting `given` omit a field that answers absence is a heklang parser and
  interpreter change, and it is additive, so nothing Phase 33 built has to anticipate it.
- **Upload API with versioning, pinning, and retention, plus hot reload** (load-graph incremental
  invalidation): when inline or live editing becomes a goal. The effect journal already records the
  script hash for this.
- **Fold library** (`event_counter`, `latest_event`, `toggle`): only after roughly fifteen real
  commands exist, and only if it compiles down to the existing `state` shape rather than becoming a
  second execution path. This is now a language question rather than hekla's.
- **Workspace crate split**: when hekla must be embeddable as a library, or when compile times
  actually hurt.
- **Vendoring Scalar for `/docs`**: when hekla has to run somewhere with no outbound network. The
  page loads the reference UI from a CDN today; `/openapi.json` itself needs nothing.
- **A timer that wakes a command**: when something a deployment cannot solve outside the runtime
  needs one. A window cap elapsing runs no handler today, and the shape proposed for it is an
  effect-arm verb (`schedule Cmd { .. } at <timestamp>`), so it is heklang's decision before it is
  hekla's and nothing here is designed around it in the meantime. What makes it deferrable rather
  than missing is that a caller with a clock can invoke the command itself, and hekla's guarantees
  (idempotency by tag, at-least-once effects) already cover one that fires twice.

### Carried-forward gaps from earlier phases

Deferrals recorded in the "honest scope" of a completed phase that no trigger above already pulls
forward. Collected here so they are not lost in the prose of the phase that introduced them.

- **Admin read-only SQL endpoint** (Phase 3): **closed, superseded.** It wanted a read-only query
  surface over the projector databases, and Phase 19's structured introspection lowered the pressure
  for it without answering the arbitrary-query case. `hekla project` and `POST /admin/projections`
  answer that case, and answer it better than SQL would have: the question is written in heklang, so
  it is total and typechecked against the deployed declarations; it folds the *log* rather than the
  derived read models, so it can ask things no read model materialised; and it needs none of the
  private table layout, which stays behind the generated read API as section 10 says it must.
- **Multi-field (composite-prefix) scan filters** (Phase 3): **closed.** Phase 37 widened a scan to
  equality across any prefix of a declared index plus a range on the column after it, and Phase 38
  added an ordering over one with a tuple cursor. Nothing of the original gap is left open.
- **Automatic dead-lettering** (Phase 4): the manual `POST /effects/{name}/skip/{position}` is the only
  escape hatch; a wedged effect is never advanced automatically.
- **`hekla fmt` and `hekla lsp`** (Phase 21): both were Starlark tooling wrapped in hekla's project
  knowledge, and were dropped rather than stubbed. They come back when heklang has a formatter and a
  language server; its tree-sitter grammar is the start of one.
- **Ciphertext below the language seam** (Phase 21): **closed.** heklang's `Value::Sealed` carries
  the stored form and its key seam is now `Keys::decrypt`, so `Log::read` hands a ciphertext through
  and a key is used once per `reveal` rather than once per record. The fold that cost 93ms against
  24ms for the same fold over plaintext now costs 25ms against 25ms (`tests/measure.rs`). It also
  deleted the placeholder a shredded key needed on the read path, and `record_of` no longer takes a
  key store: reading the log is not a place key material has to reach.

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
- **Type-shaped default tagging.** Auto-tagging indexes every field unless it is marked `@no_index`. A
  better default could key off the field type: identity-shaped fields (`Uuid`, integers, short bounded
  strings) are worth tagging, while `Money` almost never is. Refines the shipped auto-tagging default,
  and is a language question now.
- **Projector rename detection.** Store the projector's source file path in its checkpoint record.
  Moving `customer-orders.hk` then produces an explicit "rename or new projector?" error instead of
  silently rebuilding from position zero. Refines the shipped definition-change auto-rebuild.

Two items on this list were answered by Phase 21 rather than by a phase of their own:

- **A record type for folded state** is heklang's `record`, and `state` is typed, so
  `state["taken"]` has no counterpart to close.
- **Deriving command input from the event schema** stays open, and stays double-edged for the same
  reason: a command's parameters legitimately diverge from an event's fields.

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
