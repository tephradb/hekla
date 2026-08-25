//! Durable execution of effects: the journal itself.
//!
//! `tests/effect.rs` covers the happy path and the wedge as seen from `/status`.
//! These tests pin the replay machinery underneath: a retry (and a crash restart)
//! must replay every completed journaled call instead of re-firing it, `now()` must
//! hand back the same recorded instant on every attempt, two byte-identical calls
//! must line up one-to-one through their disambiguators, and an edited script must
//! replay an in-flight invocation against its new code.
//!
//! Assertions go through the observable seams: the HTTP stub's call log, the
//! effect's health signals, and the operational DB's `effect_invocation` /
//! `effect_journal` tables read back with a second connection.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kiln::effect::{HttpClient, HttpResponse, StubHttpClient};
use kiln::runtime::Runtime;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

use support::{ALICE, BOB, Boot, Harness, ctx, log_head, wait_until};

const EFFECT: &str = "notify";

const USER_EVENTS: &str = r#"
user_registered = event(
    type = "user.registered",
    fields = {
        "user_id": uuid(),
        "email": str(),
        "name": str(),
    },
)

user_activated = event(
    type = "user.activated",
    fields = {"user_id": uuid()},
)
"#;

const REGISTER_USER: &str = r#"
load("events/user.star", "user_registered")

input = schema(user_id = uuid(), email = str(), name = str())

def handle(input, state):
    return user_registered(
        user_id = input.user_id,
        email = input.email,
        name = input.name,
    )
"#;

const ACTIVATE_USER: &str = r#"
load("events/user.star", "user_activated")

input = schema(user_id = uuid())

def handle(input, state):
    return user_activated(user_id = input.user_id)
"#;

const USERS_PROJECTOR: &str = r#"
load("events/user.star", "user_registered")

users = entity(
    key = "user_id",
    fields = {
        "user_id": uuid(),
        "email": str(),
        "name": str(),
    },
    indexes = [index("by_email", ["email"])],
)

def on_event(event):
    return [put(users, {
        "user_id": event.data.user_id,
        "email": event.data.email,
        "name": event.data.name,
    })]

handle = {user_registered(): on_event}
"#;

/// A project whose only effect is `effects/notify.star`, with the given body.
fn project(effect: &str) -> TempDir {
    support::write_project(&[
        ("events/user.star", USER_EVENTS),
        ("commands/register-user.star", REGISTER_USER),
        ("effects/notify.star", effect),
    ])
}

/// The same, plus the `users` read model and the `activate-user` command, for the
/// tests that need a second event type on the log.
fn project_with_read_model(effect: &str) -> TempDir {
    support::write_project(&[
        ("events/user.star", USER_EVENTS),
        ("commands/register-user.star", REGISTER_USER),
        ("commands/activate-user.star", ACTIVATE_USER),
        ("projectors/users.star", USERS_PROJECTOR),
        ("effects/notify.star", effect),
    ])
}

fn boot(project_dir: &Path, data_dir: &Path, http: Arc<dyn HttpClient>) -> Harness {
    Boot::new(project_dir).data_dir(data_dir).http(http).start()
}

fn register(rt: &Runtime, user_id: &str) {
    let body = json!({ "user_id": user_id, "email": format!("{user_id}@x"), "name": "U" });
    let result = rt.execute("register-user", body, &ctx(), None).unwrap();
    assert_eq!(result.status, 200, "register failed: {:?}", result.body);
}

fn activate(rt: &Runtime, user_id: &str) {
    let result = rt
        .execute("activate-user", json!({ "user_id": user_id }), &ctx(), None)
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

/// [`wait_until`] with a caller-chosen budget, for the one wait that can legitimately
/// outlast the shared helper's.
fn wait_up_to<F: Fn() -> bool>(budget: Duration, label: &str, cond: F) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {label}");
}

fn wait_for_failures(harness: &Harness, at_least: u64) {
    wait_until(&format!("{at_least} failed attempts"), || {
        harness.rt.effect(EFFECT).unwrap().consecutive_failures() >= at_least
    });
}

// --- reading the operational DB back --------------------------------------

fn open_db(data_dir: &Path) -> Connection {
    Connection::open(data_dir.join("kiln.db")).unwrap()
}

