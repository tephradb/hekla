# Handoff: unique claims over erasable values

**Status: designed, not built. Blocked on heklang.**

This is the brief for a feature neither half has yet. The language surface in section 4 is
heklang's to decide and build; sections 5 to 9 are hekla's and are written so the runtime
half can be reviewed before a line of either exists.

It came out of a thread in the DCB Discord (7 to 9 October 2026) where Jef, Bastian
Waidelich, Gerard and John Bywater spent three days on "how does an event-sourced system
check whether an email is already in use, when email is PII". Their best answer was a
truncated keyed hash used as a tag, accepting collisions. Section 3 says why hekla can do
better, and section 10 records what was rejected so nobody re-derives it.

---

## 1. The one sentence everything serves

> **A uniqueness claim is current state, not history.**

It is derived, it changes, and the entire premise of the feature is that it must be
deletable. An append-only log is the one structure in hekla that is definitionally wrong
for it. Every design that failed, in the thread and here, failed by putting the claim in
the log and then looking for a way to take it back out.

## 2. The limitation this closes

hekla can enforce "this value is unique across everyone", or it can erase that value with
one key delete. Not both on the same field.

A `@subject(customer_id)` field is encrypted under a key scoped to `(subject, subject_value)`,
so customer 1's `alice@example.com` and customer 2's encrypt to different ciphertext. The
tag index can answer "every event about customer 1's address" and can never answer "does
anyone else already hold this address". `ARCHITECTURE.md` records it as an accepted
limitation:

> **There is no cross-subject uniqueness on a sealed field.** `unique = True` used to mint a
> tag under a never-erased global key, so one email could be matched across every account and
> stayed matched after erasure.

The two examples in this repository are the two horns, and nothing does both:

| | `examples/users` | `examples/orders` |
| --- | --- | --- |
| `email` | plaintext, tagged, folded on | `@subject(customer_id)`, sealed |
| unique across accounts | yes | no |
| erasable | **never**, by any mechanism hekla has | one key delete |

The case that needs both is a product where the identity *is* the PII: an app that owns its
own signup. A Shopify app never meets it, because Shopify assigns the identities and the
app only carries the PII hanging off them. That is why this has not bitten in production and
why it is deferred rather than urgent.

## 3. The design in one picture

Give the value two representations with different powers, and put each where it belongs.

```
                      the value: alice@example.com
                       /                        \
   HMAC(K_lookup, normalised)              sealed under the holder's key
              |                                        |
     hekla.db `claim` table                      the event log
     UNIQUE (namespace, token)              ciphertext, per-account
              |                                        |
   matchable across accounts,              readable by `reveal`, never
   never readable, deleted on              matchable across accounts,
   release or erase                        shredded by the existing erase
```

Jef's framing in the thread was that immutability, erasure and searchability have
incompatible needs and cannot be combined in one tag. They cannot. So do not try: the
registry is searchable and mutable, the log copy is readable and immutable, and neither can
be derived from the other without a key.

`K_lookup` is the secret of the `_hekla_global` key row that `src/crypto.rs:48` still carries
from the deleted `unique = True`. It is already non-deletable by design, already wrapped under
the master, and already survives `hekla rotate` unchanged, because a rotation rewraps a secret
without altering it. So tokens stay stable across a master rotation for free.

## 4. The heklang half (the ask)

Four additions. Each is additive; nothing already shipped has to change meaning.

### 4.1 A `unique` declaration

A namespace of claimable values, with an optional canonicalisation rule.

```hek
unique OrderRef(String)                      // exact match
unique Email(String) via canonical_email     // an equivalence class
```

The value type is `String`, `Int` or `Uuid`, the same three a subject id may be, and for the
same reason: a key is filed under text and those are the three with one canonical form to
file it as.

`via` names a `fn` whose signature must be `(T) -> T` for the namespace's `T`. Section 9
argues for a named `fn` over a keyword list and over an inline body, and that argument is
the load-bearing part of this section: the rule has to sit in the digest or the migration
story in section 7.3 is not computable.

