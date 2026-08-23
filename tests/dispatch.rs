//! Command dispatch internals: the per-handler instruction budget, the fail-closed
//! lowering of a consistency boundary, the `fold` contracts, the append condition a
//! boundaried command builds, and the recovery of a multi-event commit.
//!
//! These are the seams `tests/command.rs` cannot reach from `examples/users`: every
//! project here is a throwaway written to exercise one dispatch decision.

use std::thread;

use kiln::runtime::Runtime;
use serde_json::{Value, json};

mod support;

use support::{ALICE, BOB, Boot, ctx, log_head, write_project};

/// Run a command that must fail at dispatch and return its rendered error.
/// `ExecResult` is not `Debug`, so `Result::unwrap_err` is unavailable here.
fn exec_err(rt: &Runtime, command: &str, body: Value) -> String {
    match rt.execute(command, body, &ctx(), None) {
        Ok(result) => panic!(
            "`{command}` should have failed at dispatch, got status {} {:?}",
            result.status, result.body
        ),
        Err(err) => format!("{err:#}"),
    }
}

/// A single-field event, enough for a command that only needs to emit something.
const THING_EVENTS: &str = r#"
thing = event(type = "t.thing", fields = {"id": uuid()})
"#;

// --- the instruction budget ----------------------------------------------

/// A `handle` that spins far past any sane budget. Only the tick limit stops it.
const SPIN_HANDLE: &str = r#"
load("events/t.star", "thing")

input = schema(id = uuid())

def handle(input, state):
    total = 0
    for i in range(50000000):
        total += i
    return thing(id = input.id)
"#;

/// `query` runs away, but only on a branch the static check never evaluates: a
/// `bool()` stubs to False, so the project still loads clean.
const SPIN_QUERY: &str = r#"
load("events/t.star", "thing")

input = schema(id = uuid(), spin = bool())

def query(input):
    if input.spin:
        total = 0
        for i in range(50000000):
            total += i
    return thing(id = input.id)

def handle(input, state):
    return thing(id = input.id)
"#;

#[test]
fn a_runaway_handler_is_killed_by_the_instruction_budget() {
    let project = write_project(&[
        ("events/t.star", THING_EVENTS),
        ("commands/spin.star", SPIN_HANDLE),
        ("commands/spin-query.star", SPIN_QUERY),
    ]);
    let harness = Boot::new(project.path()).start();

    // Both loops run to the budget before dying, which is not cheap in a debug
    // build, so the two calls overlap rather than run back to back.
    let (handle_err, query_err) = thread::scope(|scope| {
        let handle = scope.spawn(|| exec_err(&harness.rt, "spin", json!({ "id": ALICE })));
        // The same budget guards `query`, on a branch the static check cannot see.
        let query = scope.spawn(|| {
            exec_err(
                &harness.rt,
                "spin-query",
                json!({ "id": ALICE, "spin": true }),
            )
        });
        (handle.join().unwrap(), query.join().unwrap())
    });
    // Reaching here at all is most of the point: with no budget these handlers only
    // stop when their 50-million-iteration loops run out, and `exec_err` would then
    // panic on a successful commit rather than see an error.
    assert!(
        handle_err.contains("handle() failed"),
        "a budget kill must surface as a handle() failure, got: {handle_err}"
    );
    assert!(
        query_err.contains("query() failed"),
        "a budget kill in query() must name query(), got: {query_err}"
    );

    // Neither runaway attempt wrote anything.
    assert_eq!(log_head(&harness.rt), 0);
    harness.shutdown();
}

// --- fail-closed boundary lowering ----------------------------------------

const BRANCHY_EVENTS: &str = r#"
registered = event(
    type = "t.registered",
    fields = {
        "id": uuid(),
        "email": str(max_length = 100),
        "secret": str(max_length = 100, indexed = False),
    },
)
"#;

/// `mode` is a `uint()`, which the static check stubs to 0, so it only ever sees the
/// clean `email` branch. The `secret` branch filters a field that is never tagged.
const BRANCH_TO_NON_INDEXED: &str = r#"
load("events/t.star", "registered")

input = schema(id = uuid(), email = str(), mode = uint())

def query(input):
    if input.mode == 0:
        return registered(email = input.email)
    return registered(secret = input.email)

def initial():
    return {"taken": False}

def fold(state, event):
    state["taken"] = True
    return state

def handle(input, state):
    if state["taken"]:
        return reject("email_taken", "that email is already registered")
    return registered(id = input.id, email = input.email, secret = "s")
"#;

/// Same shape, but the unchecked branch constrains a field the event never declares.
const BRANCH_TO_UNDECLARED: &str = r#"
load("events/t.star", "registered")

input = schema(id = uuid(), email = str(), mode = uint())

def query(input):
    if input.mode == 0:
        return registered(email = input.email)
    return registered(nope = input.email)

def handle(input, state):
    return registered(id = input.id, email = input.email, secret = "s")
"#;

