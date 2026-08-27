//! The invariant harness: `hekla verify` offline, and the continuous checks.
//!
//! A checker that never fires is indistinguishable from one that works, so every
//! check here is exercised twice: once against a healthy project, to prove it stays
//! quiet, and once against a *planted* violation, to prove it actually fires. The
//! planted cases reach behind the runtime's back (a direct SQLite write, a deleted
//! journal row), because that is the only way to produce the corruption these checks
//! exist to catch: the runtime will not produce it on request.

mod support;

use hekla::effect::StubHttpClient;
use hekla::lock::DataDirLock;
use hekla::verify::{self, Mismatch, Violation};
use rusqlite::Connection;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use support::{ALICE, BOB, Boot, CAROL, UUID_A, example_dir, load_ok, master_keys, register_user};

/// Boot `examples/users`, register two users, wait for the projector, and shut
/// down. Leaves a data directory with a populated read model and a completed effect
/// invocation, which is the state every check below runs against.
fn populated(data_dir: &Path) {
    let harness = Boot::example()
        .data_dir(data_dir)
        .with_master_key()
        .http_status(200)
        .start();
    let position = register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    register_user(&harness.rt, BOB, "bob@example.com", "Bob");
    support::wait_position(&harness.rt, "users", position);
    // The effect has to reach its position too, or there is no recorded invocation
    // for the replay check to sweep.
    support::wait_until("the effect to catch up", || {
        harness.rt.effect("send-welcome").unwrap().position() >= position
    });
    harness.shutdown();
}

/// Sweep a data directory with the fixed master key.
fn sweep(project_dir: &Path, data_dir: &Path) -> verify::Report {
    let project = load_ok(project_dir);
    verify::sweep(&project, data_dir, Some(master_keys())).expect("the sweep should run")
}

/// Open a projector's read model directly, to plant a violation in it.
fn open_model(data_dir: &Path, projector: &str) -> Connection {
    Connection::open(data_dir.join("projectors").join(format!("{projector}.db"))).unwrap()
}

// --- the healthy case ------------------------------------------------------

#[test]
fn a_healthy_project_sweeps_clean_and_says_what_it_covered() {
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    let report = sweep(&example_dir("users"), data.path());
    assert!(
        report.is_clean(),
        "expected no violations, got {:?}",
        report.violations
    );
    // A clean report that checked nothing would be worthless, so the counts are
    // part of the assertion rather than incidental.
    assert!(
        report.projectors_checked >= 2,
        "expected both projectors checked, got {}",
        report.projectors_checked
    );
    assert!(
        report.invocations_checked >= 1,
        "expected at least one invocation replayed, got {}",
        report.invocations_checked
    );
}

// --- rebuild equivalence ---------------------------------------------------

#[test]
fn a_row_edited_behind_the_projector_is_caught() {
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    open_model(data.path(), "users")
        .execute(
            "UPDATE users SET name = 'Tampered' WHERE user_id = ?1",
            [ALICE],
        )
        .unwrap();

    let report = sweep(&example_dir("users"), data.path());
    let found = report
        .violations
        .iter()
        .find(|violation| {
            matches!(
                violation,
                Violation::RebuildMismatch { key, detail, .. }
                    if key == ALICE && matches!(detail, Mismatch::Differs { .. })
            )
        })
        .unwrap_or_else(|| panic!("expected a differing row, got {:?}", report.violations));
    let text = found.to_string();
    assert!(text.contains("Tampered"), "{text}");
    assert!(text.contains("Alice"), "{text}");
}

#[test]
fn a_row_deleted_behind_the_projector_is_caught() {
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    open_model(data.path(), "users")
        .execute("DELETE FROM users WHERE user_id = ?1", [BOB])
        .unwrap();

    let report = sweep(&example_dir("users"), data.path());
    assert!(
        report.violations.iter().any(|violation| matches!(
            violation,
            Violation::RebuildMismatch { key, detail, .. }
                if key == BOB && matches!(detail, Mismatch::OnlyRebuilt(_))
        )),
        "expected the rebuild to produce a row live no longer has, got {:?}",
        report.violations
    );
}

