//! Effect durability, end to end. The `send-welcome` effect fires on registration:
//! its HTTP call is journaled and it invokes the internal `record-welcome`
//! command, which appends `user.welcomed`. These tests pin the durable properties:
//! the invocation runs once, restarting the runtime replays the journal without
//! re-firing a completed invocation, and a 5xx wedges the effect (visible in the
//! health signals) until an explicit operator skip advances it. They also pin the
//! runtime's split between what reaches the script (a plain 4xx) and what it
//! absorbs as a wedge (a transport error, and every retryable status), and that
//! draining a wedged effect leaves its invocation to replay rather than losing it.

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hekla::context::CommandContext;
use hekla::effect::StubHttpClient;
use hekla::http::{HttpClient, HttpResponse};
use hekla::runtime::Runtime;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use uuid::Uuid;

mod support;

use support::{ALICE, Boot, Harness, log_head, register_user, wait_until};

const EFFECT: &str = "SendWelcome";

fn boot(data: &Path, http: Arc<dyn HttpClient>) -> Harness {
    Boot::example().data_dir(data).http(http).start()
}

fn register(rt: &Runtime, id: &str) {
    register_user(rt, id, &format!("{id}@example.com"), "U");
}

fn effect_position(rt: &Runtime) -> u64 {
    rt.effect(EFFECT).unwrap().position()
}

/// The operational DB, read directly: the durable side of the effect state that
/// the in-memory health signals only summarise.
fn open_op_db(data: &Path) -> Connection {
    Connection::open(data.join("hekla.db")).unwrap()
}

