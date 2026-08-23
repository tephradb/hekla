//! Durable execution of effects: the journal itself.
//!
//! `tests/effect.rs` covers the happy path and the wedge as seen from `/status`.
//! These tests pin the replay machinery underneath: a retry (and a crash restart)
//! must replay every completed journaled call instead of re-firing it, `now()` must
//! hand back the same recorded instant on every attempt, two byte-identical calls
//! must line up one-to-one through their disambiguators, and `read()`/`scan()` must
//! journal what they saw (including a miss) so a replay is deterministic.
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

use support::{ALICE, BOB, Boot, Harness, MISSING, ctx, log_head, wait_position, wait_until};

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

source = [user_registered()]

def handle(event):
    return [put(users, {
        "user_id": event.data["user_id"],
        "email": event.data["email"],
        "name": event.data["name"],
    })]
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
/// tests that exercise the journaled `read()` and `scan()` builtins.
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

source = [user_registered()]

def handle(event):
    http.post(url = "https://a.test/first", body = {"id": event.data["user_id"]})
    http.post(url = "https://a.test/second", body = {})
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

source = [user_registered()]

def handle(event):
    t = now()
    http.post(url = "https://a.test/at", body = {"t": t})
    http.post(url = "https://a.test/fail", body = {})
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

source = [user_registered()]

def handle(event):
    http.post(url = "https://a.test/twice", body = {"n": 1})
    http.post(url = "https://a.test/twice", body = {"n": 1})
    http.post(url = "https://a.test/fail", body = {})
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

// --- journaled read() and scan() -------------------------------------------

const READ_AND_SCAN: &str = r#"
load("events/user.star", "user_activated")

source = [user_activated()]

def handle(event):
    row = read("users", "users", event.data["user_id"])
    page = scan("users", "users", field = "email", value = row["email"], limit = 10)
    http.post(url = "https://a.test/sync", body = {
        "name": row["name"],
        "found": len(page["items"]),
        "cursor": page["next_cursor"],
    })
"#;

#[test]
fn an_effect_reads_and_scans_a_projector_and_journals_the_results() {
    let dir = project_with_read_model(READ_AND_SCAN);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    // Let the read model catch up first: the effect reads it, and a miss would be
    // journaled permanently (see a_journaled_read_miss_is_frozen_across_retries).
    wait_position(&harness.rt, "users", 1);
    activate(&harness.rt, ALICE); // user.activated at position 2

    wait_until("the sync post", || stub.call_count() >= 1);
    let body = post_body(&stub, "/sync");
    assert_eq!(body["name"], "U", "read() returned the projected row");
    assert_eq!(body["found"], 1, "scan() filtered on the indexed email");
    assert_eq!(
        body["cursor"],
        Value::Null,
        "a single-row page has no cursor"
    );

    wait_until("the invocation to complete", || {
        harness.rt.effect(EFFECT).unwrap().position() >= 2
    });
    thread::sleep(Duration::from_millis(50));
    assert_eq!(stub.call_count(), 1, "the invocation ran once");

    let rows = journal_rows(data.path(), 2);
    assert_eq!(
        rows.len(),
        3,
        "read, scan and http are all journaled: {rows:?}"
    );
    assert_eq!(rows[0].2["email"], format!("{ALICE}@x"));
    assert_eq!(rows[1].2["items"][0]["user_id"], ALICE);
    assert_eq!(rows[1].2["next_cursor"], Value::Null);
    assert_eq!(rows[2].2["status"], 200);

    harness.shutdown();
}

/// Boot a project whose effect fires on registration and fails immediately, then
/// assert the wedge message and that nothing was journaled.
fn assert_wedges_with(effect: &str, needle: &str) {
    let dir = project_with_read_model(effect);
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE);
    wait_for_failures(&harness, 1);

    let error = harness.rt.effect(EFFECT).unwrap().last_error().unwrap();
    assert!(error.contains(needle), "expected `{needle}` in `{error}`");
    assert_eq!(
        stub.call_count(),
        0,
        "the handler failed before its http call"
    );
    assert_eq!(log_head(&harness.rt), 1, "a wedged effect appends nothing");
    assert!(
        journal_rows(data.path(), 1).is_empty(),
        "a failed call is never journaled, so a retry re-runs it"
    );

    harness.shutdown();
}