#[test]
fn a_row_invented_behind_the_projector_is_caught() {
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    open_model(data.path(), "users")
        .execute(
            "INSERT INTO users (user_id, email, name) VALUES (?1, ?2, ?3)",
            [
                "44444444-4444-4444-4444-444444444444",
                "ghost@example.com",
                "Ghost",
            ],
        )
        .unwrap();

    let report = sweep(&example_dir("users"), data.path());
    assert!(
        report.violations.iter().any(|violation| matches!(
            violation,
            Violation::RebuildMismatch { detail, .. } if matches!(detail, Mismatch::OnlyLive(_))
        )),
        "expected a live row no rebuild produces, got {:?}",
        report.violations
    );
}

// --- replay equivalence ----------------------------------------------------

#[test]
fn a_missing_journal_entry_is_caught_without_performing_the_call() {
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    // Drop the recorded `http.post`. A replay now reaches a call the journal cannot
    // answer, which on a real retry is exactly the double-send this check exists to
    // catch. The sealed host cannot send, so the check reports it instead.
    let removed = Connection::open(data.path().join("hekla.db"))
        .unwrap()
        .execute(
            "DELETE FROM effect_journal WHERE effect = 'send-welcome' AND rowid IN \
             (SELECT MIN(rowid) FROM effect_journal WHERE effect = 'send-welcome')",
            [],
        )
        .unwrap();
    assert_eq!(
        removed, 1,
        "the fixture should have a journaled call to drop"
    );

    let report = sweep(&example_dir("users"), data.path());
    let found = report
        .violations
        .iter()
        .find(|violation| matches!(violation, Violation::ReplayDivergence { .. }))
        .unwrap_or_else(|| panic!("expected a replay divergence, got {:?}", report.violations));
    let text = found.to_string();
    assert!(text.contains("no journal entry"), "{text}");
    assert!(text.contains("performed it a second time"), "{text}");
}

#[test]
fn an_edited_effect_is_skipped_rather_than_reported() {
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    // A copy of the project whose effect body differs, so the recorded invocations
    // ran under a script hash the current module no longer has. That divergence is
    // legitimate: the journal belongs to a program that no longer exists.
    let edited = tempfile::tempdir().unwrap();
    copy_dir(&example_dir("users"), edited.path());
    let effect = edited.path().join("effects/send-welcome.star");
    let source = fs::read_to_string(&effect).unwrap();
    fs::write(
        &effect,
        source.replace("https://example.test/welcome", "https://example.test/hello"),
    )
    .unwrap();

    let report = sweep(edited.path(), data.path());
    assert!(
        report.is_clean(),
        "an edited effect is not a violation, got {:?}",
        report.violations
    );
    assert!(
        report.invocations_skipped >= 1,
        "expected the edited effect's invocations to be skipped, got {}",
        report.invocations_skipped
    );
    assert_eq!(report.invocations_checked, 0);
}

// --- the data-directory lock ----------------------------------------------

#[test]
fn a_sweep_is_refused_while_a_runtime_holds_the_directory() {
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::example()
        .data_dir(data.path())
        .with_master_key()
        .start();

    let project = load_ok(&example_dir("users"));
    let err = verify::sweep(&project, data.path(), Some(master_keys()))
        .expect_err("verifying a directory a runtime holds must be refused");
    assert!(
        err.to_string().contains("in use by another hekla process"),
        "{err:#}"
    );

    harness.shutdown();
}

#[test]
fn a_second_runtime_on_one_data_directory_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::example()
        .data_dir(data.path())
        .with_master_key()
        .start();

    let err = match Boot::example()
        .data_dir(data.path())
        .with_master_key()
        .try_start()
    {
        Ok(_) => panic!("a second runtime on one data directory must be refused"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("in use by another hekla process"),
        "{err:#}"
    );

    harness.shutdown();
    // Released on drop, so the directory is usable again once the first is down.
    let second = Boot::example()
        .data_dir(data.path())
        .with_master_key()
        .start();
    second.shutdown();
}