fn invocation_status(db: &Connection, position: i64) -> Option<String> {
    db.query_row(
        "SELECT status FROM effect_invocation WHERE effect = ?1 AND position = ?2",
        params![EFFECT, position],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
}

fn watermark(db: &Connection) -> i64 {
    db.query_row(
        "SELECT watermark FROM effect_cursor WHERE effect = ?1",
        params![EFFECT],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
    // No row at all means the effect never advanced.
    .unwrap_or(0)
}

fn journal_results(db: &Connection, position: i64) -> Vec<String> {
    let mut stmt = db
        .prepare("SELECT result FROM effect_journal WHERE effect = ?1 AND position = ?2")
        .unwrap();
    let rows = stmt
        .query_map(params![EFFECT, position], |row| row.get::<_, String>(0))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn effect_fires_a_journaled_http_call_then_invokes_the_command_once() {
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let booted = boot(data.path(), stub.clone());

    register(&booted.rt, ALICE);
    // The effect posts the welcome, then invokes record-welcome, which appends
    // user.welcomed: the log head reaches 2.
    wait_until("effect to complete", || log_head(&booted.rt) >= 2);

    // Exactly one POST, to the welcome URL, carrying the registered email.
    assert_eq!(stub.call_count(), 1);
    let call = &stub.calls()[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.url, "https://example.test/welcome");
    let body: serde_json::Value =
        serde_json::from_slice(call.body.as_deref().expect("a POST body")).unwrap();
    assert_eq!(body["email"], format!("{ALICE}@example.com"));

    // record-welcome landed exactly once: the head is 2, not 3, and stays there.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(log_head(&booted.rt), 2);

    booted.shutdown();
}

#[test]
fn restarting_replays_the_journal_without_refiring() {
    let data = tempfile::tempdir().unwrap();

    // First boot: process the registration to completion.
    let stub1 = Arc::new(StubHttpClient::ok());
    let booted = boot(data.path(), stub1.clone());
    register(&booted.rt, ALICE);
    // Wait until the effect has advanced past both the registration and the
    // user.welcomed it produced, so the invocation is terminal on disk.
    wait_until("first run to settle", || effect_position(&booted.rt) >= 2);
    assert_eq!(stub1.call_count(), 1);
    booted.shutdown();

    // Second boot on the same data directory: the invocation is terminal, so the
    // effect must not re-enter handle. No new HTTP call, no duplicate event.
    let stub2 = Arc::new(StubHttpClient::ok());
    let booted = boot(data.path(), stub2.clone());
    wait_until("effect to catch up", || effect_position(&booted.rt) >= 2);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        stub2.call_count(),
        0,
        "a completed invocation must not re-fire its http call"
    );
    assert_eq!(log_head(&booted.rt), 2, "no duplicate user.welcomed");

    booted.shutdown();
}

#[test]
fn invoke_commands_boundary_dedupes_a_replay_when_the_key_is_lost() {
    // Simulates the append-then-finalize crash window: on restart the idempotency
    // key is cleared, so a replay re-invokes and reserve() re-acquires rather than
    // replaying the stored outcome. record-welcome carries a DCB boundary, so the
    // second append is a no-op reject, not a duplicate event. Two distinct keys
    // stand in for the cleared-then-re-acquired key.
    let data = tempfile::tempdir().unwrap();
    let booted = boot(data.path(), Arc::new(StubHttpClient::status(400)));

    let ctx = CommandContext::from_effect(Uuid::new_v4(), Uuid::new_v4());
    let input = json!({ "user_id": ALICE });
    let first = booted
        .rt
        .execute_from_effect("RecordWelcome", input.clone(), &ctx, Some("key-a"))
        .unwrap();
    assert_eq!(first.status, 200);

    let second = booted
        .rt
        .execute_from_effect("RecordWelcome", input, &ctx, Some("key-b"))
        .unwrap();
    assert_eq!(second.status, 422);
    assert_eq!(second.body["error"]["code"], "already_welcomed");
    assert_eq!(
        log_head(&booted.rt),
        1,
        "the boundary appended user.welcomed once"
    );

    booted.shutdown();
}

#[test]
fn a_5xx_wedges_the_effect_and_an_operator_skip_advances_it() {
    let data = tempfile::tempdir().unwrap();
    // A persistent 5xx is absorbed by the runtime and never reaches the script, so
    // the effect wedges rather than skipping.
    let stub = Arc::new(StubHttpClient::status(500));
    let booted = boot(data.path(), stub.clone());

    register(&booted.rt, ALICE); // user.registered at position 1

    wait_until("the wedge to surface in status", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
    });
    let effect = booted.rt.effect(EFFECT).unwrap();
    assert!(
        effect.last_error().is_some(),
        "a wedge records its last error"
    );
    assert_eq!(log_head(&booted.rt), 1, "a wedged effect appends nothing");

    // An explicit, manual operator skip advances past the unprocessable event.
    effect.request_skip(1);
    wait_until("the skip to advance the effect", || {
        let effect = booted.rt.effect(EFFECT).unwrap();
        effect.consecutive_failures() == 0 && effect.position() >= 1
    });
    assert_eq!(log_head(&booted.rt), 1, "skipping does not append");

    booted.shutdown();
}

#[test]
fn a_skip_armed_before_the_event_arrives_does_not_drop_it() {
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let booted = boot(data.path(), stub.clone());

    // Armed for a position the driver has not reached yet. The skip is honored only
    // once the position has genuinely failed, so a healthy event must still run.
    booted.rt.effect(EFFECT).unwrap().request_skip(1);

    register(&booted.rt, ALICE); // user.registered at position 1
    wait_until("effect to complete", || log_head(&booted.rt) >= 2);

    assert_eq!(stub.call_count(), 1, "the welcome post really fired");
    assert_eq!(
        log_head(&booted.rt),
        2,
        "record-welcome landed, so the event was processed rather than skipped"
    );
    let effect = booted.rt.effect(EFFECT).unwrap();
    assert_eq!(effect.consecutive_failures(), 0);
    assert_eq!(effect.terminal_skips(), 0);

    booted.shutdown();
}

#[test]
fn shutting_down_a_wedged_effect_drains_promptly_and_leaves_it_running() {
    let data = tempfile::tempdir().unwrap();
    let booted = boot(data.path(), Arc::new(StubHttpClient::status(500)));

    register(&booted.rt, ALICE); // user.registered at position 1

    // Five failures in, the driver is part-way through a multi-second backoff: a
    // backoff wait that slept it out instead of polling the shutdown flag would push
    // the drain past the assertion below.
    wait_until("the backoff to grow", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() >= 5
    });

    let start = Instant::now();
    booted.shutdown();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "draining a wedged effect took {elapsed:?}"
    );

    // The abandoned invocation stays `running` and the watermark stays behind it,
    // so the next boot replays the event rather than losing its side effect.
    let db = open_op_db(data.path());
    assert_eq!(
        invocation_status(&db, 1).as_deref(),
        Some("running"),
        "an interrupted invocation must not be marked terminal"
    );
    assert_eq!(watermark(&db), 0, "the watermark did not advance past it");
}

#[test]
fn a_transport_error_wedges_the_effect_and_never_reaches_the_handler() {
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::new(|_, _| {
        anyhow::bail!("connection refused")
    }));
    let booted = boot(data.path(), stub.clone());

    register(&booted.rt, ALICE); // user.registered at position 1
    wait_until("the wedge to surface", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
    });

    assert!(stub.call_count() >= 1, "the transport was really attempted");
    let effect = booted.rt.effect(EFFECT).unwrap();
    let last_error = effect.last_error().expect("a wedge records its last error");
    assert!(
        last_error.contains("https://example.test/welcome"),
        "the wedge names the failed call: {last_error}"
    );
    // Naming the call without the reason tells an operator which call failed but not
    // why. Rule 5 absorbs every attempt before the language sees one, so the language
    // reports only that the URL did not answer and the host is the only thing that
    // still holds the reason.
    assert!(
        last_error.contains("connection refused"),
        "the wedge keeps the transport reason: {last_error}"
    );
    assert_eq!(
        effect.terminal_skips(),
        0,
        "a transport error is a wedge, not a terminal skip"
    );
    assert_eq!(
        log_head(&booted.rt),
        1,
        "the handler never ran past http.post, so no user.welcomed"
    );

    booted.shutdown();

    let db = open_op_db(data.path());
    assert_eq!(invocation_status(&db, 1).as_deref(), Some("running"));
    assert!(
        journal_results(&db, 1).is_empty(),
        "a failed call is never journaled"
    );
}

