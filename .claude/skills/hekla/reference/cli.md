# The CLI

One binary, eight subcommands, `<dir>` defaulting to `.` everywhere. `hekla --version` and
`hekla <subcommand> --help` work.

Logging is `tracing` behind `RUST_LOG`, default `info`. `serve` and `verify` initialise it; the other
subcommands print with `println!` and are unaffected.

## `hekla check [DIR]`

Loads the project and reports every finding. Runs nothing, opens no data directory, needs no key.

```
error: projectors/p.hk: entity `Thing`: index `by_email` covers subject-encrypted column `email`; filter by the plaintext subject id instead
warning: commands/broad.hk: `DoBroad` folds `thing.done` with no constraint on a high-cardinality field, so it guards a broad set of events; a boundary is best keyed on an entity id

checked 2 module(s): 1 command(s), 1 projector(s), 0 effect(s), 1 event(s)
ok: no errors, 0 warning(s)
```

A finding is `<severity>: <location>[:line:col]: <message>`, with the compiler's hint, when there is
one, on a following `  = ` line. Line and column are 1-based. `location` is the project-relative file
path; a few whole-project findings name `hekla.toml` or `events` instead. Findings are sorted by
location, then by position.

The summary counts **modules**, which are commands plus projectors plus effects. Events are counted
separately and are not modules. Exit is 0 when no finding is an error, 1 otherwise; a warning never
fails the check.

**A directory with no `.hk` files reports `checked 0 module(s)` and exits 0.** So does one that does
not exist. Nothing about a missing path is an error to `check`.

## `hekla test [DIR]`

Loads the project, refuses to run if any finding is an error, then runs **every `test` declaration in
the program**, wherever it is declared. The `tests/` directory is a convention; a `test` beside its
command runs identically.

```
ok: "books a free room-night"
FAIL: "a failing expectation": @user.registered.email: expected "wrong@example.com", got "zed@example.com"

13 passed, 0 failed
```

The world is hekla's: a real tephra log in a temporary directory, real SQLite read models, a real key
store with a fixed master key, and a stubbed network driven by `respond`. The clock is pinned to
`1970-01-01T00:00:00Z`, and each `given` event gets a deterministic id counting from
`00000000-0000-0000-0000-000000000001`, so a `Uuid.derive(e.id, ...)` is assertable.

Exit is 1 on a failing test and 1 on a project with error findings. **A project with no tests prints
`0 passed, 0 failed` and exits 0.**

## `hekla serve [DIR] [--addr ADDR] [--data-dir PATH] [--verify]`

Loads the project, refuses to serve if any finding is an error (`refusing to serve: the project has
N error(s)`), then runs the runtime and the HTTP API.

- `--addr` defaults to `127.0.0.1:8080`. It must parse as a socket address; a bare port or a hostname
  is `error: invalid --addr`.
- `--data-dir` defaults to `<dir>/data`, and is created if absent.
- `--verify` turns on the continuous invariant check for this run. It cannot turn off
  `[verify] enabled = true` from `hekla.toml`.

Startup logs three lines at `info`:

```
hekla listening on http://127.0.0.1:8080
  admin console   http://127.0.0.1:8080/admin
  api reference   http://127.0.0.1:8080/docs
```

Boot fails, before binding, when:

- the project uses `@subject` anywhere and `HEKLA_MASTER_KEY` is unset:
  `error: this project uses subject-scoped encryption (a field with subject = "..."), so HEKLA_MASTER_KEY must be set`
- another process holds the data directory:
  `error: the data directory at <path> is in use by another hekla process (database is locked); stop it, or run against a copy of the directory`

Ctrl-C is a graceful shutdown: it stops dispatching new effect invocations and waits up to 30 seconds
for in-flight ones, then exits. An invocation abandoned that way stays `running` and replays at the
next start.

## `hekla verify [DIR] [--data-dir PATH]`

The offline invariant sweep. Loads the project, refuses on error findings, then takes the
data-directory lock and checks rebuild equivalence, replay equivalence and checkpoint monotonicity
(see `operations.md`).

```
checked 1 projector(s) and 0 invocation(s); skipped 1 (1 edited, 0 erased, 0 without a
journal, 0 reclaimed, 0 without their event)
ok: no violations
```