#[test]
fn the_lock_is_released_when_it_drops() {
    let dir = tempfile::tempdir().unwrap();
    let first = DataDirLock::acquire(dir.path()).unwrap();
    assert!(DataDirLock::acquire(dir.path()).is_err());
    drop(first);
    DataDirLock::acquire(dir.path()).expect("free once dropped");
}

// --- verify mode, running --------------------------------------------------

#[test]
fn verify_mode_runs_a_normal_project_without_quarantining_anything() {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&example_dir("users"), dir.path());
    fs::write(dir.path().join("hekla.toml"), "[verify]\nenabled = true\n").unwrap();

    let harness = Boot::new(dir.path())
        .with_master_key()
        .http_status(200)
        .start();
    assert!(harness.rt.verify(), "verify mode should be on");

    let position = register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    support::wait_position(&harness.rt, "users", position);
    support::wait_until("the effect to catch up", || {
        harness.rt.effect("send-welcome").unwrap().position() >= position
    });

    let status = harness.rt.status();
    assert_eq!(status["verify"], json!(true));
    for effect in status["effects"].as_array().unwrap() {
        assert_eq!(
            effect["quarantined"],
            json!(false),
            "effect quarantined under a clean run: {effect}"
        );
    }
    for projector in status["projectors"].as_array().unwrap() {
        assert_ne!(
            projector["readiness"], "quarantined",
            "projector quarantined under a clean run: {projector}"
        );
    }

    harness.shutdown();
}

#[test]
fn subject_encrypted_columns_rebuild_identically() {
    // The rebuild check compares stored bytes, which is only sound because subject
    // encryption is deterministic (AES-SIV) and a projector copies the event's
    // ciphertext rather than re-encrypting. If either ever stopped holding, every
    // encrypted column would read as a mismatch on a clean project, so this is the
    // check that the check is comparing the right thing.
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(example_dir("orders"))
        .data_dir(data.path())
        .with_master_key()
        .start();
    let position = place_order(&harness.rt, UUID_A, 7, "buyer@example.com");
    support::wait_position(&harness.rt, "customer-orders", position);
    harness.shutdown();

    let report = sweep(&example_dir("orders"), data.path());
    assert!(
        report.is_clean(),
        "subject columns should rebuild byte-identically, got {:?}",
        report.violations
    );
    assert!(report.projectors_checked >= 1);
}

#[test]
fn a_tampered_subject_column_is_still_caught() {
    // The flip side: encrypted columns are compared, not skipped. Overwriting the
    // ciphertext must surface even though the check never decrypts it.
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(example_dir("orders"))
        .data_dir(data.path())
        .with_master_key()
        .start();
    let position = place_order(&harness.rt, UUID_A, 7, "buyer@example.com");
    support::wait_position(&harness.rt, "customer-orders", position);
    harness.shutdown();

    open_model(data.path(), "customer-orders")
        .execute("UPDATE orders SET email = 'not-ciphertext'", [])
        .unwrap();

    let report = sweep(&example_dir("orders"), data.path());
    assert!(
        report.violations.iter().any(|violation| matches!(
            violation,
            Violation::RebuildMismatch { detail, .. } if matches!(detail, Mismatch::Differs { .. })
        )),
        "expected the tampered ciphertext to be caught, got {:?}",
        report.violations
    );
}

/// Place an order through `examples/orders`, returning the appended position.
fn place_order(rt: &hekla::runtime::Runtime, order_id: &str, customer_id: u64, email: &str) -> u64 {
    let result = rt
        .execute(
            "place-order",
            json!({
                "order_id": order_id,
                "customer_id": customer_id,
                "shop_id": 1,
                "email": email,
                "shipping_address": "1 Test Street",
                "order_total": "10.00",
                "notes": "",
            }),
            &support::ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "place-order failed: {:?}", result.body);
    result.body["positions"]["last"].as_u64().unwrap()
}

