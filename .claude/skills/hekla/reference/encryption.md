# Subject-scoped encryption and erasure

The language models a **seal**: a value carries the field, subject and id its key is filed under, and
only `reveal` reads it out. hekla is what makes that real. A field declared `@subject(buyer)` is
encrypted under a key scoped to `(subject, subject_value)` before it reaches storage: in the event
payload, in the tag index, and in any read-model column that receives it.

The language never sees a ciphertext and the store never sees a plaintext. Everything below is
hekla's half.

## Subjects are declared

A subject is a declared type whose values are its ids, so the key namespace has a name of its own
rather than being conjured by spelling a field:

```hek
subject Customer(Int)

event @order.placed {
  order_id: Uuid,
  buyer: Customer,
  email: String? @subject(buyer) @max(200),
}
```

Two facts, and they are not the same one. `@subject(buyer)` names a **sibling field**, which is where
the id is read from and is local to this declaration. `buyer`'s type is `Customer`, which is the
**namespace**, and is what every key row is filed under. Rename the field and nothing moves; rename
the subject and every key row does.

That is why `hekla erase` takes `Customer 42` and not `buyer 42`, and why `from: Customer, to:
Customer` on one event is two ids under one namespace.

An id may be an `Int`, a `String` or a `Uuid` and nothing else, because a key is filed under text and
those are the three with one canonical form to file it as.

## Keys

- One **subject key** per `(subject, subject_value)` pair, minted on first write, stored in
  `hekla.db` wrapped under the master.
- **`HEKLA_MASTER_KEY`** is 32 bytes, base64. Required at boot if the project declares any `subject`,
  and boot fails with a subject-specific message when it is absent. Declaring one and sealing nothing
  still requires it: a project that declares a namespace will use it, and finding out at the first
  write is worse than finding out at boot.
- **Losing the master is total, unrecoverable loss** of every subject-scoped value. Nothing else in
  the runtime fails this way.
- **`HEKLA_MASTER_KEY_PREVIOUS`** is a comma-separated list of prior masters, used to unwrap rows that
  have not been rewrapped yet. `serve`, `verify` and `rotate` all read it.
- Encryption is deterministic (AES-SIV), which is what lets an encrypted tag be matched at all. It
  leaks equality and frequency, so it is right for a high-cardinality id and **wrong for a
  low-cardinality field**: do not give a status enum a subject.

## What a subject field must look like

- **The subject id itself stays plaintext.** It is how the runtime finds the key, and after an erasure
  the log still shows `buyer: 7` with the personal fields unreadable. That is standard
  crypto-shredding.
- **A subject id is an id and nothing else.** It stores, keys, indexes, ranges and pages exactly as
  the scalar its ids are, so `buyer: Customer @key` and `index (buyer, status)` both work. What it
  will not do is arithmetic, or mix with another subject.
- **Per field, not per event.** An event with a customer and a shop wants `email` under the customer
  and `order_total` under the shop, so erasing one leaves the other's record intact.
- **A sealed field wants to be optional** (`String?`), because an erased value reads back absent. A
  projector column that receives sealed content and is not optional is a `hekla check` error.
- **A column's subject is propagated, never declared.** No entity column is written `@subject`: a
  column that receives sealed content becomes sealed, which is what lets a projector store a
  credential it may never read. The column holding that subject's ids has to be on the same entity,
  or hekla refuses the projector at load: without it there is no id to find the key with.
- **An index over a sealed column is refused.** A filter arrives as plaintext and could never match
  ciphertext without the subject; filter by the plaintext subject id instead.
- **A subject-typed column that is not the key needs a default**, because a subject id has no zero:
  customer 0 is a customer rather than an absence, the same argument `Uuid` and `Timestamp` make
  about nil and epoch-zero.
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

`hekla erase <Subject> <id> <dir>` from the CLI, naming the subject by its declared name, or
`erase(id)` from an effect arm, where the value's type is the namespace and nothing is named at all.
It deletes the key. One O(1) operation makes every value scoped to that subject unreadable and unmatchable across the
log and every read model at once, with no rewrite, compaction or index rebuild.

What each surface does afterwards:

| Surface | After an erasure |
| --- | --- |
| `GET /read/...` | the column is **omitted** from the row, exactly as an absent value is |
| `GET /admin/events/...` | `data` keeps the stored ciphertext, and `subjects.<field>.state` is `erased` |
| a projector rebuild | writes the column NULL: no read path ever mints a key |
| an effect's `reveal` | fails the invocation **terminally**, which completes that position and advances |
| `GET /admin/subjects/{subject}/{value}` | `state: absent`, indistinguishable from never-created |
| a subject **under** the erased one | the same, everywhere: its key was wrapped under this one |
| an external system | unaffected. Erasure cannot un-send an email an effect already delivered |