Exit is 1 on any violation, on a missing data directory (`error: no data directory at <path>`), and
on a directory another process holds. A project that uses `@subject` is refused without a key, the
same way `serve` is: `error: this project uses subject-scoped encryption (...), so HEKLA_MASTER_KEY
must be set to verify it`. Run it with the key the server used.

An invocation is **skipped**, not checked, when the effect's source hash has changed since the run
was recorded, when its event cannot be read, when a `reveal` it makes needs a subject key that has
been erased, when an operator skipped it, when its record could not be read, when it journaled no
call at all and the replay reaches one, or when retention reclaimed its record while the sweep was
reading it. Each is counted separately, because they mean different things.

The operator skip is the subtle one. It completes a wedged invocation without running it to an end,
so whatever the journal holds is the prefix of a run that never finished, and comparing a replay
against that prefix would fail a directory an operator deliberately made healthy. The invocation row
records the skip (`skipped_at`), so this does not depend on the shape of the journal. Invocations
recorded before that column existed fall back to the old reading, which can only recognise a skip
that journaled nothing at all, and those are counted separately as journaling no call.

A run that skipped everything still says `ok`, which is why the counts are printed.

## `hekla plan [DIR] [--data-dir PATH] [--json] [--replay] [--replay-limit N]`

What deploying this project over a data directory would change, before it changes it. Loads the
project, refuses on error findings, then compares its digest against the `declaration` rows the
directory records and forecasts what each projector would do.

```
compared 6 declaration(s) against what is deployed
  behaviour command DoA (commands/a.hk)
  behaviour command DoB (commands/b.hk)
  because `guard ShopIsConnected` changed: DoA, DoB
0 added, 0 removed, 2 changed; 0 projector(s) would rebuild
```

A declaration is `added`, `removed`, `behaviour` (it does something different behind a contract that
did not move) or `contract` (what is visible outside changed). `const`, `refusal` and `guard` are
inlined and have no row of their own, so an edit to one shows as a fan-out with the cause named
under it.

Without `--replay` it opens no event log at all. Either way it takes **no data-directory lock**, so
it runs against a directory a server has open. It changes no database: the operational DB is opened
only after its schema version is read separately and found to match, and read models are opened
read-only.

Exit is 0 whenever the plan was computed, whether or not anything would change; a change is the
answer, not a fault. It is 1 on error findings, on a directory nothing was ever deployed to, on a
schema version this build does not expect, and on a `--json` serialisation failure. `--json` puts the
whole plan, including the before and after forms, on stdout with findings on stderr.

### `--replay`

A declaration diff says an effect changed. It cannot say whether the change matters, and "would this
now send a different HTTP request" is what a deploy actually turns on. `--replay` re-runs recorded
invocations of every affected effect against the candidate code and the journal the original run
left behind.

```
  effect Notify @ 12: it reached a call the recorded run never made (http.post #0)
replayed 2 invocation(s) across 1 affected effect(s); 0 reproduce, 2 diverge
this project retains 7 day(s) of journals; anything older was reclaimed before the replay could see it
```

Nothing is mocked. The journal holds the responses the recorded run really received, so a candidate
that branches differently on a response reaches a call the journal has no entry for, and that miss is
the finding. Nothing is sent, nothing is appended, nothing is erased: it is the sealed replay `verify`
runs, with a program that has not been deployed yet.

**Affected** means the effect's own digest changed, *or* it names something whose digest changed:
a module `fn` it calls, an event it handles, a record or enum reached through either. The second
half matters, because heklang gives each of those an entry of its own and an entry's hash covers
what is written inside it. Editing the helper that builds a URL does not move the calling effect's
hash; neither does adding `@subject(...)` to an event field the arm binds, since an arm binds by
name and the digest records the name and the slot, not the type. A check that looked only at the
effect's own hash would miss both.

The closure is deliberately conservative. An effect pulled in through a reference it does not
really depend on costs one replay that reports `matched`; one left out costs the finding.

This half opens the log, through a read-only tephra follower: read-only descriptors, nothing created,
nothing deleted, no lock. It still runs against a deployment serving traffic, and
`replay_runs_while_a_server_holds_the_directory` in `tests/plan.rs` is what keeps that true. Because
a follower pins one committed prefix at the moment it opens, an invocation the live server records
*after* that is left out rather than replayed against an event the reader cannot see.

