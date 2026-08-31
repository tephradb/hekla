//! Durable execution of effects: the journal itself.
//!
//! `tests/effect.rs` covers the happy path and the wedge as seen from `/status`.
//! These tests pin the replay machinery underneath: a retry (and a crash restart)
//! must replay every completed journaled call instead of re-firing it, `now()` must
//! hand back the same recorded instant on every attempt, two byte-identical calls
//! must line up one-to-one through their ordinals, and an edited effect must replay
//! an in-flight invocation against its new code.
//!
//! Assertions go through the observable seams: the HTTP stub's call log, the
//! effect's health signals, and the operational DB's `effect_invocation` /
//! `effect_journal` tables read back with a second connection.
//!
//! One of the Starlark suite's cases is gone rather than ported. It drove a `handle`
//! into the tick budget and asserted the resulting wedge; heklang has no `while`,
//! rejects recursion and iterates only finite containers, so there is no runaway to
//! cut off and no budget to cut it off with.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use hekla::effect::StubHttpClient;
use hekla::http::{HttpClient, HttpResponse};
use hekla::runtime::Runtime;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

use support::{ALICE, BOB, Boot, Harness, ctx, log_head, wait_until};

const EFFECT: &str = "Notify";

const USER_EVENTS: &str = r#"
event @user.registered {
  user_id: Uuid,
  email: String @max(100),
  name: String @max(50),
}

event @user.activated { user_id: Uuid }
"#;

const REGISTER_USER: &str = r#"
command RegisterUser(user_id: Uuid, email: String, name: String) {
  emit @user.registered { user_id, email, name }
}
"#;

const ACTIVATE_USER: &str = r#"
command ActivateUser(user_id: Uuid) {
  emit @user.activated { user_id }
}
"#;

const USERS_PROJECTOR: &str = r#"
projector Users {
  entity User {
    user_id: Uuid @key,
    email: String @max(100) @index,
    name: String @max(50),
  }

  on @user.registered { user_id, email, name } {
    put User { user_id, email, name }
  }
}
"#;

/// A project whose only effect is `effects/notify.hk`, with the given body.
fn project(effect: &str) -> TempDir {
    support::write_project(&[
        ("events/user.hk", USER_EVENTS),
        ("commands/register-user.hk", REGISTER_USER),
        ("effects/notify.hk", effect),
    ])
}

/// The same, plus the `Users` read model and the `ActivateUser` command, for the
/// tests that need a second event type on the log.
fn project_with_read_model(effect: &str) -> TempDir {
    support::write_project(&[
        ("events/user.hk", USER_EVENTS),
        ("commands/register-user.hk", REGISTER_USER),
        ("commands/activate-user.hk", ACTIVATE_USER),
        ("projectors/users.hk", USERS_PROJECTOR),
        ("effects/notify.hk", effect),
    ])
}

fn boot(project_dir: &Path, data_dir: &Path, http: Arc<dyn HttpClient>) -> Harness {
    Boot::new(project_dir).data_dir(data_dir).http(http).start()
}

fn register(rt: &Runtime, user_id: &str) {
    let body = json!({ "user_id": user_id, "email": format!("{user_id}@x"), "name": "U" });
    let result = rt.execute("RegisterUser", body, &ctx(), None).unwrap();
    assert_eq!(result.status, 200, "register failed: {:?}", result.body);
}

fn activate(rt: &Runtime, user_id: &str) {
    let result = rt
        .execute("ActivateUser", json!({ "user_id": user_id }), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 200, "activate failed: {:?}", result.body);
}
/// A stub that answers 500 for any URL ending in `fail_suffix` and 200 otherwise,
/// so a handler wedges at a chosen call while the earlier ones succeed.
fn stub_failing_on(fail_suffix: &'static str) -> Arc<StubHttpClient> {
    Arc::new(StubHttpClient::new(move |_, request| {
        Ok(HttpResponse {
            status: if request.url.ends_with(fail_suffix) {
                500
            } else {
                200
            },
            headers: Vec::new(),
            body: b"{}".to_vec(),
        })
    }))
}

fn calls_ending(stub: &StubHttpClient, suffix: &str) -> usize {
    stub.calls()
        .iter()
        .filter(|call| call.url.ends_with(suffix))
        .count()
}

fn post_body(stub: &StubHttpClient, suffix: &str) -> Value {
    let calls = stub.calls();
    let call = calls
        .iter()
        .find(|call| call.url.ends_with(suffix))
        .unwrap_or_else(|| panic!("no request to a url ending in `{suffix}`"));
    serde_json::from_slice(call.body.as_deref().expect("a POST body")).unwrap()
}