The `via` keyword is a coin flip with `normalised by`. `via` matches the single short
preposition in `subject Customer(Int) under Shop`; `normalised by` reads better to someone
meeting the declaration for the first time, which is most readers. heklang's call.

### 4.2 Two field annotations

```hek
event @account.registered {
  account_id: Account,
  // One field, two representations. The registry gets a token; the log gets
  // ciphertext under this account's key.
  email: String? @subject(account_id) @claims(Email) @max(200),
  name: String @max(120),
}

event @account.email_released {
  account_id: Account,
  email: String? @subject(account_id) @releases(Email) @max(200),
}
```

Rules `hek check` should carry:

- `@claims(N)` and `@releases(N)` require `N` to be a declared `unique` namespace whose type
  matches the field's.
- The field must also carry `@subject(...)`. Without the sealed copy the registry cannot be
  rebuilt (section 6), and a plaintext claim in the log is the problem this feature exists to
  remove. This is the one genuinely non-obvious rule and it should be an error, not a warning.
- The event must carry the holder's subject id, which `@subject(...)` already forces.
- A namespace with a `@claims` and no `@releases` anywhere in the program is a **warning**, in
  the same family as the existing boundary-too-broad and boundary-too-narrow warnings. A
  namespace nothing ever releases is one where a changed address is stuck forever, and that is
  almost always a modelling omission rather than a choice.

### 4.3 A `claim` declaration in a command

A declared read resolved before the body, in the same position and with the same contract as
`fold`: what you declared is what the commit conditions on.

```hek
refusal EmailTaken "that email is already registered"

command RegisterAccount(account_id: Account, email: String, name: String) {
  claim held: Bool = Email(email)

  if held {
    reject EmailTaken
  }

  emit @account.registered { account_id, email, name }
}
```

The body stays pure. `held` is an input resolved by the host, exactly as a fold's value is.

**It returns `Bool`, not the holder.** Returning the holder would let anyone who can post a
command probe an address and learn which account has it. An author who needs "is it mine"
can fold their own events for it.

### 4.4 `erase` already covers the release

`erase(account_id)` frees every claim that subject holds. No new statement. The cascade is
hekla's (section 7.1); heklang needs only to keep `erase` as it is.

### 4.5 Test surface

`hekla test` needs to seed and assert registry state. The shape that matches the existing
`erased Guest "7"`:

```hek
test "an erased account frees its address" {
  given @account.registered {
    account_id: "1111...", email: "Alice@Example.com", name: "alice",
  }
  erased Account "1111..."
  run RegisterAccount {
    account_id: "2222...", email: "alice@example.com", name: "bob",
  }
  expect @account.registered { account_id: "2222..." }
}
```

That case is worth writing because it pins three things at once: the cascade from `erase`,
the canonicalisation (`Alice@Example.com` and `alice@example.com` are one claim), and that a
freed address is genuinely re-claimable rather than tombstoned.

## 5. The hekla half: the table

Beside `subject_key` in `hekla.db`:

```sql
CREATE TABLE claim (
    namespace TEXT    NOT NULL,  -- the declared `unique` name
    token     TEXT    NOT NULL,  -- HMAC(K_lookup, namespace || canonical value)
    holder    TEXT    NOT NULL,  -- the subject id holding it
    root      TEXT,              -- the holder's outermost ancestor, for a tenant erase
    position  INTEGER,           -- the event that claimed it; NULL while deciding
    PRIMARY KEY (namespace, token)
);
CREATE INDEX claim_by_holder ON claim (namespace, holder);
CREATE INDEX claim_by_root   ON claim (root);
```

The primary key is the uniqueness. Not a tag, not a fold, not a hash bucket: a `UNIQUE`
constraint, which is the primitive actually built for this and which has been sitting
underneath hekla the whole time.