/// Every journal row for one invocation, as `(call_hash, disambiguator, result)`,
/// ordered the way the handler recorded them.
fn journal_rows(data_dir: &Path, position: u64) -> Vec<(String, u64, Value)> {
    let conn = open_db(data_dir);
    let mut stmt = conn
        .prepare(
            "SELECT call_hash, disambiguator, result FROM effect_journal \
             WHERE effect = ?1 AND position = ?2 ORDER BY created_at, disambiguator",
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
load("events/user.star", "user_registered")

def on_event(event, state):
    http.post(url = "https://a.test/first", body = {"id": event.data.user_id})
    http.post(url = "https://a.test/second", body = {})

handle = {user_registered(): on_event}
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

    // Only the successful call was journaled: the failing one never reaches
    // journal_put, which is why it is the one that re-runs.
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
    // window. The next boot re-enters handle(), replays `/first` from disk and runs
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
load("events/user.star", "user_registered")

def on_event(event, state):
    t = now()
    http.post(url = "https://a.test/at", body = {"t": t})
    http.post(url = "https://a.test/fail", body = {})

handle = {user_registered(): on_event}
"#;

#[test]
fn now_replays_the_recorded_timestamp_on_every_retry() {
    // `now()` is journaled so a replay sees the same instant. If it were live, every
    // retry would produce a different timestamp, changing the POST body and hence
    // the call hash, so the `/at` request would miss the journal and re-fire.
    let dir = project(NOW_THEN_POST);
    let data = tempfile::tempdir().unwrap();
    let stub = stub_failing_on("/fail");
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_for_failures(&harness, 3);

    let retries = calls_ending(&stub, "/fail");
    assert!(
        retries >= 3,
        "the handler re-ran to its failing tail {retries} times"
    );
    assert_eq!(
        calls_ending(&stub, "/at"),
        1,
        "a fresh now() on a retry would change the body and re-fire this call"
    );
    let recorded = post_body(&stub, "/at");
    let timestamp = recorded["t"].as_str().expect("now() returns a string");
    assert!(
        timestamp.starts_with("20") && timestamp.ends_with('Z'),
        "expected an rfc3339 instant, got `{timestamp}`"
    );

    // Two journal rows: the now() string and the successful POST. The `/fail` POST
    // never gets one, which is why it alone re-runs.
    let rows = journal_rows(data.path(), 1);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].2, Value::String(timestamp.to_owned()));

    harness.shutdown();
}

// --- disambiguators --------------------------------------------------------

const IDENTICAL_TWICE: &str = r#"
load("events/user.star", "user_registered")

def on_event(event, state):
    http.post(url = "https://a.test/twice", body = {"n": 1})
    http.post(url = "https://a.test/twice", body = {"n": 1})
    http.post(url = "https://a.test/fail", body = {})

handle = {user_registered(): on_event}
"#;

#[test]
fn identical_repeated_calls_journal_under_separate_disambiguators() {
    // Two byte-identical calls share a call hash, so only the per-hash counter keeps
    // them apart. It must restart at 0 for each attempt (or every retry misses and
    // re-fires both) and must be per-hash (or two different calls collide and one
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
        "identical calls share a call hash, so only the disambiguator separates them"
    );
    assert_eq!((rows[0].1, rows[1].1), (0, 1));
    // Each row holds its own call's response, so a replay hands the handler back the
    // two results in the order it made the calls.
    assert_eq!(rows[0].2["body"]["call"], 0);
    assert_eq!(rows[1].2["body"]["call"], 1);

    harness.shutdown();
}

// --- host-built wrappers read with a dot -----------------------------------

/// Reads every field of the response struct and echoes what it saw, so the assertion
/// below pins the shape rather than just that a dot parsed.
const RESPONSE_SHAPE: &str = r#"
load("events/user.star", "user_registered")

def on_event(event, state):
    response = http.post(url = "https://a.test/first", body = {"x": 1})
    http.post(url = "https://a.test/echo", body = {
        "status": response.status,
        "ok": response.body["ok"],
        "kind": response.headers["content-type"][0],
    })

handle = {user_registered(): on_event}
"#;

