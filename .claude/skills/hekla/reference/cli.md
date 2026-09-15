# The CLI

One binary, eleven subcommands, `<dir>` defaulting to `.` everywhere. `hekla --version` and
`hekla <subcommand> --help` work.

Logging is `tracing` behind `RUST_LOG`, default `info`. `serve` and `verify` initialise it; the other
subcommands print with `println!` and are unaffected. An unparseable `RUST_LOG` is reported on stderr
(`warning: ignoring RUST_LOG="...": ...`) and the run falls back to `info` rather than pretending the
filter took.

Log lines carry ANSI colors only when stdout is a terminal, so a redirect into a file, a systemd
journal or a CI log is already clean with no flag. `--no-color` turns them off for a terminal too,
and `NO_COLOR` set to a non-empty value does the same for an operator who cannot edit the command
line. `--no-color` is global: it is accepted before or after the subcommand, and it only means
anything to `serve` and `verify`, which are the two that log.

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
store with a fixed master key, and a stubbed network driven by `respond`.

**The envelope over that world is heklang's.** An id and an append time are what a runner invents,
not what a world observes, so `hekla test` synthesises exactly what `hek test` does, and a case
reading `e.at`, `e.id` or `now()` gets one answer from both:

- the append time of the event at position *n* is `2020-01-01T00:00:00Z` plus *n* minutes, so
  `given` events are a minute apart and a `created_at: e.at` column is assertable;
- `now()` reads the instant the next append will be stamped with, which is the epoch plus one minute
  per `given` event;
- the id of the event at position *n* is `0190d1a1-0000-7000-9000-` followed by *n* padded to twelve
  digits, so a `Uuid.derive(e.id, ...)` is assertable.

A case that asserts one of these and passes under only one runner is a bug in hekla, not in the case.

Exit is 1 on a failing test and 1 on a project with error findings. **A project with no tests prints
`0 passed, 0 failed` and exits 0.**

## `hekla serve [DIR] [--addr ADDR] [--data-dir PATH] [--verify] [--no-color]`

Loads the project, refuses to serve if any finding is an error (`refusing to serve: the project has
N error(s)`), then runs the runtime and the HTTP API.

- `--addr` defaults to `127.0.0.1:8080`. It must parse as a socket address; a bare port or a hostname
  is `error: invalid --addr`.
- `--data-dir` defaults to `<dir>/data`, and is created if absent.
- `--verify` turns on the continuous invariant check for this run. It cannot turn off
  `[verify] enabled = true` from `hekla.toml`.
- `--no-color` drops the ANSI colors from the log. Redirected output has none either way.

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

## `hekla verify [DIR] [--data-dir PATH] [--no-color]`

The offline invariant sweep. Loads the project, refuses on error findings, then takes the
data-directory lock and checks rebuild equivalence, replay equivalence and checkpoint monotonicity
(see `operations.md`).

It makes the same three refusals `serve` does before sweeping anything, and for one reason: each
would otherwise turn into findings about a directory that is fine. A missing master key, an unset
credential, and a declaration that cannot read the log all produce a failed rebuild or a divergence
per invocation, which reads as corruption that is not there.

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

A **collapsed** position is counted there too, and on an `on latest` project it is usually the
largest of them: nothing ran at that position, because an `on latest` arm folded it into a later
invocation, so there is no journal of its own to reproduce. It is reported rather than left out so
that `checked + skipped` stays the number of positions the effect was delivered.

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

A type this program could not read is reported beside the credentials it has not got, in the same
words, because both are a deploy that would refuse to start rather than one that would change
something:

```
  event @booking.confirmed cannot be read here (no stored event can carry `channel`), so serving would refuse to start
    A field was added to an event that already has instances, and no payload written before it can carry one: say what it reads as with `@absent(<value>)`, or make it optional so the type itself says absence is possible.
```

In `--json` this is `unreadable`. It is `null` rather than `[]` when no event, record or enum moved
and the log was never opened, so a gate cannot read "not asked" as "asked and clean". Each entry
carries a `position` only when an event was actually decoded; the complete half of the check settles
from the recorded declarations and reads none, and reports `null` there.

**It answers a narrower question than the boot does.** Plan asks what *this deploy* would newly
break, from the diff; the boot asks whether the program can read the log at all, and asks it every
time. A directory an earlier deploy already broke plans clean and still refuses to boot. Exit is 1 if
the event log itself cannot be read, rather than reporting the diff and staying quiet about the
history question: a deploy gate that cannot answer "would this boot" should stop.

Without `--replay` it opens no event log at all, with one exception: the check above, which reads
the oldest stored event of each declared type when the diff says an `event`, `record` or `enum`
moved. That read goes through the same read-only follower `--replay` uses. Either way it takes **no
data-directory lock**, so it runs against a directory a server has open. It changes no database: the
operational DB is opened only after its schema version is read separately and found to match, and
read models are opened read-only.

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

## `hekla project <FILE> [DIR] [--data-dir PATH] [--projector NAME] [--entity NAME] [--from N] [--upto N] [--max-events N] [--rows N] [--no-decrypt] [--no-progress] [--json]`

