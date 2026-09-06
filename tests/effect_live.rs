//! Rule 15's `on live` boundary, end to end.
//!
//! An effect added to a running deployment used to replay from position 0 and fire its
//! side effects across the whole history: for a notification effect, an email to every
//! customer in the log. `on live` declares that history is not news, and the runtime
//! resolves what "history" means to a concrete position **once**, at the effect's first
//! activation against a data directory.
//!
//! The boundary is deliberately a second number rather than a watermark started at head,
//! because an effect may mix `on` and `on live` arms and one cursor cannot start at both 0
//! and head. `a_mixed_effect_declines_only_its_live_arm` is that distinction, executable.

use std::path::Path;
use std::sync::Arc;

use hekla::effect::StubHttpClient;
use hekla::runtime::Runtime;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

use support::{Boot, Harness, ctx, wait_until, write_project};

const EFFECT: &str = "Mixed";

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

/// One effect, one arm of each delivery. The plain arm must still see history; the `live`
/// one must not. Both key on the customer, so they share a lane.
const MIXED: &str = r#"
effect Mixed {
  on @order.placed { order_id, @key customer_id } {
    http.post("https://mail.test/placed", { "customer": customer_id })
  }

  on live @order.cancelled { order_id, @key customer_id } {
    http.post("https://mail.test/cancelled", { "customer": customer_id })
  }
}
"#;

/// The same project without the effect, so a test can lay down history that predates the
/// effect's first activation. That is the only way to have history at all: activation
/// happens when the effect first runs here, so an effect present from the start has none.
fn without_effect() -> TempDir {
    write_project(&[("events/order.hk", EVENTS), ("commands/order.hk", COMMANDS)])
}