/// `{status, body, headers}` is host-built with a fixed shape, so it reads with a dot
/// like `input` and `event.data`. `body` and `headers` stay subscripted inside: a body
/// is whatever parsed, and a header name is not an attribute.
#[test]
fn an_http_response_reads_its_fixed_fields_with_a_dot() {
    let dir = project(RESPONSE_SHAPE);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::new(|_n, _req| {
        Ok(HttpResponse {
            status: 201,
            body: br#"{"ok": true}"#.to_vec(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        })
    }));
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_up_to(Duration::from_secs(30), "the echo call", || {
        calls_ending(&stub, "/echo") == 1
    });

    let echoed = post_body(&stub, "/echo");
    assert_eq!(echoed["status"], 201);
    assert_eq!(echoed["ok"], true);
    assert_eq!(echoed["kind"], "application/json");
    harness.shutdown();
}

/// A body that is not JSON reads back as a string, so the struct field is a union and
/// the dot does not imply a declared type the way an event field's does.
#[test]
fn a_non_json_response_body_reads_as_a_string() {
    let dir = project(
        r#"
load("events/user.star", "user_registered")

def on_event(event, state):
    response = http.post(url = "https://a.test/first", body = {"x": 1})
    http.post(url = "https://a.test/echo", body = {"body": response.body})

handle = {user_registered(): on_event}
"#,
    );
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::new(|_n, _req| {
        Ok(HttpResponse {
            status: 200,
            body: b"not json".to_vec(),
            headers: Vec::new(),
        })
    }));
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_up_to(Duration::from_secs(30), "the echo call", || {
        calls_ending(&stub, "/echo") == 1
    });
    assert_eq!(post_body(&stub, "/echo")["body"], "not json");
    harness.shutdown();
}

/// A field the struct does not carry is an attribute error naming it, which a dict
/// `invoke_command` returns `{status, body}`, the third host-built wrapper with a fixed
/// shape, so it reads with a dot too. Its `body` stays subscripted: that is the
/// command's own response payload, with no shape the host can promise.
#[test]
fn an_invoke_command_outcome_reads_its_fields_with_a_dot() {
    let dir = support::write_project(&[
        ("events/user.star", USER_EVENTS),
        ("commands/register-user.star", REGISTER_USER),
        ("commands/activate-user.star", ACTIVATE_USER),
        (
            "effects/notify.star",
            r#"
load("events/user.star", "user_registered")

def on_event(event, state):
    outcome = invoke_command("activate-user", {"user_id": event.data.user_id})
    http.post(url = "https://a.test/echo", body = {
        "status": outcome.status,
        "type": outcome.body["events"][0]["type"],
    })

handle = {user_registered(): on_event}
"#,
        ),
    ]);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_up_to(Duration::from_secs(30), "the echo call", || {
        calls_ending(&stub, "/echo") == 1
    });

    let echoed = post_body(&stub, "/echo");
    assert_eq!(echoed["status"], 200);
    assert_eq!(echoed["type"], "user.activated");
    harness.shutdown();
}

/// subscript could not do as precisely: the misspelling is the message.
#[test]
fn an_unknown_response_field_names_itself() {
    let dir = project(
        r#"
load("events/user.star", "user_registered")

def on_event(event, state):
    response = http.post(url = "https://a.test/first", body = {"x": 1})
    log(str(response.stauts))

handle = {user_registered(): on_event}
"#,
    );
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_for_failures(&harness, 1);

    let error = harness.rt.effect(EFFECT).unwrap().last_error().unwrap();
    assert!(error.contains("stauts"), "{error}");
    harness.shutdown();
}

// --- editing an effect under an in-flight invocation -----------------------

const TWO_POSTS_V2: &str = r#"
load("events/user.star", "user_registered")

def on_event(event, state):
    http.post(url = "https://a.test/first-v2", body = {"id": event.data.user_id})
    http.post(url = "https://a.test/second", body = {})

handle = {user_registered(): on_event}
"#;

#[test]
fn an_edited_effect_replays_an_in_flight_invocation_against_the_new_code() {
    // The journal is keyed by the content hash of each call, not by call order. Edit
    // a journaled call's arguments and its hash changes, so it misses and the side
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
    let stale_hash = before[0].0.clone();

    fs::write(dir.path().join("effects/notify.star"), TWO_POSTS_V2).unwrap();

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
        after.iter().any(|(hash, _, _)| hash == &stale_hash),
        "the old row survives, keyed under a hash the new code never asks for"
    );
}

// --- the instruction budget ------------------------------------------------

const RUNAWAY: &str = r#"
load("events/user.star", "user_registered")

def on_event(event, state):
    for i in range(100000000):
        pass
    http.post(url = "https://a.test/never", body = {})

handle = {user_registered(): on_event}
"#;