`claim` declares itself in `opdb::POSITIONED_TABLES`, so `backup::clamp_to_log` trims rows
above a restored head with no new code. `src/backup.rs:1118` already fails a test when a new
table does not declare its stance, so this cannot be forgotten.

**Do not truncate the token.** The Discord thread truncated deliberately so collisions would
blur identity. Here a collision is a correctness bug: two addresses sharing a row means
releasing one frees the other. Use the full width. The privacy comes from the secret, and
from the row being in a store you delete from rather than in an immutable log.

## 6. The commit protocol

`claim held: Bool = Email(email)` does not compile to a read. It compiles to an attempted
insert, so the check and the reservation are one atomic operation and there is no
read-then-act window anywhere:

```sql
INSERT INTO claim (namespace, token, holder, position)
VALUES ('Email', :token, :account_id, NULL)
```

- **Insert succeeds** → the value was free and this command now holds it. `held = false`.
- **`UNIQUE` violation** → read the row. `position IS NOT NULL` means a committed claim, so
  `held = true`. `position IS NULL` means another command is mid-decision, which is a
  **conflict, not an answer**: abandon the attempt and retry through the existing DCB retry
  path. Rejecting on a pending row would be a false `EmailTaken`, because that command may
  still give the value back.

Then the body runs, the append happens with whatever conditions the folds declared, and the
row is confirmed with the committed position in one SQLite transaction. A command that
rejects deletes its own row before returning.

Two simultaneous signups of one address:

| | A | B |
| --- | --- | --- |
| 1 | resolves its folds | resolves its folds |
| 2 | `INSERT` wins, `held = false` | |
| 3 | | `INSERT` violates. `position IS NULL`, so this is a conflict. Retry |
| 4 | body runs, appends at 4812 | |
| 5 | `UPDATE claim SET position = 4812`. 200 | |
| 6 | | retry: `INSERT` violates, `position = 4812`. `held = true` |
| 7 | | body runs, `reject EmailTaken`. 422 |

One 200 and one 422, the same shape `tests/subject.rs:432` already asserts for the
plaintext-handle case. **B never appends anything**: a duplicate is not a race B loses, it is
structurally unreachable. Retries are bounded, failing loudly rather than hanging, in the same
way an erase/recreate loop already does.

### 6.1 Crash recovery

The only transient state is a `position IS NULL` row, and there are exactly two ways to crash
around one. Both are repaired at boot, **in this order**:

1. **Catch up the registry watermark.** Read the log forward from the watermark; for each
   `@claims` event, decrypt its sealed copy under the holder's key, canonicalise, tokenise,
   and set or insert the row.
2. **Then** `DELETE FROM claim WHERE position IS NULL`.

| Crash point | On disk | Recovery |
| --- | --- | --- |
| Before the append | pending row, no event | step 1 finds nothing, step 2 deletes it. Value free |
| Between append and confirm | pending row, event at 4812 | step 1 sets `position = 4812`. Step 2 finds nothing. Claim held |

The order is load-bearing. Swapping the steps deletes a legitimate claim whose event is
already in the log. Neither pass is a scan: both are bounded by the commands in flight at the
crash.

### 6.2 Why no lock

hekla's write path is optimistic and never blocks a writer, and this keeps it that way.

An earlier draft of this design took a sharded latch on the token across the append. It works,
but it is strictly worse: it holds a lock across IO, and it separates the check from the
reservation so that something has to span them. Making the `INSERT` itself the test removes
both problems. The contended path costs one retry; the uncontended path is a single insert.

This also records why the obvious alternative fails. A DCB append condition cannot serialise a
**first-ever** claim, because the slice it would condition on is empty and an empty condition
is vacuous: both writers see nothing, both commit. No choice of tag fixes that, because the
information needed to conflict does not exist yet. The serialisation has to come from a
constraint, which is what SQLite has.

## 7. Erasure, and the rest of the runtime

