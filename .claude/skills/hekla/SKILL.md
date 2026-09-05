---
name: hekla
description: Run, operate and debug hekla, the single-app event-sourcing runtime that executes a heklang project. Covers the CLI (check, test, serve, verify, openapi, erase, rotate), project layout and hekla.toml, the generated HTTP surface (/commands, /read, /status, /admin and the console), projector rebuilds, the effect journal and wedged effects, and subject keys, master keys and erasure. Use for anything about running a hekla project, a hekla.toml, an HTTP route a project serves, a stuck effect or projector, HEKLA_MASTER_KEY, or erasing a subject. Writing the `.hk` files themselves is the heklang skill's job.
---

# hekla

hekla is a runtime, not a language. It takes a directory of `.hk` files, loads it as **one heklang
program**, and gives it a world: [tephra] for the append-only event log, SQLite for read models, a
key store for subject-scoped encryption, an HTTP surface generated from the declarations, and a
durable journal so an effect that crashes mid-way resumes without re-firing what it already did.

There is no build step and no artefact. **Deploy is restart.**

[tephra]: https://git.tqwewe.com/tephra/tephra

## This skill assumes the heklang skill beside it

The language is heklang's and is documented in the **heklang skill**
(`git.tqwewe.com/tephra/heklang`, at `.claude/skills/heklang/`). Events, commands, guards, refusals,
projectors, effects, folds, sealed content, `fn`, types and `test` scenarios are all there, and
nothing here restates them: a second copy could only drift, and the two ship on different release
cycles.

Install both. When only this one is present, treat every question about what goes *inside* a `.hk`
file as out of scope and read `heklang/docs/` instead. Everything hekla adds *around* the language is
below: where a declaration has to live, what the runtime stamps on an event, what the tools do, what
gets served, and what breaks in production.

## Run the tools. Always.

```sh
hekla check <dir>     # load the project, report every finding, run nothing
hekla test  <dir>     # the same, then run every `test` declaration against hekla's real world
hekla serve <dir>     # the runtime and the HTTP API, on 127.0.0.1:8080 by default
hekla openapi <dir>   # the generated OpenAPI 3.1 document on stdout, findings on stderr
hekla verify <dir>    # the offline invariant sweep over a data directory
hekla erase <field> <value> <dir>    # delete one subject's key. Irreversible
hekla rotate <dir>    # rewrap every subject key under the current master
```

Install with `cargo install hekla`, or `nix run git+https://git.tqwewe.com/tephra/hekla` to run it
without installing anything. From a hekla checkout it is `cargo run -- check <dir>`. Every
subcommand defaults `<dir>` to `.`.

**Workflow for every change:** edit the `.hk` files, `hekla check`, fix what it names, `hekla test`,
then run it. `hekla check` is the gate for placement and read-model shape; `hekla test` is the gate
for behaviour. Neither is optional, and a claim about a project that has not been through both is a
guess.

**Two silent successes to guard against.** `hekla check` on a directory that holds no modules prints
`checked 0 module(s)` and **exits 0**, so a typo'd path passes CI; `hekla test` on a project with no
`test` declarations prints `0 passed, 0 failed` and exits 0 too. Assert the counts, or put
`hekla openapi` in the same job: it is the one subcommand that refuses a path that is not a directory
and a project that declares nothing.

## One directory is one program, and three directories are rules

Every `.hk` file under the project root is part of the program, wherever it sits, including one at
the root and one in a directory you invented. Three are skipped: anything beginning with `.`,
`target/`, and the runtime's own `data/`.

```
project/
  commands/            enforced   public commands: routed at POST /commands/{Name}
  commands/internal/   enforced   invokable by effects, never routed
  projectors/          enforced
  effects/             enforced
  events/              convention
  lib/                 convention   guards, refusals, fn, const, record, enum
  tests/               convention   `test` declarations
  hekla.toml           optional     operational config
  data/                created by the runtime, never read as source
```

**A directory is enforced when a declaration in the wrong one would change what the runtime does**,
which is true of exactly three. A command's directory routes it, and the literal `commands/internal/`
prefix is what keeps a command off the HTTP surface. A projector's and an effect's directory is what
makes them a projector and an effect. Everything else is a habit: an event, a guard, a refusal, a
`fn` or a `test` is read the same from any directory, and a `test` under `commands/` runs exactly as
one under `tests/` does.

