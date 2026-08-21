# kiln roadmap

Phased delivery of the design in [ARCHITECTURE.md](./ARCHITECTURE.md). Each phase builds on the code
that already exists, and each is shippable on its own except where noted. A cross-cutting rule: every
phase keeps `kiln check` honest for whatever it introduces, so the static analysis never falls behind
the language.

## Phase 0: command and projector core (done)

The current single-crate code is the baseline:

- `src/starlark_builtins.rs`: field types (`text`, `i64_`, `u64_`, `boolean`, `uuid`, `timestamp`,
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
  schema, derives tags, and feeds `emit(...)`; structured tag queries.
- Effects evaluate against effect-scoped globals (`http.*`, `invoke_command`, `now`, `log`, `read`,
  stubbed until Phase 4), so command and projector purity is structural: they never see a clock or
  the network.
- Deploy-time validation: `query` tag fields and projector/effect `source` types checked against the
  event registry, projector indexes against declared fields. Emit payloads are validated by the event
  constructor itself, at the point of emit.
- Operational DB (`kiln.db`) skeleton: idempotency table, effect journal table, effect invocation
  table, module metadata, under a versioned migration.
- `kiln check` (thorough, collects every finding in one pass) and `kiln fmt` (a conservative
  whitespace normaliser; AST-level reflow is deferred, since starlark-rust 0.14 exposes no
  pretty-printer).

## Phase 2: command runtime and HTTP API (done)

The first runnable server: execute commands over HTTP. `kiln serve` opens the tephra store and the
operational DB, loads the project (refusing to serve if `kiln check` would fail), and runs the
decision cycle behind Axum.

- Command context: correlation id (from the `x-correlation-id` header or generated), a fresh causation
  id, optional triggering event; every response echoes correlation and causation. Pinned `now()` is
  in scope only during `handle` (carried on the evaluator via `eval.extra`); `query` and `fold` run
  without it, so calling `now()` there is an error.
- Client-supplied ids need no new code (the schema's `uuid()` fields already carry them). Each emitted
  event is wrapped at the append seam in a host-stamped envelope (event id, timestamp, correlation,
  causation, optional triggering event); tags stay outside the envelope as tephra tags, and every
  store read unwraps the payload. The idempotency key is kept in the operational DB, never on an event.
- Built-in per-command idempotency: a `pending` reservation is the mutual-exclusion token for
  concurrent duplicates, moved to `done` with the full response body so a replay returns the original
  status and body verbatim (including rejections). Startup clears stale reservations; the crash window
  between append and finalize is documented and bounded by DCB for natural-id creates.
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
- `kiln test`: `tests/*.star` scenarios seed a throwaway store through the same append path and run the
  real command, asserting emitted events (type, data, tags) or the rejection.

## Phase 3: projectors and generated read API (done)

Read models and the query surface over them. `kiln serve` now runs one thread per projector and serves
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

The durable-execution model. `kiln serve` now runs one thread per effect: it subscribes to the effect's
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
  the target command's DCB boundary), `now()`, journaled `read(projector, entity, key)` plus a `scan`
  filter/paginate, and `log()` (not journaled).
- Retry split: the runtime absorbs transport errors and 5xx (they never reach the script) by wedging the
  invocation and retrying with capped backoff; a 2xx/3xx/4xx result reaches the script, so `status >= 400`
  is a real decide-what-to-do outcome. A handler error wedges the same way (retry forever, never skip).
- Graceful-shutdown draining (effects first, then projectors, then the writer), with a bounded join so a
  wedged effect cannot hang shutdown. `/status` reports each effect's position, lag, consecutive-failure
  count, and last error, so a wedge reads as broken rather than merely slow.
- `POST /effects/{name}/skip/{position}`: an explicit, manual operator action to advance a wedged effect
  past a genuinely unprocessable event. Never automatic.
- Retention sweeper task (lazy GC) for effect journals and command idempotency keys, with configurable
  windows in `kiln.toml`, sweeping in bounded chunks.

Honest scope for this phase:

- `effects.pool_size` is validated but not enforced: v1 runs one thread per effect, which already bounds
  concurrency. A real shared blocking pool is reserved for partition-key parallel lanes (a later phase),
  which the watermark-plus-completed-set format already supports.
- `invoke_command` lands the domain fact exactly-once when the target command is idempotent under replay
  (a natural-id create or an explicit DCB boundary, like the example's `record-welcome`): the
  deterministic idempotency key deduplicates in the common path, but across the narrow append-then-
  finalize crash window the key is cleared on restart (as every pending key is), so the boundary is what
  dedupes the replay, exactly as for HTTP commands. Raw `http.*` is at-least-once (a crash between a
  successful request and its journal write re-fires on replay).
- `read()`/`scan()` are journaled, so a replayed effect sees point-in-time-stale data by design; at cold
  start an effect can also outrun a projector and journal an empty read that then replays empty forever.
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

## Phase 6 and beyond (deferred, with triggers)

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
- **Workspace crate split**: when kiln must be embeddable as a library, or when compile times
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
- **Cold-start empty read** (Phase 4): an effect can outrun a projector and journal an empty
  `read()`/`scan()` that then replays empty forever; accepted, not yet guarded.

Inherent design properties, listed for completeness (not future work): `invoke_command` is exactly-once
only when the target is idempotent under replay, raw `http.*` is at-least-once, and `read()`/`scan()`
are journaled (point-in-time-stale on replay).
