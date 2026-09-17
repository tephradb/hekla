# The generated HTTP surface

Every route is generated from the project's declarations. `{Name}` is always a declared name, never a
file stem. Nothing is authenticated: whoever reaches the port can append events and steer the
runtime, so the bind address is the boundary and it defaults to `127.0.0.1:8080`.

## Wire types

A command's parameters and a read model's columns are typed by the declaration. The JSON shapes:

| Declared | On the wire |
| --- | --- |
| `Bool`, `Int` | JSON boolean, JSON number |
| `Decimal(n)`, `Money(n)` | **string** (`"120.00"`), in and out |
| `String` | string |
| `Uuid` | string (`"11111111-1111-1111-1111-111111111111"`) |
| `Timestamp` as a command parameter | RFC 3339 string **or** integer epoch microseconds |
| `Timestamp` in a read response and as a read filter | RFC 3339 string (`"2026-06-01T00:00:00Z"`) |
| `Json` | any JSON value |
| `T?` | the value, or `null`, or absent |

A `Timestamp` is the one type with two input forms, because it has two output forms: heklang's own is
epoch microseconds (what a tag holds, and what an event's payload carries), and RFC 3339 is what a
read model's column stores and every read serves. Both are accepted on the way in, so a value read
out of a row posts straight back. RFC 3339 must carry an offset (`Z` or `+00:00`); text without one,
or text that is not a timestamp at all, is a `400 invalid_input`:
`` `night`: expected Timestamp, stored text that is not RFC 3339 ``.

An **absent value is omitted from a read response**, not serialised as `null`.

## `POST /commands/{Name}`

Public commands only. The body is the parameters as a JSON object; an empty body is an empty object,
so a command with no parameters needs none.

Headers:

| Header | Effect |
| --- | --- |
| `Idempotency-Key` | a repeat replays the first commit's response verbatim, body ignored |
| `X-Correlation-Id` | used as the correlation id when it parses as a uuid, otherwise a fresh one |

Success (200):

```json
{
  "correlation_id": "eaa1d143-...",
  "causation_id": "425d5c99-...",
  "positions": { "first": 1, "last": 1 },
  "events": [
    { "type": "booking.made",
      "tags": ["booking_id:1111...", "guest_id:7", "night:1780272000000000", "rate:120.00", "room_id:12"] }
  ]
}
```

`events[].tags` are the plaintext tags only: a subject-scoped field's tag is ciphertext and is
omitted, as are the two `_hekla_` tags. An event's *fields* never appear in a response.

**A command that decided to do nothing also returns 200**, with `"events": []` and
`"positions": null`.

Errors are `{correlation_id, causation_id, error: {code, message}}`:

| Status | `code` | When |
| --- | --- | --- |
| 400 | `invalid_input` | a parameter that is unknown, missing, or the wrong type, a body that is not a JSON object, **and** the author's own `invalid "..."` (the message is theirs) |
| 422 | the refusal's name in snake_case | `reject <Name>`; `refusal RoomTaken` becomes `room_taken`, with the refusal's message |
| 409 | `concurrency_conflict` | the boundary kept changing for `HEKLA_MAX_ATTEMPTS` attempts (default 5) |
| 404 | `not_found` | ``no public command `X` ``: unknown, or declared under `commands/internal/` |

A hot boundary usually resolves inside the retry budget: 24 concurrent requests for one contended
room-night answer 1 × 200 and 23 × 422 under the default budget, and 1 × 200 and 23 × 409 with
`HEKLA_MAX_ATTEMPTS=1`. A 409 means "retry"; a 422 means "the world says no".

## `GET /read/{Projector}/{Entity}/{key}`

```json
{ "item": { "booking_id": "1111...", "room_id": 12, "night": "2026-06-01T00:00:00Z" }, "position": 1 }
```

`position` is the projector's own log position at the moment of the read. Missing row: `404
not_found` with the message `no such row`. Unknown entity or projector: `404 not_found` naming which.

## `GET /read/{Projector}/{Entity}`

```json
{ "items": [ ... ], "next_cursor": "NTU1NTU1NTUt...", "position": 4 }
```

| Query parameter | Means |
| --- | --- |
| `<field>=<value>` | equality filter; the columns given must be a **prefix** of one declared index (or the key) |
| `<field>.gte=`, `.gt=`, `.lte=`, `.lt=` | range on the column **after** that prefix; one column only, both ends may be given |
| `limit` | page size, default 50, clamped to 1..500 (so `limit=0` is one row, not an error) |
| `cursor` | the previous page's `next_cursor`; opaque (it is the key, base64) |
| `after` | wait until the projector reaches this log position before reading |
| `timeout_ms` | how long `after` waits, default 5000, capped at 30000 |

`next_cursor` is `null` on the last page. Pagination is cursor-based over the key, never offset.

`index (shop_id, status, month)` answers `?shop_id=`, `?shop_id=&status=` and all three, plus
`?shop_id=&status.gte=` and `?shop_id=&status=&month.lt=`. It refuses `?status=&month=`: reaching
those rows means visiting every shop. The chosen index is pinned with `INDEXED BY`, so a filter the
handler admits is one SQLite cannot decide to answer with a scan.

Two things follow that are worth knowing before declaring:

- **A range needs a column whose order means something.** `Money` is stored as its decimal string
  (so `>=` would sort `"2"` above `"10"`) and an enum as its variant's spelling (so a range would
  walk the alphabet, not the severity). Those, plus `Bool`, `Json` and optional columns, take
  equality only, and asking for a range on one is a 400 naming the declared type.
- **Declare the narrow index too if you filter on it alone.** The key is appended to every generated
  index, so a filter using all of an index's columns is a pure seek; a shorter prefix leaves a
  declared column between the filter and the key and SQLite sorts the match set on every page. The
  runtime picks the narrowest index that serves a filter, so `index (shop_id)` beside
  `index (shop_id, status)` makes both questions cheap.

Errors are `{error: {code, message}}` (no correlation ids outside `/commands`):

| Status | `code` | When |
| --- | --- | --- |
| 400 | `unindexed_filter` | ``filter on (a, b) is not a prefix of any declared index``, ``filter field `f` is not indexed; declare an index on it``, ``filter field `f` is not a column of entity `E` ``, or `a scan ranges over one column` |
| 400 | `invalid_input` | `limit must be a positive integer`, `cursor is not valid`, ``filter `f`: expected an integer``, ``unknown filter operator `.between` ``, or ``filter `f` is Money(2), which has no order a range could use`` |
| 404 | `not_found` | no such projector, entity or row |
| 503 | `not_caught_up` | the `after` wait timed out, with `Retry-After: 1` |
| 503 | `rebuilding` | a rebuild is in flight; carries `Retry-After: 1` because it resolves on its own |
| 503 | `stale` | the definition changed and `auto_rebuild` is off; the message names `POST /projectors/{Name}/replay` |
| 503 | `rebuild_failed` | a rebuild ran and failed; the message points at `last_error` in `/status` |
| 503 | `quarantined` | an invariant check stopped the projector; its rows are what cannot be vouched for |
| 500 | `internal` | a sealed column could not be decrypted at all (a master key that is not configured); the server log names the field |

**Read-your-writes is opt-in.** A read issued immediately after a command legitimately 404s: the
projector is asynchronous. Pass `?after=<positions.last>` when the client needs its own write.

## Operator routes

Both are outside `/admin`, and both are the only non-`GET` routes besides `/commands`.

| Route | Answers |
| --- | --- |
| `POST /projectors/{Name}/replay` | `202 {"projector": "...", "status": "replay_scheduled"}`; 404 if unknown; 503 `not_running` if the projector's thread is gone |
| `POST /effects/{Name}/skip/{position}` | `202 {"effect": "...", "position": N, "status": "skip_scheduled"}` |

A skip is **recorded, not validated**: any position answers 202, including one that does not exist.
The driver honours it only for a position that has already failed at least once, and only one request
is pending at a time, so a second call replaces the first.

## `GET /status`

The operational snapshot. Not a liveness probe: it opens the log head and every module's counters.

```json
{
  "log_head": 4, "events": 4, "uptime_seconds": 71, "verify": false,
  "commands": { "public": ["RegisterUser"], "internal": ["RecordWelcome"] },
  "projectors": [ { "name": "Users", "position": 4, "lag": 0, "readiness": "ready",
                    "running": true, "failed": false, "last_error": null,
                    "replays_completed": 0, "replays_failed": 0 } ],
  "effects": [ { "name": "SendWelcome", "position": 0, "lag": 4, "state": "wedged",
                 "consecutive_failures": 9, "last_error": "effects/send-welcome.hk:11:20: ...",
                 "wedged_lanes": 1, "pinning_key": "i:4471", "pinning_position": 2,
                 "last_terminal_error": null, "terminal_skips": 0, "quarantined": false } ]
}
```

An effect's `position` is its **durable low-water mark**, the highest position every lane has passed,
not the invocation it is working on. `pinning_key` names the lane holding it down, and
`wedged_lanes` counts every lane that is wedged, the pinning one included, without naming any of
them: this is the summary, and `GET /admin/effects/{name}` carries a `stuck_lanes` entry per lane.
See `operations.md` for what each state and counter means.

**There is no rewind endpoint, and that is deliberate.** Taking an effect back over history is
`hekla rewind`, CLI only, against a stopped process: an effect declaring `on live` is one whose author
said history must not fire, and those are exactly the effects where an accidental request would
re-send every notification the log has ever seen.

`GET /health` is `{"status": "ok"}` and nothing else.

## `GET /metrics`

The Prometheus text exposition format (`text/plain; version=0.0.4`), on the same port and
unauthenticated like everything else. Every gauge is read off the running modules at scrape time, so
there is no collector interval and nothing to configure; there is no `[metrics]` section in
`hekla.toml` and no flag.

Series are `hekla_*`. Gauges: `log_head_position`, `uptime_seconds`, `build_info{version}`,
`module_info{kind,name,hash}`, and per module `projector_up`/`effect_up`, `_position`, `_lag`,
`projector_readiness{state}` and `effect_state{state}` as state sets, plus `effect_wedged_lanes`,
`effect_consecutive_failures` and `effect_retry_backoff_seconds`. Counters: `commands_total{command,
outcome}`, `command_refusals_total{command,code}`, `command_conflict_retries_total{command}`,
`events_appended_total{event}`, `projector_events_total`, `projector_rebuilds_total{outcome}`,
`effect_invocations_total{outcome}`, `effect_restarts_total`, `effect_terminal_skips_total`,
`effect_live_suppressed_total`, `effect_collapsed_total`, `effect_http_requests_total{outcome}`,
`reads_total{projector,entity,outcome}` and `read_waits_total{projector,outcome}`.

Two things to know before writing a query against it:

- **A lane key is never a label.** `/status` and `/admin/effects` name the lane pinning an effect's
  mark; the scrape reports `hekla_effect_wedged_lanes` as a count and stops there. A lane key is a
  partition key, and a scrape is a copy taken somewhere `hekla erase` cannot reach. Every other label
  is a declaration for the same reason, so nothing here is computed from a request or an event.
- **A refusal series appears on first use.** heklang inlines a `refusal`, so hekla cannot enumerate
  the codes to prime them at boot; use `or vector(0)` over `hekla_command_refusals_total`. Every
  other counter reads `0` from the first scrape.

`docs/monitoring/hekla-alerts.yml` in the hekla repository has alerting rules with the reasoning for
each threshold.

## `GET /openapi.json` and `GET /docs`

The document is generated from the loaded project by the same code `hekla openapi` runs, so a
committed spec and the served one cannot disagree. It carries:

- one path per public command, with a real request body schema from its parameters
- two paths per projector entity, with the key typed from the key column and one query parameter per
  filterable field
- the operator, status and `/admin` routes
- `components/schemas/entity.{Projector}.{Entity}` for read responses
- `components/schemas/event.{type}` **documenting the log, not any wire shape**: an event's fields
  never appear in a response. The declared event set is load-bearing in exactly one place, the enum of
  `EmittedEvent.type`

Internal commands are absent, because they are not routed.

## `/admin` and the console

Nothing here changes the deployment, and it is served from the same URLs as the JSON: a request whose
`Accept` names `text/html` gets the console, anything else (including `*/*`, which is what curl and a
bare `fetch()` send) gets the JSON byte for byte. Responses carry `Vary: Accept`. An unrouted
`/admin/...` is a 404 even for a browser. See `introspection.md`.

All of it is a `GET` but one. `POST /admin/projections` folds an ad-hoc heklang `projector` over the
log and returns its rows, deploying nothing and writing nothing; it needs `[admin] projections =
true` in `hekla.toml` and answers 403 otherwise. The body is the source as `text/plain`, the knobs
are query parameters, and the response is what `hekla project --json` prints.

It is also the one response that can arrive in pieces. `Accept: application/x-ndjson` streams the
fold: one JSON object per line, a `{"progress":{…}}` tick while it works, and the projection itself
as the last line, identical to the buffered body. Any other `Accept` gets that buffered body and
nothing changes for it. Not SSE and not a subscription: one request, no reconnection, no fan-out,
and a bounded channel that drops ticks rather than growing. The status code still means what it
says, because the source is compiled and the window checked before the body opens.
