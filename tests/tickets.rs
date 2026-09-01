//! The `tickets` fixture, end to end against real storage.
//!
//! `hek test` runs the same project in an in-memory world where a key store is an
//! identity function, which is the right place for the rules and the wrong place for
//! the storage. These are the cases that need the real thing: a `delete` reaching
//! SQLite, an `Int @key` ordering numerically in one place and lexicographically in
//! another, a sealed `Money` column that is genuinely ciphertext at rest, and an
//! erasure that destroys a key rather than setting a flag.
//!
//! The fixture exists because `examples/` covers none of it. Between them the two
//! examples use no `delete`, no enum column, no `Decimal`, no `Json` column, no
//! `Timestamp` column and no `Int @key`, and every one of those is shipped surface.

use serde_json::{Value, json};

mod support;

use support::{
    ALICE, BOB, Boot, CAROL, Harness, UUID_A, ctx, fixture_dir, quiesce, read_row, sweep,
};

/// The fixture's org allocation, from `lib/config.hk`. Written down here rather than
/// read, so a change to one without the other is a failing test rather than a silent
/// agreement.
const ORG_CAP: usize = 6;

fn boot() -> Harness {
    Boot::new(fixture_dir("tickets"))
        .with_master_key()
        .http_status(200)
        .start()
}

fn open_body(ticket: &str, org: i64, owner: i64, contact: Option<&str>) -> Value {
    json!({
        "ticket_id": ticket,
        "org_id": org,
        "owner_id": owner,
        "title": "the printer is on fire",
        "priority": "Urgent",
        "due_at": 1_700_000_000_000_000i64,
        "fee": "12.50",
        "budget": "900.00",
        "contact": contact,
        "meta": { "source": "email", "seen": 2 },
    })
}

fn open(harness: &Harness, ticket: &str, org: i64, owner: i64, contact: Option<&str>) -> u64 {
    let result = harness
        .rt
        .execute(
            "OpenTicket",
            open_body(ticket, org, owner, contact),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    result.body["positions"]["last"].as_u64().unwrap()
}

/// A field of a row that must exist, so a missing row and a missing column are two
/// different failures and neither can pass as agreement.
fn field<'a>(row: &'a Value, name: &str) -> &'a Value {
    row.get(name)
        .unwrap_or_else(|| panic!("row {row} has no field `{name}`"))
}

/// A column that reads back absent. Asserts the row is there first: a missing row would
/// otherwise satisfy every absence assertion at once.
fn absent(row: &Value, name: &str) {
    let object = row
        .as_object()
        .unwrap_or_else(|| panic!("not a row: {row}"));
    assert!(
        !object.contains_key(name),
        "expected `{name}` to read back absent, got {row}"
    );
}

/// Every column kind a read model can hold, read back through the API that serves it.
///
/// The JSON *type* of each is the assertion, not just the value: an enum and a
/// timestamp are both TEXT in SQLite and both must come back as strings, a `Money` is a
/// decimal string rather than a float, and a `Json` column is an object rather than the
/// text it was stored as.
#[test]
fn every_column_kind_reads_back_at_its_declared_shape() {
    let harness = boot();
    let position = open(&harness, ALICE, 1, 10, Some("ada@example.com"));
    let row = read_row(&harness, "Tickets", "Ticket", ALICE, position)
        .expect("the ticket should have a row");

    assert_eq!(field(&row, "org_id"), &json!(1), "an Int is a JSON number");
    assert_eq!(
        field(&row, "title"),
        &json!("the printer is on fire"),
        "a String is a JSON string"
    );
    assert_eq!(
        field(&row, "priority"),
        &json!("Urgent"),
        "an enum is its variant name"
    );
    assert_eq!(
        field(&row, "due_at"),
        &json!("2023-11-14T22:13:20Z"),
        "a Timestamp column is RFC 3339, which is what it sorts on"
    );
    assert_eq!(
        field(&row, "fee"),
        &json!("12.50"),
        "a Money is a decimal string, trailing zero and all, never a float"
    );
    assert_eq!(
        field(&row, "meta"),
        &json!({ "source": "email", "seen": 2 }),
        "a Json column is the value, not the text it was stored as"
    );

    // The two sealed columns, which is the case `hek test` cannot reach: in its world a
    // key store is the identity function, so nothing here would be encrypted at all.
    assert_eq!(
        field(&row, "contact"),
        &json!("ada@example.com"),
        "a sealed String decrypts and reads back as itself"
    );
    assert_eq!(
        field(&row, "budget"),
        &json!("900.00"),
        "a sealed Money decrypts and is re-typed to its declared kind"
    );

    harness.shutdown();
}