fn wait_for_failures(harness: &Harness, at_least: u64) {
    wait_until(&format!("{at_least} failed attempts"), || {
        harness.rt.effect(EFFECT).unwrap().consecutive_failures() >= at_least
    });
}

// --- reading the operational DB back --------------------------------------

fn open_db(data_dir: &Path) -> Connection {
    Connection::open(data_dir.join("hekla.db")).unwrap()
}

/// Every journal row for one invocation, as `(call_hash, disambiguator, result)`,
/// ordered the way the handler recorded them.
fn journal_rows(data_dir: &Path, position: u64) -> Vec<(String, u64, Value)> {
    let conn = open_db(data_dir);
    let mut stmt = conn
        .prepare(
            "SELECT call_hash, disambiguator, result FROM effect_journal \
             WHERE effect = ?1 AND position = ?2 ORDER BY rowid",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![EFFECT, position as i64], |row| {
            let hash: String = row.get(0)?;
            let disambiguator: i64 = row.get(1)?;
            let result: String = row.get(2)?;
            Ok((hash, disambiguator as u64, result))
        })
        .unwrap();
    rows.map(|row| {
        let (hash, disambiguator, result) = row.unwrap();
        (hash, disambiguator, serde_json::from_str(&result).unwrap())
    })
    .collect()
}

fn invocation_status(data_dir: &Path, position: u64) -> Option<String> {
    let conn = open_db(data_dir);
    conn.query_row(
        "SELECT status FROM effect_invocation WHERE effect = ?1 AND position = ?2",
        params![EFFECT, position as i64],
        |row| row.get(0),
    )
    .ok()
}

// --- replay on a retry -----------------------------------------------------

const TWO_POSTS: &str = r#"
effect Notify {
  on @user.registered { user_id } {
    http.post("https://a.test/first", { "id": user_id })
    http.post("https://a.test/second", {})
  }
}
"#;

#[test]
fn retrying_a_wedged_invocation_replays_the_journal_without_refiring() {
    // The durability promise: a wedged invocation retries forever, but each retry
    // replays the already-journaled `/first` POST instead of sending it again, and
    // fails at the same point. Without the journal hit every retry would re-send
    // `/first` too: a duplicate side effect at the retry cadence, forever.
    let dir = project(TWO_POSTS);
    let data = tempfile::tempdir().unwrap();
    let stub = stub_failing_on("/second");
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE); // user.registered at position 1
    wait_for_failures(&harness, 3);

    let first = calls_ending(&stub, "/first");
    let second = calls_ending(&stub, "/second");
    assert_eq!(
        first, 1,
        "the journaled call must replay from the journal, not re-fire"
    );
    assert!(
        second >= 3,
        "the unjournaled tail re-runs on every attempt, got {second}"
    );
    assert_eq!(post_body(&stub, "/first")["id"], ALICE);

    // Only the successful call was journaled: rule 5 absorbs the 500 inside the
    // language and the call never reaches `record`, which is why it is the one that
    // re-runs.
    let rows = journal_rows(data.path(), 1);
    assert_eq!(rows.len(), 1, "exactly one journaled call: {rows:?}");
    assert_eq!(rows[0].1, 0);
    assert_eq!(rows[0].2["status"], 200);
    assert_eq!(log_head(&harness.rt), 1, "a wedged effect appends nothing");

    harness.shutdown();
}