### 7.1 Erase cascades

`erase(account_id)` stays one key-row delete plus one indexed `DELETE` on `claim_by_holder`.
A tenant erase (`Customer under Shop`) deletes on `claim_by_root`: one indexed statement
rather than the per-customer enumeration `under` exists to avoid.

This is the one place `under` acquires a cost it did not have, and it should be stated in
`reference/encryption.md` when this ships: erasing a shop now also releases every claim held
by a customer beneath it.

### 7.2 The rebuild honours erasure for free

The registry is a synchronous projection with a watermark, not a second source of truth.
Rebuilding it reads the log and decrypts each claiming event's sealed copy under the holder's
key, so **an erased holder's copy does not decrypt and the rebuild skips it**. No tombstone,
no release marker, no bookkeeping.

That is the same trick that already makes a sealed projector column verify-safe: the live
value and the rebuilt value agree on absence, so rebuild equivalence holds through an
erasure.

So `hekla verify` gains one check in a shape it already has: rebuild `claim` from position 0
and compare row for row.

### 7.3 Changing the canonicalisation is a planned migration

Because the rule is a `fn`, heklang's digest already splices it into everything that names
it, and `hekla plan` already groups and attributes that kind of change. A normalisation edit
gets the existing treatment:

```
  contract unique Email (events/account.hk)
  because `fn canonical_email` changed: Email
  Email re-tokenises: 12481 claim(s) rebuild
```

And the case that matters: **tightening a rule can collide.** If `Alice@Example.com` and
`alice@example.com` are two live claims under the old rule, a new rule maps them to one token
and the rebuild hits a `UNIQUE` violation. That must be answered before the deploy, not
crashed into at boot:

```
  re-tokenising Email under `canonical_email` merges 3 pair(s) of live claims
```

Refuse, name what was found, let the operator decide, in the same shape as the
`HEKLA_V10_SUBJECTS` migration and the `adopt` refusal.

**None of that is computable if the rule is not a declaration.** That is the whole argument
for section 9.

## 8. What this does not do

- **It does not remove a plaintext field you forgot to seal.** That still needs the segment
  rewriter `ARCHITECTURE.md` names as out of scope, and the two are complementary rather than
  competing.
- **It does not fix dated archival backups.** A copy taken before a release or an erase holds
  the registry as of its date. Same unfixable property `hekla erase` already has and says so.
- **It does not make the log forget.** The log holds per-account ciphertext, which the existing
  erase shreds. Nothing new is written there.

## 9. Normalisation: why a `fn`

Rejected first draft:

```hek
unique Email(String) normalise lower trim
```

Three reasons it is wrong. **The vocabulary has no end**: `lower` and `trim`, then NFKC,
strip-dots, strip-plus-tag, casefold rather than lower because Turkish dotted I makes `lower`
the wrong operation for a case-insensitive compare. Each is a new keyword, a new Rust
implementation and a new semantic the author cannot read. **The order is invisible**: nothing
in `normalise lower trim` says which runs first, which does not matter for those two and
matters immediately for `truncate`. **It conjures behaviour**, which is the thing this
codebase consistently refuses to do: a subject is declared "rather than being conjured by
spelling a field", and tags are derived from declarations so "there is no second list to keep
in step".

So name a `fn`. heklang already has pure, total, testable helpers and this is one:

```hek
fn canonical_email(raw: String) -> String {
  raw.trim().lower()
}

unique Email(String) via canonical_email
```

What it buys beyond taste:

- **The migration story in 7.3**, which needs the rule in the digest.
- **Reuse.** A projector rendering a canonical address and a second command claiming the same
  namespace call the same function. One definition. With a keyword list or an inline body,
  `RegisterAccount` and `ChangeEmail` can quietly disagree about what counts as the same
  address and nothing catches it.
- **It improves without new syntax.** When heklang's string API grows `replace` or `split`,
  normalisation gets better with no change to the `unique` grammar.