fn with_effect() -> TempDir {
    write_project(&[
        ("events/order.hk", EVENTS),
        ("commands/order.hk", COMMANDS),
        ("effects/mixed.hk", MIXED),
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

fn invocation_rows(db: &Connection) -> Vec<i64> {
    let mut stmt = db
        .prepare("SELECT position FROM effect_invocation WHERE effect = ?1 ORDER BY position")
        .unwrap();
    let rows = stmt.query_map(params![EFFECT], |row| row.get(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

fn boundary(db: &Connection) -> i64 {
    db.query_row(
        "SELECT live_boundary FROM effect_activation WHERE effect = ?1",
        params![EFFECT],
        |row| row.get(0),
    )
    .unwrap()
}

fn detail(rt: &Runtime) -> Value {
    hekla::introspect::effect_detail(rt.effect(EFFECT).unwrap(), rt.log_head(), None)
}

fn urls(stub: &StubHttpClient) -> Vec<String> {
    stub.calls()
        .iter()
        .map(|request| request.url.clone())
        .collect()
}

/// The replay-from-zero problem, as a test. Two cancellations happened before this effect
/// existed; adding it must not notify anyone about them.
#[test]
fn a_live_arm_declines_the_history_that_predates_it() {
    let data = tempfile::tempdir().unwrap();
    let bare = without_effect();
    let before = boot(bare.path(), data.path(), Arc::new(StubHttpClient::ok()));
    cancel(&before.rt, 1);
    cancel(&before.rt, 2);
    before.shutdown();

    let full = with_effect();
    let stub = Arc::new(StubHttpClient::ok());
    let after = boot(full.path(), data.path(), stub.clone());
    wait_until(
        "the effect to catch up over the history it declined",
        || after.rt.effect(EFFECT).unwrap().position() >= 2,
    );

    assert_eq!(urls(&stub), Vec::<String>::new(), "nothing was sent");
    assert_eq!(
        boundary(&open_op_db(after.data_dir())),
        2,
        "the boundary is the head as it stood at first activation"
    );
    assert_eq!(
        after.rt.effect(EFFECT).unwrap().live_suppressed(),
        2,
        "and it says so, rather than leaving an operator to guess why nothing fired"
    );
    after.shutdown();
}

/// **The trap.** A declined position must leave no `effect_invocation` row at all. A
/// terminal row with an empty journal reads back through `replay` as `Matched`, a claim
/// that a position nothing ran was checked and reproduced.
#[test]
fn a_declined_position_leaves_no_invocation_row() {
    let data = tempfile::tempdir().unwrap();
    let bare = without_effect();
    let before = boot(bare.path(), data.path(), Arc::new(StubHttpClient::ok()));
    cancel(&before.rt, 1);
    before.shutdown();

    let full = with_effect();
    let after = boot(full.path(), data.path(), Arc::new(StubHttpClient::ok()));
    wait_until("the effect to pass the declined position", || {
        after.rt.effect(EFFECT).unwrap().position() >= 1
    });

    assert_eq!(
        invocation_rows(&open_op_db(after.data_dir())),
        Vec::<i64>::new(),
        "declining is not completing: there is nothing to have a record of"
    );
    after.shutdown();
}

/// An event appended after the boundary is news, and fires normally. Without this the
/// test above would also pass on an effect that never ran at all.
#[test]
fn a_live_arm_fires_for_what_comes_after() {
    let data = tempfile::tempdir().unwrap();
    let bare = without_effect();
    let before = boot(bare.path(), data.path(), Arc::new(StubHttpClient::ok()));
    cancel(&before.rt, 1);
    before.shutdown();

    let full = with_effect();
    let stub = Arc::new(StubHttpClient::ok());
    let after = boot(full.path(), data.path(), stub.clone());
    cancel(&after.rt, 2);

    wait_until("the new cancellation to be notified", || {
        !stub.calls().is_empty()
    });
    assert_eq!(urls(&stub), ["https://mail.test/cancelled"]);
    after.shutdown();
}

/// The reason the boundary is a second number and not a watermark started at head: one
/// cursor cannot begin at both 0 and head, and this effect needs both at once.
#[test]
fn a_mixed_effect_declines_only_its_live_arm() {
    let data = tempfile::tempdir().unwrap();
    let bare = without_effect();
    let before = boot(bare.path(), data.path(), Arc::new(StubHttpClient::ok()));
    cancel(&before.rt, 1); // declined: `on live`
    place(&before.rt, 1); // processed: plain `on`
    before.shutdown();

    let full = with_effect();
    let stub = Arc::new(StubHttpClient::ok());
    let after = boot(full.path(), data.path(), stub.clone());
    wait_until("the plain arm to work through the history", || {
        !stub.calls().is_empty()
    });
    wait_until("the effect to catch up", || {
        after.rt.effect(EFFECT).unwrap().position() >= 2
    });

    assert_eq!(
        urls(&stub),
        ["https://mail.test/placed"],
        "the plain arm saw history and the live arm did not"
    );
    assert_eq!(
        invocation_rows(&open_op_db(after.data_dir())),
        vec![2],
        "only the position the plain arm handled has a record"
    );
    after.shutdown();
}

/// The boundary is resolved once, per data directory. A restart re-reads it rather than
/// re-resolving it, or every restart would silently declare everything since the last one
/// to be history.
#[test]
fn the_boundary_survives_a_restart_rather_than_moving_to_the_new_head() {
    let data = tempfile::tempdir().unwrap();
    let bare = without_effect();
    let before = boot(bare.path(), data.path(), Arc::new(StubHttpClient::ok()));
    cancel(&before.rt, 1);
    before.shutdown();

    let full = with_effect();
    let first = boot(full.path(), data.path(), Arc::new(StubHttpClient::ok()));
    wait_until("first activation", || {
        first.rt.effect(EFFECT).unwrap().position() >= 1
    });
    assert_eq!(first.rt.effect(EFFECT).unwrap().live_boundary(), 1);
    assert_eq!(detail(&first.rt)["live_boundary"], 1);
    // Three more events, so a boundary that re-resolved would move to 4.
    cancel(&first.rt, 2);
    cancel(&first.rt, 3);
    cancel(&first.rt, 4);
    first.shutdown();

    let again = boot(full.path(), data.path(), Arc::new(StubHttpClient::ok()));
    assert_eq!(
        again.rt.effect(EFFECT).unwrap().live_boundary(),
        1,
        "`on live` means after this effect first ran here, not after the last restart"
    );
    again.shutdown();
}