The CLI form takes no lock, so it works against a running server, and the next request sees it: the
decrypt cache lives for one request only.

**Erasure is a point-in-time shred, not a tombstone.** A later event writing the same subject's field
mints a fresh key, so values written after the erase are readable while everything before it stays
shredded. Values written under the superseded key report `stale` rather than `erased`.

## A subject can be deleted with its tenant

A subject may declare a parent, and then its keys are wrapped under the parent's:

```hek
subject Shop(Int)
subject Customer(Int) under Shop
```

Deleting the shop's key row is still **one row delete**, and every customer key beneath it becomes
unopenable at the same instant. There is no walk, no second write, and no projector whose job is to
enumerate a shop's customers so they can be erased one at a time. That enumeration, and the 50,000
journal rows it produced, is what `under` removes.

**Every event sealing under a child must carry its ancestors' ids.** The runtime learns which key to
wrap under from the event in front of it and has nowhere else to look, so an event with a
`Customer`-sealed field and no `Shop` field is refused: by `hek check` at the annotation, and again
by hekla at load, before a deployment starts.

**What a parent gives up.** Per-field subjects are sold on the opposite property: erasing a shop
provably cannot touch a customer's `email`. Under a parent it does, deliberately, and there is no way
to have both for one pair of subjects. Declare the parent when "delete this tenant" is a thing you
must be able to do, and not otherwise.

**What nothing can check.** The parent relation is asserted per event. If one event says customer 88
is in shop 7 and another says shop 9, the key was minted once, under whichever arrived first;
deleting shop 9 then leaves customer 88 readable, and deleting shop 7 destroys data the author
believes is shop 9's. Both events are individually well formed, so neither heklang nor hekla has
anything to point at. Getting the parent right is the author's.

### Declaring a parent onto keys that already exist

A key row's wrapping is decided the first time the subject is seen and never revisited, so adding
`under Shop` to a `Customer` that already has rows would leave every one of them wrapped under the
master: new customers hang from their shop, older ones do not, and erasing the shop reports success
having missed them.

**hekla adopts them at boot**, before it serves anything, and refuses to serve if it cannot. A
settled store pays one indexed count for the question, so this is free once it is done.

The parent comes from the log rather than a guess. Every event sealing under a child carries its
ancestors' ids in plaintext, so hekla folds the log and files each key under the parent the
**earliest** such event named, which is the same rule a write would have followed. Events written
before the parent was declared are covered too, because adding the ancestor's field to an event type
that already has instances is itself refused unless the declaration says what the old payloads mean:

```hek
subject Customer(Int) under Shop

event @order.placed {
  customer_id: Customer,
  // Added with the parent. Orders written before this belong to shop 1.
  shop_id: Shop @absent(1),
  email: String? @subject(customer_id) @max(200),
}
```

`hekla adopt` runs the same pass ahead of a deploy, against a live directory and without taking the
lock, so a large migration need not happen during a boot. `hekla plan` reports how many keys would
move, and `hekla verify` reports any that have not.

Three things it deliberately does not do. It never mints a new secret: the wrapping moves and the key
does not, so every value already sealed stays exactly as readable as it was. It never re-parents a
row that is already a child, because a parent that disagrees with the declaration is the
"whichever arrived first" case above and re-filing it on sight would fight that rule on every write.
And it never moves a child back to the master when `under` is removed, because that would narrow a
shred somebody may already rely on.

### Erased, or tampered with

A child's key is wrapped under a key derived from its parent's secret, so a wrapping that
will not open has two possible causes and the runtime tells them apart rather than
guessing. Each key row records which *generation* of its parent it was wrapped under: a
domain-separated digest of that parent's secret, which carries no key material.

| What happened | How it reads | Why |
| --- | --- | --- |
| the parent's row is gone | **erased** | nothing will ever derive that key again |
| the parent was erased and written to again | **erased** | the generation recorded on the child does not match the live one, so this row predates the shred |
| the generation matches and the wrapping still will not open | **unreadable**, and a `500` from the read API | the key it was wrapped under has not moved, so the bytes were altered outside hekla |

The distinction is worth the column. Collapsing the first two into "unreadable" would `500`
every read of a legitimately shredded row and wedge its write path for good; collapsing the
third into "erased" would tell an operator their shred worked when what actually happened
is that somebody wrote to the key store.

