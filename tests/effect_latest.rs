//! Rule 15's `on latest`, end to end.
//!
//! `docs/effects.md` rule 15: an `on latest` arm runs once per key per dispatch batch, at
//! the newest matching position in it. **It is not "skip history"**: history is processed,
//! and because a fold stops at the trigger's own position inclusive, the one surviving
//! invocation has already seen every event before it. What varies is batch size.
//!
//! Every test here lays its backlog down through a project that does *not* declare the
//! effect, then boots one that does, which is the only way to have a backlog at all: an
//! effect present from the start sees each event as it is appended. That is also the shape
//! the rule exists for, since a catch-up is where the redundant work is.
//!
//! The collapse group is `(arm, key)` and never the key alone, so
//! `two_arms_keying_alike_do_not_collapse_into_each_other` is the discriminating test
//! rather than a variation: two arms have two bodies, and folding one into the other would
//! drop work rather than repeat it.

use std::path::Path;
use std::sync::Arc;

use hekla::effect::StubHttpClient;
use hekla::runtime::Runtime;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

use support::{Boot, Harness, ctx, sweep, wait_until, write_project};

const EVENTS: &str = r#"
event @order.placed { order_id: Uuid, customer_id: Int }
event @order.cancelled { order_id: Uuid, customer_id: Int }
"#;

const COMMANDS: &str = r#"
command PlaceOrder(order_id: Uuid, customer_id: Int) {
  emit @order.placed { order_id, customer_id }
}

command CancelOrder(order_id: Uuid, customer_id: Int) {
  emit @order.cancelled { order_id, customer_id }
}
"#;

/// The plain case: one collapsing arm, keyed on the customer. The body reports the fold so
/// a test can show the survivor saw the whole prefix rather than only its own event.
const SYNC: &str = r#"
effect Sync {
  on latest @order.placed as e { @key customer_id } {
    fold orders: Int = 0
      on @order.placed(customer_id) => orders + 1
    http.post("https://sync.test/publish", { "customer": customer_id, "orders": orders })
  }
}
"#;

/// Two arms, both keyed on the customer, so they share a lane and differ only by arm.
/// Keying them differently would pass whether or not the arm was part of the grouping.
const PAIR: &str = r#"
effect Sync {
  on latest @order.placed as e { @key customer_id } {
    http.post("https://sync.test/placed", { "customer": customer_id })
  }

  on latest @order.cancelled as e { @key customer_id } {
    http.post("https://sync.test/cancelled", { "customer": customer_id })
  }
}
"#;

/// One arm listing both types. Different event types collapse together when one arm names
/// them both, which is the case the rule exists for: a shop reconnecting and then editing
/// two plans should publish once.
const WIDE: &str = r#"
effect Sync {
  on latest @order.placed, @order.cancelled as e { @key customer_id } {
    http.post("https://sync.test/publish", { "customer": customer_id })
  }
}
"#;

/// A collapsing arm beside a plain one. A catchup policy may change how much work happens
/// and may never change what the log says: the `on` arm is untouched by the other.
const MIXED: &str = r#"
effect Sync {
  on latest @order.placed as e { @key customer_id } {
    http.post("https://sync.test/publish", { "customer": customer_id })
  }

  on @order.cancelled as e { @key customer_id } {
    http.post("https://sync.test/cancelled", { "customer": customer_id })
  }
}
"#;

const EFFECT: &str = "Sync";

fn bare() -> TempDir {
    write_project(&[("events/order.hk", EVENTS), ("commands/order.hk", COMMANDS)])
}

fn with(effect: &str) -> TempDir {
    write_project(&[
        ("events/order.hk", EVENTS),
        ("commands/order.hk", COMMANDS),
        ("effects/sync.hk", effect),
    ])
}

fn boot(dir: &Path, data: &Path, http: Arc<StubHttpClient>) -> Harness {
    Boot::new(dir).data_dir(data).http(http).start()
}

fn place(rt: &Runtime, customer: u64) {
    emit(rt, "PlaceOrder", customer);
}

fn cancel(rt: &Runtime, customer: u64) {
    emit(rt, "CancelOrder", customer);
}