/// What the read API serves is decrypted; what SQLite holds is not. Without this the
/// assertions above would pass just as well against a projector that stored plaintext.
#[test]
fn a_sealed_column_is_ciphertext_at_rest() {
    let harness = boot();
    let position = open(&harness, ALICE, 1, 10, Some("ada@example.com"));
    support::wait_position(&harness.rt, "Tickets", position);

    let shared = harness.rt.projector("Tickets").unwrap();
    let model = hekla::read_model::ReadModel::open_readonly(&shared.db_path).unwrap();
    let entity = hekla::read_api::find_entity(&shared.entities, "Ticket").unwrap();
    let stored = model.get(entity, ALICE).unwrap().expect("a row at rest");

    for (column, plaintext) in [("contact", "ada@example.com"), ("budget", "900.00")] {
        let held = field(&stored, column).as_str().expect("stored as text");
        assert_ne!(
            held, plaintext,
            "`{column}` must not be stored in the clear"
        );
        assert!(
            !held.contains(plaintext),
            "`{column}` must not contain its plaintext: {held}"
        );
    }
    // A plaintext column beside them is untouched, so the encryption is per field and
    // not per row.
    assert_eq!(field(&stored, "title"), &json!("the printer is on fire"));

    harness.shutdown();
}

/// `update` and `delete` reaching real SQL, and `patch` counting in both directions.
/// The examples exercise neither `delete` nor a decrementing counter.
#[test]
fn every_write_statement_reaches_the_read_model() {
    let harness = boot();
    open(&harness, ALICE, 1, 10, Some("ada@example.com"));
    let position = open(&harness, BOB, 1, 11, None);
    quiesce(&harness);

    let totals = read_row(&harness, "Tickets", "OrgTotals", "1", position)
        .expect("patch materialises the row from zeros");
    assert_eq!(field(&totals, "opened"), &json!(2));
    assert_eq!(field(&totals, "closed"), &json!(0));
    assert_eq!(
        field(&totals, "spend"),
        &json!("25.00"),
        "money accumulates exactly, without a float in the middle"
    );

    let result = harness
        .rt
        .execute(
            "RetitleTicket",
            json!({ "ticket_id": ALICE, "title": "resolved" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    let position = result.body["positions"]["last"].as_u64().unwrap();
    let row = read_row(&harness, "Tickets", "Ticket", ALICE, position).expect("still there");
    assert_eq!(field(&row, "title"), &json!("resolved"));
    assert_eq!(
        field(&row, "fee"),
        &json!("12.50"),
        "`update` touches the named column and nothing else"
    );

    let result = harness
        .rt
        .execute(
            "CloseTicket",
            json!({ "ticket_id": ALICE, "org_id": 1 }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
    let position = result.body["positions"]["last"].as_u64().unwrap();
    assert!(
        read_row(&harness, "Tickets", "Ticket", ALICE, position).is_none(),
        "`delete` removes the row"
    );
    assert!(
        read_row(&harness, "Tickets", "Ticket", BOB, position).is_some(),
        "and removes only that one"
    );
    let totals = read_row(&harness, "Tickets", "OrgTotals", "1", position).unwrap();
    assert_eq!(
        field(&totals, "opened"),
        &json!(2),
        "opened never decrements"
    );
    assert_eq!(field(&totals, "closed"), &json!(1));

    harness.shutdown();
}

/// Two subjects on one event, erased independently, against a key store where erasing
/// really destroys the key.
#[test]
fn each_subject_is_erased_without_touching_the_other() {
    let harness = boot();
    let position = open(&harness, ALICE, 1, 10, Some("ada@example.com"));
    // A second owner in the same organisation, so the erasure below has something it
    // must leave alone.
    let position = position.max(open(&harness, BOB, 1, 11, Some("bob@example.com")));
    quiesce(&harness);

    harness
        .rt
        .keystore()
        .unwrap()
        .erase("owner_id", "10")
        .unwrap();

    let row = read_row(&harness, "Tickets", "Ticket", ALICE, position).expect("the row remains");
    absent(&row, "contact");
    assert_eq!(
        field(&row, "budget"),
        &json!("900.00"),
        "the organisation's figure is scoped to a different key and is untouched"
    );
    assert_eq!(
        field(&row, "owner_id"),
        &json!(10),
        "a subject id stays plaintext: it is how the key was found"
    );

    let other = read_row(&harness, "Tickets", "Ticket", BOB, position).unwrap();
    assert_eq!(
        field(&other, "contact"),
        &json!("bob@example.com"),
        "another owner's address is a different key"
    );

    // The other direction.
    harness.rt.keystore().unwrap().erase("org_id", "1").unwrap();
    let row = read_row(&harness, "Tickets", "Ticket", ALICE, position).unwrap();
    absent(&row, "budget");
    assert_eq!(field(&row, "title"), &json!("the printer is on fire"));

    harness.shutdown();
}

/// The wide slice. The cap is a rule about every ticket in the organisation, so it has
/// to hold at append time; a read model would race it.
#[test]
fn an_organisations_allocation_runs_out_and_its_neighbour_is_unaffected() {
    let harness = boot();
    for n in 0..ORG_CAP {
        let ticket = uuid::Uuid::from_u128(n as u128 + 1).to_string();
        open(&harness, &ticket, 1, 10, None);
    }

    let result = harness
        .rt
        .execute("OpenTicket", open_body(CAROL, 1, 10, None), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 422, "{:?}", result.body);
    assert_eq!(
        result.body["error"]["code"], "org_full",
        "{:?}",
        result.body
    );

    // Another organisation has its own allocation, which is what keying the wide slice
    // on `org_id` buys.
    let result = harness
        .rt
        .execute("OpenTicket", open_body(CAROL, 2, 10, None), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);

    harness.shutdown();
}

/// The effect's two halves, against a real journal: the call goes out with the
/// decrypted address, and the internal command it invokes lands exactly one event.
#[test]
fn the_effect_notifies_and_records_exactly_once() {
    let stub = std::sync::Arc::new(hekla::effect::StubHttpClient::ok());
    let harness = Boot::new(fixture_dir("tickets"))
        .with_master_key()
        .http(stub.clone())
        .start();

    open(&harness, ALICE, 1, 10, Some("ada@example.com"));
    quiesce(&harness);

    assert_eq!(stub.call_count(), 1, "one ticket, one notification");
    let sent: Value = serde_json::from_slice(stub.calls()[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(
        field(&sent, "to"),
        &json!("ada@example.com"),
        "the effect reveals, so the address leaves as plaintext"
    );
    assert_eq!(field(&sent, "ticket"), &json!(ALICE));
    assert_eq!(
        harness.rt.log_head(),
        2,
        "the opened ticket and the notification it invoked, and nothing else"
    );

    harness.shutdown();
}

/// The fixture's own invariants, swept. Asserted as equalities rather than floors: a
/// clean report that checked nothing reads exactly like a clean sweep, and every silent
/// skip path in the sweep is a number that would stop matching.
#[test]
fn the_fixture_sweeps_clean_and_covers_everything_it_should() {
    let data = tempfile::tempdir().unwrap();
    {
        let stub = std::sync::Arc::new(hekla::effect::StubHttpClient::ok());
        let harness = Boot::new(fixture_dir("tickets"))
            .data_dir(data.path())
            .with_master_key()
            .http(stub.clone())
            .start();
        // Every ticket has a contact, so every invocation journals a call and every one
        // is replayable. A ticket without one is the case below.
        open(&harness, ALICE, 1, 10, Some("ada@example.com"));
        open(&harness, BOB, 2, 11, Some("bo@example.com"));
        open(&harness, UUID_A, 2, 11, Some("cy@example.com"));
        quiesce(&harness);
        harness.shutdown();
    }

    let report = sweep(&fixture_dir("tickets"), data.path());
    assert!(
        report.is_clean(),
        "the fixture should sweep clean, got {:?}",
        report.violations
    );
    assert_eq!(
        report.projectors_checked, 1,
        "the fixture declares one projector and it has a model on disk"
    );
    assert_eq!(
        report.invocations_checked, 3,
        "one invocation per opened ticket, all three replayed"
    );
    assert_eq!(
        report.invocations_skipped, 0,
        "nothing was skipped, so the count above is coverage and not luck"
    );
}

/// An invocation that journaled nothing is skipped rather than checked, and the sweep
/// cannot tell it from one whose journal the retention sweeper reclaimed. Both come
/// back as an empty journal, and `sweep_effect` reads that as "nothing to replay
/// against".
///
/// Pinned rather than fixed, because the two are genuinely indistinguishable on disk:
/// telling them apart needs a durable marker that does not exist today. It is worth
/// knowing because it is the one way a clean report can cover less than it looks like
/// it does, which is why `invocations_skipped` is asserted here and everywhere else.
#[test]
fn an_invocation_that_called_nothing_is_skipped_rather_than_replayed() {
    let data = tempfile::tempdir().unwrap();
    {
        let harness = Boot::new(fixture_dir("tickets"))
            .data_dir(data.path())
            .with_master_key()
            .http_status(200)
            .start();
        // No contact, so the arm logs and returns before it reaches `http.post`. `log`
        // is not journaled, so the invocation completes with an empty journal.
        open(&harness, ALICE, 1, 10, None);
        quiesce(&harness);
        harness.shutdown();
    }

    let report = sweep(&fixture_dir("tickets"), data.path());
    assert!(report.is_clean(), "{:?}", report.violations);
    assert_eq!(report.invocations_checked, 0);
    assert_eq!(
        report.invocations_skipped, 1,
        "a clean report that replayed nothing has to say so in its counts"
    );
}

/// A subject written to again after an erasure gets new key material, and everything
/// sealed under the destroyed key stays shredded.
///
/// The append path mints a key on first use (`KeyStore::encrypt_subject`) while the
/// projection path only ever uses one that already exists
/// (`encrypt_subject_existing`), and the asymmetry is the whole point: a rebuild can
/// never resurrect what an erasure destroyed, but a person who comes back and gives
/// their address again has given new data, and new data is readable.
///
/// Pinned here because `tests/model.rs` deliberately steps over this case: heklang's
/// harness models the key lifecycle as the one-way flag rule 12 needs, so the two
/// worlds answer it differently and the model constrains its sequences rather than
/// papering over the difference.
#[test]
fn a_subject_written_to_after_an_erasure_gets_a_new_key_and_keeps_the_old_data_shredded() {
    let harness = boot();
    let position = open(&harness, ALICE, 1, 10, Some("ada@example.com"));
    support::wait_position(&harness.rt, "Tickets", position);

    harness
        .rt
        .keystore()
        .unwrap()
        .erase("owner_id", "10")
        .unwrap();

    // The same owner opens another ticket, which seals a fresh address under a key
    // minted on the spot.
    let position = open(&harness, BOB, 1, 10, Some("ada-again@example.com"));

    let fresh = read_row(&harness, "Tickets", "Ticket", BOB, position).expect("the new row");
    assert_eq!(
        field(&fresh, "contact"),
        &json!("ada-again@example.com"),
        "new content about the same subject is readable under the new key"
    );

    let old = read_row(&harness, "Tickets", "Ticket", ALICE, position).expect("the old row");
    absent(&old, "contact");

    // And a rebuild does not recover it either: the projector re-seals from the log,
    // whose payload is still ciphertext under the key that is gone.
    support::replay_and_wait(&harness.rt, "Tickets");
    let old = read_row(&harness, "Tickets", "Ticket", ALICE, position).expect("the old row");
    absent(&old, "contact");

    harness.shutdown();
}