What it cannot see, all counted or named rather than assumed away:

- **An erased subject.** A handler that branches on revealed plaintext cannot be re-run once its key
  is shredded. Counted as unreplayable, never as a divergence.
- **An operator skip.** A skipped invocation was completed on an operator's say-so without ever
  running to an end, so its journal is the prefix of a run that stopped where it wedged. The row
  records the skip, so this holds whatever that prefix contains.
- **An invocation that journaled nothing**, where the candidate now reaches a call, and that was
  recorded before the row could carry a skip marker. There an operator skip and a run that called
  nothing are the same row, so there is nothing to compare against. (When the candidate also calls
  nothing the two agree, and that *is* checked.)
- **A record that could not be read.** A busy op-DB is ordinary against a live directory and says
  nothing about the candidate, so a failed read is a gap in coverage rather than a divergence. The
  same holds one level up: if an effect's history cannot be listed at all, that effect is named and
  the rest of the plan still stands.
- **Retention.** A reclaimed invocation loses its row and its journal together, so it is invisible
  here rather than skipped. Nothing can count what is gone, so `retention.effect_journal_days` is
  printed instead. It is the *candidate's* window and an upper bound: the deployed configuration is
  what actually governed the sweeping, and hekla does not record it.
- **`--replay-limit`** (default 1000, at least 1, per effect). A busy effect's week of history is
  unbounded in a way a deploy gate is not, so only that many of the most recent invocations are
  replayed, and any effect the cap bit is named in the report.
- **An older version of the effect.** Only invocations the *deployed* program recorded are replayed.
  Retention outlives an edit, so rows written by a version this deploy is not replacing are still on
  disk, and replaying those would report a difference the running code already has.
- **A terminal `fail`.** Rule 4 makes giving up an outcome rather than an error, and the row it
  leaves is the one a success leaves, so nothing on disk says which happened. A candidate that would
  newly `fail` on recorded events is reported as a divergence for exactly that reason: the record
  cannot vouch for it. `verify` replays the program that wrote the row, where failing where it failed
  is a reproduction, so it never sees this.
- **An invocation retention reclaimed mid-run.** `--replay` is built to run against a directory a
  server is still sweeping, so a row can go between being listed and being read. Counted rather than
  read as a run that called nothing.

An effect that `reveal`s needs `HEKLA_MASTER_KEY`. Without one its invocations are counted
`no_master_key` and the rest of the project still replays, so a CI job can plan against production
without holding the production key and still be told plainly what it did not see. The decision is
per effect, read off the form: one sealed field somewhere does not blind an effect that never
touches a key. A key that is *present* but cannot unwrap what is stored (a half-configured
rotation) degrades the same way rather than failing the command, and the report names the reason:
a wrong key must not be worse than no key.

A divergence does not change the exit code. `plan` reports; a gate reads `--json`, which carries
`divergences` and `coverage` beside the rest. Both are `null` when no replay ran: an empty
`divergences` list would be a clean replay result, and a gate must not read one off a run that never
opened the log. `--replay-limit` requires `--replay`, for the same reason a cap nobody can see is
not acceptable: a limit on a replay that is not happening would be accepted and dropped.

## `hekla openapi [DIR]`

Prints the generated OpenAPI 3.1 document to stdout and every finding to stderr, so
`hekla openapi . > openapi.json` writes only JSON.

The strictest subcommand about its argument, because its output gets committed:

- `error: `<path>` is not a directory` when the path is not one
- `refusing to generate: the project has N error(s)` on any error finding
- `error: `<path>` declares no commands, projectors, effects or events, so there is nothing to
  describe; is this a hekla project directory?` when the project is empty

The document is the same value the server serves at `/openapi.json`.

## `hekla erase <SUBJECT_FIELD> <SUBJECT_VALUE> [DIR] [--data-dir PATH]`

Deletes one subject's key from the operational database. Irreversible, O(1), and immediately visible
across the log and every read model.

```
erased subject `customer_id` = `7`
no key for subject `customer_id` = `7` (already erased or never created)
```

Both are exit 0, and the second does not distinguish "already erased" from "never existed". No master
key is needed (it is a row delete), and **no lock is taken**, so it runs against a live server; the
next request that touches that subject sees the erasure, since the decrypt cache lives for one
request only.

