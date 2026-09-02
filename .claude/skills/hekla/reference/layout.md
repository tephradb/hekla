# Project layout, config and what the runtime adds

## Discovery

Every `.hk` file under the project root is part of one program. There is no import, no manifest and
no file order. Three things are skipped, and nothing else is:

- any directory whose name begins with `.`
- any directory named `target`
- a directory named `data` **at the project root** (the runtime's own; a `data/` deeper in the tree is
  read normally)

Non-`.hk` files are ignored. A file that does not parse fails the project from wherever it sits, so a
scratch module belongs under a dot-directory.

## The three enforced directories

| Directory | Enforced | What it decides |
| --- | --- | --- |
| `commands/` | yes | the command is routed at `POST /commands/{Name}` |
| `commands/internal/` | yes | the command is invokable by an effect and never routed |
| `projectors/` | yes | the declaration is a projector |
| `effects/` | yes | the declaration is an effect |
| everything else | no | nothing |

Rules:

- Only the literal `commands/internal/` prefix marks a command internal. Deeper nesting elsewhere is
  free and stays public: `commands/billing/refund.hk` is routed.
- A `projector` or `effect` outside its directory is an error, not a warning, and the module is
  dropped from the project as well as reported.
- `events/`, `lib/` and `tests/` are habits. An event, guard, refusal, `fn`, `const`, `record`, `enum`
  or `test` is read identically from any directory.
- File names are never read. `commands/place-order.hk` declaring `command PlaceOrder` is
  `/commands/PlaceOrder`. The examples use kebab-case files and PascalCase declarations.
- Names are global and unique per kind, across every file.

## `hekla.toml`

Optional, at the project root, every value defaulted. `deny_unknown_fields` is on: a misspelled key is
a load error (`error: hekla.toml: parsing <path>: ...`), which stops `check`, `serve` and everything
else.

```toml
[effects]
pool_size = 16              # default 16, must be >= 1. Validated, and reserved: v1 runs one
                            # thread per effect

[retention]
effect_journal_days = 7     # default 7, max 36500. How long a completed invocation's journal
                            # survives before the sweeper reclaims it

[projectors]
auto_rebuild = true         # default true. Rebuild a projector whose definition changed, rather
                            # than leaving it `stale` for an operator

[verify]
enabled = false             # default false. Replay every completed invocation against a sealed
                            # journal and quarantine an effect that diverges
```

`serve --verify` sets `[verify] enabled` for one run and cannot unset it. `GET /admin/system` reports
the effective config, which is the way to check what a running process actually loaded.

## What the runtime stamps on an event

An event's fields are the author's. Around them goes an envelope: `event_id`, `correlation_id`,
`causation_id`, an optional `triggering_event_id`, and the append `timestamp`. A projector's or an
effect's trigger binding exposes three of them as `e.id`, `e.at` and `e.position`. A command's fold
sees none of it: a fold arm binds the event's declared fields and nothing else.

Prefer `e.at` to `now()` for the append instant. `now()` is for time that is genuinely domain data.

**Every field is a tag unless it opts out with `@no_index`.** Tags are what a slice and the log
filters match on, which is why a filter on a `@no_index` field is a compile error rather than a query
that silently matches nothing. A subject-scoped field's tag holds ciphertext: it is stripped from a
command's response, shows in `/admin/events` as the ciphertext it is, and cannot be filtered on from
outside.

**`_hekla_` is the runtime's tag namespace**, and an event field that starts with it is a check error.
Two tags live there: a keyed command's idempotency tag and the correlation tag every event carries.
Both are stripped from command responses and both are visible in `/admin` as `hekla_tags`.

## Idempotency

Two mechanisms, for two problems.

**Id-based dedupe needs nothing.** A new entity's id is a parameter, so a retried request carries the
same id, the command's own slice sees the existing event, and the second attempt returns 200 with
`events: []` and `positions: null`.

**`Idempotency-Key` is for when nothing in the input distinguishes intent.** The runtime hashes the
header with the command name into a `_hekla_idem` tag, stamps it on every emitted event, and guards
the append against that tag existing anywhere in the log. A repeat returns the original commit's
events, positions, correlation id and causation id verbatim, **even if the body differs**, without
re-running the command. A first attempt that rejected appended nothing and so left no tag: a retry
re-decides.

**An effect's `invoke` gets a key automatically**, derived from the journal identity of the call (the
effect, the position, the call key and its ordinal), so a crash between the append and the journal
write replays into the existence clause instead of appending twice.

## Deployment

- The project is read at startup. There is no hot reload; deploy is restart.
- One process per data directory, enforced by an exclusive lock.
- The whole of the deployable artefact is the source tree plus `HEKLA_MASTER_KEY`. There is no build
  step, no compile cache and no schema migration for the log.
- Changing an entity's shape or a projector's subscription changes its definition hash and triggers a
  rebuild at the next start (`operations.md`). Changing a handler body does not.
- Changing an effect's source changes its script hash, which is recorded per invocation and reported
  by `/admin`, and makes `hekla verify` skip invocations recorded under the old code.

## Embedding

`cargo add hekla`. The binary is a shim over the library and everything it does is public:
`hekla::cli::run()` is the whole of `main`, and under it `hekla::loader::LoadedProject::load`,
`hekla::runtime::Runtime::open`, `hekla::server::app`, `hekla::openapi` and `hekla::testing` are the
seams the CLI itself uses.

Embedding means driving the runtime from your own process, not writing handlers in Rust. There is
deliberately no Rust, TypeScript or WASM authoring path, now or later: heklang being the only
authoring surface is what makes determinism structural rather than policed.
