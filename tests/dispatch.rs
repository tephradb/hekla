//! Command dispatch internals: the append condition a boundaried command builds, the
//! fold contracts, and the recovery of a multi-event commit.
//!
//! These are the seams `tests/command.rs` cannot reach from `examples/users`: every
//! project here is a throwaway written to exercise one dispatch decision.
//!
//! Seven of the Starlark suite's cases are gone rather than ported, because the thing
//! each of them guarded no longer exists:
//!
//! - **The instruction budget.** `MAX_TICKS` existed because a Starlark `handle` could
//!   loop forever. heklang has no `while`, rejects recursion, and iterates only finite
//!   containers, so termination is structural and there is nothing to meter.
//! - **Fail-closed lowering of an unchecked `query` branch.** The static check used to
//!   evaluate `query` once against a stubbed input, so a branch it never took could
//!   lower to a tag that matched nothing. A heklang slice is declared by the `state`
//!   that folds it and its fields are checked statically on every branch, so a
//!   constraint on an unindexed or undeclared field is a parse error rather than a
//!   runtime one.
//! - **A `fold` or a fold arm that returns `None`.** A heklang arm is an expression of
//!   the state's declared type. There is no falling off the end.
//! - **A `fold` or a `handle` that mutates the state it was handed.** heklang has no
//!   mutable binding, so the contract those two enforced at runtime is now the absence
//!   of the syntax that broke it.
//! - **A fold reading a stable `event.id`.** A heklang fold arm binds the event's
//!   declared fields; a record's id is the host's, and no expression can reach it.

use std::thread;

use serde_json::{Value, json};
use uuid::Uuid;

mod support;

use support::{ALICE, BOB, Boot, CAROL, UUID_A, ctx, log_head, write_project};

// --- multi-event recovery -------------------------------------------------

const BATCH_EVENTS: &str = r#"
event @t.opened { id: Uuid, who: String @max(50) }
event @t.logged { id: Uuid }
"#;

const OPEN_BATCH: &str = r#"
command Open(id: Uuid, who: String) {
  emit @t.opened { id, who }
  emit @t.logged { id }
}
"#;

#[test]
fn a_multi_event_command_replays_byte_identically() {
    let project = write_project(&[
        ("events/t.hk", BATCH_EVENTS),
        ("commands/open.hk", OPEN_BATCH),
    ]);
    let harness = Boot::new(project.path()).start();

    let body = json!({ "id": ALICE, "who": "alice" });
    let first = harness
        .rt
        .execute("Open", body.clone(), &ctx(), Some("k1"))
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);
    assert_eq!(first.body["events"].as_array().unwrap().len(), 2);
    assert_eq!(first.body["events"][0]["type"], "t.opened");
    assert_eq!(first.body["events"][1]["type"], "t.logged");
    assert_eq!(
        first.body["positions"]["last"].as_u64().unwrap(),
        first.body["positions"]["first"].as_u64().unwrap() + 1,
        "a two-event batch spans two positions"
    );

    // The replay is caught by the append's existence clause and recovered from the
    // log. Recovery has to accumulate the whole range and every event in order, and
    // report the original request's identity, or the client is told that less
    // committed than actually did.
    let replay = harness
        .rt
        .execute("Open", body, &ctx(), Some("k1"))
        .unwrap();
    assert_eq!(replay.status, 200, "{:?}", replay.body);
    assert_eq!(
        replay.body, first.body,
        "a multi-event replay must reproduce the original outcome exactly"
    );
    assert_eq!(log_head(&harness.rt), 2, "the replay appended nothing");
    harness.shutdown();
}

// --- a boundary with no fold ----------------------------------------------

const NOTED_EVENTS: &str = r#"
event @t.noted { id: Uuid, topic: String @max(50) }
"#;

/// `guard` is the boundary with no fold: it declares a slice and binds nothing, so
/// the events inside it are read (and `after` is taken before them) but no arm runs.
const NOTE: &str = r#"
command Note(id: Uuid, topic: String) {
  guard @t.noted(topic)

  emit @t.noted { id, topic }
}
"#;

