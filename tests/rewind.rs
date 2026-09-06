//! `hekla rewind`, the only way back to history for an `on live` arm.
//!
//! `docs/effects.md` rule 15 owes the runtime a rewind and says what it must be: CLI only,
//! against a stopped process, and **not** an HTTP endpoint. An effect declaring `on live`
//! is by definition one whose author said history must not fire, and those are exactly the
//! effects where an accidental rewind re-sends every notification the log has ever seen.
//!
//! The journal does not save you, and these tests pin that rather than working around it:
//! a rewind deletes the recorded invocations above its target, which is both what makes it
//! a rewind at all (`begin_invocation` would otherwise report `AlreadyTerminal` and skip
//! everything) and what makes it re-fire.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use hekla::effect::StubHttpClient;
use hekla::runtime::Runtime;
use rusqlite::{Connection, params};
use serde_json::json;
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

fn emit(rt: &Runtime, command: &str, customer: u64) {
    let body = json!({
        "order_id": uuid::Uuid::new_v4().to_string(),
        "customer_id": customer,
    });
    let result = rt.execute(command, body, &ctx(), None).unwrap();
    assert_eq!(result.status, 200, "{command} failed: {:?}", result.body);
}

fn rewind(dir: &Path, data: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hekla"));
    command
        .arg("rewind")
        .arg(EFFECT)
        .args(args)
        .arg(dir)
        .arg("--data-dir")
        .arg(data);
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn open_op_db(data: &Path) -> Connection {
    Connection::open(data.join("hekla.db")).unwrap()
}

fn watermark(db: &Connection) -> i64 {
    db.query_row(
        "SELECT watermark FROM effect_cursor WHERE effect = ?1",
        params![EFFECT],
        |row| row.get(0),
    )
    .unwrap()
}

fn boundary(db: &Connection) -> i64 {
    db.query_row(
        "SELECT live_boundary FROM effect_activation WHERE effect = ?1",
        params![EFFECT],
        |row| row.get(0),
    )
    .unwrap()
}

fn invocation_count(db: &Connection) -> i64 {
    db.query_row(
        "SELECT count(*) FROM effect_invocation WHERE effect = ?1",
        params![EFFECT],
        |row| row.get(0),
    )
    .unwrap()
}

/// Lay down a data directory with history the `live` arm declined and two positions the
/// plain arm processed, then stop. Returns the projects so they outlive the caller.
fn seeded(data: &Path) -> (TempDir, TempDir) {
    let bare = without_effect();
    let before = boot(bare.path(), data, Arc::new(StubHttpClient::ok()));
    emit(&before.rt, "CancelOrder", 1); // position 1: history for the live arm
    before.shutdown();

    let full = with_effect();
    let after = boot(full.path(), data, Arc::new(StubHttpClient::ok()));
    emit(&after.rt, "PlaceOrder", 1); // position 2
    emit(&after.rt, "PlaceOrder", 2); // position 3
    wait_until("the effect to catch up", || {
        after.rt.effect(EFFECT).unwrap().position() >= 3
    });
    after.shutdown();
    (bare, full)
}

#[test]
fn it_refuses_while_a_server_holds_the_directory() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());
    let running = boot(full.path(), data.path(), Arc::new(StubHttpClient::ok()));

    let output = rewind(full.path(), data.path(), &["0", "--yes"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("in use by another hekla process")
            && stderr(&output).contains("stop the server first"),
        "{}",
        stderr(&output)
    );
    running.shutdown();
}

/// Without this the command appears to work and does nothing: `begin_invocation` reports
/// `AlreadyTerminal` for every position it moved past, and the effect sails over them.
#[test]
fn it_discards_the_recorded_invocations_above_the_target() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());
    assert_eq!(invocation_count(&open_op_db(data.path())), 2);

    let output = rewind(full.path(), data.path(), &["1", "--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let db = open_op_db(data.path());
    assert_eq!(watermark(&db), 1);
    assert_eq!(
        invocation_count(&db),
        0,
        "the records above position 1 are what would otherwise skip the work"
    );
}

/// The point of the whole command: those positions run again, and perform again.
#[test]
fn a_rewound_effect_re_fires_on_the_next_boot() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());

    let output = rewind(full.path(), data.path(), &["1", "--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let stub = Arc::new(StubHttpClient::ok());
    let again = boot(full.path(), data.path(), stub.clone());
    wait_until("the rewound positions to be reprocessed", || {
        stub.call_count() >= 2
    });
    assert_eq!(stub.call_count(), 2, "both placements notified again");
    again.shutdown();
}

/// The default preserves what the author declared. A rewind aimed at a plain arm must not
/// quietly re-send every notification an `on live` arm declined.
#[test]
fn the_live_boundary_holds_unless_asked_for() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());
    assert_eq!(boundary(&open_op_db(data.path())), 1);

    let output = rewind(full.path(), data.path(), &["0", "--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        boundary(&open_op_db(data.path())),
        1,
        "the boundary is untouched by a plain rewind"
    );
    assert!(
        stdout(&output).contains("unchanged; pass --live to lower it"),
        "and the summary says so: {}",
        stdout(&output)
    );

    let stub = Arc::new(StubHttpClient::ok());
    let again = boot(full.path(), data.path(), stub.clone());
    wait_until("the plain arm's positions to be reprocessed", || {
        stub.call_count() >= 2
    });
    assert!(
        stub.calls()
            .iter()
            .all(|request| request.url.ends_with("/placed")),
        "the declined cancellation stayed declined"
    );
    again.shutdown();
}