#[test]
fn a_crashed_invocation_resumes_from_the_journal_and_runs_only_the_tail() {
    // Shutting down mid-wedge leaves the invocation `running`, the real crash
    // window. The next boot re-enters the arm, replays `/first` from disk and runs
    // only the unjournaled tail live.
    let dir = project(TWO_POSTS);
    let data = tempfile::tempdir().unwrap();

    let stub1 = stub_failing_on("/second");
    let harness = boot(dir.path(), data.path(), stub1.clone());
    register(&harness.rt, ALICE);
    wait_for_failures(&harness, 1);
    harness.shutdown();

    assert_eq!(
        invocation_status(data.path(), 1).as_deref(),
        Some("running"),
        "an interrupted invocation stays running so it replays"
    );
    assert_eq!(journal_rows(data.path(), 1).len(), 1);
    assert!(calls_ending(&stub1, "/first") >= 1);

    // Second boot, this time with a transport that answers everything.
    let stub2 = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub2.clone());
    wait_until("the replay to finish", || {
        harness.rt.effect(EFFECT).unwrap().position() >= 1
    });

    assert_eq!(
        calls_ending(&stub2, "/first"),
        0,
        "the completed call must replay from the journal across a restart"
    );
    assert_eq!(
        stub2.call_count(),
        1,
        "only the unjournaled tail runs live: {:?}",
        stub2
            .calls()
            .iter()
            .map(|c| c.url.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(calls_ending(&stub2, "/second"), 1);
    assert_eq!(harness.rt.effect(EFFECT).unwrap().consecutive_failures(), 0);

    harness.shutdown();
    assert_eq!(
        invocation_status(data.path(), 1).as_deref(),
        Some("terminal")
    );
    assert_eq!(journal_rows(data.path(), 1).len(), 2);
}

// --- journaled now() -------------------------------------------------------

const NOW_THEN_POST: &str = r#"
effect Notify {
  on @user.registered {
    let t = now()
    http.post("https://a.test/at", { "t": t })
    http.post("https://a.test/fail", {})
  }
}
"#;

#[test]
fn now_replays_the_recorded_timestamp_on_every_retry() {
    // `now()` is journaled so a replay sees the same instant. If it were live, every
    // retry would produce a different timestamp, changing the POST body and hence
    // the call key, so the `/at` request would miss the journal and re-fire.
    let dir = project(NOW_THEN_POST);
    let data = tempfile::tempdir().unwrap();
    let stub = stub_failing_on("/fail");
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_for_failures(&harness, 3);

    let retries = calls_ending(&stub, "/fail");
    assert!(
        retries >= 3,
        "the arm re-ran to its failing tail {retries} times"
    );
    assert_eq!(
        calls_ending(&stub, "/at"),
        1,
        "a fresh now() on a retry would change the body and re-fire this call"
    );
    let recorded = post_body(&stub, "/at");
    // A `Timestamp` on the wire is epoch microseconds, per rule 8's table. The RFC
    // 3339 string the envelope stamps is hekla's own form and stops at the seam.
    let micros = recorded["t"].as_i64().expect("now() is a Timestamp");
    assert!(
        micros > 1_600_000_000_000_000,
        "expected epoch microseconds, got {micros}"
    );

    // Two journal rows: the pinned instant and the successful POST. The `/fail` POST
    // never gets one, which is why it alone re-runs.
    let rows = journal_rows(data.path(), 1);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].2["micros"], micros);

    harness.shutdown();
}

// --- ordinals --------------------------------------------------------------

const IDENTICAL_TWICE: &str = r#"
effect Notify {
  on @user.registered {
    http.post("https://a.test/twice", { "n": 1 })
    http.post("https://a.test/twice", { "n": 1 })
    http.post("https://a.test/fail", {})
  }
}
"#;

#[test]
fn identical_repeated_calls_journal_under_separate_ordinals() {
    // Two byte-identical calls share a call key, so only the per-key ordinal keeps
    // them apart. It must restart at 0 for each attempt (or every retry misses and
    // re-fires both) and must be per-key (or two different calls collide and one
    // replays the other's result).
    let dir = project(IDENTICAL_TWICE);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::new(|index, request| {
        Ok(HttpResponse {
            status: if request.url.ends_with("/fail") {
                500
            } else {
                200
            },
            headers: Vec::new(),
            body: format!("{{\"call\":{index}}}").into_bytes(),
        })
    }));
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_for_failures(&harness, 2);

    assert_eq!(
        calls_ending(&stub, "/twice"),
        2,
        "both identical calls fire once in total, not once per attempt"
    );

    let rows = journal_rows(data.path(), 1);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows[0].0, rows[1].0,
        "identical calls share a call key, so only the ordinal separates them"
    );
    assert_eq!((rows[0].1, rows[1].1), (0, 1));
    // Each row holds its own call's response, so a replay hands the arm back the two
    // results in the order it made the calls.
    assert_eq!(rows[0].2["body"]["call"], 0);
    assert_eq!(rows[1].2["body"]["call"], 1);

    harness.shutdown();
}

// --- what a response and an outcome expose ---------------------------------

/// Reads both fields of the response and echoes what it saw, so the assertion below
/// pins the shape rather than just that a dot parsed.
const RESPONSE_SHAPE: &str = r#"
effect Notify {
  on @user.registered {
    let response = http.post("https://a.test/first", { "x": 1 })
    http.post("https://a.test/echo", {
      "status": response.status,
      "ok": response.body.bool("ok"),
      "id": response.body.string("id"),
    })
  }
}
"#;

