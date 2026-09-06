//! Rule 15's lanes, end to end.
//!
//! `docs/effects.md` rule 15: events with the same `@key` are processed in log order,
//! events with different keys may be processed concurrently. The headline property is the
//! one the eight-hour stall is about (a lane that cannot make progress must not stop an
//! unrelated aggregate) and the rest of this file is what that property costs: a
//! watermark that is now a low-water mark, per-lane rows above it, and an operator who can
//! find out *which* lane is stuck.
//!
//! Nothing here asserts an interleaving. Cross-lane order is exactly what lanes make free,
//! so a test that pinned one would fail on a quiet machine (`tests/concurrent.rs` says the
//! same thing about commands). What is asserted is set, count and prefix.

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use hekla::effect::StubHttpClient;
use hekla::http::{HttpClient, HttpResponse};
use hekla::runtime::Runtime;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use tempfile::TempDir;

mod support;

use support::{Boot, Harness, orders_project_with, place_order, wait_until};

const EFFECT: &str = "Notify";

/// Keyed on the customer, so two customers are two lanes and one customer's orders stay
/// in log order. The body carries the customer id so the stub can fail one lane.
const NOTIFY: &str = r#"
effect Notify {
  on @order.placed { order_id, @key customer_id } {
    http.post("https://mail.test/send", { "customer": customer_id })
  }
}
"#;

fn project() -> TempDir {
    orders_project_with(&[("effects/notify.hk", NOTIFY)])
}

/// Answers 500 for `wedged` and 200 for everyone else, so one lane cannot make progress
/// while the others can.
fn only_one_customer_fails(wedged: u64) -> Arc<StubHttpClient> {
    Arc::new(StubHttpClient::new(move |_, request| {
        let body: Value = request
            .body
            .as_deref()
            .and_then(|body| serde_json::from_slice(body).ok())
            .unwrap_or(Value::Null);
        let customer = body.get("customer").and_then(Value::as_u64);
        Ok(HttpResponse {
            status: if customer == Some(wedged) { 500 } else { 200 },
            headers: Vec::new(),
            body: b"{}".to_vec(),
        })
    }))
}

fn boot(dir: &Path, data: &Path, http: Arc<dyn HttpClient>) -> Harness {
    // The orders fixture declares a subject-scoped `email`, so the runtime asks for a
    // master key even though this effect never reveals one.
    Boot::new(dir)
        .data_dir(data)
        .http(http)
        .with_master_key()
        .start()
}

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
    .unwrap_or(0)
}