It refuses a data directory with no database rather than creating one:
`error: no operational database at <path>/hekla.db`.

## `hekla rotate [DIR] [--data-dir PATH]`

Rewraps every subject key under the primary `HEKLA_MASTER_KEY`, unwrapping with
`HEKLA_MASTER_KEY_PREVIOUS` as needed. Ciphertext is untouched, so reads keep working throughout.

```
rewrapped 2 subject key(s) under the primary master
```

A second run rewraps 0 keys: everything is already under the primary. Failure modes:

- `error: HEKLA_MASTER_KEY must be set to rotate`
- ``error: no master `<id>` to unwrap subject `<field>``` when the key a row is wrapped under is
  neither the primary nor in `HEKLA_MASTER_KEY_PREVIOUS`. Nothing is written; fix the environment and
  run it again.
- the same missing-database refusal as `erase`.

**Rotating under a running server breaks it.** The process holds the masters it booted with, so once
the rows are rewrapped it can no longer unwrap them: reads of a sealed column answer 500 and
`/admin` reports the field `unreadable`. Either start the process with the new primary and the old in
`HEKLA_MASTER_KEY_PREVIOUS` before rotating, or restart it afterwards with the new key.

## Environment

| Variable | Read by | Means |
| --- | --- | --- |
| `HEKLA_MASTER_KEY` | `serve`, `verify`, `rotate` | base64 of 32 bytes; required if any field declares a `@subject` |
| `HEKLA_MASTER_KEY_PREVIOUS` | the same | comma-separated prior masters, for unwrapping during rotation |
| `HEKLA_MAX_ATTEMPTS` | `serve` | how many times a command re-decides after a DCB conflict before answering 409. Default 5, capped at 15, read once per process |
| `HEKLA_UI_DIR` | `serve` | serve the admin console's assets from this directory instead of the ones compiled in |
| `RUST_LOG` | `serve`, `verify` | tracing filter, default `info` |

## Every finding `hekla check` reports

Errors from hekla itself. Everything else in a run is a heklang diagnostic, which carries a code, a
span and a hint of its own (see the heklang skill's `reference/errors.md`). In particular the entity
key rules (a key that is optional, `Bool`, `Money`, `Json` or sealed) and an index over a field the
entity has not got are heklang's, and report with a span; hekla's own entity errors are the three
below, which report against the file and not a position.

| Message | Cause | Fix |
| --- | --- | --- |
| ``command `X` must be declared under commands/`` | a `command` outside `commands/` | move it; `commands/internal/` and any deeper nesting count |
| ``projector `X` must be declared under projectors/`` | a `projector` elsewhere | move it |
| ``effect `X` must be declared under effects/`` | an `effect` elsewhere | move it |
| ``entity `E`: filterable field `f` collides with a reserved read query param (one of: limit, cursor, after, timeout_ms)`` | a key or indexed column named like a read parameter | rename the column |
| ``entity `E`: index `i` covers subject-encrypted column `c`; filter by the plaintext subject id instead`` | an index over a column that receives sealed content | index the subject id instead |
| ``column `c` of entity `E` is sealed under `s`, so erasing that subject leaves it absent, but its declared type cannot be absent: make it optional`` | a sealed column typed `T` rather than `T?` | make it optional |
| ``event `t` field `_hekla_x` uses the reserved `_hekla_` prefix, which is hekla's own tag namespace`` | an event field in the runtime's tag namespace (located at `events`, with no file) | rename the field |
| ``hekla.toml: parsing <path>: ...`` | an unparseable or invalid config | the cause names the key |
| ``<file>: reading: ...`` / ``<path>: walking: ...`` | a file or directory the loader could not read | fix the permissions; a project that cannot be walked must not deploy |

Warnings, which never fail the check:

| Message | Cause |
| --- | --- |
| ``X folds `t` with no constraint on a high-cardinality field, so it guards a broad set of events; a boundary is best keyed on an entity id`` | a slice whose filters name no `Uuid`, `Int`, `String`, `Money` or `Timestamp` field |
| ``X constrains most of `t`'s fields, which looks like a copied `emit`; a slice is a subset match and over-constraining can match nothing`` | on an event of 4 or more fields, filters on 75% or more of them |
