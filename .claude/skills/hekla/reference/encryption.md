# Subject-scoped encryption and erasure

The language models a **seal**: a value carries the field, subject and id its key is filed under, and
only `reveal` reads it out. hekla is what makes that real. A field declared
`@subject(sibling_field)` is encrypted under a key scoped to `(subject_field, subject_value)` before
it reaches storage: in the event payload, in the tag index, and in any read-model column that
receives it.

The language never sees a ciphertext and the store never sees a plaintext. Everything below is
hekla's half.

## Keys

- One **subject key** per `(subject_field, subject_value)` pair, minted on first write, stored in
  `hekla.db` wrapped under the master.
- **`HEKLA_MASTER_KEY`** is 32 bytes, base64. Required at boot if the project declares any
  `@subject`, and boot fails with a subject-specific message when it is absent.
- **Losing the master is total, unrecoverable loss** of every subject-scoped value. Nothing else in
  the runtime fails this way.
- **`HEKLA_MASTER_KEY_PREVIOUS`** is a comma-separated list of prior masters, used to unwrap rows that
  have not been rewrapped yet. `serve`, `verify` and `rotate` all read it.
- Encryption is deterministic (AES-SIV), which is what lets an encrypted tag be matched at all. It
  leaks equality and frequency, so it is right for a high-cardinality id and **wrong for a
  low-cardinality field**: do not give a status enum a subject.

## What a subject field must look like

- **The subject id itself stays plaintext.** It is how the runtime finds the key, and after an erasure
  the log still shows `guest_id: 7` with the personal fields unreadable. That is standard
  crypto-shredding.
- **Per field, not per event.** An event with a customer and a shop wants `email` under the customer
  and `order_total` under the shop, so erasing one leaves the other's record intact.
- **A sealed field wants to be optional** (`String?`), because an erased value reads back absent. A
  projector column that receives sealed content and is not optional is a `hekla check` error.
- **A column's subject is propagated, never declared.** No entity column is written `@subject`: a
  column that receives sealed content becomes sealed, which is what lets a projector store a
  credential it may never read.
- **An index over a sealed column is refused.** A filter arrives as plaintext and could never match
  ciphertext without the subject; filter by the plaintext subject id instead.
- **A field appended with no subject can never be erased**, and nothing warns about it. Which fields
  are personal is a judgement hekla cannot make from a name, so decide it on day one.
- **There is no cross-subject uniqueness on a sealed field.** "One account per email" would need
  equality over two ciphertexts. Keep a plaintext handle beside the sealed address and fold on that;
  erasing the subject does not reopen the handle it claimed.

## Sealing a record, a list or a map

`@subject(...)` takes any declared type, not just a scalar. The seal's text for a composite is the
JSON document rule 8 already writes, and `reveal` parses it back, so an effect gets an `Address` and
reads `.city` off it.

```hek
record Address { line1: String @max(200), city: String @max(100), postcode: String @max(10) }

event @order.placed {
  order_id: Uuid,
  customer_id: Int,
  ship_to: Address @subject(customer_id),
}
```

This is the shape to reach for. The alternative is one sealed `String` per part, where adding a tenth
part is a schema-evolution event on every event that carries one, and a `@subject` field takes `?` and
never `@absent`.

- **The annotation goes on the event field, never inside the record.** `@subject(x)` names a sibling
  holding the id, and a field reached through a container has no sibling to name. A record *type* on
  a subject-bound event field is fine; `@subject` on a field of a `record` declaration is refused.
- **A leaf that looks like a number stays text.** The document quotes its own leaves, so a badge of
  `"0042"` reveals as `"0042"` rather than as `42`.
- **A nested `Timestamp` is epoch microseconds, in the payload seal and the column seal alike.** Only
  a *top-level* `Timestamp` column is rewritten to RFC 3339, and nothing rewrites inside a document.
- **It goes whole or not at all.** An erased subject's record column reads back absent, the same as a
  scalar's; there is no half-parsed object.
- **A field added to the record since a seal was written reads as its `@absent` literal**, in an
  effect's `reveal` and in a read-model column alike. That is what makes growing a subject-bound
  record safe: the plaintext is behind a key, so no migration is available even in principle.
- **A seal whose text is not the document its declaration promises wedges the lane and retries.** That
  is a field whose type changed under the log, and it is deliberately *not* terminal: an erased
  subject is unrecoverable, while a mismatch has its plaintext intact behind a live key and one edit
  to one `.hk` line fixes it. `GET /admin/events/{position}` still shows the raw text, because the
  operator diagnosing the wedge is the one who needs to read it.

Needs heklang 0.9 or newer. Against 0.8 a composite seal reveals as a mismatch and the lane wedges.

## Erasure

`hekla erase <field> <value> <dir>` from the CLI, or `erase(...)` from an effect arm. It deletes the
key. One O(1) operation makes every value scoped to that subject unreadable and unmatchable across the
log and every read model at once, with no rewrite, compaction or index rebuild.

What each surface does afterwards:

| Surface | After an erasure |
| --- | --- |
| `GET /read/...` | the column is **omitted** from the row, exactly as an absent value is |
| `GET /admin/events/...` | `data` keeps the stored ciphertext, and `subjects.<field>.state` is `erased` |
| a projector rebuild | writes the column NULL: no read path ever mints a key |
| an effect's `reveal` | fails the invocation **terminally**, which completes that position and advances |
| `GET /admin/subjects/{field}/{value}` | `state: absent`, indistinguishable from never-created |
| an external system | unaffected. Erasure cannot un-send an email an effect already delivered |

The CLI form takes no lock, so it works against a running server, and the next request sees it: the
decrypt cache lives for one request only.

**Erasure is a point-in-time shred, not a tombstone.** A later event writing the same subject's field
mints a fresh key, so values written after the erase are readable while everything before it stays
shredded. Values written under the superseded key report `stale` rather than `erased`.

## Rotation

`hekla rotate` rewraps every subject key under the primary master, unwrapping with the previous ones
as needed. Ciphertext is untouched, so the data does not move and reads keep working.

The order that matters:

1. Start (or restart) the server with the new key as `HEKLA_MASTER_KEY` and the old one in
   `HEKLA_MASTER_KEY_PREVIOUS`.
2. Run `hekla rotate` with the same environment.
3. Drop `HEKLA_MASTER_KEY_PREVIOUS` once a second `rotate` reports `rewrapped 0 subject key(s)`.

**Rotating under a process that does not have the new key breaks it**: it can no longer unwrap the
rewrapped rows, so reads of a sealed column answer `500 internal` and `/admin` reports the field
`unreadable`. The server log names the master id it is missing. Restarting with the new key fixes it;
nothing is lost.

## Where plaintext exists

Only at the edges: a command's HTTP input (the client supplied it), a read-API response, an effect's
`reveal(...)`, and `GET /admin/events...` with decryption on. Everywhere in between it is ciphertext.

Three places decrypt, and all three fail the same way once a key is gone. `/admin` is the widest of
them (every field of every event, rather than the columns one projector materialised), which is why a
decrypting request there writes an audit line and the console fetches one event at a time.

A journaled call's **arguments are never stored, only hashed**, so introspection cannot resurrect
plaintext an erasure was meant to shred.

## Testing it

`hekla test` runs against a real key store with a fixed master key, so an erasure case is worth
writing here in a way it would not be in an in-memory harness:

```hek
erased guest_id "7"
project Bookings
expect Booking["1111..."] { email: none }
```

The column really holds AES-SIV ciphertext and the key is really deleted. `expect skipped` is the
effect-side counterpart: an arm that hits a shredded key.