fn lane_rows(db: &Connection) -> Vec<(String, i64)> {
    let mut stmt = db
        .prepare("SELECT lane, position FROM effect_lane WHERE effect = ?1 ORDER BY lane")
        .unwrap();
    let rows = stmt
        .query_map(params![EFFECT], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn status_of(rt: &Runtime) -> Value {
    rt.status()["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|effect| effect["name"] == EFFECT)
        .cloned()
        .unwrap()
}

fn order(rt: &Runtime, customer: u64) {
    place_order(
        rt,
        &uuid::Uuid::new_v4().to_string(),
        customer,
        "who@example.com",
    );
}

/// **The eight-hour stall, as an executable test.** One unprocessable event used to block
/// every unrelated aggregate behind it, because an effect had one lane. Customer 1 cannot
/// be notified; customer 2 must not have to wait for that to be fixed.
#[test]
fn a_wedged_lane_does_not_block_another_key() {
    let dir = project();
    let data = tempfile::tempdir().unwrap();
    let stub = only_one_customer_fails(1);
    let booted = boot(dir.path(), data.path(), stub.clone());

    order(&booted.rt, 1); // position 1, wedges
    order(&booted.rt, 2); // position 2, must not wait for it

    wait_until(
        "the healthy lane to finish while the other is wedged",
        || {
            let db = open_op_db(booted.data_dir());
            invocation_status(&db, 2).as_deref() == Some("terminal")
                && booted.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
        },
    );

    let db = open_op_db(booted.data_dir());
    assert_eq!(
        invocation_status(&db, 1).as_deref(),
        Some("running"),
        "customer 1 is still stuck, which is the point"
    );
    booted.shutdown();
}

/// The watermark stops being "what I have processed" and becomes "what every lane has
/// passed". A lane racing ahead must not move it over a position still in flight, because
/// that position is exactly what the next boot has to replay.
#[test]
fn the_watermark_is_a_low_water_mark() {
    let dir = project();
    let data = tempfile::tempdir().unwrap();
    let booted = boot(dir.path(), data.path(), only_one_customer_fails(1));

    order(&booted.rt, 1);
    order(&booted.rt, 2);

    wait_until("the lane ahead of the wedge to record itself", || {
        !lane_rows(&open_op_db(booted.data_dir())).is_empty()
    });

    let db = open_op_db(booted.data_dir());
    assert_eq!(
        watermark(&db),
        0,
        "the mark cannot pass position 1, however far lane 2 has run"
    );
    assert_eq!(
        lane_rows(&db),
        vec![("i:2".to_owned(), 2)],
        "and the lane that ran ahead says so, keyed by its partition key"
    );
    booted.shutdown();
}

/// Lag alone is useless on a partitioned effect: every other lane is racing ahead, so the
/// number says "something is stuck" and nothing about what. Naming the key is what makes
/// it actionable, and the position is what the skip endpoint takes.
#[test]
fn status_names_the_lane_holding_the_watermark_down() {
    let dir = project();
    let data = tempfile::tempdir().unwrap();
    let booted = boot(dir.path(), data.path(), only_one_customer_fails(1));

    order(&booted.rt, 1);
    order(&booted.rt, 2);

    wait_until("the wedge to surface", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
    });
    wait_until("the healthy lane to clear", || {
        invocation_status(&open_op_db(booted.data_dir()), 2).as_deref() == Some("terminal")
    });

    let status = status_of(&booted.rt);
    assert_eq!(status["state"], "wedged");
    assert_eq!(
        status["pinning_key"], "i:1",
        "the customer whose lane is stuck, not just that one is"
    );
    assert_eq!(status["pinning_position"], 1);
    assert_eq!(status["wedged_lanes"], 1, "the other lane is healthy");
    booted.shutdown();
}

/// The escape from a wedged lane is the same explicit operator skip it always was; what
/// changed is that the position to skip is the one `/status` names. Once it goes, the
/// prefix is free and the rows above it are no longer telling anyone anything.
#[test]
fn skipping_the_pinning_position_releases_the_prefix_and_sweeps_its_lanes() {
    let dir = project();
    let data = tempfile::tempdir().unwrap();
    let booted = boot(dir.path(), data.path(), only_one_customer_fails(1));

    order(&booted.rt, 1);
    order(&booted.rt, 2);
    wait_until("the wedge to surface", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
    });

    let effect = booted.rt.effect(EFFECT).unwrap();
    let (_, pinned) = effect.pinning().expect("a pinning lane while wedged");
    effect.request_skip(pinned);

    wait_until("the mark to jump past the skipped position", || {
        watermark(&open_op_db(booted.data_dir())) >= 2
    });
    wait_until("the lane rows above it to be swept", || {
        lane_rows(&open_op_db(booted.data_dir())).is_empty()
    });

    let effect = booted.rt.effect(EFFECT).unwrap();
    assert_eq!(effect.consecutive_failures(), 0, "nothing is wedged now");
    assert_eq!(effect.wedged_lanes(), 0);
    assert_eq!(effect.pinning(), None, "and nothing pins the mark");
    booted.shutdown();
}

/// A lane that recorded progress above the mark must not redo it after a restart. The
/// rows are only an optimisation (`begin_invocation` would refuse the work anyway) so
/// what this pins is that the shortcut agrees with the authority.
#[test]
fn a_restart_does_not_re_notify_a_lane_that_ran_ahead() {
    let dir = project();
    let data = tempfile::tempdir().unwrap();
    let first = only_one_customer_fails(1);
    let booted = boot(dir.path(), data.path(), first.clone());

    order(&booted.rt, 1);
    order(&booted.rt, 2);
    wait_until("the healthy lane to finish", || {
        invocation_status(&open_op_db(booted.data_dir()), 2).as_deref() == Some("terminal")
    });
    let sent = first.call_count();
    booted.shutdown();

    // Same wedge, so the mark is still behind position 2 and the reboot re-scans over it.
    let second = only_one_customer_fails(1);
    let again = boot(dir.path(), data.path(), second.clone());
    wait_until("the reboot to wedge on the same position", || {
        again.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
    });
    thread_settle();

    assert_eq!(
        second
            .calls()
            .iter()
            .filter(|request| {
                request
                    .body
                    .as_deref()
                    .is_some_and(|body| String::from_utf8_lossy(body).contains("\"customer\": 2"))
            })
            .count(),
        0,
        "customer 2 was already notified before the restart ({sent} calls then)"
    );
    again.shutdown();
}

/// Retries of the wedged lane are what this waits out: the reboot's stub answers 500 for
/// customer 1 forever, so a fixed sleep is the only way to give a re-notification the
/// chance to happen before asserting it did not.
fn thread_settle() {
    thread::sleep(Duration::from_millis(400));
}

/// A panic must not strand its lane.
///
/// One pool serves every effect, so an unwinding handler is caught rather than taking a
/// worker with it. Catching it is only half: the lane has to be released *and offered
/// back*, because `admit` refuses to re-arm a lane that already has a place and `promote`
/// only touches parked ones. Released without an offer, the lane sat queued with nothing
/// to run it, its positions pinned the watermark for the life of the process, and
/// `/status` reported the effect as merely lagging because no failure had been recorded.
#[test]
fn a_panicking_call_does_not_strand_its_lane() {
    let dir = project();
    let data = tempfile::tempdir().unwrap();
    // Panics once, then behaves. If the lane is stranded the retry never happens and the
    // watermark never moves.
    let stub = Arc::new(StubHttpClient::new(move |index, _| {
        assert!(index > 0, "the first call panics");
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: b"{}".to_vec(),
        })
    }));
    let booted = boot(dir.path(), data.path(), stub.clone());

    order(&booted.rt, 1);

    wait_until("the lane to recover and finish the position", || {
        watermark(&open_op_db(booted.data_dir())) >= 1
    });
    assert!(
        stub.call_count() >= 2,
        "the position was retried rather than abandoned"
    );
    booted.shutdown();
}