**A declaration is named by its declaration.** `commands/place-order.hk` declaring
`command PlaceOrder(...)` is routed at `POST /commands/PlaceOrder`. The file name is never read.

## I need to X, so I do Y

| I need to | Do |
| --- | --- |
| expose an operation to a client | declare the command under `commands/`; it is `POST /commands/{Name}` |
| keep an operation off the network | declare it under `commands/internal/`; only `invoke` reaches it |
| make data queryable | declare a projector entity; the key and each `@index` become the filters |
| let a client read its own write | have it pass `?after=<positions.last>` from the command's 200 |
| make a retried request safe | give the command a slice on its own id, or send `Idempotency-Key` |
| know why an effect is stuck | `GET /status`, then `GET /admin/effects/{Name}/invocations` |
| get a wedged effect past one event | fix the code and restart, or `POST /effects/{Name}/skip/{position}` |
| change a projector's shape | edit it and restart (`auto_rebuild`), or `POST /projectors/{Name}/replay` |
| forget a person | `hekla erase <subject_field> <value> <dir>`, or `erase(...)` in an effect arm |
| change the master key | set the new `HEKLA_MASTER_KEY`, keep the old in `HEKLA_MASTER_KEY_PREVIOUS`, `hekla rotate` |
| prove a deployment did not diverge | stop it (or copy the data directory) and `hekla verify` |
| pin the API in CI | `hekla openapi . > openapi.json` and diff it |
| see what the log actually holds | `GET /admin/events`, or open `/admin` in a browser |
| back up | copy the data directory of a stopped process |

## What each subcommand needs

| | project | data dir | takes the lock | master key | exit 1 on |
| --- | --- | --- | --- | --- | --- |
| `check` | yes | no | no | no | any error finding |
| `test` | yes | no (throwaway) | no | no (pinned) | an error finding, or a failing test |
| `serve` | yes | yes, creates | **yes** | if any `@subject` | errors, a bad addr, a held lock |
| `verify` | yes | yes, must exist | **yes** | if any `@subject` | any violation |
| `openapi` | yes | no | no | no | not a directory, errors, nothing declared |
| `erase` | for the path only | must hold `hekla.db` | no | no | no database at that path |
| `rotate` | for the path only | must hold `hekla.db` | no | **required** | no key, no database |

`erase` takes no lock, so it works against a running server and takes effect on the next request.
`verify` does, which is why the documented shape is to verify a copy of the directory.

## The HTTP surface, in one table

Every `{Name}` is a declared name. Nothing is authenticated: the bind address is the boundary, and it
defaults to `127.0.0.1`.

| Route | Answers |
| --- | --- |
| `POST /commands/{Name}` | 200 committed, 400 `invalid`, 422 `reject`, 409 conflict, 404 unknown or internal |
| `GET /read/{Projector}/{Entity}/{key}` | `{item, position}`, or 404 |
| `GET /read/{Projector}/{Entity}?<field>=&limit=&cursor=&after=&timeout_ms=` | `{items, next_cursor, position}` |
| `POST /projectors/{Name}/replay` | 202, schedules a rebuild-and-swap |
| `POST /effects/{Name}/skip/{position}` | 202, records an operator skip request |
| `GET /status` | per-module position, lag, readiness, effect state, failure count, last error |
| `GET /health` | liveness only |
| `GET /openapi.json`, `GET /docs` | the generated document, and a reference over it |
| `GET /admin/...` | read-only introspection, or the console when `Accept: text/html` |

A command's success body is `{correlation_id, causation_id, positions: {first, last}, events: [...]}`,
and every error body is `{correlation_id, causation_id, error: {code, message}}` (the two ids are
absent outside `/commands`). A refusal's `code` is its name in snake_case: `refusal RoomTaken` reaches
a client as `room_taken` with 422.

## Rules that are easy to break