Folds an undeployed projector over the event log once and prints its rows. `FILE` is a standalone
`.hk` file declaring one `projector`; it is compiled together with the project, folded into a read
model in a temporary directory, and that directory is deleted when the command returns.

```
$ hekla project ~/by-customer.hk . --data-dir /srv/hekla/data
folded 38214 event(s) of @order.placed into `projector ByCustomer` in 1.70s
  entity PerCustomer, key customer_id, 3 row(s)
    sealed: last_email under customer_id
    customer_id  orders  last_email
    1            21044   ada@example.test
    2            14903   -
    7            2267    grace@example.test
14,903 sealed column write(s) dropped across 1 erased subject(s), so those cells read absent
a snapshot at position 412903, the tip pinned when this opened; anything appended since is not in it
ok: 3 row(s) from 38214 event(s)
```

**Nothing is deployed and nothing is recorded**: no declaration row, no database under
`data/projectors/`, no checkpoint, no route, no metric. A later `hekla plan` cannot tell it ran.
It reads the log through the same read-only follower `hekla plan --replay` uses, so it takes **no
data-directory lock** and runs against a directory a server has open.

**The scratch file is compiled with the project.** That is what lets it name events it does not
declare, call the project's `fn` helpers and read its `const`s. Two consequences worth knowing:

- A projector in it need not sit under `projectors/`; that placement rule is relaxed for the scratch
  module alone. A `command` or an `effect` written there is still an error, because neither can be
  folded over anything.
- A name it shares with a deployed declaration is heklang's own duplicate-declaration diagnostic,
  reported against the scratch file rather than against the project's.

A file that lives inside the project directory is compiled once, not twice, so
`hekla project question.hk` from the project root works.

**Sealed columns.** A projection that seals a column needs `HEKLA_MASTER_KEY`, because a projector
re-seals under the column's own field name and reads its own stored loads back as plaintext. It is
refused up front rather than part-way through a scan:

```
error: this projection seals column `last_email` under `customer_id`, so folding it needs HEKLA_MASTER_KEY
       a projector re-seals a column under the column's own field name, so the fold cannot carry the log's ciphertext through untouched
```

`--no-decrypt` withholds only the *rendering*, like `/admin/events?decrypt=false`; the fold still
holds the key. An erased subject's column reads absent, and the count under the table comes from the
sink, which knows it dropped the write, rather than from an inference over the rows: an optional the
handler never wrote is never reported as an erasure. It counts **writes**, not rows, so a projector
that patches the same row on every event reports one drop per event.

**`--upto` bounds the answer, not the work.** tephra's forward read has no upper bound, so the read
is planned over the whole log and the window is applied as the fold walks it. A narrow `--upto` over
a large log costs what the whole log costs. `--max-events` is the bound that reaches the planner.

**Bounds always name what they left out**, because a bounded answer must not read like a complete
one. `--max-events` ends the report on `partial:` instead of `ok:` and JSON's `scanned.stopped` is
`"max-events"` instead of `null`; `--from`/`--upto` add a line saying which positions were not
folded; `--rows` prints `... and N more row(s)` and sets `truncated`.

Exit is 0 whenever the projection ran, whatever it found, for the same reason `plan` exits 0 on a
change: the rows are the answer, not a fault. It is 1 when `FILE` is not a `.hk` file, `DIR` is not a
directory, the project has error findings, the file declares no projector or more than one with no
`--projector`, the projector declares no handler, a sealed column has no master key, the data
directory holds no log or a schema version this build does not expect, or `--json` fails to
serialise. Findings go to stderr, so `hekla project q.hk . --json > out.json` writes only JSON.

**What it cannot see.** Only the event log, so effect invocations, journals, retry counts and
checkpoints stay `/admin`'s to answer. No `reveal`, since a projector holds no host, though it can
key and group on a sealed column because the encryption is deterministic. No checkpoint: every run
folds from the start of its window, so a question asked on every request is a deployment rather than
a projection. And nothing appended during the run, because the follower pinned its prefix when it
opened.

**Over HTTP** the same fold is `POST /admin/projections`, for a caller with no shell on the box. It
needs `[admin] projections = true` in `hekla.toml`, bounds the event budget itself rather than
letting the caller do it, and returns exactly what `--json` prints here. See
`reference/introspection.md`. A whole-log fold stays this command's job: it holds no request open
while it runs one.

## `hekla openapi [DIR]`

Prints the generated OpenAPI 3.1 document to stdout and every finding to stderr, so
`hekla openapi . > openapi.json` writes only JSON.

The strictest subcommand about its argument, because its output gets committed:

- `error: `<path>` is not a directory` when the path is not one
- `refusing to generate: the project has N error(s)` on any error finding
- `error: `<path>` declares no commands, projectors, effects or events, so there is nothing to
  describe; is this a hekla project directory?` when the project is empty

The document is the same value the server serves at `/openapi.json`.

## `hekla secrets [DIR]`

Every `secret` the project declares, where this machine reads it from, and whether it is set. One
line each:

```
  DISCORD_WEBHOOK  set      2633c771             file /run/secrets/discord
  SENTRY_DSN       unset    (optional)           env HEKLA_SECRET_SENTRY_DSN
  STRIPE_KEY       MISSING                       env STRIPE_LIVE_KEY
```