/// A response is `{status, body}` and nothing else. `status` is an `Int` read with a
/// dot; `body` is an opaque `Json` read through the fallible one-step accessors of
/// rule 8, because a body has no declared shape to promise.
///
/// The Starlark version also read `response.headers["content-type"]`. heklang's
/// response carries no headers at all: the one header the runtime acts on is
/// `Retry-After`, and rule 5 makes it the host's business precisely so an arm cannot
/// see it. So this asserts two fields where that asserted three.
#[test]
fn a_response_exposes_its_status_and_its_body_and_nothing_else() {
    let dir = project(RESPONSE_SHAPE);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::new(|_n, _req| {
        Ok(HttpResponse {
            status: 201,
            body: br#"{"ok": true, "id": "abc"}"#.to_vec(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        })
    }));
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_until("the echo call", || calls_ending(&stub, "/echo") == 1);

    let echoed = post_body(&stub, "/echo");
    assert_eq!(echoed["status"], 201);
    assert_eq!(echoed["ok"], true);
    assert_eq!(echoed["id"], "abc");
    harness.shutdown();
}

/// A body that is not JSON is not a transport failure. Rule 5 already decided the
/// attempt reached the far side, so the arm sees the status either way and the body
/// answers every accessor with `none` rather than wedging the effect.
#[test]
fn a_non_json_response_body_is_not_a_failure_and_reads_as_absent() {
    let dir = project(
        r#"
effect Notify {
  on @user.registered {
    let response = http.post("https://a.test/first", { "x": 1 })
    http.post("https://a.test/echo", {
      "status": response.status,
      "ok": response.body.bool("ok"),
    })
  }
}
"#,
    );
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::new(|_n, req| {
        Ok(HttpResponse {
            status: 200,
            body: if req.url.ends_with("/first") {
                b"not json".to_vec()
            } else {
                b"{}".to_vec()
            },
            headers: Vec::new(),
        })
    }));
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_until("the echo call", || calls_ending(&stub, "/echo") == 1);

    let echoed = post_body(&stub, "/echo");
    assert_eq!(echoed["status"], 200);
    assert_eq!(
        echoed["ok"],
        Value::Null,
        "an unparseable body reads absent"
    );
    assert_eq!(
        harness.rt.effect(EFFECT).unwrap().consecutive_failures(),
        0,
        "a body the far side sent is not this runtime's failure"
    );
    harness.shutdown();
}

/// `invoke` answers with an outcome, read through `.ok()`, `.code()` and `.message()`.
/// It is deliberately not a status and a body: rule 6 cuts the retryable outcomes out
/// of the type entirely, so `Conflict` and `Unavailable` are unrepresentable here
/// rather than filtered.
#[test]
fn an_invoke_outcome_reads_through_its_three_accessors() {
    let dir = support::write_project(&[
        ("events/user.hk", USER_EVENTS),
        ("commands/register-user.hk", REGISTER_USER),
        (
            "commands/activate-user.hk",
            r#"
refusal AlreadyActive "that user is already active"

command ActivateUser(user_id: Uuid) {
  state activated: Bool = fold false
    on @user.activated(user_id) => true

  if activated {
    return reject AlreadyActive
  }

  emit @user.activated { user_id }
}
"#,
        ),
        (
            "effects/notify.hk",
            r#"
effect Notify {
  on @user.registered { user_id } {
    let first = invoke ActivateUser { user_id }
    let second = invoke ActivateUser { user_id }
    http.post("https://a.test/echo", {
      "first_ok": first.ok(),
      "second_ok": second.ok(),
      "code": second.code(),
      "message": second.message(),
    })
  }
}
"#,
        ),
    ]);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_until("the echo call", || calls_ending(&stub, "/echo") == 1);

    let echoed = post_body(&stub, "/echo");
    assert_eq!(echoed["first_ok"], true);
    assert_eq!(echoed["second_ok"], false);
    assert_eq!(echoed["code"], "already_active");
    assert_eq!(echoed["message"], "that user is already active");
    // Two distinct `invoke` calls, so two journal rows even though the arguments
    // match: the outcomes differ and each replay must get its own back.
    assert_eq!(journal_rows(data.path(), 1).len(), 3);
    harness.shutdown();
}