/// A handler that panics every time must behave like any other failure that will not
/// clear: back off rather than spin, report itself, and be escapable.
///
/// The first fix for a panicking handler caught the unwind and re-offered the lane, which
/// recovered a one-off but left a deterministic panic retrying with no delay and no record.
/// Nothing called `record_lane_failure`, so the attempt count stayed at zero, and the
/// operator skip is gated on the position having failed at least once: the documented
/// escape from an unprocessable event was unreachable for exactly this case, while
/// `/status` called the effect `lagging` and named no lane.
#[test]
fn a_handler_that_always_panics_wedges_its_lane_and_can_be_skipped() {
    let dir = project();
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::new(|_, _| panic!("the transport exploded")));
    let booted = boot(dir.path(), data.path(), stub.clone());

    order(&booted.rt, 1);

    wait_until("the panic to be reported as a wedge", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
    });
    let effect = booted.rt.effect(EFFECT).unwrap();
    assert_eq!(effect.state(1), "wedged", "not `lagging`");
    assert!(
        effect.last_error().unwrap_or_default().contains("panicked"),
        "the failure says what it was: {:?}",
        effect.last_error()
    );
    let (lane, at) = effect.pinning().expect("the lane is named");
    assert_eq!((lane.as_str(), at), ("i:1", 1));
    assert!(
        effect.retry_in_ms().is_some(),
        "and it is backing off rather than retrying flat out"
    );

    // The escape hatch has to work, which it cannot without an attempt count.
    effect.request_skip(at);
    wait_until("the skip to advance past the panicking position", || {
        watermark(&open_op_db(booted.data_dir())) >= 1
    });
    booted.shutdown();
}
