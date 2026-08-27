# hekla roadmap

Phased delivery of the design in [ARCHITECTURE.md](./ARCHITECTURE.md). Each phase builds on the code
that already exists, and each is shippable on its own except where noted. A cross-cutting rule: every
phase keeps `hekla check` honest for whatever it introduces, so the static analysis never falls behind
the language.

## Phase 0: command and projector core (done)

The current single-crate code is the baseline:

- `src/starlark_builtins.rs`: field types (`str`, `int`, `uint`, `bool`, `uuid`, `timestamp`,
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
  Phase 18 widened the absorbed set to every retryable status.
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
  necessary rather than delivering it.
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
  wanting, but it is a different transport with its own backpressure and shutdown story.
- **The overview's sparkline is not a metric.** It is bucketed in the browser from the timestamps on
  one page of events, and is labelled as such. hekla has no metrics endpoint and this does not
  pretend to be one.
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

## Deferred, with triggers

Each item is placed with the condition that would pull it forward, so nothing is built before it is
warranted.

- **Upload API with versioning, pinning, and retention, plus hot reload** (load-graph incremental
  invalidation): when inline or live editing becomes a goal. The effect journal already records the
  script hash for this.
- **Partition-key parallel effect lanes**: when a single effect's throughput on slow APIs hurts. The
  checkpoint format (watermark plus completed-set) already supports it.
- **Metrics and Prometheus**: when there is something to operate at scale.
- **Fold library** (`event_counter`, `latest_event`, `toggle`): only after roughly fifteen real
  commands exist, and only if it compiles down to the existing `query` / `fold` shape rather than
  becoming a second execution path.
- **Workspace crate split**: when hekla must be embeddable as a library, or when compile times
  actually hurt.
- **Vendoring Scalar for `/docs`**: when hekla has to run somewhere with no outbound network. The
  page loads the reference UI from a CDN today; `/openapi.json` itself needs nothing.

### Carried-forward gaps from earlier phases

Deferrals recorded in the "honest scope" of a completed phase that no trigger above already pulls
forward. Collected here so they are not lost in the prose of the phase that introduced them.

- **Admin read-only SQL endpoint** (Phase 3): a read-only query surface over the projector databases.
  Phase 19 delivered structured introspection over the same state, which lowers the pressure for it
  without answering the arbitrary-query case.
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
