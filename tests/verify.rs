//! The invariant harness: `hekla verify` offline, and the continuous checks.
//!
//! A checker that never fires is indistinguishable from one that works, so every
//! check here is exercised twice: once against a healthy project, to prove it stays
//! quiet, and once against a *planted* violation, to prove it actually fires. The
//! planted cases reach behind the runtime's back (a direct SQLite write, a deleted
//! journal row), because that is the only way to produce the corruption these checks
//! exist to catch: the runtime will not produce it on request.

mod support;

use hekla::crypto::KeyStore;
use hekla::effect::StubHttpClient;
use hekla::invariant::{Mismatch, Violation};
use hekla::lock::DataDirLock;
use hekla::opdb::OpDb;
use hekla::verify;
use rusqlite::Connection;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
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
    support::wait_position(&harness.rt, "Users", position);
    // The effect has to reach its position too, or there is no recorded invocation
    // for the replay check to sweep.
    support::wait_until("the effect to catch up", || {
        harness.rt.effect("SendWelcome").unwrap().position() >= position
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

/// A key store over a stopped data directory, for planting and inspecting subject keys.
fn keystore(data_dir: &Path) -> KeyStore {
    let opdb = Arc::new(Mutex::new(OpDb::open(&data_dir.join("hekla.db")).unwrap()));
    KeyStore::new(opdb, master_keys())
}

/// The log head, read with a runtime up and shut down again, since a sweep needs the
/// directory to itself. The default stub answers 400, so nothing an effect does on the
/// way past can move the number this reads.
fn log_head(project_dir: &Path, data_dir: &Path) -> u64 {
    let harness = Boot::new(project_dir)
        .data_dir(data_dir)
        .with_master_key()
        .start();
    let head = harness.rt.log_head();
    harness.shutdown();
    head
}

/// Delete journal rows of one kind, and assert the fixture really had some to drop.
fn drop_journal(data_dir: &Path, effect: &str, kind: &str) -> usize {
    let removed = Connection::open(data_dir.join("hekla.db"))
        .unwrap()
        .execute(
            "DELETE FROM effect_journal WHERE effect = ?1 AND kind = ?2",
            rusqlite::params![effect, kind],
        )
        .unwrap();
    assert!(
        removed >= 1,
        "the fixture should have a journaled `{kind}` call to drop"
    );
    removed
}

/// The one `ReplayDivergence` in a report, or a panic naming what was found instead.
fn divergence(report: &verify::Report) -> String {
    report
        .violations
        .iter()
        .find(|violation| matches!(violation, Violation::ReplayDivergence { .. }))
        .unwrap_or_else(|| panic!("expected a replay divergence, got {:?}", report.violations))
        .to_string()
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

    open_model(data.path(), "Users")
        .execute(
            "UPDATE \"User\" SET name = 'Tampered' WHERE user_id = ?1",
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

    open_model(data.path(), "Users")
        .execute("DELETE FROM \"User\" WHERE user_id = ?1", [BOB])
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

/// An `Int @key` orders numerically in SQL and lexicographically in the merge join, and
/// `10` before `2` is where the two part company. With the sides walked in different
/// orders, deleting row 2 made the join report row 10 as both only-live and
/// only-rebuilt: three violations for one deleted row, two of them about a row that is
/// fine. Never a false clean, but an operator cannot act on a report that invents rows.
#[test]
fn an_int_keyed_entity_reports_one_deleted_row_and_not_three() {
    let dir = support::write_project(&[
        (
            "events/tick.hk",
            "event @tick.happened { group: Int, label: String @max(20) }\n",
        ),
        (
            "commands/tick.hk",
            "command Tick(group: Int, label: String) { emit @tick.happened { group, label } }\n",
        ),
        (
            "projectors/groups.hk",
            r#"
projector Groups {
  entity Group {
    group: Int @key,
    label: String @max(20),
  }

  on @tick.happened { group, label } { put Group { group, label } }
}
"#,
        ),
    ]);

    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(dir.path()).data_dir(data.path()).start();
    // Two keys whose numeric and textual orders disagree, which is the whole fixture.
    for group in [2, 10] {
        let result = harness
            .rt
            .execute(
                "Tick",
                json!({ "group": group, "label": "keep" }),
                &support::ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }
    support::wait_position(&harness.rt, "Groups", 2);
    harness.shutdown();

    let removed = open_model(data.path(), "Groups")
        .execute("DELETE FROM \"Group\" WHERE \"group\" = 2", [])
        .unwrap();
    assert_eq!(removed, 1, "the fixture should have row 2 to delete");

    let report = sweep(dir.path(), data.path());
    assert_eq!(
        report.violations.len(),
        1,
        "one deleted row is one violation, got {:?}",
        report.violations
    );
    let Violation::RebuildMismatch { key, detail, .. } = &report.violations[0] else {
        panic!(
            "expected a rebuild mismatch, got {:?}",
            report.violations[0]
        );
    };
    assert_eq!(key, "2");
    assert!(
        matches!(detail, Mismatch::OnlyRebuilt(_)),
        "the deleted row is only in the rebuild, got {detail:?}"
    );
}

#[test]
fn a_row_invented_behind_the_projector_is_caught() {
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    open_model(data.path(), "Users")
        .execute(
            "INSERT INTO \"User\" (user_id, email, name) VALUES (?1, ?2, ?3)",
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
            "DELETE FROM effect_journal WHERE effect = 'SendWelcome' AND rowid IN \
             (SELECT MIN(rowid) FROM effect_journal WHERE effect = 'SendWelcome')",
            [],
        )
        .unwrap();
    assert_eq!(
        removed, 1,
        "the fixture should have a journaled call to drop"
    );

    let report = sweep(&example_dir("users"), data.path());
    let text = divergence(&report);
    assert!(text.contains("no journal entry"), "{text}");
    assert!(text.contains("performed it a second time"), "{text}");
}

/// The half of the seal that blocking the transport does not cover.
///
/// Dropping the `invoke` row rather than the `http.post` one sends the replay into
/// heklang's `invoke`, which on a journal miss runs the target command and appends for
/// real. A replay carries no journal identity, so `idempotency_tag` finds nothing and
/// the append has no existence clause to suppress the duplicate either; the sweep opens
/// a live write coordinator, so it lands. An audit that appends to the log it is
/// auditing is worse than no audit.
///
/// `LogNote` folds nothing on purpose. `examples/users` cannot show this: its
/// `RecordWelcome` has an idempotence fold, and that boundary absorbs the second append
/// so the log never moves. That is the right design and the wrong fixture.
#[test]
fn a_missing_invoke_entry_is_caught_without_appending_to_the_log() {
    let dir = support::write_project(&[
        (
            "events/note.hk",
            r#"
event @note.made { note_id: Uuid, body: String @max(100) }
event @note.logged { note_id: Uuid }
"#,
        ),
        (
            "commands/make-note.hk",
            r#"
command MakeNote(note_id: Uuid, body: String) { emit @note.made { note_id, body } }
"#,
        ),
        (
            "commands/internal/log-note.hk",
            r#"
command LogNote(note_id: Uuid) { emit @note.logged { note_id } }
"#,
        ),
        (
            "effects/record-note.hk",
            r#"
effect RecordNote {
  on @note.made { note_id } {
    http.post("https://example.test/note", { "id": note_id })
    invoke LogNote { note_id }
  }
}
"#,
        ),
    ]);

    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(dir.path())
        .data_dir(data.path())
        .http_status(200)
        .start();
    let result = harness
        .rt
        .execute(
            "MakeNote",
            json!({ "note_id": UUID_A, "body": "a note" }),
            &support::ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    // The effect's own `invoke` moves the head, so wait for the appended event rather
    // than for the trigger's position.
    support::wait_until("the invoked command to land", || harness.rt.log_head() >= 2);
    support::wait_until("the effect to catch up", || {
        harness.rt.effect("RecordNote").unwrap().position() >= 1
    });
    let before = harness.rt.log_head();
    harness.shutdown();

    drop_journal(data.path(), "RecordNote", "invoke");

    let report = sweep(dir.path(), data.path());
    let text = divergence(&report);
    assert!(text.contains("no journal entry"), "{text}");
    assert_eq!(
        log_head(dir.path(), data.path()),
        before,
        "a sweep must not append to the log it audits"
    );
}

/// The same hole, reached through `erase` instead of `invoke`.
///
/// The effect has no `reveal`, deliberately: an invocation whose plaintext is gone
/// terminal-skips before it reaches the erase, which is the documented carve-out and
/// would hide the case. The key is minted back by hand before the sweep, because the
/// first run's own erase destroyed it and a sweep cannot shred what is already gone.
#[test]
fn a_missing_erase_entry_is_caught_without_shredding_a_key() {
    let dir = support::write_project(&[
        (
            "events/account.hk",
            r#"
event @account.closed {
  account_id: Int,
  email: String? @subject(account_id) @max(200),
}
"#,
        ),
        (
            "commands/close-account.hk",
            r#"
command CloseAccount(account_id: Int, email: String?) {
  emit @account.closed { account_id, email }
}
"#,
        ),
        (
            "effects/shred-account.hk",
            r#"
effect ShredAccount {
  on @account.closed { account_id } {
    http.post("https://example.test/farewell", { "id": account_id })
    erase(account_id)
  }
}
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
            "CloseAccount",
            json!({ "account_id": 42, "email": "gone@example.com" }),
            &support::ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    let position = result.body["positions"]["last"].as_u64().unwrap();
    support::wait_until("the effect to catch up", || {
        harness.rt.effect("ShredAccount").unwrap().position() >= position
    });
    harness.shutdown();

    let planted = keystore(data.path());
    planted
        .encrypt_subject("account_id", "42", "email", "x")
        .unwrap();
    drop_journal(data.path(), "ShredAccount", "erase");

    let report = sweep(dir.path(), data.path());
    let text = divergence(&report);
    assert!(text.contains("no journal entry"), "{text}");
    assert!(
        keystore(data.path())
            .encrypt_subject_existing("account_id", "42", "email", "x")
            .unwrap()
            .is_some(),
        "a sweep must not shred a key in the store it audits"
    );
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
    let effect = edited.path().join("effects/send-welcome.hk");
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

/// The twin of the test above, and the reason the digest is worth adopting.
///
/// The skip gate compares an invocation's recorded hash against the effect's current
/// one. When that was a hash of the file's bytes, reindenting an effect or adding a
/// comment silently dropped every recorded invocation out of the replay check, and
/// nothing said so: `hekla verify` just quietly stopped checking that effect until new
/// invocations accumulated. The digest hashes what the effect does, so a reformat keeps
/// the coverage.
#[test]
fn a_cosmetically_edited_effect_is_still_checked() {
    let data = tempfile::tempdir().unwrap();
    populated(data.path());

    let reformatted = tempfile::tempdir().unwrap();
    copy_dir(&example_dir("users"), reformatted.path());
    let effect = reformatted.path().join("effects/send-welcome.hk");
    let source = fs::read_to_string(&effect).unwrap();
    // Every line moves and a comment appears; nothing the effect does changes.
    let rewritten = format!(
        "// Reformatted, and not otherwise touched.\n{}",
        source
            .lines()
            .map(|line| {
                if line.trim().is_empty() {
                    line.to_owned()
                } else {
                    format!("  {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
    fs::write(&effect, rewritten).unwrap();

    let report = sweep(reformatted.path(), data.path());
    assert!(
        report.is_clean(),
        "a reformatted effect still replays identically, got {:?}",
        report.violations
    );
    assert!(
        report.invocations_checked >= 1,
        "a reformat must not cost the replay check its coverage, got checked={} skipped={}",
        report.invocations_checked,
        report.invocations_skipped
    );
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
    support::wait_position(&harness.rt, "Users", position);
    support::wait_until("the effect to catch up", || {
        harness.rt.effect("SendWelcome").unwrap().position() >= position
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
    support::wait_position(&harness.rt, "CustomerOrders", position);
    harness.shutdown();

    let report = sweep(&example_dir("orders"), data.path());
    assert!(
        report.is_clean(),
        "subject columns should rebuild byte-identically, got {:?}",
        report.violations
    );
    assert!(report.projectors_checked >= 1);
}

/// An erased subject must not make a projector look corrupt forever.
///
/// The rebuild check compares stored bytes, which is sound only while the two sides
/// have the same bytes to hold. Erasure breaks that: a shred rewrites nothing, so the
/// live row keeps the ciphertext it was written with, while a rebuild re-encrypts from
/// the log, finds no key, and writes NULL rather than minting the key the erasure
/// destroyed. Both read back absent, so they differ only at rest.
///
/// Found by erasing a subject on a real server and running `hekla verify`, which is
/// the ordinary operational sequence: a GDPR request, then the nightly sweep.
#[test]
fn an_erased_subject_is_not_a_rebuild_mismatch() {
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(example_dir("orders"))
        .data_dir(data.path())
        .with_master_key()
        .start();
    let position = place_order(&harness.rt, UUID_A, 7, "buyer@example.com");
    support::wait_position(&harness.rt, "CustomerOrders", position);

    harness.shutdown();

    // Control: clean before the erasure, so the assertion below is about the shred and
    // not about the fixture. The sweep takes the data-directory lock, so it runs with
    // the runtime stopped.
    assert!(sweep(&example_dir("orders"), data.path()).is_clean());

    let opdb = std::sync::Arc::new(std::sync::Mutex::new(
        hekla::opdb::OpDb::open(&data.path().join("hekla.db")).unwrap(),
    ));
    assert!(
        hekla::crypto::KeyStore::new(opdb, master_keys())
            .erase("customer_id", "7")
            .unwrap(),
        "the subject key must exist to be erased"
    );

    let report = sweep(&example_dir("orders"), data.path());
    assert!(
        report.is_clean(),
        "an erased subject is a shred, not corruption: {:?}",
        report.violations
    );
    assert!(report.projectors_checked >= 1, "the check really ran");
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
    support::wait_position(&harness.rt, "CustomerOrders", position);
    harness.shutdown();

    open_model(data.path(), "CustomerOrders")
        .execute("UPDATE \"Order\" SET email = 'not-ciphertext'", [])
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
            "PlaceOrder",
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
            "events/customer.hk",
            r#"
event @customer.closed {
  customer_id: Int,
  // Optional because an erased subject reads back absent, which is the whole point
  // of the scenario below.
  email: String? @subject(customer_id) @max(200),
}
"#,
        ),
        (
            "commands/close-account.hk",
            r#"
command CloseAccount(customer_id: Int, email: String?) {
  emit @customer.closed { customer_id, email }
}
"#,
        ),
        (
            "effects/forget-customer.hk",
            r#"
effect ForgetCustomer {
  on @customer.closed { customer_id, email } {
    let address = reveal(email)
    http.post("https://example.test/farewell", { "to": address })
    erase(customer_id)
  }
}
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
            "CloseAccount",
            json!({ "customer_id": 42, "email": "gone@example.com" }),
            &support::ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    let position = result.body["positions"]["last"].as_u64().unwrap();

    support::wait_until("the effect to catch up", || {
        harness.rt.effect("ForgetCustomer").unwrap().position() >= position
    });
    let effect = harness.rt.effect("ForgetCustomer").unwrap();
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
            "events/thing.hk",
            r#"
event @thing.happened { id: Uuid }
event @thing.ignored { id: Uuid }
"#,
        ),
        (
            "commands/do-it.hk",
            r#"
command DoIt(id: Uuid) {
  emit @thing.ignored { id }
}
"#,
        ),
        (
            "commands/note-it.hk",
            r#"
command NoteIt(id: Uuid) {
  emit @thing.happened { id }
}
"#,
        ),
        (
            "projectors/tracker.hk",
            r#"
projector Tracker {
  entity Thing {
    id: Uuid @key,
  }

  on @thing.happened { id } {
    put Thing { id }
  }
}
"#,
        ),
    ]);

    let harness = Boot::new(dir.path()).start();
    // Append only events the projector's query does not select, so its checkpoint
    // moves on the non-matching tail while its model stays empty.
    for id in [ALICE, BOB] {
        let result = harness
            .rt
            .execute("DoIt", json!({ "id": id }), &support::ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }
    let tail = support::log_head(&harness.rt);
    support::wait_until("the projector to track the non-matching tail", || {
        harness.rt.projector("Tracker").unwrap().position() >= tail
    });

    // The rebuild has to finish while the boundary is still empty, which is the whole
    // scenario: appending the matching event first would give it something to apply
    // and hide the bug.
    support::replay_and_wait(&harness.rt, "Tracker");

    // Prove the thread survived the replay by making it do work afterwards: a
    // matching event has to land in the model. Asserting on position or `failed`
    // alone would pass against a dead thread, since neither moves when it dies.
    let result = harness
        .rt
        .execute("NoteIt", json!({ "id": CAROL }), &support::ctx(), None)
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    let head = support::log_head(&harness.rt);
    support::wait_until(
        "the projector to apply a matching event after the replay",
        || harness.rt.projector("Tracker").unwrap().position() >= head,
    );
    assert!(
        support::read_row(&harness, "Tracker", "Thing", CAROL, head).is_some(),
        "the rebuilt model must hold the matching event"
    );

    let shared = harness.rt.projector("Tracker").unwrap();
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
            rusqlite::params!["SendWelcome", 1i64, "planted for the restart test"],
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

    let effect = harness.rt.effect("SendWelcome").unwrap();
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
        harness.rt.effect("SendWelcome").unwrap().position() >= position
    });
    harness.shutdown();

    let status: String = Connection::open(data.path().join("hekla.db"))
        .unwrap()
        .query_row(
            "SELECT status FROM effect_invocation WHERE effect = 'SendWelcome'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "terminal",
        "a checked invocation must be terminal, or a restart re-runs it live"
    );
}

/// An erasure followed by a write to the same subject must not make the sweep report a
/// healthy projector as corrupt.
///
/// The narrowest composition of two features that are each fine alone. Erasing shreds
/// the key and leaves the live row holding ciphertext nothing can read; writing to that
/// subject again mints a fresh key, because the append path creates one on first use.
/// Now the key *exists*, so the old `keystore.erased` question answered "no" and the
/// stale ciphertext was compared byte for byte against a rebuild that could not decrypt
/// it and wrote NULL instead.
///
/// Both sides read as absent through the read API, so nothing is wrong with the data.
/// What was wrong was the question: an invariant sweep has to ask whether the stored
/// bytes are *readable*, which is what a reader sees, not whether some key is filed
/// under that subject.
#[test]
fn a_subject_erased_and_then_written_to_again_still_sweeps_clean() {
    let data = tempfile::tempdir().unwrap();
    {
        let harness = Boot::new(support::fixture_dir("tickets"))
            .data_dir(data.path())
            .with_master_key()
            .http_status(200)
            .start();
        open_ticket(&harness.rt, ALICE, 1, 10);
        support::quiesce(&harness);

        harness.rt.keystore().unwrap().erase("org_id", "1").unwrap();

        // The same organisation opens another ticket, which mints a key under the
        // subject the erasure destroyed. ALICE's `budget` stays sealed under the old
        // one and is now unreadable, but it is still sitting in the live row.
        open_ticket(&harness.rt, BOB, 1, 10);
        support::quiesce(&harness);
        harness.shutdown();
    }

    let report = sweep(&support::fixture_dir("tickets"), data.path());
    assert!(
        report.is_clean(),
        "a shredded column that a later write re-keyed is not corruption: {:?}",
        report.violations
    );
    assert_eq!(
        report.projectors_checked, 1,
        "and the projector really was checked"
    );
}

/// Open a ticket through the `tickets` fixture.
fn open_ticket(rt: &hekla::runtime::Runtime, ticket: &str, org: i64, owner: i64) {
    let result = rt
        .execute(
            "OpenTicket",
            json!({
                "ticket_id": ticket,
                "org_id": org,
                "owner_id": owner,
                "title": "the printer is on fire",
                "priority": "Urgent",
                "due_at": 1_700_000_000_000_000i64,
                "fee": "12.50",
                "budget": "900.00",
                "contact": "ada@example.com",
                "meta": {},
            }),
            &support::ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "OpenTicket failed: {:?}", result.body);
}
