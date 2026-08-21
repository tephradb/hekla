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

## Phase 3: projectors and generated read API

Read models and the query surface over them.

- One sequential task per projector; checkpoint as watermark plus completed-set, committed in the
  same transaction as the state.
- `get(entity, key)` reading through uncommitted writes in the current batch; `put` / `patch` /
  `delete`.
- Generated read API: `GET /read/{projector}/{entity}/{key}` plus indexed filter/scan (400 on an
  unindexed filter), cursor pagination, projector position in every response.
- Projector replay (rebuild-and-swap via rename) and `POST /projectors/{name}/replay`.
- Admin-only, read-only SQL endpoint (off in production). Status shows position and lag.

## Phase 4: effects (durable execution)

The durable-execution model.

- Effect task, sequential per effect, strict position order.
- Journal in the operational DB: content-hash keys, recorded script hash, retention sweep, terminal
  record journaled.
- Builtins: journaled `http.*`, `invoke_command` (public or internal), `now()`, `log()`, and
  journaled `read(projector, entity, key)` plus filter/scan.
- Runtime retry (transport and 5xx with backoff); `status >= 400` terminal to the script.
- Graceful-shutdown draining; restart hash-mismatch warning; effect lag in status.
- Retention sweeper task (lazy GC) for effect journals and command idempotency keys, with
  configurable windows in `kiln.toml`.
- Explicit blocking-pool size with a saturation-waits-not-spawns policy in `kiln.toml`.

## Phase 5 and beyond (deferred, with triggers)

Each item is placed with the condition that would pull it forward, so nothing is built before it is
warranted.

- **Upload API with versioning, pinning, and retention, plus hot reload** (load-graph incremental
  invalidation): when inline or live editing becomes a goal. The effect journal already records the
  script hash for this.
- **Partition-key parallel effect lanes**: when a single effect's throughput on slow APIs hurts. The
  checkpoint format (watermark plus completed-set) already supports it.
- **Read-your-writes**: using the log position already returned by reads and command responses.
- **Encryption and crypto-shredding**: when PII-at-rest requirements land.
- **Metrics and Prometheus**: when there is something to operate at scale.
- **Fold library** (`event_counter`, `latest_event`, `toggle`): only after roughly fifteen real
  commands exist, and only if it compiles down to the existing `query` / `fold` shape rather than
  becoming a second execution path.
- **Workspace crate split**: when kiln must be embeddable as a library, or when compile times
  actually hurt.