Also rejected: making the declaration itself the function.

```hek
unique Email(raw: String) { raw.trim().lower() }
```

Tempting, since there is no name to resolve and nothing can go out of step. Dropped because
it is a second way to write a pure function in a language that has spent several phases
collapsing to one way to do things, and because it cannot be called from anywhere else, so
the moment the canonical form is wanted in a projector or a response the rule is duplicated
again.

## 10. What was rejected, and why

So none of it is re-derived. The first four are the Discord thread's.

| Approach | Why not |
| --- | --- |
| Tag the raw value | PII in an immutable tag index, never erasable. This is `examples/users` today |
| Tag `HMAC(app_secret, email)` (Gerard) | The tag survives the shred, so the address stays taken forever unless hand-rolled release events cover every path. Pseudonymous personal data permanently in the log. This *is* the deleted `unique = True` |
| Truncated hash as a bucket (Jef, the thread's conclusion) | Needs an exact comparison after the bucket narrows, and heklang refuses equality on sealed content. Without that step it falsely rejects colliding strangers, which is worse than the bug it fixes |
| Tokenisation with the map outside the store (John Bywater) | Correct about sensitive data and wrong about uniqueness, as Gerard noted: you would have to load every existing value to know whether one is used. This design is tokenisation *plus* the index that makes the lookup O(1) |
| Periodic log compaction that strips shredded data | Breaks rebuild equivalence for any projector touching a removed claim, forces positions to be hollowed rather than removed, and trades away tephra's "a segment is never rewritten", which is what makes copying a live log safe. Still worth building one day as an **offline operator-run** remediation for the unsealed-field case, which this does not cover |
| A subject key scoped per value, tag derived from it | Falls to 6.2: a first-ever claim has no key, so nothing is derivable, so the append condition is vacuous and both writers commit. Minting on the fold fixes the race and creates unbounded growth from probing, since every attempted value leaves a row |
| The same, with a latch across the append | Works. Strictly worse than 6: a lock held across IO, and the check separated from the reservation |

## 11. If this gets built

Order, and what would make me stop.

1. **The `claim` table, the protocol in section 6, and its crash recovery**, behind a hekla-side
   test harness with no language surface at all. Everything load-bearing is here, and all of it
   can be tested before heklang has a single new token: drive it with a hand-built program the
   way `tests/check.rs` already builds `ACCOUNT_EVENTS`.
2. **Concurrency**, as a thread test in the shape of `tests/subject.rs:432`, plus a crash test
   per row of the 6.1 table.
3. **Only then the heklang surface** in section 4.
4. **`verify` and `plan`** last, since 7.2 and 7.3 are both reporting over machinery that by
   then exists.

The thing to measure first is step 2. If the retry path under contention is not clean, the
design changes shape and nothing after step 2 is worth writing.

## 12. Evidence

What is already true, for whoever picks this up:

- `src/heklang_host.rs:1297` already writes a sealed field's ciphertext as a tag, so the
  write half of searchable encryption ships today.
- `src/heklang_host.rs:732` closes the read half: "A filter is never on a subject-scoped
  field: heklang rejects an equality on sealed content at parse time (rule 12)."
- `src/crypto.rs:48` still carries `GLOBAL_SUBJECT = "_hekla_global"`, non-deletable, left
  from `unique = True`. `src/dispatch.rs:373` still references the `_hekla_uniq_` reserved tag
  prefix.
- `src/crypto.rs:217` `encrypt_subject_existing` returns `None` for a missing key specifically
  so a read path cannot resurrect one, which is the semantic 7.2 relies on.
- `src/backup.rs:700` `clamp_to_log`, and the `POSITIONED_TABLES` / `WATERMARK_COLUMNS`
  registries a new table declares itself in.
- `ARCHITECTURE.md`, accepted limitations, for the `unique = True` removal and the
  segment-rewrite deferral.