#[test]
fn a_runaway_handler_is_cut_off_by_the_tick_budget_and_wedges() {
    // The per-handler tick budget turns a runaway loop into an ordinary wedge. With
    // no budget the effect thread would spin forever: no failure in `/status`, a
    // frozen watermark, and a shutdown that blocks for the full join timeout.
    let dir = project(RUNAWAY);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, BOB);
    // The budget is 10M instructions, not wall clock, and an unoptimised starlark
    // interpreter can take well over a minute to burn through them.
    wait_up_to(Duration::from_secs(120), "the tick budget to trip", || {
        harness.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
    });

    let error = harness.rt.effect(EFFECT).unwrap().last_error().unwrap();
    assert!(error.contains("handle entry for"), "{error}");
    assert_eq!(stub.call_count(), 0, "the loop never reached the http call");
    assert!(journal_rows(data.path(), 1).is_empty());

    // The thread is alive and still honoring operator input, not hung.
    harness.rt.effect(EFFECT).unwrap().request_skip(1);
    wait_until("the skip to advance the effect", || {
        let effect = harness.rt.effect(EFFECT).unwrap();
        effect.consecutive_failures() == 0 && effect.position() >= 1
    });

    let started = Instant::now();
    harness.shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "shutdown must not wait out the join timeout"
    );
}

// --- per-type handle dispatch ---------------------------------------------

/// The keys are the subscription. `user_activated` is deliberately absent, so the
/// effect never reads those events at all, which is the point: there is no second list
/// that could disagree with this one. The second arm is constrained, so it fires only
/// for the registration that matches it.
const PER_TYPE_EFFECT: &str = r#"
load("events/user.star", "user_registered")

handle = {
    user_registered(): lambda event, state: http.post(
        url = "https://a.test/welcome/" + event.data.user_id,
        body = {"email": event.data.email},
    ),
    user_registered(name = "VIP"): lambda event, state: http.post(
        url = "https://a.test/vip/" + event.data.user_id,
        body = {},
    ),
}
"#;

#[test]
fn a_per_type_effect_handle_subscribes_to_exactly_its_arms() {
    let dir = project_with_read_model(PER_TYPE_EFFECT);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::new(|_, _| {
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }));
    let harness = boot(dir.path(), data.path(), stub.clone());

    // A plain registration matches only the unconstrained arm.
    register(&harness.rt, ALICE);
    // A VIP registration matches both, so both fire for the one event.
    let vip = json!({ "user_id": BOB, "email": "bob@x", "name": "VIP" });
    let result = harness
        .rt
        .execute("register-user", vip, &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    activate(&harness.rt, ALICE);

    wait_until("both registrations to be handled", || {
        harness.rt.effect(EFFECT).unwrap().position() >= 2
    });
    thread::sleep(Duration::from_millis(100));

    assert_eq!(calls_ending(&stub, ALICE), 1, "one arm matches ALICE");
    let urls: Vec<String> = stub.calls().iter().map(|call| call.url.clone()).collect();
    assert!(
        urls.contains(&format!("https://a.test/welcome/{BOB}"))
            && urls.contains(&format!("https://a.test/vip/{BOB}")),
        "both matching arms fire for one event, got {urls:?}"
    );
    // Declaration order, so a replay journals and replays the same call sequence.
    let welcome = urls
        .iter()
        .position(|u| u.ends_with(&format!("welcome/{BOB}")));
    let vip_call = urls.iter().position(|u| u.ends_with(&format!("vip/{BOB}")));
    assert!(
        welcome < vip_call,
        "arms run in declaration order: {urls:?}"
    );

    // `user.activated` is not in any key, so the effect never subscribed to it and
    // there is no invocation to account for.
    assert_eq!(
        invocation_status(data.path(), 3),
        None,
        "an unsubscribed type is never read"
    );
    assert_eq!(harness.rt.effect(EFFECT).unwrap().consecutive_failures(), 0);
    harness.shutdown();
}

// --- the boundary is folded, never journaled -------------------------------

/// Folds the registrations for this user and reports the count in the POST body, so
/// the assertion below reads the state the handler actually saw.
const FOLDING_EFFECT: &str = r#"
load("events/user.star", "user_registered", "user_activated")

def query(event):
    return [user_activated(user_id = event.data.user_id)]

initial = {"activations": 0}

fold = {user_activated(): lambda state, event: {"activations": state["activations"] + 1}}

def on_event(event, state):
    http.post(
        url = "https://a.test/seen",
        body = {"activations": state["activations"]},
    )

handle = {user_registered(): on_event}
"#;

#[test]
fn a_fold_reads_state_written_one_position_earlier_without_a_projector() {
    // The case this whole design exists for. Under the old `read()` an effect at
    // position N that needed state written at N-1 could observe the projector before
    // it caught up, journal the miss as `null`, and then replay that null forever: a
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
    // had run by the time the handler happened to execute, which is exactly the
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
    wait_up_to(Duration::from_secs(20), "further retries", || {
        calls_ending(&stub, "/seen") >= 5
    });

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