1. **A `Timestamp` goes in two ways and comes out one.** A command parameter takes RFC 3339
   (`"2026-06-01T00:00:00Z"`) or epoch microseconds (`1780272000000000`), and the two are one value.
   A read response and a read filter are RFC 3339 only, and a stored tag is always the microseconds.
   An RFC 3339 input must carry an offset; without one it is a `400 invalid_input` naming RFC 3339.
2. **A `Money(n)` is a decimal string on the wire**, in and out (`"120.00"`). A JSON number would be
   a float somewhere in the chain, which is the one thing money must never be.
3. **A command that decided to do nothing still returns 200**, with `events: []` and
   `positions: null`. That is what an idempotent replay looks like; only an error carries `error`.
4. **A read right after a command can 404.** The projector is asynchronous. Pass
   `?after=<positions.last>` to wait for it (default 5s, capped at 30s, `503 not_caught_up` on
   timeout), or accept the race deliberately.
5. **Only the key and declared indexes are filterable.** Anything else is a `400 unindexed_filter`
   telling you to declare the index, never a table scan.
6. **An absent column is omitted from a read response**, not serialised as `null`. An erased subject's
   column looks exactly like a column that was never written.
7. **`/status` reports an effect's durable watermark, not what it is working on.** A wedge at position
   3 with the watermark at 1 reads as `position: 1, lag: 3`. The position that is actually stuck is
   the invocation with `status: running` in `GET /admin/effects/{Name}/invocations`.
8. **An operator skip is a request, not a command.** The endpoint answers 202 whatever position you
   name; the driver honours it only once *that* position has failed at least once, and only one
   request is pending at a time, so naming the wrong position clears the one you meant.
9. **A project that uses `@subject` will not boot without `HEKLA_MASTER_KEY`**, and losing that key
   is unrecoverable loss of every subject-scoped value. Nothing else in the runtime fails this way.
10. **Erasing is deleting a key, so it is instant, total and irreversible**, across the log and every
    read model at once. It also unwedges: an effect retrying a `reveal` of the erased subject stops
    retrying and fails terminally, which completes that position and advances.
11. **One process per data directory.** The runtime takes an exclusive lock, and `hekla verify` takes
    it too, so the sweep runs against a stopped instance or a copy.
12. **Changing what a projector does changes its definition hash**, and the read model on disk no
    longer matches. That covers its entity shapes, its subscription and its handler bodies, plus any
    `const`, `guard` or `refusal` it reaches, since those are inlined. With `auto_rebuild = true`
    (the default) it rebuilds at startup and reads answer 503 meanwhile; with it off the projector
    goes `stale` and every read of it is a 503 until someone posts a replay. Reformatting, adding a
    comment or renaming a local is *not* a change: the hash is over what runs, not over the text.
13. **Directory placement is checked per declaration, not per file.** `hekla check` reads every `.hk`
    file wherever it is, so a scratch module that is not meant to compile belongs under a
    dot-directory or it fails the whole project.
14. **`hekla check` is not `hek check` plus nothing.** It is the compiler's diagnostics plus what only
    hekla knows: directory placement, an index over a sealed column, a filterable column
    named like a read query parameter, a sealed column that cannot say it is absent, and the reserved
    `_hekla_` tag namespace. The entity *key* rules are heklang's and report with a span.

## Reference

| File | Covers |
| --- | --- |
| `reference/cli.md` | every subcommand, its flags, its output, its exit codes, and every finding `hekla check` can report |
| `reference/layout.md` | the directory rules, `hekla.toml` in full, the envelope, tagging, idempotency, deployment |
| `reference/http.md` | commands, the read API, the operator routes, `/status`, wire types, every error code |
| `reference/introspection.md` | the `/admin` surface, subject states, and the console |
| `reference/operations.md` | projector readiness and rebuilds, effect states, the journal, wedges, verify and quarantine, storage and backup |
| `reference/encryption.md` | subjects, the master key, rotation, erasure, and what each surface shows afterwards |

`example/` is a complete twelve-file project (events, three commands including an internal one, a
projector with two entities, an effect, shared refusals, four test files and a `hekla.toml`) that
passes `hekla check` and `hekla test`, and that CI runs on every push. Read it for the shape of a
project before writing one.
