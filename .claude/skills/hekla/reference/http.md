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
| 400 | `invalid_input` | a parameter that is unknown, missing, or the wrong type, a body that is not a JSON object, **and** the author's own `invalid("...")` (the message is theirs) |
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
| `<field>=<value>` | filter; only the key and declared indexes are filterable, and **only one at a time** |
| `limit` | page size, default 50, clamped to 1..500 (so `limit=0` is one row, not an error) |
| `cursor` | the previous page's `next_cursor`; opaque (it is the key, base64) |
| `after` | wait until the projector reaches this log position before reading |
| `timeout_ms` | how long `after` waits, default 5000, capped at 30000 |

`next_cursor` is `null` on the last page. Pagination is cursor-based over the key, never offset.

Errors are `{error: {code, message}}` (no correlation ids outside `/commands`):

| Status | `code` | When |
| --- | --- | --- |
| 400 | `unindexed_filter` | ``filter field `f` is not indexed; declare an index on it``, or `only a single indexed filter field is supported` |
| 400 | `invalid_input` | `limit must be a positive integer`, `cursor is not valid`, or ``filter `f`: expected an integer`` and its siblings |
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
                 "last_terminal_error": null, "terminal_skips": 0, "quarantined": false } ]
}
```

An effect's `position` is its **durable watermark**, not the invocation it is working on. See
`operations.md` for what each state and counter means.

`GET /health` is `{"status": "ok"}` and nothing else.

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

Read-only, `GET` only, and served from the same URLs as the JSON: a request whose `Accept` names
`text/html` gets the console, anything else (including `*/*`, which is what curl and a bare `fetch()`
send) gets the JSON byte for byte. Responses carry `Vary: Accept`. An unrouted `/admin/...` is a 404
even for a browser. See `introspection.md`.