/// And `--live` is how you override it, which is the obligation: it is the only way back
/// to history for an arm whose boundary is already persisted.
#[test]
fn live_lowers_the_boundary_so_history_fires() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());

    let output = rewind(full.path(), data.path(), &["0", "--live", "--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(boundary(&open_op_db(data.path())), 0);

    let stub = Arc::new(StubHttpClient::ok());
    let again = boot(full.path(), data.path(), stub.clone());
    wait_until("every position to be reprocessed", || {
        stub.call_count() >= 3
    });
    assert!(
        stub.calls()
            .iter()
            .any(|request| request.url.ends_with("/cancelled")),
        "the history the live arm had declined now fires"
    );
    again.shutdown();
}

/// `--yes` answers the question; it does not silence the answer. An operator reading a
/// deploy log should still see what the rewind was about to do.
#[test]
fn yes_skips_the_prompt_and_keeps_the_summary() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());

    let printed = stdout(&rewind(full.path(), data.path(), &["1", "--yes"]));
    assert!(printed.contains("watermark      3 -> 1"), "{printed}");
    assert!(
        printed.contains("recorded invocation(s)")
            && printed.contains("performs their side effects again"),
        "{printed}"
    );
    // Named arm by arm: on a mixed effect, which arm is `on live` is the useful part.
    assert!(
        printed.contains("on @order.placed { @key customer_id }")
            && printed.contains("on live @order.cancelled { @key customer_id }"),
        "the arms are named rather than counted: {printed}"
    );
}

/// A forward "rewind" is a fat-fingered digit, and it needs no log access to catch.
#[test]
fn it_refuses_to_move_forward() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());
    let output = rewind(full.path(), data.path(), &["9", "--yes"]);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("is ahead of it, not a rewind"),
        "{}",
        stdout(&output)
    );
    assert_eq!(watermark(&open_op_db(data.path())), 3, "nothing moved");
}

#[test]
fn it_refuses_an_effect_the_project_does_not_declare() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());
    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .args(["rewind", "Nope", "0", "--yes"])
        .arg(full.path())
        .arg("--data-dir")
        .arg(data.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no effect `Nope`") && stderr(&output).contains("Mixed"),
        "it lists what is declared: {}",
        stderr(&output)
    );
}

/// A stale runbook flag against the wrong effect should be visible, not silently ignored.
#[test]
fn live_on_an_effect_with_no_live_arm_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let plain = write_project(&[
        ("events/order.hk", EVENTS),
        ("commands/order.hk", COMMANDS),
        (
            "effects/mixed.hk",
            r#"
effect Mixed {
  on @order.placed { order_id, @key customer_id } {
    http.post("https://mail.test/placed", { "customer": customer_id })
  }
}
"#,
        ),
    ]);
    let booted = boot(plain.path(), data.path(), Arc::new(StubHttpClient::ok()));
    emit(&booted.rt, "PlaceOrder", 1);
    wait_until("the effect to catch up", || {
        booted.rt.effect(EFFECT).unwrap().position() >= 1
    });
    booted.shutdown();

    let output = rewind(plain.path(), data.path(), &["0", "--live", "--yes"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("has no `on live` arm"),
        "{}",
        stderr(&output)
    );
}

/// The rule, encoded rather than merely observed: rewinding must stay off the HTTP
/// surface, because the effects it is most dangerous for are exactly the ones an
/// accidental request would hurt most.
#[test]
fn there_is_no_rewind_route() {
    let routes = hekla::server::routes();
    assert!(
        !routes.iter().any(|route| route.contains("rewind")),
        "rewind is CLI-only by design: {routes:?}"
    );
}

/// A prompt nobody can answer is a usage error, not a decline. Exiting zero here would
/// tell a deploy script the rewind happened, which is the worst of both: no rewind, and
/// no sign that there was not one.
#[test]
fn without_a_terminal_and_without_yes_it_refuses_rather_than_reporting_success() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());

    let output = rewind(full.path(), data.path(), &["1"]);
    assert!(
        !output.status.success(),
        "a rewind that did not happen must not exit zero"
    );
    assert!(
        stderr(&output).contains("not a terminal"),
        "{}",
        stderr(&output)
    );
    assert_eq!(watermark(&open_op_db(data.path())), 3, "and nothing moved");
    // The summary is still printed, so a log shows what was about to happen.
    assert!(
        stdout(&output).contains("watermark      3 -> 1"),
        "{}",
        stdout(&output)
    );
}

/// Rewinding *to* the current watermark is a real operation, and the one a `blocked`
/// effect needs: it discards every lane row and every recorded invocation above the mark
/// without moving the mark. Refusing it made the escape hatch the block message names a
/// no-op, and left an effect blocked at watermark 0 with no smaller position to pass.
#[test]
fn rewinding_to_the_watermark_discards_the_work_above_it() {
    let data = tempfile::tempdir().unwrap();
    let (_bare, full) = seeded(data.path());
    let before = watermark(&open_op_db(data.path()));

    let output = rewind(full.path(), data.path(), &[&before.to_string(), "--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        watermark(&open_op_db(data.path())),
        before,
        "the mark does not move; what goes is the work recorded above it"
    );
    assert_eq!(
        invocation_count(&open_op_db(data.path())),
        2,
        "work at or below the mark is genuinely done and stays: the effect resumes \
         strictly after the position named"
    );
}
