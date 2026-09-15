# Introspection: `/admin` and the console

Nothing here changes the deployment; `replay` and `skip` stay outside the prefix. All of it is a
`GET` but one, and that one, `POST /admin/projections`, writes nothing either: it folds a projector
that is never deployed into a read model that is thrown away. Always served, because the bind
address is already the boundary for a surface that appends events without authentication, and one
prefix is what a proxy can deny. The one exception to "always served" is the projections `POST`,
which needs `[admin] projections = true` and answers `403` otherwise.

`GET /admin` is an index of everything below, and `hekla serve` prints the URL at startup.

## The routes

| Route | Answers |
| --- | --- |
| `/admin/events` | the log, newest first |
| `/admin/events/{position}` | one event: envelope, payload, subject states, tags |
| `/admin/traces/{correlation_id}` | every event of one causal chain, and the invocations in it |
| `/admin/effects`, `/admin/effects/{name}` | durable state per effect |
| `/admin/effects/{name}/invocations` | invocations, newest first |
| `/admin/effects/{name}/invocations/{position}` | one invocation and every call it journaled |
| `/admin/projectors`, `/admin/projectors/{name}` | readiness, entity shapes, definition hash |
| `/admin/commands`, `/admin/commands/{name}` | every command and its parameters, internal ones included |
| `/admin/schema` | the loaded project: every declaration with its hash and signature hash |
| `/admin/system` | version, uptime, data directory, op-DB schema version, keystore, effective config |
| `/admin/subjects`, `/admin/subjects/{field}/{value}` | which subjects still hold key material, never the material |
| `/admin/projections` | `GET`: the limits, and whether the `POST` is served. `POST`: fold an ad-hoc projector over the log |
| `/admin/assets/{file}` | the console's own files; the one path under the prefix that is not negotiated |

## `/admin/events`

```
?type=user.registered   repeatable; types OR together
?tag=email:a@b.com      repeatable; tags AND together
?cursor=<position>      the previous page's next_cursor
?direction=forward      oldest first; the default walks backward
?limit=<n>
```

Each event carries `position`, `type`, `declared`, `event_id`, `correlation_id`, `causation_id`,
`timestamp`, `data`, `subjects`, `tags` and `hekla_tags`. `declared: false` means the log holds a type
the loaded project no longer declares, which is a fact about a deployment, not corruption.
`hekla_tags` are the runtime's own `_hekla_` tags, kept here rather than stripped, because what an
operator is looking at is what the log holds.

**Payloads are decrypted by default.** `?decrypt=false` shows the stored ciphertext instead. A
decrypting request writes an audit line in the server log, which is why the console fetches lists
undecrypted and one event decrypted.

Each subject-scoped field reports its own state, so one unreadable value marks one field rather than
failing the request:

| State | Means |
| --- | --- |
| `decrypted` | `data` holds plaintext |
| `encrypted` | nothing was attempted: `?decrypt=false`, or no master key is configured |
| `erased` | the subject has no key. Irreversible; `data` holds ciphertext forever |
| `stale` | the subject has a key, but this value was written under a superseded one (erased, then recreated by a later event) or is corrupt |
| `unreadable` | the key could not be obtained at all: a corrupt wrapping, or a master that is not configured (this is what a rotation the process did not see looks like). The server log names it |

```json
"subjects": {
  "email": { "subject": "guest_id", "subject_value": "7", "state": "decrypted" }
}
```

## `/admin/effects/{name}`

```json
{ "name": "SendWelcome", "state": "wedged", "position": 1, "watermark": 1, "lag": 3,
  "consecutive_failures": 8, "last_error": "effects/send-welcome.hk:11:20: ...",
  "wedged_lanes": 1, "pinning_key": "i:4471", "pinning_position": 2,
  "live_boundary": 0, "live_suppressed": 0, "latest_collapsed": 0,
  "last_terminal_error": null, "terminal_skips": 0, "quarantined": false, "quarantine": null,
  "retry_in_ms": 39719, "sources": ["user.registered"] }
```

`position` is the in-memory watermark and `watermark` is the persisted one (`null` when the effect has
never persisted one, which means the next boot replays from position 0). Both are **low-water marks**:
the highest position every lane has passed, not the newest thing finished.

