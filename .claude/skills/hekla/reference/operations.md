# Operations

## Storage

```
data/
  events/                tephra segments, the immutable source of truth
  projectors/{Name}.db   one SQLite database per projector: read-model tables and its checkpoint
  hekla.db               the operational DB: effect journals, subject keys, declarations
```

Backup is copying the directory of a **stopped** process. A `cp -r` of a live one is not
crash-consistent (SQLite WAL, a segment mid-append), and the sweep would then report divergence the
copy caused. Projector databases are rebuildable from the log, so only `events/` and `hekla.db` are
irreplaceable.

Opening those files directly is not a supported surface; the table layout is private.

**One process per data directory.** The runtime takes an exclusive lock and refuses to start
otherwise, and `hekla verify` takes the same lock. `hekla erase` and `hekla rotate` do not.

## Projectors

Each projector runs one sequential task, is the only writer of its database, and commits its rows and
its checkpoint in one transaction, so state and position cannot disagree.

A projector records the hash of its **definition** (its subscription and entity schema, not its
handler bodies) inside its read model. At startup the recorded hash is compared with the current one,
which decides its readiness:

| Readiness | Means | Reads | Resolves by |
| --- | --- | --- | --- |
| `ready` | on-disk model matches the definition | served | |
| `rebuilding` | definition changed, rebuild in flight | 503 `rebuilding` with `Retry-After` | itself |
| `stale` | definition changed, `auto_rebuild = false` | 503 `stale` naming the replay route | an operator replay |
| `rebuild_failed` | a rebuild ran and failed | 503 `rebuild_failed` pointing at `last_error` | fixing the cause, then a replay |
| `quarantined` | an invariant check failed | 503 `quarantined` | an operator, after looking |

`/status` reports `readiness` beside `position`, `lag`, `running`, `failed`, `replays_completed` and
`replays_failed`. `running` separates a projector idling for an operator from one whose thread is
gone: a replay posted to the latter is refused with `503 not_running` ("has stopped; see its
`last_error` in /status, then restart the server") rather than accepted and dropped.

A replay is rebuild-and-swap: build a fresh database from position 0, seal it, rename it in. A reader
that opens the file mid-swap never sees a torn one, and state and position move together.

Changing an entity's columns, its indexes, its key or a projector's set of handled event types is a
definition change. Editing what a handler does is not, and needs a replay if the rows should change.

## Effects

One reader thread per effect owns its subscription; the invocations themselves run on a process-wide
worker pool, one lane at a time. An event's `@key` names its lane: one lane processes in log order,
different lanes do not wait for each other. An effect slower than its arrival rate falls behind, and
that lag is the correct behaviour rather than an unbounded queue.

**A wedged lane does not block another key.** A failing invocation records itself, parks its lane
until the backoff expires and returns its worker, so the position stays at the head of that lane's
queue and every other lane keeps running. This is the fix for one unprocessable event stalling an
entire effect; the parallelism is a bonus, and `[effects] pool_size = 1` still gets it.

### The states

`/status` and `/admin/effects` derive one word, in this order:

| State | Means |
| --- | --- |
| `quarantined` | a verify-mode check found a divergence. Nothing clears it on its own |
| `blocked` | an arm's `@key` changed while lanes were outstanding. Stopped, waiting for a person; nothing is retrying |
| `wedged` | `consecutive_failures > 0`: a lane is retrying under backoff, or the driver is re-subscribing after a store error |
| `lagging` | position below the log head, with no failures |
| `healthy` | caught up |

Quarantine outranks the rest because one restored from an earlier process has a zero failure count.
`blocked` outranks `wedged` because nothing is retrying: a block clears only when someone redeploys.
A wedge outranks lag because a wedged effect lags precisely because it is wedged.

### Reading the counters

- **`position` is the durable low-water mark**: the highest position *every lane* has passed. A wedge
  at position 3 shows `position: 2` even when positions 4 and 5 in other lanes are long finished.
- **`pinning_key` names the lane holding it down** and `pinning_position` is where. That position is
  what an operator skip takes, and the key is what makes lag actionable: a partitioned effect can lag
  by thousands while every lane but one is healthy.
- **`wedged_lanes`** is how many lanes are stuck. `consecutive_failures` counts the pinning lane's
  attempts and cannot say how many lanes are in that state.
- `consecutive_failures` and `last_error` are the wedge. `last_error` carries the arm's own source
  location, which is usually enough to name the call that will not complete.
- `terminal_skips` and `last_terminal_error` are the opposite: work that was abandoned deliberately
  and advanced past. An author's `fail(...)` and a `reveal` of an erased subject both land here.
- `retry_in_ms` is how long until the next attempt.


### When an effect will not start (`blocked`)

An arm's `@key` changed while lanes were still outstanding, so the per-lane rows above the
watermark are keyed under a scheme the new key never produces. `last_error` names the event types
whose lane moved and how many lanes are outstanding. The way out:

- **Redeploy the previous key and let it drain.** Watch `/admin/effects/{Name}` until
  `wedged_lanes` is 0 and `lag` is 0, then deploy the new key. This costs nothing.
`hekla plan` reports the repartition before the deploy, so this is avoidable rather than something
to discover at boot. The block is an operator signal rather than a correctness gate: reprocessing
under a new key skips rather than re-fires, because `begin_invocation` is the authority on what has
run and rows above the mark are never swept. What stopping buys is that a repartition is noticed by
whoever caused it.
### Retries, and where they happen

Two loops, deliberately separate. **heklang re-sends** on a transport error and on every retryable
status (408, 425, 429 and any 5xx), so those never reach an arm and a `status >= 400` in an arm is
always a real decision. **hekla retries the invocation** when that loop is exhausted or a host call
fails: capped exponential backoff from 200ms doubling to 60s, forever, replaying journaled calls each
attempt so nothing already performed fires again.

A `Retry-After` on a retryable response raises the invocation's backoff (delta-seconds, capped at 5
minutes) and never lowers it. The header never reaches a program.