/// A field the response does not carry is a load error naming it, not a wedge.
///
/// The Starlark version drove this to a runtime attribute error and read it out of
/// `/status`, because a misspelling could only be found by executing the line. A
/// response has a declared shape here, so the misspelling never boots.
#[test]
fn an_unknown_response_field_is_refused_at_load() {
    support::assert_error(
        &[
            ("events/user.hk", USER_EVENTS),
            ("commands/register-user.hk", REGISTER_USER),
            (
                "effects/notify.hk",
                r#"
effect Notify {
  on @user.registered {
    let response = http.post("https://a.test/first", { "x": 1 })
    log("{response.stauts}")
  }
}
"#,
            ),
        ],
        "stauts",
    );
}

// --- editing an effect under an in-flight invocation -----------------------

const TWO_POSTS_V2: &str = r#"
effect Notify {
  on @user.registered { user_id } {
    http.post("https://a.test/first-v2", { "id": user_id })
    http.post("https://a.test/second", {})
  }
}
"#;

#[test]
fn an_edited_effect_replays_an_in_flight_invocation_against_the_new_code() {
    // The journal is keyed by the content of each call, not by call order. Edit a
    // journaled call's arguments and its key changes, so it misses and the side
    // effect fires again against the new code, while untouched calls still replay.
    // This is the at-least-once boundary an operator hits when hotfixing a wedged
    // effect.
    let dir = project(TWO_POSTS);
    let data = tempfile::tempdir().unwrap();

    let harness = boot(dir.path(), data.path(), stub_failing_on("/second"));
    register(&harness.rt, ALICE);
    wait_for_failures(&harness, 1);
    harness.shutdown();

    let before = journal_rows(data.path(), 1);
    assert_eq!(before.len(), 1);
    let stale_key = before[0].0.clone();

    fs::write(dir.path().join("effects/notify.hk"), TWO_POSTS_V2).unwrap();

    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());
    wait_until("the replay to finish", || {
        harness.rt.effect(EFFECT).unwrap().position() >= 1
    });

    let urls: Vec<String> = stub.calls().iter().map(|call| call.url.clone()).collect();
    assert_eq!(
        urls,
        vec![
            "https://a.test/first-v2".to_owned(),
            "https://a.test/second".to_owned(),
        ],
        "the edited call misses the journal and re-fires; the tail runs as usual"
    );

    harness.shutdown();
    assert_eq!(
        invocation_status(data.path(), 1).as_deref(),
        Some("terminal")
    );

    let after = journal_rows(data.path(), 1);
    assert_eq!(after.len(), 3, "{after:?}");
    assert!(
        after.iter().any(|(key, _, _)| key == &stale_key),
        "the old row survives, keyed under a call the new code never makes"
    );
}

// --- per-type arm dispatch -------------------------------------------------

/// The arms are the subscription. `@user.activated` is deliberately absent, so the
/// effect never reads those events at all, which is the point: there is no second list
/// that could disagree with this one.
///
/// The Starlark version had two arms on one event type, the second constrained, and
/// asserted both fired. Rule 1 makes an event select exactly one arm and two arms
/// naming one type a parse error, so what is left to pin is that the subscription is
/// exactly the arms and nothing else.
const PER_TYPE_EFFECT: &str = r#"
effect Notify {
  on @user.registered { user_id, email } {
    http.post("https://a.test/welcome/{user_id}", { "email": email })
  }
}
"#;

#[test]
fn an_effects_arms_are_exactly_its_subscription() {
    let dir = project_with_read_model(PER_TYPE_EFFECT);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE); // position 1
    register(&harness.rt, BOB); // position 2
    activate(&harness.rt, ALICE); // position 3, unsubscribed

    wait_until("both registrations to be handled", || {
        harness.rt.effect(EFFECT).unwrap().position() >= 2
    });
    thread::sleep(Duration::from_millis(100));

    let urls: Vec<String> = stub.calls().iter().map(|call| call.url.clone()).collect();
    assert_eq!(
        urls,
        vec![
            format!("https://a.test/welcome/{ALICE}"),
            format!("https://a.test/welcome/{BOB}"),
        ],
        "one call per subscribed event, in log order"
    );

    // `@user.activated` is in no arm, so the effect never subscribed to it and there
    // is no invocation to account for.
    assert_eq!(
        invocation_status(data.path(), 3),
        None,
        "an unsubscribed type is never read"
    );
    assert_eq!(harness.rt.effect(EFFECT).unwrap().consecutive_failures(), 0);
    harness.shutdown();
}