/// Copy a project directory recursively, so a test can edit a variant of an example
/// without touching the checked-in one.
fn copy_dir(from: &Path, to: &Path) {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry.unwrap();
        let rel = entry.path().strip_prefix(from).unwrap();
        let target = to.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).unwrap();
        } else {
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
fn an_effect_that_erases_what_it_revealed_is_not_reported_as_divergent() {
    // The `erase last` rule the authoring guide recommends produces an invocation
    // that deliberately cannot replay: the unjournaled `reveal` re-runs against a
    // key the invocation itself deleted. That is a documented cost, not a broken
    // invariant, and reporting it would quarantine every effect written the
    // recommended way.
    let dir = support::write_project(&[
        ("hekla.toml", "[verify]\nenabled = true\n"),
        (
            "events/customer.star",
            r#"
customer_closed = event(
    type = "customer.closed",
    fields = {
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 200),
    },
)
"#,
        ),
        (
            "commands/close-account.star",
            r#"
load("events/customer.star", "customer_closed")

input = schema(customer_id = uint(), email = str())

def handle(input, state):
    return customer_closed(customer_id = input.customer_id, email = input.email)
"#,
        ),
        (
            "effects/forget-customer.star",
            r#"
load("events/customer.star", "customer_closed")

def forget(event, state):
    address = reveal(event.data.email)
    http.post(url = "https://example.test/farewell", body = {"to": address})
    erase("customer_id", str(event.data.customer_id))

handle = {customer_closed(): forget}
"#,
        ),
    ]);

    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(dir.path())
        .data_dir(data.path())
        .with_master_key()
        .http_status(200)
        .start();
    let result = harness
        .rt
        .execute(
            "close-account",
            json!({ "customer_id": 42, "email": "gone@example.com" }),
            &support::ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    let position = result.body["positions"]["last"].as_u64().unwrap();

    support::wait_until("the effect to catch up", || {
        harness.rt.effect("forget-customer").unwrap().position() >= position
    });
    let effect = harness.rt.effect("forget-customer").unwrap();
    assert!(
        !effect.quarantined(),
        "erase-last must not quarantine: {:?}",
        effect.last_error()
    );
    harness.shutdown();

    let report = sweep(dir.path(), data.path());
    assert!(
        report.is_clean(),
        "erase-last is not a divergence, got {:?}",
        report.violations
    );
}

#[test]
fn a_projector_matching_nothing_survives_a_replay() {
    // Regression: the caught-up branch advances a selective projector's checkpoint
    // past a non-matching tail, so a projector whose query matches nothing still
    // reports a position. A rebuild then starts from 0 and, if it stops at its last
    // *matching* event (of which there are none), publishes 0. Treating that as a
    // checkpoint regression killed the thread while leaving readiness `ready`, so
    // the read API kept serving a model rebuilt from nothing and every
    // read-your-writes wait resolved against it.
    let dir = support::write_project(&[
        (
            "events/thing.star",
            r#"
happened = event(type = "thing.happened", fields = {"id": uuid()})
ignored = event(type = "thing.ignored", fields = {"id": uuid()})
"#,
        ),
        (
            "commands/do-it.star",
            r#"
load("events/thing.star", "ignored")

input = schema(id = uuid())

def handle(input, state):
    return ignored(id = input.id)
"#,
        ),
        (
            "commands/note-it.star",
            r#"
load("events/thing.star", "happened")

input = schema(id = uuid())

def handle(input, state):
    return happened(id = input.id)
"#,
        ),
        (
            "projectors/tracker.star",
            r#"
load("events/thing.star", "happened")

things = entity(key = "id", fields = {"id": uuid()})

handle = {happened(): lambda event: [put(things, {"id": event.data.id})]}
"#,
        ),
    ]);

    let harness = Boot::new(dir.path()).start();
    // Append only events the projector's query does not select, so its checkpoint
    // moves on the non-matching tail while its model stays empty.
    for id in [ALICE, BOB] {
        let result = harness
            .rt
            .execute("do-it", json!({ "id": id }), &support::ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }
    let tail = support::log_head(&harness.rt);
    support::wait_until("the projector to track the non-matching tail", || {
        harness.rt.projector("tracker").unwrap().position() >= tail
    });

    harness.rt.projector("tracker").unwrap().request_replay();
    // The rebuild has to finish while the boundary is still empty, which is the whole
    // scenario: appending the matching event first would give it something to apply
    // and hide the bug. Nothing public signals "replay done", so this waits out the
    // idle poll plus the rebuild rather than racing them.
    thread::sleep(Duration::from_secs(1));

    // Prove the thread survived the replay by making it do work afterwards: a
    // matching event has to land in the model. Asserting on position or `failed`
    // alone would pass against a dead thread, since neither moves when it dies.
    let result = harness
        .rt
        .execute("note-it", json!({ "id": CAROL }), &support::ctx(), None)
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    let head = support::log_head(&harness.rt);
    support::wait_until(
        "the projector to apply a matching event after the replay",
        || harness.rt.projector("tracker").unwrap().position() >= head,
    );
    assert!(
        support::read_row(&harness, "tracker", "things", CAROL, head).is_some(),
        "the rebuilt model must hold the matching event"
    );

    let shared = harness.rt.projector("tracker").unwrap();
    assert!(shared.running(), "the thread must survive a replay");
    assert_eq!(shared.readiness().label(), "ready");
    assert_eq!(shared.last_error(), None);

    harness.shutdown();
}
#[test]
fn a_recorded_quarantine_stops_the_effect_at_the_next_boot() {
    // The restart half of the finding: quarantine used to be an in-memory flag, so
    // the restart that a stuck effect invites cleared it and the effect resumed as
    // though nothing had been found. Planted directly because a live divergence
    // cannot be provoked from Starlark: the language is pure and every impure call is
    // journaled, so a replay of a healthy handler always agrees with its first run.
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    Connection::open(data.path().join("hekla.db"))
        .unwrap()
        .execute(
            "INSERT INTO effect_quarantine (effect, position, reason) VALUES (?1, ?2, ?3)",
            rusqlite::params!["send-welcome", 1i64, "planted for the restart test"],
        )
        .unwrap();
    // Rewind the cursor so the effect would happily re-process everything if the
    // quarantine were not honoured, making the assertion about the quarantine rather
    // than about there being no work left.
    Connection::open(data.path().join("hekla.db"))
        .unwrap()
        .execute("UPDATE effect_cursor SET watermark = 0", [])
        .unwrap();
    Connection::open(data.path().join("hekla.db"))
        .unwrap()
        .execute("DELETE FROM effect_invocation", [])
        .unwrap();

    let http = Arc::new(StubHttpClient::status(200));
    let harness = Boot::example()
        .data_dir(data.path())
        .with_master_key()
        .http(http.clone())
        .start();
    thread::sleep(Duration::from_secs(1));

    let effect = harness.rt.effect("send-welcome").unwrap();
    assert!(
        effect.quarantined(),
        "a recorded quarantine must be honoured at boot"
    );
    assert_eq!(
        effect.last_error().as_deref(),
        Some("planted for the restart test")
    );
    assert_eq!(
        http.call_count(),
        0,
        "a quarantined effect must not process anything: {:?}",
        http.calls()
            .iter()
            .map(|c| c.url.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        harness.rt.status()["effects"][0]["quarantined"],
        json!(true),
        "/status must surface it"
    );
    assert_eq!(
        harness.rt.status()["effects"][0]["state"],
        json!("quarantined"),
        "a quarantine outranks every other label, including the lag it causes"
    );

    harness.shutdown();
}

#[test]
fn a_verified_invocation_is_completed_before_it_is_checked() {
    // The other half: the check runs against an invocation already marked terminal,
    // so a boot after a violation skips the position instead of re-entering the
    // handler live and performing the call the sealed replay refused.
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&example_dir("users"), dir.path());
    fs::write(dir.path().join("hekla.toml"), "[verify]\nenabled = true\n").unwrap();

    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(dir.path())
        .data_dir(data.path())
        .with_master_key()
        .http_status(200)
        .start();
    let position = register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    support::wait_until("the effect to catch up", || {
        harness.rt.effect("send-welcome").unwrap().position() >= position
    });
    harness.shutdown();

    let status: String = Connection::open(data.path().join("hekla.db"))
        .unwrap()
        .query_row(
            "SELECT status FROM effect_invocation WHERE effect = 'send-welcome'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "terminal",
        "a checked invocation must be terminal, or a restart re-runs it live"
    );
}
