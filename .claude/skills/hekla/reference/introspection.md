# Introspection: `/admin` and the console

Read-only. Every route is a `GET` and none of them writes; `replay` and `skip` stay outside the
prefix. Always served, because the bind address is already the boundary for a surface that appends
events without authentication, and one prefix is what a proxy can deny.

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
| `/admin/schema` | the loaded project: every declaration with its hash and signature hash |
| `/admin/system` | version, uptime, data directory, op-DB schema version, keystore, effective config |
| `/admin/subjects`, `/admin/subjects/{field}/{value}` | which subjects still hold key material, never the material |
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
  "last_terminal_error": null, "terminal_skips": 0, "quarantined": false, "quarantine": null,
  "retry_in_ms": 39719, "sources": ["user.registered"] }
```

`position` is the in-memory watermark and `watermark` is the persisted one (`null` when the effect has
never persisted one, which means the next boot replays from position 0). `sources` is the event types
the arms name, and it is always a list: there is no way to subscribe to everything.

## `/admin/effects/{name}/invocations[/{position}]`

The list gives one row per invocation: `position`, `status` (`running` or `terminal`), `created_at`,
`completed_at`, `script_hash`. **This is where a wedge actually shows**: the stuck position is the one
with `status: running`, while `/status` still reports the older watermark.

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

## `/admin/schema` and `/admin/system`

`/admin/schema` is the project this process loaded, including internal commands, each command's input
kinds, and every declaration with the file it came from, its `hash` (what it does) and its
`signature_hash` (what of it is visible outside). Events, enums, records and `fn`s are in there too,
not just the three module kinds. It is how to tell what a running server is actually executing.

`/admin/system` reports `version`, `uptime_seconds`, `data_dir`, `opdb_schema_version`, `log_head`,
`verify`, the keystore (`configured`, `master_key_ids`) and the **effective** `hekla.toml`.

## `/admin/traces/{correlation_id}`

Every event of one causal chain: the command's own events plus anything an effect appended in
reaction, transitively, with the invocations that produced them. It pages, and a chain longer than one
page reports `complete: false` with a cursor.

Only events appended by a version of hekla that stamps the correlation tag are findable: a query
filters on tags, and the id has always been in the envelope but not always in a tag.

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

| Key | Does |
| --- | --- |
| `⌘K` / `Ctrl-K` | jump to a position, a correlation id, an effect, a projector, or a view |
| `j` / `k` | move the row cursor |
| `Enter` | open the row |
| `Esc` | close the drawer or dialog |
| `/` | focus the filter |