// --- the boundary is folded, never journaled -------------------------------

/// Folds the activations for this user and reports the count in the POST body, so the
/// assertion below reads the state the arm actually saw.
const FOLDING_EFFECT: &str = r#"
effect Notify {
  on @user.registered { user_id } {
    state activations: Int = fold 0
      on @user.activated(user_id) => activations + 1

    http.post("https://a.test/seen", { "activations": activations })
  }
}
"#;

#[test]
fn a_fold_reads_state_written_one_position_earlier_without_a_projector() {
    // The case this whole design exists for. Under a read of a projector's row, an
    // effect at position N that needed state written at N-1 could observe the read
    // model before it caught up, journal the miss, and then replay it forever: a
    // permanent wedge only an operator skip could clear. A fold cannot miss, because
    // it reads the log itself, up to this event's own position.
    let dir = project_with_read_model(FOLDING_EFFECT);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());

    // position 1: the activation the effect will fold. It is not subscribed to, so
    // the effect never runs for it, exactly as a projector-written row would not be.
    activate(&harness.rt, ALICE);
    // position 2: the registration the effect does fire on, one position later.
    register(&harness.rt, ALICE);
    wait_until("the effect to post", || calls_ending(&stub, "/seen") == 1);

    assert_eq!(
        post_body(&stub, "/seen")["activations"],
        1,
        "the fold saw the event appended one position earlier"
    );
    assert_eq!(
        harness.rt.effect(EFFECT).unwrap().consecutive_failures(),
        0,
        "no wedge: a fold cannot observe a state that has not caught up"
    );
    harness.shutdown();
}

#[test]
fn a_fold_is_not_journaled_and_reproduces_itself_on_every_retry() {
    // The fold is derived from the log prefix and the triggering position, so it needs
    // no journal entry: recording it would buy nothing and would freeze a point-in-time
    // answer. Only the POST is journaled, and every retry re-folds and agrees.
    let dir = project_with_read_model(FOLDING_EFFECT);
    let data = tempfile::tempdir().unwrap();
    let stub = stub_failing_on("/seen");
    let harness = boot(dir.path(), data.path(), stub.clone());

    activate(&harness.rt, ALICE); // position 1
    register(&harness.rt, ALICE); // position 2, the trigger
    wait_for_failures(&harness, 3);

    let rows = journal_rows(data.path(), 2);
    assert!(
        rows.is_empty(),
        "a 5xx is absorbed before the journal, and the fold writes nothing: {rows:?}"
    );
    let bodies: Vec<Value> = stub
        .calls()
        .iter()
        .filter(|call| call.url.ends_with("/seen"))
        .map(|call| serde_json::from_slice(call.body.as_deref().unwrap()).unwrap())
        .collect();
    assert!(bodies.len() >= 3, "the invocation retried: {bodies:?}");
    assert!(
        bodies.iter().all(|body| body["activations"] == 1),
        "every retry re-folds and gets the same answer: {bodies:?}"
    );
    harness.shutdown();
}

#[test]
fn the_boundary_stops_at_the_triggering_position() {
    // Folding to the log head instead would make the state depend on how far the log
    // had run by the time the arm happened to execute, which is exactly the
    // nondeterminism the design removes. Here the effect is deliberately kept behind:
    // it wedges on position 2 while position 3 lands, and every retry must still see
    // the log as it stood at position 2.
    let dir = project_with_read_model(FOLDING_EFFECT);
    let data = tempfile::tempdir().unwrap();
    let stub = stub_failing_on("/seen");
    let harness = boot(dir.path(), data.path(), stub.clone());

    activate(&harness.rt, ALICE); // position 1
    register(&harness.rt, ALICE); // position 2, the wedged trigger
    wait_for_failures(&harness, 2);

    // A second activation lands while the effect is stuck at position 2.
    activate(&harness.rt, ALICE); // position 3
    wait_until("further retries", || calls_ending(&stub, "/seen") >= 5);

    let bodies: Vec<Value> = stub
        .calls()
        .iter()
        .filter(|call| call.url.ends_with("/seen"))
        .map(|call| serde_json::from_slice(call.body.as_deref().unwrap()).unwrap())
        .collect();
    assert!(
        bodies.iter().all(|body| body["activations"] == 1),
        "the fold must not pick up position 3, which is past the trigger: {bodies:?}"
    );
    harness.shutdown();
}