#[test]
fn read_and_scan_reject_an_unknown_projector_and_an_unindexed_filter() {
    assert_wedges_with(
        r#"
load("events/user.star", "user_registered")

source = [user_registered()]

def handle(event):
    read("nope", "users", event.data["user_id"])
    http.post(url = "https://a.test/never", body = {})
"#,
        "no projector `nope`",
    );

    assert_wedges_with(
        r#"
load("events/user.star", "user_registered")

source = [user_registered()]

def handle(event):
    scan("users", "users", field = "name", value = "U")
    http.post(url = "https://a.test/never", body = {})
"#,
        "not indexed",
    );
}

const READ_A_MISSING_ROW: &str = r#"
load("events/user.star", "user_registered")

source = [user_registered()]

def handle(event):
    row = read("users", "users", "99999999-9999-9999-9999-999999999999")
    http.post(url = "https://a.test/seen", body = {"found": row != None})
    http.post(url = "https://a.test/fail", body = {})
"#;

#[test]
fn a_journaled_read_miss_is_frozen_across_retries() {
    // A read of an absent row records `null`, and that null is what every later
    // retry replays: the row can never be observed, even once the projector catches
    // up. This is deliberate (replay determinism), and the only way out of the
    // resulting wedge is an operator `request_skip`. Pinned here so a "fix" that
    // re-queries on retry cannot land silently.
    let dir = project_with_read_model(READ_A_MISSING_ROW);
    let data = tempfile::tempdir().unwrap();
    let stub = stub_failing_on("/fail");
    let harness = boot(dir.path(), data.path(), stub.clone());

    register(&harness.rt, ALICE); // position 1: the effect reads MISSING and misses
    wait_for_failures(&harness, 1);

    // The row the wedged invocation looked for now exists and is projected.
    register(&harness.rt, MISSING);
    wait_position(&harness.rt, "users", 2);
    wait_for_failures(&harness, 4);

    let retries = calls_ending(&stub, "/fail");
    assert!(
        retries >= 4,
        "the handler re-ran to its failing tail {retries} times"
    );
    assert_eq!(
        calls_ending(&stub, "/seen"),
        1,
        "the retries replay the recorded read, so this call never re-fires"
    );
    assert_eq!(
        post_body(&stub, "/seen")["found"],
        false,
        "the handler saw the miss"
    );

    let rows = journal_rows(data.path(), 1);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows[0].2,
        Value::Null,
        "the miss stays recorded as null even though the row now exists"
    );

    // An operator skip is the only escape from a permanently frozen miss.
    harness.rt.effect(EFFECT).unwrap().request_skip(1);
    wait_until("the skip to advance the effect", || {
        let effect = harness.rt.effect(EFFECT).unwrap();
        effect.consecutive_failures() == 0 && effect.position() >= 1
    });

    harness.shutdown();
}

// --- editing an effect under an in-flight invocation -----------------------

const TWO_POSTS_V2: &str = r#"
load("events/user.star", "user_registered")

source = [user_registered()]

def handle(event):
    http.post(url = "https://a.test/first-v2", body = {"id": event.data["user_id"]})
    http.post(url = "https://a.test/second", body = {})
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

source = [user_registered()]

def handle(event):
    for i in range(100000000):
        pass
    http.post(url = "https://a.test/never", body = {})
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
    assert!(error.contains("handle() failed"), "{error}");
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
    user_registered(): lambda event: http.post(
        url = "https://a.test/welcome/" + event.data["user_id"],
        body = {"email": event.data["email"]},
    ),
    user_registered(name = "VIP"): lambda event: http.post(
        url = "https://a.test/vip/" + event.data["user_id"],
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