### What it costs

- **O(1) deletes and O(1) journal rows, not O(1) storage.** The child rows stay on disk holding bytes
  nobody can read. The hourly retention sweep reclaims them, in the same bounded chunks it sweeps the
  effect journal with, repeating until a pass finds nothing so a grandchild is reached once its
  parent is gone. Nothing waits on it: what it reclaims is already unreadable.
- **A rotation gets cheaper, not dearer.** `hekla rotate` rewraps roots only. A child's wrapping key
  is derived from its parent's *secret*, and a rotation rewraps that secret under a new master
  without changing it, so every child stays correctly wrapped without being touched.
- **`hekla erase` prompts now.** It used not to, on the grounds that naming the subject bounded the
  blast radius. A subject with children makes that false, so it prints a summary (including how many
  keys go with it) and asks, exactly as `hekla rewind` does. `--yes` skips the question and never the
  summary.
- **An unreachable child row is replaced, not repaired.** A child that outlives its parent's erasure
  holds ciphertext nobody can ever open. Writing that subject again mints a fresh key over the row
  rather than failing: the old content is already unrecoverable, so refusing would protect nothing
  and would wedge the write path. The replacement is a compare-and-set on the bytes the writer
  actually read, so two writers racing to replace one dead row cannot delete each other's fresh key.
- **An erase landing mid-write sends that write round again**, rather than committing content that is
  unreadable the moment it lands. The retry is bounded; a subject being erased and recreated in a
  tight loop fails the write loudly instead of hanging.

## Rotation

`hekla rotate` rewraps every **root** subject key under the primary master, unwrapping with the
previous ones as needed. Ciphertext is untouched, so the data does not move and reads keep working.
A nested subject's key is derived from its parent's secret rather than wrapped under a master, so it
needs no rewrap and the count reports only the roots.

The order that matters:

1. Start (or restart) the server with the new key as `HEKLA_MASTER_KEY` and the old one in
   `HEKLA_MASTER_KEY_PREVIOUS`.
2. Run `hekla rotate` with the same environment.
3. Drop `HEKLA_MASTER_KEY_PREVIOUS` once a second `rotate` reports `rewrapped 0 subject key(s)`.

**Rotating under a process that does not have the new key breaks it**: it can no longer unwrap the
rewrapped rows, so reads of a sealed column answer `500 internal` and `/admin` reports the field
`unreadable`. The server log names the master id it is missing. Restarting with the new key fixes it;
nothing is lost.

## Upgrading a directory written before a subject was a type

Only for a data directory last written by hekla 0.6.0 or earlier that has sealed at least one field.
Nothing else meets this, and a project that never declared a `@subject` never meets it at all.

A key used to be filed under the **field** an `@subject` annotation named. It is now filed under the
declared subject's own name. The wrapped secret carries across the migration untouched; the label
does not, and nothing stored records which field became which subject. A row under a label the
program never looks up is unreachable, and everything sealed under it reads back `absent`, which is
precisely what an erased subject reads back. The two are indistinguishable by design, so this
presents as a clean start serving data that quietly looks deleted.

So the upgrade **refuses** until it is told the mapping, naming every namespace it found:

```
$ HEKLA_V10_SUBJECTS="customer_id=Customer,shop_id=Shop" hekla serve ./shop --data ./data
```

A refused upgrade writes nothing, including the schema version, so the previous release still opens
the directory and still reads the data while the mapping is worked out. A namespace meant to keep the
spelling it has is written as itself, `legacy_ref=legacy_ref`; that is also how a namespace is
abandoned, and it stays unreachable. Two namespaces may merge into one subject, which is what an
annotation renamed mid-life leaves behind, unless both hold a key for the same id: one subject files
one key per id, so that merge would have to drop a secret and everything sealed under it.

The right-hand side is taken on trust. The migration runs before any program is loaded, so there are
no declarations to check a subject name against; a mistyped one leaves rows unreachable exactly as
carrying them blind would. The difference is that it is now a thing an operator wrote down rather
than a thing that happened to them. See `reference/cli.md` for the full rules.

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
erased Guest "7"
project Bookings
expect Booking["1111..."] { email: none }
```

The column really holds AES-SIV ciphertext and the key is really deleted. `expect skipped` is the
effect-side counterpart: an arm that hits a shredded key.

`erased` and `expect erase(Guest, "7")` both name a **key row**, which is a namespace and the id as a
host files it, so they read as a pair rather than as a value. That is the same pair the CLI takes.
The `erase(id)` statement in an arm names a value instead, and the value's type is the namespace.