One HTTP attempt is capped at 10s to connect and 30s overall. Neither is configurable.

### Getting past a wedge

1. `pinning_key` names the lane and `pinning_position` is where it is stuck. Read `last_error` and
   that invocation's journaled calls (`/admin/effects/{Name}/invocations/{position}`).
2. Fix the cause. A code fix plus a restart replays the running invocation, and every completed call
   comes back from the journal instead of firing again.
3. If the event is genuinely unprocessable, `POST /effects/{Name}/skip/{position}`, naming
   `pinning_position`. The driver honours it only for a position that has already failed. Several
   requests can be pending at once, one per wedged lane, and a request the watermark passes is
   forgotten. Nothing is ever skipped automatically.
4. Erasing the subject a `reveal` needs also clears it, by turning the failure terminal.

Worth doing promptly for a reason beyond the lag figure: see the retention note below.

### The journal

Every impure call looks itself up in the journal first: a recorded result is returned, otherwise the
call is performed and its result appended. Journal rows and the terminal record commit call by call
in autocommit, never in one transaction per invocation, which is what makes a crash mid-arm
recoverable.

The key is the call itself (for HTTP, the verb, the URL and the body) plus an ordinal separating
identical repeated calls, stored as a sha256. It is not a sequence number, so editing or reordering an
arm does not corrupt replay: the failure mode of editing during a deploy is "a different path was
taken", not "a side effect fired twice". Duplicates need an edit to the URL or body of a call that
already fired.

`log(...)` and `reveal(...)` are not journaled. A duplicated log line is harmless; a replayed
`reveal` would serve stale plaintext against a destroyed key.

Journals live in the operational DB, never in the event log. A sweeper runs hourly and deletes
completed invocations older than `[retention] effect_journal_days`, **bounded by the effect's persisted
watermark**: positions above it are exactly what the next boot replays, and reclaiming them would let
their side effects fire twice. An effect that has never persisted a watermark is never swept.

**A wedged lane therefore holds retention down for every lane.** The watermark cannot pass a stuck
position, so a lane wedged for a month makes a month of journal rows unsweepable across the whole
effect. The lag figure is not the thing that hurts; this is. `fail(...)` and an operator skip are
what resolve it, which is why `/status` names the pinning key.

### Deploys

Graceful shutdown stops dispatching new invocations and waits up to 30 seconds for in-flight ones, so
the common deploy has nothing in flight. An invocation abandoned by the timeout stays `running` and
replays at the next start. An in-flight invocation whose recorded source hash differs from the code on
disk is logged by name at startup.

## Invariant checks

Three invariants, two entry points.

- **Rebuild equivalence**: a projector rebuilt from position 0 matches the live one row for row,
  compared exactly (subject encryption is deterministic, so ciphertext compares). The rebuild is
  bounded at the live model's own checkpoint. Offline only.
- **Replay equivalence**: a recorded invocation re-run from its journal reaches the same calls in the
  same order and performs none of them. It runs against a **sealed** host: a journal miss is a
  violation rather than a call, so the check is structurally incapable of causing the double-fire it
  hunts.
- **Checkpoint monotonicity**: no position reached by tailing moves backwards. A rebuild publishes its
  checkpoint without this guard, since a bounded rebuild legitimately lands behind.

`hekla verify <dir>` sweeps offline, takes the lock, and exits non-zero on a violation: run it against
a stopped instance or a copy of the directory, which exercises the backup at the same time.
`serve --verify` (or `[verify] enabled = true`) runs the per-operation half continuously, at the cost
of a second handler run per completed invocation.

**A violation quarantines the component.** It stops advancing, `/status` names what broke, its reads
answer 503, and the rest of the runtime keeps serving. A quarantine is durable: it is restored at the
next boot, so a restart does not clear it.

## Concurrency and limits

- Commands run on a blocking pool; a DCB conflict re-decides up to `HEKLA_MAX_ATTEMPTS` times
  (default 5, capped at 15) with jittered backoff before answering 409.
- One task per projector, one reader thread per effect, and one process-wide pool running effect
  lanes. `[effects] pool_size` bounds that pool. One pool rather than one per effect, because the
  contended resource is the single operational-database mutex every journaled call goes through.
- Nothing bounds a runaway program because nothing can run away: heklang has no `while`, rejects
  recursion, and iterates only finite containers.