**Never a value.** The fingerprint is a short sha256 of the credential domain-separated by its
declared name, which is enough to tell staging from production and no use for anything else.

A source that is there and *unreadable* reports the reason rather than reading as unset, so an
operator is not sent looking for a file that is right in front of them:

```
  STRIPE_KEY       MISSING                       file /run/secrets/stripe (Permission denied)
```

Reads the project, `[secrets]` and the environment, and nothing else: no data directory, no log, no
lock, so it runs anywhere `check` does and against a live deployment. Exits 1 when a **required**
credential is unset, so it is a pre-deploy gate on its own for the thing `serve` would otherwise
refuse to start over. An unset `secret NAME?` is reported and exits 0.

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

## `hekla rewind <EFFECT> <POSITION> [DIR] [--data-dir PATH] [--live] [--yes]`

Moves an effect's watermark backwards so it reprocesses everything after `POSITION`, and performs
those side effects again. Irreversible. `0` reprocesses the whole log; the effect resumes *strictly
after* the position you name, which is the same meaning the watermark has everywhere else.

This is the only way back to history for an `on live` arm. Flipping the arm to `on` and redeploying
does not help: the boundary is already persisted, and it is not re-resolved on a later boot.

```
$ hekla rewind SendWelcome 0 ./project
effect `SendWelcome`
  watermark      412 -> 0
  live boundary  380 (unchanged; pass --live to lower it)
  discards       412 recorded invocation(s), and the journal rows behind them
  lanes          3 row(s) of per-lane progress
  arms
                 on @user.registered { @key user_id }
                 on live @user.deleted { @key user_id }  still declined below the boundary

This re-runs those positions and performs their side effects again.
Continue? [y/N]
```

**It takes the data-directory lock**, so it refuses while a server is running. That is not caution
for its own sake: a rewind against a live process would race the effect's in-memory mark and be
overwritten by the next publish, so it would appear to apply and quietly not.

**Deleting the recorded invocations is what makes it a rewind.** Without that, `begin_invocation`
reports every position already terminal and the effect sails straight over them; the command would
look like it worked and do nothing. It is also what makes those positions *re-fire*: the journal rows
cascade with their invocations, so the calls they recorded are performed again rather than replayed.
The journal will not save you here, and it would not have anyway: it is swept on a retention window.

`--live` also lowers the `on live` boundary. **Off by default**, because an author who wrote `on live`
declared that history must not fire, and a rewind aimed at a plain arm should not quietly re-send
every notification that arm declined. A pure-`live` effect rewound without the flag correctly does
nothing. Lowering it is permanent. `--live` on an effect with no `live` arm is an error rather than a
no-op, so a stale runbook flag against the wrong effect is visible.

`--yes` answers the prompt. It does **not** silence the summary: a deploy log should still show what
the rewind was about to do. Without a terminal and without `--yes` it refuses rather than prompting,
so a piped `yes` cannot arm it.

It also clears a quarantine above the target, since the diverging invocation is exactly what is being
discarded; leaving it would make the rewind a no-op the operator has to discover for themselves.

Rewinding **to** the current watermark is allowed and is not a no-op: the mark does not move,
but every `effect_lane` row and every recorded invocation above it goes. That is the form a
`blocked` effect needs, and the form its message names.

Refusals: a held lock, an effect the project does not declare (it lists the ones it does), a data
directory with no database, `--live` on an effect with no `live` arm, and a `POSITION` ahead of
the watermark (`... is at position N; M is ahead of it, not a rewind`, exit 0, nothing moved).

**Why this prompts when `erase` does not.** An erase carries its blast radius in its own arguments,
because you named the subject. `hekla rewind SendWelcome 0` tells you nothing about the four hundred
emails it is about to re-send. That asymmetry is the whole reason for the summary and the prompt.

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
| `HEKLA_SECRET_<NAME>` | `serve`, `verify`, `plan`, `secrets` | the fallback source for a declared `secret NAME` that `[secrets]` does not name. Never read by `check` or `test` |
| `RUST_LOG` | `serve`, `verify` | tracing filter, default `info`. An unparseable value is reported on stderr and ignored |
| `NO_COLOR` | `serve`, `verify` | any non-empty value drops the ANSI colors from the log, the same as `--no-color`. Empty is not an opt-out |

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
| ``X folds `t` on `f`, which carries `@absent`; an event written before that field existed has no tag for it, so this slice can only ever match events appended since`` | a slice filtering on a field that declares `@absent` |
| ``N is declared and nothing reads it, so this deployment is asked for a credential the program never uses`` | a `secret` no effect reads. Answered from the digest, so a credential only a `test` reaches counts as unread |
| ``[secrets] names `N`, which the project does not declare`` | a `hekla.toml` entry naming no `secret` declaration: a line that parses and does nothing |

**`hekla check` never reads the environment**, so a credential being *unset* is never a finding. That
is `hekla secrets`' question and `hekla serve`'s refusal. A CI gate that needed production
credentials to pass would either be run with them or be skipped, and both are worse than the problem.