fn emit(rt: &Runtime, command: &str, customer: u64) {
    let body = json!({
        "order_id": uuid::Uuid::new_v4().to_string(),
        "customer_id": customer,
    });
    let result = rt.execute(command, body, &ctx(), None).unwrap();
    assert_eq!(result.status, 200, "{command} failed: {:?}", result.body);
}

fn open_op_db(data: &Path) -> Connection {
    Connection::open(data.join("hekla.db")).unwrap()
}

/// Every recorded invocation, as `(position, collapsed_from)`.
fn invocations(db: &Connection) -> Vec<(i64, Option<i64>)> {
    let mut stmt = db
        .prepare(
            "SELECT position, collapsed_from FROM effect_invocation \
             WHERE effect = ?1 ORDER BY position",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![EFFECT], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
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

/// What the stub was asked to do, as `(url, body)` pairs in call order.
fn calls(stub: &StubHttpClient) -> Vec<(String, Value)> {
    stub.calls()
        .iter()
        .map(|request| {
            let body = request
                .body
                .as_deref()
                .and_then(|body| serde_json::from_slice(body).ok())
                .unwrap_or(Value::Null);
            (request.url.clone(), body)
        })
        .collect()
}

fn urls(stub: &StubHttpClient) -> Vec<String> {
    calls(stub).into_iter().map(|(url, _)| url).collect()
}

/// Seed a backlog through a project without the effect, then boot one with it.
fn after_backlog(
    effect: &'static str,
    data: &Path,
    seed: impl FnOnce(&Runtime),
) -> (TempDir, Harness, Arc<StubHttpClient>) {
    // Bound, not a temporary of the `boot` call: a `TempDir` deletes its directory when it
    // drops, and dropping it at the end of that statement would pull the project out from
    // under a server that is still running.
    let seeding = bare();
    let before = boot(seeding.path(), data, Arc::new(StubHttpClient::ok()));
    seed(&before.rt);
    before.shutdown();

    let project = with(effect);
    let stub = Arc::new(StubHttpClient::ok());
    let after = boot(project.path(), data, stub.clone());
    (project, after, stub)
}

fn caught_up(harness: &Harness, head: u64) {
    wait_until("the effect to catch up over the backlog", || {
        harness.rt.effect(EFFECT).unwrap().position() >= head
    });
}

/// The headline. Three orders for one customer are one publish, at the newest of them, and
/// the fold in that one invocation has counted all three.
#[test]
fn a_backlog_for_one_key_is_one_invocation_at_the_newest_position() {
    let data = tempfile::tempdir().unwrap();
    let (_project, after, stub) = after_backlog(SYNC, data.path(), |rt| {
        place(rt, 1);
        place(rt, 1);
        place(rt, 1);
    });
    caught_up(&after, 3);

    assert_eq!(
        calls(&stub),
        [(
            "https://sync.test/publish".to_owned(),
            json!({ "customer": 1, "orders": 3 })
        )],
        "one invocation, and it folded the whole prefix rather than only its own event"
    );
    assert_eq!(
        invocations(&open_op_db(after.data_dir())),
        [(3, Some(1))],
        "the survivor records the range it stands for; the folded positions have no rows"
    );
    assert_eq!(
        after.rt.effect(EFFECT).unwrap().latest_collapsed(),
        2,
        "and the effect says how much work it did not do"
    );
    after.shutdown();
}

/// Catches a key-extraction bug that the test above would pass on its own: two customers
/// are two lanes and two groups, so both run.
#[test]
fn two_keys_are_two_invocations() {
    let data = tempfile::tempdir().unwrap();
    let (_project, after, stub) = after_backlog(SYNC, data.path(), |rt| {
        place(rt, 7);
        place(rt, 8);
    });
    caught_up(&after, 2);

    let mut seen: Vec<u64> = calls(&stub)
        .iter()
        .map(|(_, body)| body["customer"].as_u64().unwrap())
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, [7, 8], "neither customer collapsed into the other");
    assert_eq!(after.rt.effect(EFFECT).unwrap().latest_collapsed(), 0);
    after.shutdown();
}

/// **The discriminating test.** Both arms key on the customer and both events carry the
/// same one, so the keys are equal and only the arm tells the two groups apart. A
/// dispatcher that grouped by the lane would fire once here and lose a body.
#[test]
fn two_arms_keying_alike_do_not_collapse_into_each_other() {
    let data = tempfile::tempdir().unwrap();
    let (_project, after, stub) = after_backlog(PAIR, data.path(), |rt| {
        place(rt, 1);
        cancel(rt, 1);
    });
    caught_up(&after, 2);

    let mut seen = urls(&stub);
    seen.sort();
    assert_eq!(
        seen,
        ["https://sync.test/cancelled", "https://sync.test/placed"],
        "two arms have two bodies, so neither folds into the other"
    );
    assert_eq!(after.rt.effect(EFFECT).unwrap().latest_collapsed(), 0);
    after.shutdown();
}

/// The other half of the same rule: one arm listing several types folds across them.
#[test]
fn one_arm_collapses_across_the_types_it_lists() {
    let data = tempfile::tempdir().unwrap();
    let (_project, after, stub) = after_backlog(WIDE, data.path(), |rt| {
        place(rt, 1);
        cancel(rt, 1);
        place(rt, 1);
    });
    caught_up(&after, 3);

    assert_eq!(urls(&stub), ["https://sync.test/publish"]);
    assert_eq!(
        invocations(&open_op_db(after.data_dir())),
        [(3, Some(1))],
        "the range spans the cancellation in the middle, which is in the same group"
    );
    after.shutdown();
}

/// A collapsing arm changes nothing for the arm beside it.
#[test]
fn a_plain_arm_beside_a_collapsing_one_still_runs_every_time() {
    let data = tempfile::tempdir().unwrap();
    let (_project, after, stub) = after_backlog(MIXED, data.path(), |rt| {
        place(rt, 1);
        cancel(rt, 1);
        place(rt, 1);
        cancel(rt, 1);
    });
    caught_up(&after, 4);

    let seen = urls(&stub);
    assert_eq!(
        seen.iter()
            .filter(|url| url.ends_with("/cancelled"))
            .count(),
        2,
        "the `on` arm ran for both, {seen:?}"
    );
    assert_eq!(
        seen.iter().filter(|url| url.ends_with("/publish")).count(),
        1,
        "and the `on latest` arm ran once, {seen:?}"
    );
    after.shutdown();
}

/// The invocation records the range it stands for, and `hekla verify` re-derives which
/// positions that was rather than trusting a list: the group is every position in the
/// range whose arm and lane match, which is what the two integers buy.
#[test]
fn a_replay_reproduces_the_grouping() {
    let data = tempfile::tempdir().unwrap();
    let project = {
        let (project, after, _stub) = after_backlog(SYNC, data.path(), |rt| {
            place(rt, 1);
            place(rt, 1);
            place(rt, 2);
            place(rt, 1);
        });
        caught_up(&after, 4);
        after.shutdown();
        project
    };

    let report = sweep(project.path(), data.path());
    assert!(report.is_clean(), "{report}");
    assert_eq!(
        report.invocations_checked, 2,
        "one surviving invocation per key"
    );
    assert_eq!(
        report.skipped.collapsed, 2,
        "and the positions they folded are accounted for rather than unreported"
    );
    assert_eq!(
        report.invocations_checked + report.skipped.total(),
        4,
        "every position the effect was delivered is covered by one or the other"
    );
}

/// A folded position has no invocation row of its own, so what stops it running after a
/// restart is the mark and the lane row, exactly as for any other retired position.
#[test]
fn a_restart_does_not_re_run_what_was_folded() {
    let data = tempfile::tempdir().unwrap();
    let project = {
        let (project, after, _stub) = after_backlog(SYNC, data.path(), |rt| {
            place(rt, 1);
            place(rt, 1);
            place(rt, 1);
        });
        caught_up(&after, 3);
        after.shutdown();
        project
    };

    let stub = Arc::new(StubHttpClient::ok());
    let again = boot(project.path(), data.path(), stub.clone());
    caught_up(&again, 3);
    assert_eq!(
        urls(&stub),
        Vec::<String>::new(),
        "the whole batch is behind the mark, folded positions included"
    );
    again.shutdown();
}

/// Lay down four positions for one key, then plant the row a crash would have left at
/// position 3, and boot with the watermark still at 0 so the whole backlog is re-delivered.
fn after_a_crash_at_three(data: &Path, status: &str, collapsed_from: Option<i64>) -> TempDir {
    let seeding = bare();
    let before = boot(seeding.path(), data, Arc::new(StubHttpClient::ok()));
    for _ in 0..4 {
        place(&before.rt, 1);
    }
    before.shutdown();

    open_op_db(data)
        .execute(
            "INSERT INTO effect_invocation \
             (effect, position, script_hash, status, created_at, collapsed_from) \
             VALUES (?1, 3, 'planted', ?2, 't0', ?3)",
            params![EFFECT, status, collapsed_from],
        )
        .unwrap();
    seeding
}

/// A crash between completing an invocation and advancing the mark leaves a `terminal` row
/// above it, and the whole batch is re-delivered.
///
/// **The group has to re-form as it was.** Position 3 takes the group over and is told it
/// is already terminal, so nothing runs and nothing is recorded twice. Refusing it the
/// takeover instead would leave 1 and 2 to be dispatched on their own, firing an invocation
/// older than the one position 3 already performed, which is what `on latest` exists to
/// prevent. Folding it away instead would put a position with its own invocation inside
/// another's range, and a replay would count it twice.
#[test]
fn a_completed_position_re_forms_its_group_rather_than_re_running_what_it_covered() {
    let data = tempfile::tempdir().unwrap();
    let _seeding = after_a_crash_at_three(data.path(), "terminal", Some(1));

    let project = with(SYNC);
    let stub = Arc::new(StubHttpClient::ok());
    let after = boot(project.path(), data.path(), stub.clone());
    caught_up(&after, 4);

    assert_eq!(
        urls(&stub).len(),
        1,
        "only position 4: the rest is what the crashed process already did"
    );
    assert_eq!(
        invocations(&open_op_db(after.data_dir())),
        [(3, Some(1)), (4, None)],
        "no new row below 4, and no range spanning position 3"
    );
    after.shutdown();
}

/// A crash mid-invocation leaves a `running` row, and that one has a journal.
///
/// **It has to be the position that runs.** Folded away, nothing would ever complete it: the
/// row and its journal would never be swept, because the sweep only reclaims `terminal`
/// rows, and `/admin/effects` would show an invocation running for ever.
#[test]
fn an_unfinished_position_is_the_one_that_runs_and_is_completed() {
    let data = tempfile::tempdir().unwrap();
    let _seeding = after_a_crash_at_three(data.path(), "running", None);

    let project = with(SYNC);
    let stub = Arc::new(StubHttpClient::ok());
    let after = boot(project.path(), data.path(), stub.clone());
    caught_up(&after, 4);

    assert_eq!(
        urls(&stub).len(),
        2,
        "the unfinished invocation at 3, and position 4 behind it"
    );
    assert_eq!(
        invocations(&open_op_db(after.data_dir())),
        [(3, Some(1)), (4, None)],
        "position 3 ran as the group's newest and recorded what it folded"
    );
    assert_eq!(
        invocation_status(&open_op_db(after.data_dir()), 3),
        Some("terminal".to_owned()),
        "the row the crash left is finished, not abandoned above the mark for ever"
    );
    after.shutdown();
}

/// Live rather than catching up: events arriving one at a time are a batch of one and
/// collapse with nothing, so nothing is lost by an effect that is keeping up.
#[test]
fn an_effect_that_is_keeping_up_folds_nothing() {
    let data = tempfile::tempdir().unwrap();
    let project = with(SYNC);
    let stub = Arc::new(StubHttpClient::ok());
    let harness = boot(project.path(), data.path(), stub.clone());

    for expected in 1..=3 {
        place(&harness.rt, 1);
        wait_until("the invocation for this event", || {
            stub.calls().len() >= expected
        });
    }

    assert_eq!(urls(&stub).len(), 3, "one publish per event");
    assert_eq!(harness.rt.effect(EFFECT).unwrap().latest_collapsed(), 0);
    harness.shutdown();
}