#[test]
fn a_guarded_command_with_no_fold_arm_still_commits() {
    let project = write_project(&[("events/t.hk", NOTED_EVENTS), ("commands/note.hk", NOTE)]);
    let harness = Boot::new(project.path()).start();

    let first = harness
        .rt
        .execute(
            "Note",
            json!({ "id": ALICE, "topic": "rust" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);

    // The boundary now matches the first event. `after` is the log length taken
    // before the fold, so a slice that already holds an event must not conflict
    // against it; if it did, this append would burn every retry.
    let second = harness
        .rt
        .execute("Note", json!({ "id": BOB, "topic": "rust" }), &ctx(), None)
        .unwrap();
    assert_eq!(
        second.status, 200,
        "a guarded boundary must not self-conflict: {:?}",
        second.body
    );
    assert_eq!(second.body["positions"]["first"], 2);
    assert_eq!(log_head(&harness.rt), 2);
    harness.shutdown();
}

// --- invalid from the body ------------------------------------------------

const REGISTERED_EVENTS: &str = r#"
event @t.registered { id: Uuid, email: String @max(100) }
"#;

const INVALID_INPUT_COMMAND: &str = r#"
command Register(id: Uuid, email: String) {
  if !email.contains("@") {
    return invalid("email must contain @")
  }

  emit @t.registered { id, email }
}
"#;

#[test]
fn a_command_returning_invalid_is_a_400_and_appends_nothing() {
    let project = write_project(&[
        ("events/t.hk", REGISTERED_EVENTS),
        ("commands/register.hk", INVALID_INPUT_COMMAND),
    ]);
    let harness = Boot::new(project.path()).start();

    let bad = harness
        .rt
        .execute(
            "Register",
            json!({ "id": ALICE, "email": "nope" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(
        bad.status, 400,
        "`invalid` is a malformed body, not a state rejection: {:?}",
        bad.body
    );
    assert_eq!(bad.body["error"]["code"], "invalid_input");
    assert!(
        bad.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must contain @"),
        "{:?}",
        bad.body
    );
    assert_eq!(log_head(&harness.rt), 0);

    // The other arm of the same command still commits, so the 400 is the outcome and
    // not the command being broken.
    let ok = harness
        .rt
        .execute(
            "Register",
            json!({ "id": ALICE, "email": "a@example.com" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(ok.status, 200, "{:?}", ok.body);
    harness.shutdown();
}

// --- a commit with no events ----------------------------------------------

const THING_EVENTS: &str = r#"
event @t.thing { id: Uuid }
"#;

const NOOP_COMMAND: &str = r#"
command Noop(id: Uuid) {
  return
}
"#;

#[test]
fn a_command_that_emits_nothing_commits_with_null_positions() {
    let project = write_project(&[
        ("events/t.hk", THING_EVENTS),
        ("commands/noop.hk", NOOP_COMMAND),
    ]);
    let harness = Boot::new(project.path()).start();

    let body = json!({ "id": ALICE });
    let first = harness
        .rt
        .execute("Noop", body.clone(), &ctx(), Some("k1"))
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);
    assert!(
        first.body["positions"].is_null(),
        "nothing was appended, so there is no position range: {:?}",
        first.body
    );
    assert_eq!(first.body["events"], json!([]));
    assert_eq!(log_head(&harness.rt), 0, "an empty emit must not append");

    // Nothing carries the idempotency tag, so there is nothing to recover from the
    // log: the replay legitimately re-runs the command and reports its own identity.
    let second_ctx = ctx();
    let replay = harness
        .rt
        .execute("Noop", body, &second_ctx, Some("k1"))
        .unwrap();
    assert_eq!(replay.status, 200, "{:?}", replay.body);
    assert_eq!(
        replay.body["correlation_id"],
        second_ctx.correlation_id.to_string()
    );
    assert!(replay.body["positions"].is_null());
    assert_eq!(log_head(&harness.rt), 0);
    harness.shutdown();
}

// --- input binding --------------------------------------------------------

const SCALAR_EVENTS: &str = r#"
event @t.recorded { qty: Int, flag: Bool, code: String @max(3) }
"#;

const RECORD_COMMAND: &str = r#"
command Record(qty: Int, flag: Bool, code: String) {
  emit @t.recorded { qty, flag, code }
}
"#;

/// Two different rejections, both 400, and the split is worth naming. A wrongly typed
/// or missing parameter never reaches the program: `bind_args` converts the body
/// against the declaration and fails first. An over-length string does reach it, and
/// comes back as `Outcome::Invalid` from the `emit`, because `@max` is a property of
/// the event field rather than of the parameter.
#[test]
fn a_body_that_does_not_bind_is_rejected_with_400() {
    let project = write_project(&[
        ("events/t.hk", SCALAR_EVENTS),
        ("commands/record.hk", RECORD_COMMAND),
    ]);
    let harness = Boot::new(project.path()).start();

    // Each body is well-formed apart from the one named field.
    let cases: [(&str, serde_json::Value); 5] = [
        ("qty", json!({"qty": 1.5, "flag": true, "code": "abc"})),
        ("flag", json!({"qty": 1, "flag": "yes", "code": "abc"})),
        ("code", json!({"qty": 1, "flag": true, "code": "abcd"})),
        // Absent, rather than holding the wrong thing: a different mistake with a
        // different answer.
        ("flag", json!({"qty": 1, "code": "abc"})),
        // A key the command does not declare. heklang reads the parameters it knows
        // and never looks at the rest, so nothing but this check would notice a typo.
        (
            "quantity",
            json!({"qty": 1, "flag": true, "code": "abc", "quantity": 2}),
        ),
    ];
    for (field, body) in cases {
        let result = harness.rt.execute("Record", body, &ctx(), None).unwrap();
        assert_eq!(
            result.status, 400,
            "`{field}` should not have bound: {:?}",
            result.body
        );
        assert_eq!(result.body["error"]["code"], "invalid_input");
        let message = result.body["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(field),
            "the error should name `{field}`, got: {message}"
        );
    }
    assert_eq!(log_head(&harness.rt), 0, "no bad body reached the log");

    let ok = harness
        .rt
        .execute(
            "Record",
            json!({ "qty": -3, "flag": true, "code": "abc" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(
        ok.status, 200,
        "binding must accept a well-typed body: {:?}",
        ok.body
    );
    harness.shutdown();
}

// --- per-type fold dispatch -----------------------------------------------

/// Two event types over one owner, so a command can fold both.
const ACCOUNT_EVENTS: &str = r#"
event @t.opened { id: Uuid, owner: String @max(50) }
event @t.frozen { id: Uuid, owner: String @max(50) }
event @t.noticed { id: Uuid, owner: String @max(50) }
"#;

/// Two `state`s, one per type, each folding its own slice of one boundary.
const PER_TYPE_FOLD: &str = r#"
refusal Frozen "that owner is frozen"
refusal AlreadyOpen "that owner already has an account"

command Open(id: Uuid, owner: String) {
  state opened: Bool = fold false
    on @t.opened(owner) => true

  state frozen: Bool = fold false
    on @t.frozen(owner) => true

  if frozen {
    return reject Frozen
  }
  if opened {
    return reject AlreadyOpen
  }

  emit @t.opened { id, owner }
}
"#;

/// Emits the second event type, so the per-type fold has something to dispatch on.
const FREEZE: &str = r#"
command Freeze(id: Uuid, owner: String) {
  emit @t.frozen { id, owner }
}
"#;

#[test]
fn a_per_type_fold_dispatches_by_event_type() {
    let project = write_project(&[
        ("events/t.hk", ACCOUNT_EVENTS),
        ("commands/open.hk", PER_TYPE_FOLD),
        ("commands/freeze.hk", FREEZE),
    ]);
    let harness = Boot::new(project.path()).start();

    let body = json!({ "id": ALICE, "owner": "kim" });
    let first = harness.rt.execute("Open", body, &ctx(), None).unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);

    // The `@t.opened` arm ran, so the second attempt sees `opened` and not `frozen`.
    let second = harness
        .rt
        .execute("Open", json!({ "id": BOB, "owner": "kim" }), &ctx(), None)
        .unwrap();
    assert_eq!(second.status, 422, "{:?}", second.body);
    assert_eq!(second.body["error"]["code"], "already_open");

    // Now the other arm: a frozen event flips the other flag, and the rejection
    // changes with it, so each arm is genuinely reached by its own type.
    harness
        .rt
        .execute("Freeze", json!({ "id": BOB, "owner": "kim" }), &ctx(), None)
        .unwrap();
    let third = harness
        .rt
        .execute("Open", json!({ "id": CAROL, "owner": "kim" }), &ctx(), None)
        .unwrap();
    assert_eq!(third.status, 422, "{:?}", third.body);
    assert_eq!(third.body["error"]["code"], "frozen");
    harness.shutdown();
}

// --- fan-out across two slices of one type --------------------------------

const TIERED_EVENTS: &str = r#"
event @t.opened { id: Uuid, owner: String @max(50), tier: String @max(20) }
"#;

/// Writes history without inspecting it, so the command below can be asked about a
/// log it did not build.
const RECORD_OPEN: &str = r#"
command Record(id: Uuid, owner: String, tier: String) {
  emit @t.opened { id, owner, tier }
}
"#;

/// Two slices of one event type, the narrower a strict subset of the wider. Both are
/// declared by this one command, so both are in its append condition, and a record
/// matching the narrow one must be applied by both arms.
const FAN_OUT_FOLD: &str = r#"
refusal NarrowOnly "the narrow arm ran without the wide one"
refusal WideOnly "only the wide arm ran"
refusal BothRan "both arms ran"

command Open(id: Uuid, owner: String, tier: String) {
  state seen: Int = fold 0
    on @t.opened(owner) => seen + 1

  state gold: Int = fold 0
    on @t.opened(owner, tier: "gold") => gold + 1

  if gold > seen {
    return reject NarrowOnly
  }
  if seen > gold {
    return reject WideOnly
  }
  if seen > 0 {
    return reject BothRan
  }

  emit @t.opened { id, owner, tier }
}
"#;

/// Both arms run for a record the narrow slice selects, and only the wide one runs
/// for a record it does not.
///
/// `narrow_only` is unreachable by construction, since a subset cannot match without
/// its superset matching too. It is written anyway: a regression that dropped the
/// wide arm would surface as that code rather than as a silent pass.
#[test]
fn a_fold_fans_out_across_two_slices_of_one_type() {
    let project = write_project(&[
        ("events/t.hk", TIERED_EVENTS),
        ("commands/record.hk", RECORD_OPEN),
        ("commands/open.hk", FAN_OUT_FOLD),
    ]);
    let harness = Boot::new(project.path()).start();

    let record = |id: &str, owner: &str, tier: &str| {
        let result = harness
            .rt
            .execute(
                "Record",
                json!({ "id": id, "owner": owner, "tier": tier }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    };

    // An empty boundary: both counters are their seeds, so nothing rejects.
    let first = harness
        .rt
        .execute(
            "Open",
            json!({ "id": ALICE, "owner": "kim", "tier": "gold" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);

    // That commit is in both slices, so both counters moved together.
    let second = harness
        .rt
        .execute(
            "Open",
            json!({ "id": BOB, "owner": "kim", "tier": "gold" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(second.status, 422, "{:?}", second.body);
    assert_eq!(second.body["error"]["code"], "both_ran");

    // A different owner is a different boundary, and this one holds a record the
    // narrow slice does not select, so only the wide arm ran for it.
    record(CAROL, "sam", "silver");
    let third = harness
        .rt
        .execute(
            "Open",
            json!({ "id": ALICE, "owner": "sam", "tier": "gold" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(third.status, 422, "{:?}", third.body);
    assert_eq!(third.body["error"]["code"], "wide_only");
    harness.shutdown();
}

// --- a type in the boundary with no arm -----------------------------------

/// `@t.opened` is guarded, so it is in the append condition and is read, but nothing
/// folds it. `@t.frozen` is folded.
const GUARDED_AND_FOLDED: &str = r#"
refusal Frozen(frozen: Int) "saw {frozen} frozen event(s)"

command Open(id: Uuid, owner: String) {
  guard @t.opened(owner)

  state frozen: Int = fold 0
    on @t.frozen(owner) => frozen + 1

  if frozen > 0 {
    return reject Frozen { frozen }
  }

  emit @t.opened { id, owner }
}
"#;

#[test]
fn an_event_type_in_the_boundary_with_no_fold_arm_is_read_but_not_folded() {
    let project = write_project(&[
        ("events/t.hk", ACCOUNT_EVENTS),
        ("commands/open.hk", GUARDED_AND_FOLDED),
        ("commands/freeze.hk", FREEZE),
    ]);
    let harness = Boot::new(project.path()).start();

    // An `@t.opened` record has no arm. It is still inside the condition, so `after`
    // has to be taken before the read, or the next command would conflict against
    // history it already saw.
    let first = harness
        .rt
        .execute("Open", json!({ "id": ALICE, "owner": "kim" }), &ctx(), None)
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);
    let second = harness
        .rt
        .execute("Open", json!({ "id": BOB, "owner": "kim" }), &ctx(), None)
        .unwrap();
    assert_eq!(
        second.status, 200,
        "an unfolded slice must not self-conflict: {:?}",
        second.body
    );

    // The folded type does move the state, so it is not simply stuck at its seed.
    harness
        .rt
        .execute("Freeze", json!({ "id": BOB, "owner": "kim" }), &ctx(), None)
        .unwrap();
    let third = harness
        .rt
        .execute("Open", json!({ "id": CAROL, "owner": "kim" }), &ctx(), None)
        .unwrap();
    assert_eq!(third.status, 422, "{:?}", third.body);
    assert_eq!(third.body["error"]["message"], "saw 1 frozen event(s)");
    harness.shutdown();
}

// --- state accumulates ----------------------------------------------------

const COUNTING_FOLD: &str = r#"
refusal Enough(seen: Int) "seen {seen}"

command Notice(id: Uuid, owner: String) {
  state seen: Int = fold 0
    on @t.noticed(owner) => seen + 1

  if seen >= 2 {
    return reject Enough { seen }
  }

  emit @t.noticed { id, owner }
}
"#;

#[test]
fn folded_state_accumulates_across_events() {
    let project = write_project(&[
        ("events/t.hk", ACCOUNT_EVENTS),
        ("commands/notice.hk", COUNTING_FOLD),
    ]);
    let harness = Boot::new(project.path()).start();

    for id in [ALICE, BOB] {
        let result = harness
            .rt
            .execute("Notice", json!({ "id": id, "owner": "kim" }), &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }
    let third = harness
        .rt
        .execute(
            "Notice",
            json!({ "id": CAROL, "owner": "kim" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(third.status, 422, "{:?}", third.body);
    assert_eq!(third.body["error"]["message"], "seen 2");
    harness.shutdown();
}

// --- a folded record is schema-shaped -------------------------------------

/// A note whose body is optional, so a stored payload can legitimately omit it.
const OPTIONAL_EVENTS: &str = r#"
event @t.noted { id: Uuid, body: String? @max(50) }
"#;

/// Emits without a body, so the stored payload carries no value for it.
const NOTE_WITHOUT_BODY: &str = r#"
command Note(id: Uuid) {
  emit @t.noted { id, body: none }
}
"#;

/// Folds the absent field. It has to read as `none` rather than raising: a record is
/// built from the event's declared fields, not from whatever the payload carried.
const READ_ABSENT_FIELD: &str = r#"
refusal Absent "an omitted optional field reads as none"

command Read(id: Uuid) {
  state seen: String? = fold "unset"
    on @t.noted(id) { body } => body

  if seen.is_none() {
    return reject Absent
  }

  emit @t.noted { id, body: seen }
}
"#;

#[test]
fn an_omitted_optional_field_reads_as_none() {
    let project = write_project(&[
        ("events/t.hk", OPTIONAL_EVENTS),
        ("commands/note.hk", NOTE_WITHOUT_BODY),
        ("commands/read.hk", READ_ABSENT_FIELD),
    ]);
    let harness = Boot::new(project.path()).start();

    let first = harness
        .rt
        .execute("Note", json!({ "id": ALICE }), &ctx(), None)
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);

    // The fold reads a field the payload never carried a value for. A payload-shaped
    // map would have had nothing there at all; a schema-shaped record gives `none`.
    let second = harness
        .rt
        .execute("Read", json!({ "id": ALICE }), &ctx(), None)
        .unwrap();
    assert_eq!(second.status, 422, "{:?}", second.body);
    assert_eq!(second.body["error"]["code"], "absent");
    harness.shutdown();
}

// --- the incremental re-fold across a conflict -----------------------------

/// A counted resource: the boundary is one room's whole seating history, so every
/// concurrent take collides on it.
const SEAT_EVENTS: &str = r#"
event @t.taken { id: Uuid, room: String @max(20) }
"#;

/// Capacity two. The interesting property is what a loser does on its retry: the
/// winner's event is inside the slice it already folded up to, so a retry that folded
/// nothing new would decide on a stale count and conflict again until the budget ran
/// out, while a retry that re-folds sees the seat go and rejects.
const TAKE_SEAT: &str = r#"
refusal Full "no seats left"

command Take(id: Uuid, room: String) {
  state seats: Int = fold 0
    on @t.taken(room) => seats + 1

  if seats >= 2 {
    return reject Full
  }

  emit @t.taken { id, room }
}
"#;

#[test]
fn a_retry_decides_on_the_events_that_beat_it() {
    let project = write_project(&[
        ("events/t.hk", SEAT_EVENTS),
        ("commands/take.hk", TAKE_SEAT),
    ]);
    let harness = Boot::new(project.path()).start();

    let outcomes: Vec<(u16, Value)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let rt = &harness.rt;
                scope.spawn(move || {
                    let result = rt
                        .execute(
                            "Take",
                            json!({ "id": Uuid::new_v4(), "room": "r1" }),
                            &ctx(),
                            None,
                        )
                        .unwrap();
                    (result.status, result.body)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let committed = outcomes.iter().filter(|(status, _)| *status == 200).count();
    assert_eq!(
        committed, 2,
        "the boundary admits exactly the capacity: {outcomes:?}"
    );
    for (status, body) in &outcomes {
        if *status == 200 {
            continue;
        }
        assert_eq!(
            *status, 422,
            "a loser rejects on the state its retry folded rather than exhausting the \
             retry budget on a stale one: {body:?}"
        );
        assert_eq!(body["error"]["code"], "full", "{body:?}");
    }
    assert_eq!(
        log_head(&harness.rt),
        2,
        "no attempt appended past the capacity"
    );
    harness.shutdown();
}

/// The same order id, submitted at once by everyone. The narrow slice is what makes
/// this exactly-once: each attempt folds only that one id, so the winner's event is in
/// every loser's boundary and every loser re-folds into the no-op arm rather than
/// appending a second time.
#[test]
fn one_id_submitted_concurrently_appends_exactly_once() {
    let project = write_project(&[
        (
            "events/t.hk",
            "event @t.opened { id: Uuid, who: String @max(50) }\n",
        ),
        (
            "commands/open.hk",
            r#"
command Open(id: Uuid, who: String) {
  state opened: Bool = fold false
    on @t.opened(id) => true

  if opened {
    return
  }
  emit @t.opened { id, who }
}
"#,
        ),
    ]);
    let harness = Boot::new(project.path()).start();
    let id = Uuid::new_v4();

    let outcomes: Vec<(u16, Value)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..24)
            .map(|_| {
                let rt = &harness.rt;
                scope.spawn(move || {
                    let result = rt
                        .execute("Open", json!({ "id": id, "who": "a" }), &ctx(), None)
                        .unwrap();
                    (result.status, result.body)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for (status, body) in &outcomes {
        assert_eq!(
            *status, 200,
            "a repeat is a no-op, never a conflict or a rejection: {body:?}"
        );
    }
    assert_eq!(
        log_head(&harness.rt),
        1,
        "24 concurrent submissions of one id append once"
    );
    harness.shutdown();
}

// --- numbers across the JSON boundary --------------------------------------

/// A `Json` field is unchecked passthrough in both directions, and a number is the
/// hard half of that. `Json::Num` carries the wire's own text rather than an `i64`, so
/// what a caller wrote is what the log holds and what a reader gets back: `10.50` keeps
/// its trailing zero and a decimal past what an `f64` can hold keeps its digits.
///
/// hekla owes the other half of that, which is why `serde_json` is built with
/// `arbitrary_precision`: without it a `Value::Number` is an `f64` and the text is
/// already rounded before heklang is ever handed it.
#[test]
fn a_json_field_keeps_a_number_exactly_as_written() {
    let project = write_project(&[
        (
            "events/doc.hk",
            "event @doc.saved { id: Uuid, body: Json }\n",
        ),
        (
            "commands/save.hk",
            "command Save(id: Uuid, body: Json) { emit @doc.saved { id, body } }\n",
        ),
        (
            "projectors/docs.hk",
            r#"
projector Docs {
  entity Doc {
    id: Uuid @key,
    body: Json,
  }

  on @doc.saved { id, body } {
    put Doc { id, body }
  }
}
"#,
        ),
    ]);
    let harness = Boot::new(project.path()).start();

    // Spelled as wire text, because the whole question is what survives it.
    let cases = [
        ("trailing_zero", "10.50"),
        ("past_an_f64", "1.234567890123456789"),
        ("shortest_repr", "0.30000000000000004"),
        ("past_an_i64_mantissa", "9007199254740993"),
        ("negative", "-0.5"),
        ("whole", "3"),
        ("nested", "[1.5, {\"deep\": 2.50}]"),
    ];
    let raw = format!(
        "{{{}}}",
        cases
            .iter()
            .map(|(name, text)| format!("\"{name}\":{text}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let body: Value = serde_json::from_str(&raw).expect("the fixture is valid JSON");
    let id = Uuid::new_v4();

    let result = harness
        .rt
        .execute("Save", json!({ "id": id, "body": body }), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);

    let position = log_head(&harness.rt);
    let row = support::read_row(&harness, "Docs", "Doc", &id.to_string(), position)
        .expect("the projected row");
    let stored = row.get("body").expect("the body column");

    for (name, wire) in cases {
        let found = stored
            .get(name)
            .map(ToString::to_string)
            .unwrap_or_default();
        let wire = wire.replace(", ", ",").replace(": ", ":");
        assert_eq!(found, wire, "`{name}` did not survive the round trip");
    }
    harness.shutdown();
}

/// The inbound half of rule 8 still holds for the types that declare a wire form: a
/// `Money(n)` is a quoted string, and an unquoted number is not one. `Json::Num` widened
/// what a `Json` field can carry without widening that.
#[test]
fn a_money_parameter_still_refuses_an_unquoted_number() {
    let project = write_project(&[
        (
            "events/t.hk",
            "event @t.charged { id: Uuid, total: Money(2) }\n",
        ),
        (
            "commands/charge.hk",
            "command Charge(id: Uuid, total: Money(2)) { emit @t.charged { id, total } }\n",
        ),
    ]);
    let harness = Boot::new(project.path()).start();

    let refused = harness
        .rt
        .execute(
            "Charge",
            json!({ "id": Uuid::new_v4(), "total": 10.5 }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(refused.status, 400, "{:?}", refused.body);
    assert_eq!(log_head(&harness.rt), 0, "nothing reaches the log");

    let taken = harness
        .rt
        .execute(
            "Charge",
            json!({ "id": Uuid::new_v4(), "total": "10.50" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(taken.status, 200, "{:?}", taken.body);
    harness.shutdown();
}

// --- an append that read nothing ------------------------------------------

const STAGED_EVENTS: &str = r#"
event @note.made { note_id: Uuid, kind: String @max(20) }
event @other.happened { n: Int }
"#;

/// A command whose early return sits *above* its first declaration run. heklang stages
/// a command's reads, so this path resolves no slices at all and appends with an empty
/// condition; the path below it folds and appends with a real one.
const MAKE_NOTE: &str = r#"
command MakeNote(note_id: Uuid, kind: String) {
  if kind == "quick" {
    emit @note.made { note_id, kind }
    return
  }

  state made: Bool = fold false
    on @note.made(note_id) => true

  if made {
    return
  }
  emit @note.made { note_id, kind }
}
"#;

const CHURN: &str = "command Churn(n: Int) { emit @other.happened { n } }\n";

/// `docs/host.md`: an `after` is half of a predicate, not an expected version, and a
/// command that read nothing comes back with no slices and `after` at zero rather than
/// at a head it never asked for. A host that mistook the pair for a version check would
/// make every unboundaried append fail the moment anything else touched the log.
///
/// Staging is what makes this newly reachable: before, a command's reads were hoisted
/// above its body, so a command holding a `state` always resolved its slices. Now an
/// early return above the first declaration run skips them, and the same command
/// appends under both conditions depending on the argument.
#[test]
fn a_command_that_returned_above_its_first_fold_appends_against_a_moving_log() {
    let project = write_project(&[
        ("events/t.hk", STAGED_EVENTS),
        ("commands/make-note.hk", MAKE_NOTE),
        ("commands/churn.hk", CHURN),
    ]);
    let harness = Boot::new(project.path()).start();

    // Something else is writing the whole time, so a stale `after` would show.
    for n in 0..5 {
        let result = harness
            .rt
            .execute("Churn", json!({ "n": n }), &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }

    for id in [ALICE, BOB, CAROL] {
        let result = harness
            .rt
            .execute(
                "MakeNote",
                json!({ "note_id": id, "kind": "quick" }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(
            result.status, 200,
            "a command that read nothing cannot be beaten to the log: {:?}",
            result.body
        );
    }
    assert_eq!(log_head(&harness.rt), 8, "five churns and three notes");

    // The other path through the same command does read, and its boundary still holds:
    // the second write for one id folds into the no-op arm rather than appending.
    for _ in 0..2 {
        let result = harness
            .rt
            .execute(
                "MakeNote",
                json!({ "note_id": UUID_A, "kind": "slow" }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }
    assert_eq!(
        log_head(&harness.rt),
        9,
        "the folded path appended once, not twice"
    );

    harness.shutdown();
}