#[test]
fn a_query_branch_the_static_check_never_sees_fails_closed() {
    let project = write_project(&[
        ("events/t.star", BRANCHY_EVENTS),
        ("commands/reg.star", BRANCH_TO_NON_INDEXED),
        ("commands/reg-undeclared.star", BRANCH_TO_UNDECLARED),
    ]);
    // `Boot::start` asserts the project loads without errors, which is half the
    // point: the static check evaluates `query` once with a stub input, so neither
    // bad branch is visible to it.
    let harness = Boot::new(project.path()).start();

    // The checked branch works, so the commands are not simply broken.
    let ok = harness
        .rt
        .execute(
            "reg",
            json!({ "id": ALICE, "email": "a@example.com", "mode": 0 }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(ok.status, 200, "{:?}", ok.body);

    // The unchecked branch must fail rather than lower to a tag that matches
    // nothing, which would fold an empty boundary and commit through the invariant.
    let err = exec_err(
        &harness.rt,
        "reg",
        json!({ "id": BOB, "email": "a@example.com", "mode": 1 }),
    );
    assert!(
        err.contains("which is not indexed"),
        "a non-indexed constraint must fail closed, got: {err}"
    );

    let err = exec_err(
        &harness.rt,
        "reg-undeclared",
        json!({ "id": BOB, "email": "a@example.com", "mode": 1 }),
    );
    assert!(
        err.contains("undeclared field"),
        "an undeclared constraint must fail closed, got: {err}"
    );

    // Only the one legitimate commit landed.
    assert_eq!(log_head(&harness.rt), 1);
    harness.shutdown();
}

// --- multi-event recovery -------------------------------------------------

const BATCH_EVENTS: &str = r#"
opened = event(type = "t.opened", fields = {"id": uuid(), "who": str(max_length = 50)})
logged = event(type = "t.logged", fields = {"id": uuid()})
"#;

const OPEN_BATCH: &str = r#"
load("events/t.star", "opened", "logged")

input = schema(id = uuid(), who = str())

def handle(input, state):
    return [opened(id = input.id, who = input.who), logged(id = input.id)]
"#;

#[test]
fn a_multi_event_command_replays_byte_identically() {
    let project = write_project(&[
        ("events/t.star", BATCH_EVENTS),
        ("commands/open.star", OPEN_BATCH),
    ]);
    let harness = Boot::new(project.path()).start();

    let body = json!({ "id": ALICE, "who": "alice" });
    let first = harness
        .rt
        .execute("open", body.clone(), &ctx(), Some("k1"))
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
        .execute("open", body, &ctx(), Some("k1"))
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
noted = event(type = "t.noted", fields = {"id": uuid(), "topic": str(max_length = 50)})
"#;

/// A boundary with no `fold`: the events inside it are read (so `after` advances) but
/// never folded, and `handle` decides on a `None` state.
const NOTE: &str = r#"
load("events/t.star", "noted")

input = schema(id = uuid(), topic = str())

def query(input):
    return noted(topic = input.topic)

def handle(input, state):
    return noted(id = input.id, topic = input.topic)
"#;

#[test]
fn a_boundaried_command_without_fold_still_commits() {
    let project = write_project(&[
        ("events/t.star", NOTED_EVENTS),
        ("commands/note.star", NOTE),
    ]);
    let harness = Boot::new(project.path()).start();

    let first = harness
        .rt
        .execute(
            "note",
            json!({ "id": ALICE, "topic": "rust" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);

    // The boundary now matches the first event. It is read but not folded, so only
    // the read loop can advance `after`; if it did not, the append condition would
    // conflict against this command's own history and burn every retry.
    let second = harness
        .rt
        .execute("note", json!({ "id": BOB, "topic": "rust" }), &ctx(), None)
        .unwrap();
    assert_eq!(
        second.status, 200,
        "a fold-less boundary must not self-conflict: {:?}",
        second.body
    );
    assert_eq!(second.body["positions"]["first"], 2);
    assert_eq!(log_head(&harness.rt), 2);
    harness.shutdown();
}

// --- the fold contract ----------------------------------------------------

const UNIQUE_EMAIL_EVENTS: &str = r#"
registered = event(
    type = "t.registered",
    fields = {"id": uuid(), "email": str(max_length = 100)},
)
"#;

/// `fold` falls off the end, so it returns None and the guard it stands behind
/// (`state["taken"]`) would read as "nothing there".
const BROKEN_FOLD: &str = r#"
load("events/t.star", "registered")

input = schema(id = uuid(), email = str())

def query(input):
    return registered(email = input.email)

def initial():
    return {"taken": False}

def fold(state, event):
    state["taken"] = True

def handle(input, state):
    if state["taken"]:
        return reject("email_taken", "that email is already registered")
    return registered(id = input.id, email = input.email)
"#;

#[test]
fn a_fold_that_returns_none_fails_the_command() {
    let project = write_project(&[
        ("events/t.star", UNIQUE_EMAIL_EVENTS),
        ("commands/reg.star", BROKEN_FOLD),
    ]);
    let harness = Boot::new(project.path()).start();

    let body = json!({ "id": ALICE, "email": "dup@example.com" });
    // The boundary is empty on the first call, so the broken fold never runs.
    let first = harness
        .rt
        .execute("reg", body.clone(), &ctx(), None)
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);

    // The second call folds the committed event. A None state must be a hard error,
    // not a silently absent guard that lets the duplicate through.
    let err = exec_err(
        &harness.rt,
        "reg",
        json!({ "id": BOB, "email": "dup@example.com" }),
    );
    assert!(
        err.contains("fold() must return the updated state"),
        "a fold that falls off the end must fail loudly, got: {err}"
    );
    assert_eq!(log_head(&harness.rt), 1, "the duplicate must not commit");
    harness.shutdown();
}

// --- invalid_input from handle --------------------------------------------

const INVALID_INPUT_COMMAND: &str = r#"
load("events/t.star", "registered")

input = schema(id = uuid(), email = str())

def handle(input, state):
    if "@" not in input.email:
        return invalid_input("email must contain @")
    return registered(id = input.id, email = input.email)
"#;

#[test]
fn handle_returning_invalid_input_is_a_400_and_appends_nothing() {
    let project = write_project(&[
        ("events/t.star", UNIQUE_EMAIL_EVENTS),
        ("commands/reg.star", INVALID_INPUT_COMMAND),
    ]);
    let harness = Boot::new(project.path()).start();

    let bad = harness
        .rt
        .execute("reg", json!({ "id": ALICE, "email": "nope" }), &ctx(), None)
        .unwrap();
    assert_eq!(
        bad.status, 400,
        "invalid_input is a malformed body, not a state rejection: {:?}",
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

    // The other arm of the same handle still commits, so the 400 is the outcome and
    // not the command being broken.
    let ok = harness
        .rt
        .execute(
            "reg",
            json!({ "id": ALICE, "email": "a@example.com" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(ok.status, 200, "{:?}", ok.body);
    harness.shutdown();
}

// --- a commit with no events ----------------------------------------------

const NOOP_COMMAND: &str = r#"
input = schema(id = uuid())

def handle(input, state):
    return []
"#;

#[test]
fn a_command_that_emits_nothing_commits_with_null_positions() {
    let project = write_project(&[
        ("events/t.star", THING_EVENTS),
        ("commands/noop.star", NOOP_COMMAND),
    ]);
    let harness = Boot::new(project.path()).start();

    let body = json!({ "id": ALICE });
    let first = harness
        .rt
        .execute("noop", body.clone(), &ctx(), Some("k1"))
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);
    assert!(
        first.body["positions"].is_null(),
        "nothing was appended, so there is no position range: {:?}",
        first.body
    );
    assert_eq!(first.body["events"], json!([]));
    assert_eq!(log_head(&harness.rt), 0, "an empty emit must not append");

    // Nothing carries the idempotency tag, and this command has no boundary, so
    // there is nothing to recover: the replay legitimately re-runs `handle` and
    // reports its own identity.
    let second_ctx = ctx();
    let replay = harness
        .rt
        .execute("noop", body, &second_ctx, Some("k1"))
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

// --- input schema type checking -------------------------------------------

const SCALAR_EVENTS: &str = r#"
recorded = event(
    type = "t.recorded",
    fields = {"qty": int(), "code": str(max_length = 3)},
)
"#;

const RECORD_COMMAND: &str = r#"
load("events/t.star", "recorded")

input = schema(qty = int(), count = uint(), flag = bool(), code = str(max_length = 3))

def handle(input, state):
    return recorded(qty = input.qty, code = input.code)
"#;

#[test]
fn badly_typed_scalar_inputs_are_rejected_with_400() {
    let project = write_project(&[
        ("events/t.star", SCALAR_EVENTS),
        ("commands/record.star", RECORD_COMMAND),
    ]);
    let harness = Boot::new(project.path()).start();

    // Each body is well-formed apart from the one named field.
    let cases: [(&str, serde_json::Value); 4] = [
        (
            "count",
            json!({"qty": 1, "count": -1, "flag": true, "code": "abc"}),
        ),
        (
            "qty",
            json!({"qty": 1.5, "count": 0, "flag": true, "code": "abc"}),
        ),
        (
            "flag",
            json!({"qty": 1, "count": 0, "flag": "yes", "code": "abc"}),
        ),
        (
            "code",
            json!({"qty": 1, "count": 0, "flag": true, "code": "abcd"}),
        ),
    ];
    for (field, body) in cases {
        let result = harness.rt.execute("record", body, &ctx(), None).unwrap();
        assert_eq!(
            result.status, 400,
            "`{field}` should not have type-checked: {:?}",
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
            "record",
            json!({ "qty": -3, "count": 0, "flag": true, "code": "abc" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(
        ok.status, 200,
        "the schema must accept a well-typed body: {:?}",
        ok.body
    );
    harness.shutdown();
}