#[test]
fn a_4xx_reaches_the_handler_and_completes_the_invocation() {
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::status(404));
    let booted = boot(data.path(), stub.clone());

    register(&booted.rt, ALICE); // user.registered at position 1
    wait_until("the effect to advance", || effect_position(&booted.rt) >= 1);
    thread::sleep(Duration::from_millis(100)); // any retry would show up here

    assert_eq!(stub.call_count(), 1, "a 4xx is a result, not a retry");
    let effect = booted.rt.effect(EFFECT).unwrap();
    assert_eq!(effect.consecutive_failures(), 0, "a 4xx does not wedge");
    assert_eq!(effect.terminal_skips(), 0);
    assert_eq!(
        log_head(&booted.rt),
        1,
        "the handler logged the rejection instead of invoking record-welcome"
    );

    booted.shutdown();

    let db = open_op_db(data.path());
    assert_eq!(invocation_status(&db, 1).as_deref(), Some("terminal"));
    let journal = journal_results(&db, 1);
    assert_eq!(journal.len(), 1, "one journaled call: the http.post");
    let recorded: serde_json::Value = serde_json::from_str(&journal[0]).unwrap();
    assert_eq!(recorded["status"], 404, "the 4xx response is journaled");
}

/// Every response is journaled, so a status the handler could not have recovered
/// from would be baked into the invocation: raising on it would replay the recorded
/// refusal on every attempt and wedge forever without re-sending. That is why 429
/// (and 408, and 425) are absorbed like a 5xx rather than handed to the script.
#[test]
fn a_429_is_retried_by_the_runtime_and_never_reaches_the_handler() {
    let data = tempfile::tempdir().unwrap();
    // Rate limited twice, then through.
    let stub = Arc::new(StubHttpClient::new(|index, _| {
        Ok(HttpResponse {
            status: if index < 2 { 429 } else { 200 },
            headers: Vec::new(),
            body: b"{}".to_vec(),
        })
    }));
    let booted = boot(data.path(), stub.clone());

    register(&booted.rt, ALICE); // user.registered at position 1
    wait_until("the effect to get past the rate limit", || {
        log_head(&booted.rt) >= 2
    });

    assert_eq!(
        stub.call_count(),
        3,
        "the runtime really re-sent the request rather than replaying the 429"
    );
    let effect = booted.rt.effect(EFFECT).unwrap();
    assert_eq!(
        effect.terminal_skips(),
        0,
        "a rate limit abandons no work: it is a wedge that clears itself"
    );
    wait_until("the wedge to clear", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() == 0
    });

    booted.shutdown();

    let db = open_op_db(data.path());
    assert_eq!(invocation_status(&db, 1).as_deref(), Some("terminal"));
    let statuses: Vec<serde_json::Value> = journal_results(&db, 1)
        .iter()
        .map(|result| serde_json::from_str::<serde_json::Value>(result).unwrap()["status"].clone())
        .collect();
    assert!(
        !statuses.iter().any(|status| status == 429),
        "a retryable status must never be journaled, or the retry would replay it: {statuses:?}"
    );
}

/// A limiter that names its window gets that window waited out.
///
/// Where the wait happens moved with rule 5. It used to be the only wait there was: a
/// 429 never reached the script, so the invocation wedged on the first one and this
/// driver's backoff honored the header. Now the language re-sends immediately a few
/// times first, so a limiter that refuses once and then relents is absorbed inside the
/// invocation and no wait is owed. What is still owed, and is what this pins, is the
/// gap before the *next* invocation once those attempts are exhausted: 200ms on the
/// driver's own ladder, and a full second when the server asked for one.
#[test]
fn a_retry_after_holds_the_next_invocation_for_the_window_the_server_named() {
    let data = tempfile::tempdir().unwrap();
    // Refuses every time: the attempts rule 5 absorbs all fail, so the invocation
    // wedges and the driver has to decide when to come back.
    let stub = Arc::new(StubHttpClient::new(|_, _| {
        Ok(HttpResponse {
            status: 429,
            headers: vec![("retry-after".to_owned(), "1".to_owned())],
            body: b"{}".to_vec(),
        })
    }));
    let booted = boot(data.path(), stub.clone());

    register(&booted.rt, ALICE); // user.registered at position 1
    wait_until("the first invocation to wedge", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() >= 1
    });
    let after_first = stub.call_count();
    assert!(
        after_first > 1,
        "rule 5 re-sends within the invocation, so a wedge means several attempts: \
         {after_first}"
    );

    // The driver's own first backoff is 200ms, so anything still waiting well past
    // that is waiting on the header rather than on the ladder.
    thread::sleep(Duration::from_millis(700));
    assert_eq!(
        stub.call_count(),
        after_first,
        "the next invocation started inside the 1s the server asked for"
    );

    wait_until("the window to expire and the effect to try again", || {
        stub.call_count() > after_first
    });

    booted.shutdown();
}