`pinning_key` is the partition key of the lane holding that mark down and `pinning_position` is where
it is stuck: the position an operator skip takes. `wedged_lanes` says how many lanes are stuck at
all, which `consecutive_failures` (the pinning lane's attempts) cannot. `live_boundary` is the log
head at this effect's first activation here, which `on live` arms decline at or below;
`live_suppressed` counts what they have declined since this process started, and
`latest_collapsed` counts the positions an `on latest` arm has folded into another invocation, so lag
falling without a matching number of invocations reads as the arm doing what it declared.

`sources` is the event types the arms name, and it is always a list: there is no way to subscribe to
everything.

## `/admin/effects/{name}/invocations[/{position}]`

The list gives one row per invocation: `position`, `status` (`running` or `terminal`), `created_at`,
`completed_at`, `script_hash`. Under lanes several rows can be `running` at once, one per lane, so
**the stuck position to act on is `pinning_position`** rather than whichever running row is newest,
skipping the newest would not release the watermark.

One invocation adds its journaled calls:

```json
{ "position": 1, "status": "terminal", "script_hash": "a55e18...",
  "calls": [ { "seq": 0, "kind": "http", "disambiguator": 0,
               "call_hash": "6fc04166...", "created_at": "...",
               "result": { "status": 405, "body": null } } ],
  "next_cursor": null }
```

`kind` is one of four, each with its own `result` shape:

| `kind` | `result` |
| --- | --- |
| `http` | `{"status": 200, "body": <json>}`; `body` is `null` when the response body was not JSON |
| `invoke` | `{"ok": true, "code": null, "message": null}`; a rejection is `ok: false` with the refusal's code |
| `now` | `{"micros": <epoch micros>}` |
| `erase` | `{}` |

`kind` is `null` on a row written before the runtime recorded it, and unrecoverable then: it exists
only inside the hash pre-image.

**A call's arguments are never stored, only hashed**: storing them would let plaintext that came out
of a `reveal()` outlive the erasure of the subject it belonged to. So an invocation view reports what
came back and never what was sent, and which command an `invoke` targeted is not recoverable from it.

The call list pages with `?cursor=`, so a truncated list never reads as the whole sequence.

## `/admin/subjects`

The inventory: per subject field, how many live keys; per subject, when its key was created and which
master it is wrapped under (`master_key_id`). Never the key material itself.

`/admin/subjects/{field}/{value}` answers 200 either way, with
`{"subject_field": ..., "subject_value": ..., "state": "live"}` or `"state": "absent"`. `absent` does
not distinguish erased from never-created: after a shred there is nothing left to tell them apart
with.

## `/admin/projectors/{name}`

Readiness, lag, position, entity shapes and the `definition_hash` the read model was built under.
`?counts=true` adds a row count per entity, which is a full scan and so opt-in.

A field's `kind` is the type as declared, with the optional marker on the type and any constraint
after it: `String? @max(200)`, `Money(2)?`, `Timestamp`, `Json`. An enum reports its variants rather
than its name, bracketed when optional: `(Low | Normal | Urgent)?`.

In a browser this same URL is also where the console browses the rows themselves; see *Browsing a
read model* below.

## `/admin/commands/{name}`

```json
{ "name": "RegisterUser", "internal": false, "path": "commands/register-user.hk",
  "hash": "b5f6a780…",
  "input": [ { "name": "user_id", "kind": "Uuid", "optional": false } ] }
```

The same object `/admin/schema` lists under `commands`, from the same renderer. `internal: true` means
`commands/internal/`: an effect reaches it through `invoke_command` and `POST /commands/{name}` answers
404, so it is described here and absent from the generated OpenAPI document.

## `/admin/schema` and `/admin/system`

`/admin/schema` is the project this process loaded, including internal commands, each command's input
kinds, and every declaration with the file it came from, its `hash` (what it does) and its
`signature_hash` (what of it is visible outside). Events, enums, records and `fn`s are in there too,
not just the three module kinds. It is how to tell what a running server is actually executing.

`/admin/system` reports `version`, `uptime_seconds`, `data_dir`, `opdb_schema_version`, `log_head`,
`verify`, the keystore (`configured`, `master_key_ids`), the declared `secrets`, and the **effective**
`hekla.toml`.

`secrets` is an inventory and never the material, exactly as `master_key_ids` is: per credential a
`name`, whether it is `optional`, the `source` this process read it from (`env NAME` or `file PATH`),
`resolved`, and a `fingerprint` (a short sha256 domain-separated by the declared name, enough to tell
staging from production and no use for anything else). Every entry a running process reports is
resolved, because a required one that was not would have stopped it booting. `/admin/schema` carries
the same list, since a `secret` is a declaration.

## `/admin/traces/{correlation_id}`

Every event of one causal chain: the command's own events plus anything an effect appended in
reaction, transitively, with the invocations that produced them. It pages, and a chain longer than one
page reports `complete: false` with a cursor.

Only events appended by a version of hekla that stamps the correlation tag are findable: a query
filters on tags, and the id has always been in the envelope but not always in a tag.

## `/admin/projections`

Fold an ad-hoc projector over the log and get its rows back, deploying nothing. The same thing
`hekla project` does from a shell, for a caller that has no shell.

`GET` always answers, whether or not the `POST` is served:

```json
{ "enabled": true,
  "max_events": { "default": 100000, "limit": 5000000 },
  "rows": { "default": 50, "limit": 500 },
  "log_head": 412903 }
```

`POST` takes the heklang source as the body and its knobs as query parameters:

```sh
curl --data-binary @question.hk \
  'localhost:8080/admin/projections?max_events=100000&rows=20'
```

`projector` (when the body declares more than one), `entity`, `from`, `upto`, `max_events`, `rows`,
`decrypt`. An unknown parameter is a 400 rather than ignored, because a typo in `max_events` costs
the bound the caller was trying to impose. The response body is **exactly** what
`hekla project --json` prints, so one shape reaches both.

**Off by default.** Set `[admin] projections = true` in `hekla.toml`; otherwise the `POST` is a 403
naming the setting. It is the one route that runs code a caller supplied rather than code the
project declares, which is why a deployment has to say yes to it. What bounds it once on: heklang is
total, so a projection terminates, and a projector holds no clock, no network, no general read and
no way to append.

**Bounded by the server.** `max_events` defaults to 100,000 and is clamped to 5,000,000 whatever is
asked for, because this folds a log where the rest of `/admin` reads a page. A run that spends its
budget reports `scanned.stopped: "max-events"` and its rows are part of the answer, not all of it.
Folding a whole log is `hekla project`'s job, which holds no request open while it does one.

**Compiled against what the process booted with**, never re-read from disk. A module edited under a
running server is not visible to a projection, which is deliberate: it would otherwise typecheck
against declarations the process is not running and then misread stored payloads.

A compile failure is a 400 carrying the compiler's own diagnostics:

```json
{ "error": { "code": "invalid_input", "message": "the projector does not compile" },
  "findings": ["error: <projection>:3:5: event @user.nope is not declared"] }
```

## The console

The same URLs, chosen by `Accept`. `text/html` gets the console; everything else, `*/*` included, gets
the JSON byte for byte. So `curl localhost:8080/admin/effects/SendWelcome` is JSON and opening that
URL in a browser is the effect's view of it. Responses carry `Vary: Accept`. An unrouted `/admin/...`
is a 404 in both representations.

The console is compiled into the binary: plain ES modules plus one vendored 13KB runtime, served from
`/admin/assets/{file}`, no network and no build step. `HEKLA_UI_DIR=./ui` serves the assets from disk
instead, for editing them without a recompile.

Two things it does that the raw API does not: it can post a replay or a skip (each behind a
confirmation that makes you type the module's name), and it decrypts one event at a time, so one audit
line means one operator read one event.

### Running a command

`/admin/commands` lists them; opening a public one gives a form generated from its parameters, and
`Run` posts `POST /commands/{Name}`. A plain button, not the typed confirmation `replay` and `skip`
carry: a command is the application's front door, and the console is exactly as powerful here as
`curl` against the same port.

The form is worth more than a `curl` snippet because it makes the wire rules structural. A `Money(n)`
leaves as a decimal string, `now` on a `Timestamp` emits an offset-carrying RFC 3339, and an `Int` past
2^53 keeps its digits (the body is built as JSON text, so nothing round-trips through a JS number).

- An empty **optional** is omitted. The `JSON` tab is the override for the other accepted form (an
  explicit null), for an empty string, and for anything else: it is seeded from the form on every
  switch and what is in it is what gets posted.
- `Idempotency-Key` and `X-Correlation-Id` are under `headers`. Running twice with the same key
  replays the first commit's response verbatim and appends nothing.
- A **422 is the command working**: it folded its boundary and declined, the code is a declared
  `refusal`, and nothing was appended. It renders as a refusal, not a failure. A 409 offers a retry.
- A committed run links each appended position to its event and the correlation id to its trace.
  `positions: null` means the command decided to append nothing, which is a success.
- An **internal** command opens the same page without a form, since nothing outside the process can
  post to it.

### Browsing a read model

`/admin/projectors/{Name}` describes each entity's shape; `Rows →` on an entity card opens its data
beneath it. The rows are fetched through the public read API rather than through `/admin`, so the page
shows what an application sees over the same port: key order, one indexed filter, cursor paging, and
subject columns decrypted. The selection lives in the query string, so a row is a link:

```
/admin/projectors/CustomerOrders?entity=Order&field=customer_id&value=42&cursor=<c>&row=<key>
```

- The filter is a select over every column, with the ones that are not indexed disabled, so the read
  API's `unindexed_filter` 400 is visible before you can hit it.
- A column the response omits reads as `null` where only null is possible, and `absent` where the
  subject's key may be gone. The read API drops both cases, and only the declaration says which one a
  given column can be in.
- A row's panel links to the events that built it, when a source event type declares the entity key as
  an **indexed and unscoped** field. An unindexed field is not a tag, and a subject-scoped one is
  tagged with its ciphertext, so neither can be matched from a plaintext key; the panel says so rather
  than linking to an empty result.
- Rows do not refresh on the 3s poll (a scan is not a `/status` read); the header carries a `⟳`.
- A projector that is rebuilding, stale or quarantined cannot serve rows at its current definition.
  The section reports the server's own message, which names the fix, rather than an error.

| Key | Does |
| --- | --- |
| `⌘K` / `Ctrl-K` | jump to a position, a correlation id, an effect, a projector, or a view |
| `j` / `k` | move the row cursor |
| `Enter` | open the row |
| `Esc` | close the drawer or dialog |
| `/` | focus the filter |
