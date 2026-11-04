//! The `tickets` fixture, end to end against real storage.
//!
//! `hek test` runs the same project in an in-memory world where a key store is an
//! identity function, which is the right place for the rules and the wrong place for
//! the storage. These are the cases that need the real thing: a `delete` reaching
//! SQLite, an `Int @key` ordering numerically in one place and lexicographically in
//! another, a sealed column that is genuinely ciphertext at rest, and an erasure that
//! destroys a key rather than setting a flag.
//!
//! The seals are the widest of those. A key store that is the identity function makes a
//! seal's ciphertext its plaintext, so under `hek test` a composite that sealed as its
//! document and one that sealed as a quoted string read back alike, and a payload seal
//! and a column seal are the same bytes whether or not anything re-rendered one.
//!
//! The fixture exists because `examples/` covers none of it. Between them the two
//! examples use no `delete`, no enum column, no `Decimal`, no `Json` column, no
//! `Timestamp` column and no `Int @key`, and every one of those is shipped surface.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use hekla::crypto::KeyStore;
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

/// The three sealed composites every ticket here carries, each written once so the value
/// a request body sends and the value a row has to read back are the same one.
///
/// `filed_at` is epoch microseconds, which is what rule 8 puts on the wire and what both
/// seals hold unchanged: `column_form` rewrites a top-level `Timestamp` only, so one
/// nested inside a document is never reached. `badge` is a string that looks like a
/// number, which is the leaf a composite seal must not flatten.
fn reporter() -> Value {
    json!({
        "name": "Ada Lovelace",
        "badge": "0042",
        "filed_at": 1_700_000_000_000_000i64,
        "team": "printers",
    })
}

fn watchers() -> Value {
    json!(["ops@example.com", "sec@example.com"])
}

fn labels() -> Value {
    json!({ "area": "printers", "shift": "night" })
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
        // Micros on the wire, and RFC 3339 once it is a column, which is the one sealed
        // field whose two seals differ on purpose.
        "contacted_at": 1_700_040_000_000_000i64,
        // The three composites, as rule 8 writes each on the wire: a record and a map
        // are objects and a list is an array. `filed_at` is epoch micros, and the badge
        // is a string that looks like a number, which is what a composite seal must not
        // flatten.
        "reporter": reporter(),
        "watchers": watchers(),
        "labels": labels(),
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

    // A sealed `Timestamp`, which is the kind whose column is a different shape from
    // its payload. It reads back RFC 3339 like the plain `due_at` above it, so a
    // reader cannot tell which of the two happened to be personal.
    assert_eq!(
        field(&row, "contacted_at"),
        &json!("2023-11-15T09:20:00Z"),
        "a sealed Timestamp column is RFC 3339, the same as a plain one"
    );

    // The three composites, which share one column kind and one seal text: the JSON
    // document rule 8 already writes. What a reader gets back is the value, because
    // `decrypt_row` re-types the plaintext through the same table a plain column takes.
    assert_eq!(
        field(&row, "reporter"),
        &reporter(),
        "a sealed record reads back as the record, not as the text it was sealed as"
    );
    assert_eq!(
        field(&row, "reporter")["badge"],
        json!("0042"),
        "a leaf that looks like a number stays one: 42 is a different badge"
    );
    assert_eq!(
        field(&row, "reporter")["filed_at"],
        json!(1_700_000_000_000_000i64),
        "a Timestamp inside a document stays micros, where `due_at` beside it is RFC 3339"
    );
    assert_eq!(
        field(&row, "watchers"),
        &watchers(),
        "a sealed list reads back as an array"
    );
    assert_eq!(
        field(&row, "labels"),
        &labels(),
        "a sealed map reads back as an object"
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

    // A composite is sealed whole rather than leaf by leaf, so what must not appear at
    // rest is any leaf of it: a document stored in the clear would show its own.
    for (column, plaintext) in [
        ("contact", "ada@example.com"),
        ("budget", "900.00"),
        ("contacted_at", "2023-11-15"),
        ("reporter", "Ada Lovelace"),
        ("watchers", "ops@example.com"),
        ("labels", "printers"),
    ] {
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

/// A composite is parsed out of its payload seal and rendered back into its column seal,
/// and the two have to be the same bytes.
///
/// A subject key is deterministic, so identical ciphertext is identical plaintext, and
/// the column's plaintext reached its seal through `unsealed_json` and `seal_text`. A
/// restringify that reordered a key, dropped a trailing zero or re-escaped a character
/// would show up here and nowhere else: both halves still decrypt to a readable
/// document, so every assertion about what a reader sees would go on passing.
///
/// **This is the path that parses.** The append path does not: `stored_seal` moving a
/// seal decrypts to text and re-encrypts that same text, with no JSON in the middle to
/// get wrong.
///
/// Stated per column rather than over every sealed one, because `due_at` is the field it
/// would not hold for. A `Timestamp` column stores RFC 3339 where a payload stores
/// micros, so sealing one would seal two different texts on purpose.
#[tokio::test]
async fn a_seal_holds_the_same_bytes_in_the_payload_and_in_the_column() {
    let harness = boot();
    let app = harness.app();
    let (status, result) = support::post_command(
        &app,
        "OpenTicket",
        open_body(ALICE, 1, 10, Some("ada@example.com")),
        None,
    )
    .await;
    assert_eq!(status, 200, "{result:?}");
    let position = result["positions"]["last"].as_u64().unwrap();
    support::wait_position_async(&harness.rt, "Tickets", position).await;

    // `decrypt=false`, so this is the ciphertext the log holds rather than what an
    // operator reading the page would be shown.
    let (status, event) =
        support::get(&app, &format!("/admin/events/{position}?decrypt=false")).await;
    assert_eq!(status, 200, "{event:?}");

    let shared = harness.rt.projector("Tickets").unwrap();
    let model = hekla::read_model::ReadModel::open_readonly(&shared.db_path).unwrap();
    let entity = hekla::read_api::find_entity(&shared.entities, "Ticket").unwrap();
    let stored = model.get(entity, ALICE).unwrap().expect("a row at rest");

    for column in ["reporter", "watchers", "labels", "contact", "budget"] {
        let sealed = field(&event["data"], column);
        assert!(
            sealed.as_str().is_some_and(|text| !text.is_empty()),
            "`{column}` should be ciphertext in the payload, got {sealed}"
        );
        assert_eq!(
            sealed,
            field(&stored, column),
            "`{column}`'s payload seal and column seal must hold the same text"
        );
    }
    // And the exception, which is why the list above is written out. `contacted_at` is
    // the same moment in both places and not the same text: micros in the payload,
    // RFC 3339 in the column, because that is the shape a column sorts on. A
    // deterministic key then makes two texts two ciphertexts.
    assert_ne!(
        field(&event["data"], "contacted_at"),
        field(&stored, "contacted_at"),
        "a sealed Timestamp seals its column form, which is not its payload form"
    );

    harness.shutdown();
}

/// A field added to a record since a seal was written reads as its `@absent` literal, on
/// the payload copy of that content and on the read-model copy alike.
///
/// This is `docs/declarations.md`'s promise, and the whole point of it is that adding a
/// field to a subject-bound record needs no migration: the plaintext is behind a key, so
/// there is no rewrite available even if anyone wanted one. Keeping it on one copy and
/// not the other would be worse than not keeping it at all, because an author would see
/// every effect go on working while every row written before today failed.
///
/// The two copies are read by two different calls. `reveal` goes through
/// `Value::from_sealed`, which reads stored history; the column used to go through
/// `Value::from_json`, which reads a request body and is right to refuse a missing key
/// there. Only the second was wrong, and only an `update` reaches it, because that is
/// the statement that reads a row back before putting it whole.
#[test]
fn a_field_younger_than_a_seal_reads_as_its_absent_literal_on_both_copies() {
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(hekla::effect::StubHttpClient::ok());
    {
        let project = support::load_ok(&fixture_dir("tickets"));
        let (_coordinator, store) = support::open_store(data.path());
        let opdb = Arc::new(Mutex::new(
            hekla::opdb::OpDb::open(&data.path().join("hekla.db")).unwrap(),
        ));
        let keystore = Arc::new(KeyStore::new(opdb, support::master_keys()));
        let mut body = open_body(ALICE, 1, 10, Some("ada@example.com"));
        for sealed in ["contacted_at", "reporter", "watchers", "labels"] {
            body[sealed] = json!("seeded");
        }
        let mut event =
            hekla::heklang_host::event_from_json(&project.program, "ticket.opened", &body).unwrap();
        // All four sealed by hand, because `event_from_json` reads a sealed field at its
        // stored shape and seals a document passed as text re-quoted. Only `reporter` is
        // the point; the other three are here so the arm reaches it rather than wedging
        // on a neighbour.
        //
        // `reporter` is the document sealed before `Reporter` declared `team`. Nothing
        // in the runtime writes an old declaration any more, and the plaintext is behind
        // a key, so building one is the only way to have one.
        let older = json!({
            "name": "Ada Lovelace",
            "badge": "0042",
            "filed_at": 1_700_000_000_000_000i64,
        });
        let seals = [
            ("contacted_at", "Owner", "10", "1700040000000000".to_owned()),
            ("reporter", "Owner", "10", older.to_string()),
            ("watchers", "Org", "1", watchers().to_string()),
            ("labels", "Org", "1", labels().to_string()),
        ];
        for (name, subject, id, text) in seals {
            let content = keystore.encrypt_subject(subject, id, name, &text).unwrap();
            event.fields.insert(
                name.to_owned(),
                heklang::Value::Sealed {
                    field: name.to_owned(),
                    subject: subject.to_owned(),
                    id: id.to_owned(),
                    content: content.into(),
                },
            );
        }
        support::seed_event_value(&store, &project, &ctx(), keystore, &event);
    }

    let harness = Boot::new(fixture_dir("tickets"))
        .data_dir(data.path())
        .with_master_key()
        .http(stub.clone())
        .start();
    quiesce(&harness);

    // The payload copy, through the effect's `reveal`. It reaches `.name` and `.badge`,
    // so the record parsed, and it never saw the field that is not there.
    assert_eq!(stub.call_count(), 1, "the effect ran rather than wedging");
    let sent: Value = serde_json::from_slice(stub.calls()[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(field(&sent, "reporter"), &json!("Ada Lovelace"));

    // The read-model copy, through an `update`, which reads the row back and puts it
    // whole. Without the literal this is where it failed, on a column the statement does
    // not even name.
    let result = harness
        .rt
        .execute(
            "RetitleTicket",
            json!({ "ticket_id": ALICE, "title": "resolved" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(
        result.status, 200,
        "an `update` must not fail on a field younger than the seal: {:?}",
        result.body
    );
    let position = result.body["positions"]["last"].as_u64().unwrap();
    let row = read_row(&harness, "Tickets", "Ticket", ALICE, position).expect("the row");
    assert_eq!(field(&row, "title"), &json!("resolved"));
    assert_eq!(
        field(&row, "reporter")["team"],
        json!(""),
        "the column carries the literal, not the absence"
    );
    assert_eq!(
        field(&row, "reporter")["badge"],
        json!("0042"),
        "and everything the seal did carry is unchanged"
    );

    harness.shutdown();
}

/// A seal whose text is not the document its declaration promises wedges the lane, and
/// goes on wedging it.
///
/// What writes one is a declaration that moved: `reporter` was `String @subject(owner_id)`
/// when the log was written and is `Reporter @subject(owner_id)` now, so every seal
/// already in the log holds a bare name where a document is expected. Built by hand here
/// because nothing in the runtime can produce one any more: `seal_text` renders a
/// composite with `to_string`, so every seal it writes parses.
///
/// **A wedge rather than a skip**, and the asymmetry with an erased subject is the point.
/// `reveal` of an erased subject is terminal because no retry recovers a destroyed key.
/// This is the opposite: the plaintext is intact behind a key that still opens, and one
/// edit to one `.hk` line makes the next attempt succeed. Skipping would burn the event
/// silently, with the data still there.
#[tokio::test]
async fn a_seal_that_is_not_the_document_it_promises_wedges_rather_than_skipping() {
    let data = tempfile::tempdir().unwrap();
    {
        let project = support::load_ok(&fixture_dir("tickets"));
        let (_coordinator, store) = support::open_store(data.path());
        let opdb = Arc::new(Mutex::new(
            hekla::opdb::OpDb::open(&data.path().join("hekla.db")).unwrap(),
        ));
        let keystore = Arc::new(KeyStore::new(opdb, support::master_keys()));
        // Sealed under the field name and the subject the declaration says, so it
        // decrypts: the fault is what comes out, not whether anything does.
        let content = keystore
            .encrypt_subject("Owner", "10", "reporter", "Ada Lovelace")
            .unwrap();
        // Every sealed field arrives here as text, because a seeded event is read at its
        // stored shape and a stored seal is a host's ciphertext. `contact` and `budget`
        // are text in a request body anyway; the three composites are not, so they are
        // spelled as text and `reporter` is then replaced outright.
        let mut body = open_body(ALICE, 1, 10, Some("ada@example.com"));
        for sealed in ["contacted_at", "reporter", "watchers", "labels"] {
            body[sealed] = json!("seeded");
        }
        let mut event =
            hekla::heklang_host::event_from_json(&project.program, "ticket.opened", &body).unwrap();
        event.fields.insert(
            "reporter".to_owned(),
            heklang::Value::Sealed {
                field: "reporter".to_owned(),
                subject: "Owner".to_owned(),
                id: "10".to_owned(),
                content: content.into(),
            },
        );
        support::seed_event_value(&store, &project, &ctx(), keystore, &event);
    }

    let stub = Arc::new(hekla::effect::StubHttpClient::ok());
    let harness = Boot::new(fixture_dir("tickets"))
        .data_dir(data.path())
        .with_master_key()
        .http(stub.clone())
        .start();
    let shared = harness.rt.effect("NotifyOwner").unwrap();
    support::wait_until("the lane to wedge", || shared.consecutive_failures() >= 2);

    let stuck = shared.stuck_lanes();
    assert_eq!(stuck.total, 1, "one ticket, one wedged lane");
    let lane = &stuck.listed[0];
    assert!(
        lane.error.contains("Reporter") && lane.error.contains("not the JSON"),
        "the message should name the type the declaration promised and what it got: {}",
        lane.error
    );
    assert!(
        lane.attempt >= 2,
        "a mismatch is retried rather than skipped, so the count climbs: {}",
        lane.attempt
    );
    assert_eq!(
        stub.call_count(),
        0,
        "`reveal` fails before the call, so nothing left the process"
    );
    assert_eq!(
        harness.rt.log_head(),
        1,
        "the seeded event and nothing else: a skip would have let the arm finish"
    );

    // The operator diagnosing this is the one who needs to see the bad bytes, so the
    // admin surface reads them where `reveal` refuses to. `unsealed_json` falls back to
    // the raw text for a `Json` field that does not parse, which is the same answer
    // `a_json_column_that_does_not_parse_reads_back_as_its_raw_text` pins for a column.
    let (status, event) = support::get(&harness.app(), "/admin/events/1").await;
    assert_eq!(status, 200, "one unreadable field must not fail the page");
    assert_eq!(
        field(&event["data"], "reporter"),
        &json!("Ada Lovelace"),
        "the text is shown as itself rather than hidden behind the parse that refused it"
    );
    assert_eq!(event["subjects"]["reporter"]["state"], "decrypted");

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
    // The sealed columns beside it, which `update` rewrites whether or not it names
    // them: heklang reads the row, replaces `title`, and puts the whole thing back. So
    // every one of these makes a round trip through `row` and `put` that a `put`-only
    // test never takes, and each is where a value that could not survive being read back
    // at its declared shape would land as something else.
    assert_eq!(
        field(&row, "contacted_at"),
        &json!("2023-11-15T09:20:00Z"),
        "a sealed Timestamp survives an `update` as the moment it was"
    );
    assert_eq!(
        field(&row, "reporter"),
        &reporter(),
        "and a sealed record as the record, not as its document quoted into a string"
    );
    assert_eq!(field(&row, "watchers"), &watchers());
    assert_eq!(field(&row, "labels"), &labels());
    assert_eq!(field(&row, "contact"), &json!("ada@example.com"));

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

    harness.rt.keystore().unwrap().erase("Owner", "10").unwrap();

    let row = read_row(&harness, "Tickets", "Ticket", ALICE, position).expect("the row remains");
    absent(&row, "contact");
    absent(&row, "contacted_at");
    // A sealed composite goes whole or not at all. There is no half of a record to read
    // back: the key opens the document or it opens nothing, and the column is then the
    // same absence an optional scalar's is.
    absent(&row, "reporter");
    assert_eq!(
        field(&row, "budget"),
        &json!("900.00"),
        "the organisation's figure is scoped to a different key and is untouched"
    );
    assert_eq!(
        field(&row, "watchers"),
        &watchers(),
        "and so are the organisation's composites"
    );
    assert_eq!(field(&row, "labels"), &labels());
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
    harness.rt.keystore().unwrap().erase("Org", "1").unwrap();
    let row = read_row(&harness, "Tickets", "Ticket", ALICE, position).unwrap();
    absent(&row, "budget");
    absent(&row, "watchers");
    absent(&row, "labels");
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
    let stub = Arc::new(hekla::effect::StubHttpClient::ok());
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
    // The half `reveal` could not do before: a composite comes back as what it was
    // sealed from, so the handler reaches `.name` and `.badge` rather than holding the
    // text of a document it has no way to open.
    assert_eq!(
        field(&sent, "reporter"),
        &json!("Ada Lovelace"),
        "a revealed record is a record, and a field of it is readable"
    );
    assert_eq!(
        field(&sent, "badge"),
        &json!("0042"),
        "including the leaf that looks like a number"
    );
    assert_eq!(
        field(&sent, "watchers"),
        &watchers(),
        "and a revealed list is a list"
    );
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
        let stub = Arc::new(hekla::effect::StubHttpClient::ok());
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
        report.skipped.total(),
        0,
        "nothing was skipped, so the count above is coverage and not luck"
    );
}

/// An invocation that journaled nothing is replayed against its empty journal, not
/// skipped, and counts as covered.
///
/// This used to be pinned the other way, on the theory that an empty journal was
/// indistinguishable from one the retention sweeper had reclaimed. It is not:
/// `sweep_effect_journal` deletes the `effect_invocation` row and `effect_journal`
/// cascades off it, so a reclaimed invocation never reaches the sweep at all. An empty
/// journal on a row that is still here is a run that genuinely called nothing, and
/// replaying it (empty against empty) checks something real: that the handler still
/// takes the branch that calls nothing.
#[test]
fn an_invocation_that_called_nothing_is_replayed_against_its_empty_journal() {
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
    assert_eq!(
        report.invocations_checked, 1,
        "a run that called nothing is still a run whose calls can be compared"
    );
    assert_eq!(report.skipped.total(), 0);
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

    harness.rt.keystore().unwrap().erase("Owner", "10").unwrap();

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

/// A named fold is a declaration a deployment records, and the two things a reader needs
/// off that row are its kind and where it is written.
///
/// Both were wrong when folds first reached hekla: `/system` served `"kind": "fold"`
/// against an OpenAPI enum of eight kinds that did not list it, so a strict generated
/// client rejected the response, and `loader::module_paths` had no arm for a fold, so the
/// row said `null` where the file is and `hekla plan` could report a changed fold without
/// naming one.
#[tokio::test]
async fn a_named_fold_is_recorded_with_its_kind_and_its_file() {
    let harness = boot();
    let (status, schema) = support::get(&harness.app(), "/admin/schema").await;
    assert_eq!(status, 200);

    let rows: Vec<(&str, &str, Option<&str>)> = schema["declarations"]
        .as_array()
        .expect("declarations")
        .iter()
        .map(|row| {
            (
                row["kind"].as_str().unwrap(),
                row["name"].as_str().unwrap(),
                row["module"].as_str(),
            )
        })
        .collect();

    assert!(
        rows.contains(&("fold", "TicketOpened", Some("lib/folds.hk"))),
        "a fold is recorded by kind and by the file it is declared in: {rows:?}"
    );
    assert!(rows.contains(&("fold", "TicketClosed", Some("lib/folds.hk"))));

    // The document has to admit every kind a row can carry, or a generated client
    // rejects a response this deployment really serves. Read from the served document
    // rather than from `DECLARATION_KINDS`, so this is the two halves meeting.
    let (status, document) = support::get(&harness.app(), "/openapi.json").await;
    assert_eq!(status, 200);
    let kinds: Vec<&str> =
        document["components"]["schemas"]["DeclarationSummary"]["properties"]["kind"]["enum"]
            .as_array()
            .expect("the kind enum")
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
    for (kind, name, _) in &rows {
        assert!(
            kinds.contains(kind),
            "`{name}` is recorded as `{kind}`, which the document does not declare: {kinds:?}"
        );
    }

    harness.shutdown();
}

/// **The derived tables in a running deployment**, which is what every slice before this
/// one was building toward: a command appends, the store's thread follows the log, and the
/// column holds what folding it returns.
#[tokio::test]
async fn the_derived_tables_follow_the_log_in_a_running_deployment() {
    let harness = boot();
    open(&harness, UUID_A, 1, 10, None);
    quiesce(&harness);

    // One head for the whole store and nothing behind it, which is what makes it
    // servable: every column of every table is at one log position.
    let (status, system) = support::get(&harness.app(), "/status").await;
    assert_eq!(status, 200);
    assert_eq!(system["tables"]["behind"], 0, "{}", system["tables"]);
    assert_eq!(system["tables"]["serving"], true, "{}", system["tables"]);
    assert_eq!(
        system["tables"]["head"], system["log_head"],
        "the store has followed the log to its head: {}",
        system["tables"]
    );
    assert_eq!(system["tables"]["running"], true);
    assert_eq!(system["tables"]["last_error"], serde_json::Value::Null);

    let path = harness.data_dir().join(hekla::fold_runtime::FILE_NAME);
    let conn = rusqlite::Connection::open(&path).expect("the store exists");
    // A hash of the key signature, so found through the fold rather than spelled.
    let ticket = hekla::tables::TableDef::all(harness.rt.program())
        .into_iter()
        .find(|table| {
            table
                .columns
                .iter()
                .any(|column| column.fold == "TicketOpened")
        })
        .expect("a table keeps `TicketOpened`")
        .name;
    let opened: i64 = conn
        .query_row(
            &format!("SELECT \"ticket_opened\" FROM \"{ticket}\" WHERE \"ticket\" = ?1"),
            [UUID_A],
            |row| row.get(0),
        )
        .expect("the row the command's event minted");
    assert_eq!(opened, 1, "`TicketOpened` folded to true for this ticket");
    let closed: i64 = conn
        .query_row(
            &format!("SELECT \"ticket_closed\" FROM \"{ticket}\" WHERE \"ticket\" = ?1"),
            [UUID_A],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        closed, 0,
        "no arm of `TicketClosed` named this key, so the column holds its seed"
    );

    harness.shutdown();
}

/// The derived tables over HTTP: one row by its whole key, its collections and sealed
/// columns included and opened, and the position it was read at.
///
/// The three sealed columns are the owner's and the row is the ticket's, so this is the
/// case a cell has to carry its own subject id for. Erasing the owner then makes them read
/// absent, with no pass over the store.
#[tokio::test]
async fn a_derived_row_is_served_with_its_sealed_columns_opened() {
    let harness = boot();
    let position = open(&harness, UUID_A, 1, 10, Some("ada@example.com"));
    let app = harness.app();
    const FOLDS: &str = "TicketOpened,TicketClosed,TicketContact,TicketContactedAt,TicketReporter";

    let (status, body) = support::get(
        &app,
        &format!("/row?folds={FOLDS}&ticket_id={UUID_A}&after={position}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let row = &body["item"];
    assert_eq!(
        row["ticket_id"], UUID_A,
        "the key, named as the first fold names it"
    );
    assert_eq!(row["TicketOpened"], true);
    assert_eq!(row["TicketClosed"], false);
    assert_eq!(row["TicketContact"], "ada@example.com");
    // RFC 3339 in a column, as a projector serves it, though the log holds micros.
    assert_eq!(row["TicketContactedAt"], "2023-11-15T09:20:00Z");
    // From the side table, joined back into the one row.
    assert_eq!(row["TicketReporter"], reporter());
    assert!(body["position"].as_u64().unwrap() >= position);
    assert!(
        row.get("TicketOpenedAt").is_none(),
        "only the folds listed: {row}"
    );

    harness.rt.keystore().unwrap().erase("Owner", "10").unwrap();
    let (status, body) =
        support::get(&app, &format!("/row?folds={FOLDS}&ticket_id={UUID_A}")).await;
    assert_eq!(status, 200, "{body}");
    for gone in ["TicketContact", "TicketContactedAt", "TicketReporter"] {
        absent(&body["item"], gone);
    }
    assert_eq!(
        body["item"]["TicketOpened"], true,
        "the rest of the row stands"
    );

    harness.shutdown();
}

/// A scan filters on what an index serves and pages over the whole key; a key no event
/// named is a 404, and a filter no index serves is a 400 rather than a table scan.
#[tokio::test]
async fn a_derived_table_is_scanned_by_the_leading_parts_of_its_key() {
    let harness = boot();
    let mut position = 0;
    for (ticket, org, owner) in [(ALICE, 1, 10), (BOB, 1, 11), (CAROL, 2, 10)] {
        position = position.max(open(&harness, ticket, org, owner, None));
    }
    let app = harness.app();

    let (status, body) = support::get(
        &app,
        &format!("/rows?folds=OwnerTickets&org_id=1&after={position}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let owners: Vec<i64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["owner_id"].as_i64().unwrap())
        .collect();
    assert_eq!(owners, vec![10, 11], "org 1's rows, in key order");

    // A page of one, then the rest from its cursor.
    let (_, first) = support::get(&app, "/rows?folds=OwnerTickets&limit=1").await;
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_str().expect("another page follows");
    let (_, rest) = support::get(
        &app,
        &format!("/rows?folds=OwnerTickets&limit=10&cursor={cursor}"),
    )
    .await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 2);
    assert_eq!(rest["next_cursor"], Value::Null);
    assert_eq!(first["items"][0]["OwnerTickets"], 1);

    let (status, body) = support::get(&app, "/rows?folds=OwnerTickets&owner_id=10").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "unindexed_filter");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("(org_id)")),
        "the refusal names a set of parameters that serves: {body}"
    );

    let (status, body) = support::get(&app, "/row?folds=OwnerTickets&org_id=9&owner_id=9").await;
    assert_eq!(status, 404, "{body}");
    let (status, body) = support::get(&app, "/row?folds=OwnerTickets&org_id=1").await;
    assert_eq!(status, 400, "{body}");

    // A table is storage, never an address: its id is no fold, and folds of two keys
    // share no row.
    let ticket = hekla::tables::TableDef::all(harness.rt.program())
        .into_iter()
        .find(|table| {
            table
                .columns
                .iter()
                .any(|column| column.fold == "TicketOpened")
        })
        .expect("a table keeps `TicketOpened`")
        .name;
    for uri in [
        format!("/rows?folds={ticket}"),
        format!("/rows?folds={ticket}_side"),
    ] {
        let (status, body) = support::get(&app, &uri).await;
        assert_eq!(status, 404, "a table id is not a fold: {uri}: {body}");
    }
    let (status, body) = support::get(&app, "/rows?folds=OwnerTickets,TicketOpened").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid_input", "{body}");

    harness.shutdown();
}

/// A request a derived table cannot answer is a 400 that says why, never a 500 and never
/// a row that silently is not there.
#[tokio::test]
async fn a_malformed_derived_read_is_refused_with_its_reason() {
    let harness = boot();
    let position = open(&harness, UUID_A, 1, 10, None);
    let app = harness.app();
    let (status, _) =
        support::get(&app, &format!("/rows?folds=TicketOpened&after={position}")).await;
    assert_eq!(status, 200);

    for (uri, why) in [
        (
            "/row?folds=TicketOpened&ticket_id=not-a-uuid",
            "a malformed key part",
        ),
        (
            "/rows?folds=OwnerTickets&org_id=one",
            "a key part of the wrong type",
        ),
        (
            "/rows?folds=OwnerTickets&cursor=bm90IGEgY3Vyc29y",
            "a cursor this server did not issue",
        ),
        (
            "/row?folds=TicketOpened&ticket_id=x&limit=1",
            "a parameter a row read does not take",
        ),
        (
            "/rows?folds=OwnerTickets&org=1",
            "a layout part name rather than the fold's parameter",
        ),
        ("/rows?org_id=1", "no folds listed"),
        (
            "/rows?folds=OwnerTickets,OwnerTickets",
            "a fold listed twice",
        ),
    ] {
        let (status, body) = support::get(&app, uri).await;
        assert_eq!(status, 400, "{why}: {body}");
        assert_eq!(body["error"]["code"], "invalid_input", "{why}: {body}");
    }
    for (uri, status, code) in [
        ("/rows?folds=NoSuchFold", 404, "not_found"),
        ("/rows?folds=OrgTicketsSince", 400, "not_stored"),
    ] {
        let (got, body) = support::get(&app, uri).await;
        assert_eq!(got, status, "{uri}: {body}");
        assert_eq!(body["error"]["code"], code, "{uri}: {body}");
    }

    // An edited cursor: right table, right length, a value of the wrong type.
    let table = hekla::tables::TableDef::all(harness.rt.program())
        .into_iter()
        .find(|table| {
            table
                .columns
                .iter()
                .any(|column| column.fold == "OwnerTickets")
        })
        .expect("a table keeps `OwnerTickets`")
        .name;
    let cursor = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        format!(r#"{{"t":"{table}","k":["x","y"]}}"#),
    );
    let (status, body) =
        support::get(&app, &format!("/rows?folds=OwnerTickets&cursor={cursor}")).await;
    assert_eq!(status, 400, "{body}");

    harness.shutdown();
}

/// A fold's parameter is a query parameter of every read of it, so one named like a
/// control parameter could never be given. `hekla check` says so rather than the first
/// read. A key part `layout` derives is no longer public, so it may be named anything.
#[test]
fn a_fold_parameter_named_like_a_control_parameter_is_refused() {
    let project = support::write_project(&[
        ("hekla.toml", ""),
        ("events/page.hk", "event @page.viewed { limit: Int }\n"),
        (
            "lib/folds.hk",
            "fold Views(limit: Int) -> views: Int = 0 {\n  on @page.viewed(limit) => views + 1\n}\n",
        ),
    ]);
    let loaded = hekla::loader::LoadedProject::load(project.path());
    assert!(loaded.has_errors(), "{:?}", loaded.findings);
    assert!(
        loaded
            .findings
            .iter()
            .any(|finding| format!("{finding:?}").contains("control parameter")),
        "{:?}",
        loaded.findings
    );

    // `limit_id` keys a part named `limit`, which nothing reads by that name.
    let project = support::write_project(&[
        ("hekla.toml", ""),
        ("events/page.hk", "event @page.viewed { limit_id: Int }\n"),
        (
            "lib/folds.hk",
            "fold Views(limit_id: Int) -> views: Int = 0 {\n  on @page.viewed(limit_id) => views + 1\n}\n",
        ),
    ]);
    let loaded = hekla::loader::LoadedProject::load(project.path());
    assert!(!loaded.has_errors(), "{:?}", loaded.findings);
}

/// One fold by name: its arguments named as the fold declares them, its value alone, and
/// the reasons it has none.
#[tokio::test]
async fn a_fold_is_read_by_name_with_its_own_parameter_names() {
    let harness = boot();
    let mut position = open(&harness, UUID_A, 1, 10, Some("ada@example.com"));
    position = position.max(open(&harness, BOB, 1, 10, None));
    let app = harness.app();

    let (status, body) = support::get(
        &app,
        &format!("/folds/OwnerTickets?org_id=1&owner_id=10&after={position}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["value"], 2);
    assert!(body["position"].as_u64().unwrap() >= position);

    // Sealed, keyed on the ticket, and opened on the way out.
    let (status, body) =
        support::get(&app, &format!("/folds/TicketContact?ticket_id={UUID_A}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["value"], "ada@example.com");
    // Query-only, so no command may fold it, and this is where it is read.
    let (status, body) =
        support::get(&app, &format!("/folds/TicketOpenedAt?ticket_id={UUID_A}")).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["value"].as_str().is_some_and(|at| at.ends_with('Z')),
        "{body}"
    );
    // A fold answering `none` is a value, and it is null.
    let (_, body) = support::get(&app, &format!("/folds/TicketContact?ticket_id={BOB}")).await;
    assert_eq!(body["value"], Value::Null, "{body}");

    for (uri, status, code) in [
        // The table's part name is not the fold's parameter name.
        ("/folds/OwnerTickets?org=1&owner=10", 400, "invalid_input"),
        ("/folds/OwnerTickets?org_id=1", 400, "invalid_input"),
        (
            "/folds/OwnerTickets?org_id=1&owner_id=ten",
            400,
            "invalid_input",
        ),
        ("/folds/OwnerTickets?org_id=9&owner_id=9", 404, "not_found"),
        ("/folds/NoSuchFold", 404, "not_found"),
        ("/folds/OrgTicketsSince?since=0&org_id=1", 400, "not_stored"),
    ] {
        let (got, body) = support::get(&app, uri).await;
        assert_eq!(got, status, "{uri}: {body}");
        assert_eq!(body["error"]["code"], code, "{uri}: {body}");
    }
    let (_, body) = support::get(&app, "/folds/OrgTicketsSince?since=0&org_id=1").await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`since`"),
        "the refusal names the parameter no arm matches: {body}"
    );

    harness.shutdown();
}

/// `/folds/{Fold}` reads `after` and `timeout_ms` as its wait, so a fold parameter of either
/// name could never be passed. Its key part is named otherwise (`after: Org` is the part
/// `org`), which is why the check is on the fold's parameter and not on the part.
#[test]
fn a_fold_parameter_named_like_the_wait_is_refused() {
    let project = support::write_project(&[
        ("hekla.toml", ""),
        (
            "events/page.hk",
            "subject Org(Int)\nevent @page.viewed { after: Org }\n",
        ),
        (
            "lib/folds.hk",
            "fold Views(after: Org) -> views: Int = 0 {\n  on @page.viewed(after) => views + 1\n}\n",
        ),
    ]);
    let loaded = hekla::loader::LoadedProject::load(project.path());
    assert!(
        loaded
            .findings
            .iter()
            .any(|finding| format!("{finding:?}").contains("control parameter")),
        "{:?}",
        loaded.findings
    );
}

/// A key part computed from the record: each ticket counts towards the month it is due
/// in, which no event carries. Read by row, by scan and by name like any other column.
#[tokio::test]
async fn a_computed_key_part_buckets_rows_by_what_the_record_holds() {
    let harness = boot();
    let mut position = 0;
    // 2023-11-14, 2023-11-14 and 2023-12-14.
    for (ticket, due_at) in [
        (ALICE, 1_700_000_000_000_000i64),
        (BOB, 1_700_000_000_000_000),
        (CAROL, 1_702_592_000_000_000),
    ] {
        let mut body = open_body(ticket, 1, 10, None);
        body["due_at"] = due_at.into();
        let result = harness
            .rt
            .execute("OpenTicket", body, &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
        position = position.max(result.body["positions"]["last"].as_u64().unwrap());
    }
    let app = harness.app();

    let (status, body) = support::get(
        &app,
        &format!("/rows?folds=OrgTicketsDue&org_id=1&after={position}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let months: Vec<(String, i64)> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["month"].as_str().unwrap().to_owned(),
                row["OrgTicketsDue"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        months,
        vec![("2023-11".to_owned(), 2), ("2023-12".to_owned(), 1)]
    );

    let (status, body) =
        support::get(&app, "/row?folds=OrgTicketsDue&org_id=1&month=2023-12").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["item"]["OrgTicketsDue"], 1);

    let (status, body) = support::get(&app, "/folds/OrgTicketsDue?org_id=1&month=2023-11").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["value"], 2);
    let (status, body) = support::get(&app, "/folds/OrgTicketsDue?org_id=1&month=2024-01").await;
    assert_eq!(
        status, 404,
        "a month no ticket is due in has no row: {body}"
    );

    harness.shutdown();
}

/// A range on the key part after the pinned ones, the way a month of a tenant's history is
/// read: `org=1&month.gte=..&month.lt=..`. A range anywhere an index would not seek to is a
/// 400, as an unindexed equality is.
#[tokio::test]
async fn a_derived_table_is_ranged_on_the_part_after_its_pinned_ones() {
    let harness = boot();
    let mut position = 0;
    // Due in 2023-11, 2023-11 and 2023-12, all in org 1.
    for (ticket, due_at) in [
        (ALICE, 1_700_000_000_000_000i64),
        (BOB, 1_700_000_000_000_000),
        (CAROL, 1_702_592_000_000_000),
    ] {
        let mut body = open_body(ticket, 1, 10, None);
        body["due_at"] = due_at.into();
        let result = harness
            .rt
            .execute("OpenTicket", body, &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
        position = position.max(result.body["positions"]["last"].as_u64().unwrap());
    }
    let app = harness.app();
    let months = |body: &Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["month"].as_str().unwrap().to_owned())
            .collect()
    };

    let (status, body) = support::get(
        &app,
        &format!("/rows?folds=OrgTicketsDue&org_id=1&month.gte=2023-12&after={position}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(months(&body), vec!["2023-12"]);

    let (_, body) = support::get(
        &app,
        "/rows?folds=OrgTicketsDue&org_id=1&month.gte=2023-11&month.lt=2023-12",
    )
    .await;
    assert_eq!(
        months(&body),
        vec!["2023-11"],
        "both ends, the upper exclusive"
    );

    let (status, body) = support::get(&app, "/rows?folds=OrgTicketsDue&org_id.gte=1").await;
    assert_eq!(
        status, 200,
        "the first part needs nothing pinned before it: {body}"
    );
    assert_eq!(months(&body), vec!["2023-11", "2023-12"]);

    let (status, body) = support::get(&app, "/rows?folds=OrgTicketsDue&month.gte=2023-11").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "unindexed_filter");

    for (uri, why) in [
        (
            "/rows?folds=OrgTicketsDue&org_id=1&month.after=2023-11",
            "not an operator",
        ),
        (
            "/rows?folds=OrgTicketsDue&org_id=1&month.gte=2023-11&month.gt=2023-10",
            "two lower bounds",
        ),
        (
            "/rows?folds=OrgTicketsDue&org_id=1&org_id.gte=1",
            "pinned and bounded at once",
        ),
        (
            "/rows?folds=OrgTicketsDue&org_id.gte=1&month.gte=2023-11",
            "two parts bounded",
        ),
    ] {
        let (status, body) = support::get(&app, uri).await;
        assert_eq!(status, 400, "{why}: {body}");
        assert_eq!(body["error"]["code"], "invalid_input", "{why}: {body}");
    }

    harness.shutdown();
}

/// A fold's `@index` orders its table: `order_by=-OwnerTickets` with the part before it
/// pinned, or a whole-table order. A row without a value is left out, pages follow the
/// order, and an ordering nothing declares is a 400 rather than a sort.
#[tokio::test]
async fn a_derived_table_is_ordered_by_a_column_its_fold_indexes() {
    let harness = boot();
    let position = owners_and_closes(&harness);
    let app = harness.app();

    let (status, body) = support::get(
        &app,
        &format!("/rows?folds=OwnerTickets&org_id=1&order_by=-OwnerTickets&after={position}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        column(&body, "owner_id"),
        vec![json!(10), json!(12), json!(11)]
    );

    // A page at a time, the cursor carrying the order.
    let (_, first) = support::get(
        &app,
        "/rows?folds=OwnerTickets&org_id=1&order_by=OwnerTickets&limit=2",
    )
    .await;
    assert_eq!(column(&first, "owner_id"), vec![json!(11), json!(12)]);
    let cursor = first["next_cursor"].as_str().expect("another page follows");
    let (status, rest) = support::get(
        &app,
        &format!("/rows?folds=OwnerTickets&org_id=1&order_by=OwnerTickets&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, 200, "{rest}");
    assert_eq!(column(&rest, "owner_id"), vec![json!(10)]);
    assert_eq!(rest["next_cursor"], Value::Null);
    let (status, body) = support::get(
        &app,
        &format!("/rows?folds=OwnerTickets&org_id=1&order_by=-OwnerTickets&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, 400, "a cursor read under another order: {body}");

    // The whole table, the open tickets left out.
    let (status, body) =
        support::get(&app, "/rows?folds=TicketClosedAt&order_by=-TicketClosedAt").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        column(&body, "ticket_id"),
        vec![json!(ALICE), json!(BOB)],
        "closed last first, and no row for a ticket still open"
    );

    for (uri, code, why) in [
        (
            "/rows?folds=OwnerTickets&org_id=1&order_by=owner_id",
            "invalid_input",
            "not a declared ordering",
        ),
        (
            "/rows?folds=OwnerTickets&order_by=OwnerTickets",
            "unindexed_filter",
            "the leading part not pinned",
        ),
        (
            "/rows?folds=OwnerTickets&org_id=1&owner_id=10&order_by=OwnerTickets",
            "unindexed_filter",
            "more pinned than lead the index",
        ),
        (
            "/rows?folds=OwnerTickets&org_id=1&owner_id.gte=10&order_by=OwnerTickets",
            "unindexed_filter",
            "a range beside an order",
        ),
    ] {
        let (status, body) = support::get(&app, uri).await;
        assert_eq!(status, 400, "{why}: {body}");
        assert_eq!(body["error"]["code"], code, "{why}: {body}");
    }

    harness.shutdown();
}

/// The store holds a `Money` as its decimal string, so an `@index` on one would sort
/// `"10.00"` under `"9.00"`. `hekla check` says so where it is declared.
#[test]
fn an_index_on_a_column_the_store_cannot_order_is_refused() {
    let project = support::write_project(&[
        ("hekla.toml", ""),
        (
            "events/order.hk",
            "event @order.placed { order_id: Int, total: Money(2) }\n",
        ),
        (
            "lib/folds.hk",
            "fold OrderTotal(order_id: Int) -> Money(2) @index = 0 {\n  on @order.placed(order_id) { total } => total\n}\n",
        ),
    ]);
    let loaded = hekla::loader::LoadedProject::load(project.path());
    assert!(
        loaded
            .findings
            .iter()
            .any(|finding| format!("{finding:?}").contains("does not hold in an order")),
        "{:?}",
        loaded.findings
    );
}

/// A row's history, newest first: only the records that moved a column, each with its value
/// before and after and the event that did it, sealed columns opened as a row read opens
/// them. A fold's history by its own parameters, and a range arm's refused.
#[tokio::test]
async fn a_derived_row_and_a_fold_have_a_history() {
    let harness = boot();
    let opened = open(&harness, ALICE, 1, 10, Some("ada@example.com"));
    let second = open(&harness, BOB, 1, 10, None);
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
    let closed = result.body["positions"]["last"].as_u64().unwrap();
    let app = harness.app();
    let positions = |body: &Value| -> Vec<u64> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["position"].as_u64().unwrap())
            .collect()
    };

    let (status, body) = support::get(
        &app,
        &format!("/row/history?folds=TicketOpened,TicketClosed,TicketContact&ticket_id={ALICE}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(positions(&body), vec![closed, opened], "newest first");
    let newest = &body["items"][0];
    assert_eq!(newest["event"]["type"], "ticket.closed");
    assert!(
        newest["event"].get("data").is_none(),
        "the payload is `/admin/events`'s, not a public route's: {newest}"
    );
    assert!(
        newest["event"]["at"]
            .as_str()
            .is_some_and(|at| at.ends_with('Z'))
    );
    assert_eq!(
        newest["changed"],
        json!({ "TicketClosed": { "before": false, "after": true } })
    );
    let first = &body["items"][1]["changed"];
    assert_eq!(
        first["TicketOpened"],
        json!({ "before": false, "after": true })
    );
    assert_eq!(
        first["TicketContact"],
        json!({ "before": null, "after": "ada@example.com" }),
        "a sealed column opened, as a row read opens it"
    );
    assert!(body["position"].as_u64().unwrap() >= closed);

    // Two tickets, one change: the second ran the arm and moved nothing.
    let (status, body) = support::get(&app, "/folds/OrgHasTickets/history?org_id=1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(positions(&body), vec![opened]);

    // A seed that is not `none`, written back: no change.
    let (status, body) = support::get(&app, "/folds/OrgPlan/history?org_id=1").await;
    assert_eq!(status, 200, "{body}");
    assert!(positions(&body).is_empty(), "{body}");

    // A page at a time, and a lower bound.
    let (_, first) = support::get(
        &app,
        "/folds/OwnerTickets/history?org_id=1&owner_id=10&limit=1",
    )
    .await;
    assert_eq!(positions(&first), vec![second]);
    assert_eq!(
        first["items"][0]["changed"]["OwnerTickets"],
        json!({ "before": 1, "after": 2 })
    );
    let before = first["next_before"].as_u64().expect("another page follows");
    let (_, rest) = support::get(
        &app,
        &format!("/folds/OwnerTickets/history?org_id=1&owner_id=10&before={before}"),
    )
    .await;
    assert_eq!(positions(&rest), vec![opened]);
    assert_eq!(rest["next_before"], Value::Null);
    let (_, since) = support::get(
        &app,
        &format!("/folds/OwnerTickets/history?org_id=1&owner_id=10&since={second}"),
    )
    .await;
    assert_eq!(positions(&since), vec![second]);

    for (uri, code) in [
        (
            "/folds/OwnerAnyClosed/history?org_id=1&owner_id=10",
            "no_history",
        ),
        (
            "/row/history?folds=OwnerTickets,OwnerAnyClosed&org_id=1&owner_id=10",
            "no_history",
        ),
        (
            "/row/history?folds=OwnerTickets&org_id=1&owner_id=10&columns=OwnerTickets",
            "invalid_input",
        ),
        (
            "/folds/OwnerTickets/history?org_id=1&owner_id=10&columns=x",
            "invalid_input",
        ),
        ("/folds/OwnerTickets/history?org_id=1", "invalid_input"),
        (
            "/folds/OwnerTickets/history?org_id=1&owner_id=10&before=x",
            "invalid_input",
        ),
    ] {
        let (status, body) = support::get(&app, uri).await;
        assert_eq!(status, 400, "{uri}: {body}");
        assert_eq!(body["error"]["code"], code, "{uri}: {body}");
    }
    let (_, body) = support::get(&app, "/folds/TicketsByOrgCloses/history?closes=0").await;
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("through `OwnerOrgCloses`")),
        "the refusal names the fold that ranges: {body}"
    );
    // A row's history is of the folds listed and no other, so a fold of the same key with
    // no history is simply not asked for.
    let (status, body) =
        support::get(&app, "/row/history?folds=OwnerTickets&org_id=1&owner_id=10").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(positions(&body), vec![second, opened]);
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["changed"]
                .as_object()
                .is_some_and(|changed| changed.keys().eq(["OwnerTickets"]))),
        "{body}"
    );

    harness.shutdown();
}

/// The tickets fixture with its `hekla.toml`'s `[folds]` table replaced, so `serve_sealed`
/// lists only `served` and every other setting, retention included, stays the fixture's.
fn tickets_serving(served: &[&str]) -> tempfile::TempDir {
    fn copy(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    copy(&fixture_dir("tickets"), dir.path());
    let listed: Vec<String> = served.iter().map(|fold| format!("\"{fold}\"")).collect();
    let config = dir.path().join("hekla.toml");
    let fixture = fs::read_to_string(&config).unwrap();
    let (rest, _) = fixture
        .split_once("[folds]")
        .expect("the fixture lists its served seals under [folds], last");
    fs::write(
        &config,
        format!("{rest}[folds]\nserve_sealed = [{}]\n", listed.join(", ")),
    )
    .unwrap();
    dir
}

/// A sealed fold is stored and folded, and no read route serves it until
/// `[folds] serve_sealed` names it: a 404 that says why on every route, and no path in the
/// document. Its unsealed neighbours, and the key, are served as before.
#[tokio::test]
async fn a_sealed_fold_is_not_served_unless_the_project_names_it() {
    let project = tickets_serving(&["TicketContact"]);
    let harness = Boot::new(project.path())
        .with_master_key()
        .http_status(200)
        .start();
    let position = open(&harness, UUID_A, 1, 10, Some("ada@example.com"));
    let app = harness.app();

    let (status, body) = support::get(
        &app,
        &format!("/row?folds=TicketOpened,TicketContact&ticket_id={UUID_A}&after={position}"),
    )
    .await;
    assert_eq!(status, 200, "a named seal is served: {body}");
    assert_eq!(body["item"]["TicketContact"], "ada@example.com");
    assert_eq!(body["item"]["ticket_id"], UUID_A, "and the key with it");

    for path in [
        format!("/folds/TicketContactedAt?ticket_id={UUID_A}"),
        format!("/folds/TicketContactedAt/history?ticket_id={UUID_A}"),
        format!("/row?folds=TicketOpened,TicketContactedAt&ticket_id={UUID_A}"),
        format!("/row?folds=TicketReporter,TicketOpened&ticket_id={UUID_A}"),
        "/rows?folds=TicketContactedAt".to_owned(),
        format!("/row/history?folds=TicketOpened,TicketContactedAt&ticket_id={UUID_A}"),
    ] {
        let (status, body) = support::get(&app, &path).await;
        assert_eq!(status, 404, "{path}: {body}");
        assert_eq!(body["error"]["code"], "sealed", "{path}: {body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("serve_sealed"),
            "{path}: the refusal names the fix: {body}"
        );
    }

    let (status, doc) = support::get(&app, "/openapi.json").await;
    assert_eq!(status, 200);
    let paths = doc["paths"].as_object().unwrap();
    assert!(paths.contains_key("/folds/TicketContact"));
    for unserved in ["/folds/TicketContactedAt", "/folds/TicketReporter"] {
        assert!(
            !paths.contains_key(unserved),
            "{unserved} is not documented"
        );
    }

    let (status, admin) = support::get(&app, "/admin/tables").await;
    assert_eq!(status, 200);
    let listed: Vec<(String, bool)> = admin["tables"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|table| table["folds"].as_array().unwrap().iter())
        .filter(|fold| fold["fold"].as_str().unwrap().starts_with("TicketContact"))
        .map(|fold| {
            (
                fold["fold"].as_str().unwrap().to_owned(),
                fold["served"].as_bool().unwrap(),
            )
        })
        .collect();
    assert!(
        listed.contains(&("TicketContact".to_owned(), true)),
        "{listed:?}"
    );
    assert!(
        listed.contains(&("TicketContactedAt".to_owned(), false)),
        "the console still lists it: {listed:?}"
    );

    harness.shutdown();
}

/// Owner 10 opens three tickets in org 1, owner 11 one and owner 12 two; BOB's ticket is
/// closed and then ALICE's, and the rest stay open. The position the last of it is at.
fn owners_and_closes(harness: &Harness) -> u64 {
    let mut position = 0;
    for (ticket, owner) in [
        (ALICE, 10),
        (BOB, 10),
        (CAROL, 10),
        (UUID_A, 11),
        (support::UUID_B, 12),
        (support::UUID_C, 12),
    ] {
        position = position.max(open(harness, ticket, 1, owner, None));
    }
    for ticket in [BOB, ALICE] {
        let result = harness
            .rt
            .execute(
                "CloseTicket",
                json!({ "ticket_id": ticket, "org_id": 1 }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
        position = position.max(result.body["positions"]["last"].as_u64().unwrap());
    }
    position
}

/// One column of a page, row by row.
fn column(body: &Value, name: &str) -> Vec<Value> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[name].clone())
        .collect()
}

/// A scan filters on an indexed fold's own value, by the fold's name: equal to one value or
/// within a range, which is a seek along the index that orders by it, so the page comes
/// back in that order and `order_by` only turns it round. Anything an index does not serve
/// is still a 400.
#[tokio::test]
async fn a_scan_filters_on_the_value_of_a_fold_it_could_order_by() {
    let harness = boot();
    let position = owners_and_closes(&harness);
    let app = harness.app();

    let (status, body) = support::get(
        &app,
        &format!("/rows?folds=OwnerTickets&org_id=1&OwnerTickets.gte=2&after={position}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(column(&body, "owner_id"), vec![json!(12), json!(10)]);

    let (status, body) = support::get(
        &app,
        "/rows?folds=OwnerTickets&org_id=1&OwnerTickets.lte=2&order_by=-OwnerTickets",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        column(&body, "owner_id"),
        vec![json!(12), json!(11)],
        "a deadline queue: due by now, latest first"
    );

    let (status, body) =
        support::get(&app, "/rows?folds=OwnerTickets&org_id=1&OwnerTickets=1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(column(&body, "owner_id"), vec![json!(11)]);

    // A page at a time, the cursor carrying the range's order.
    let (_, first) = support::get(
        &app,
        "/rows?folds=OwnerTickets&org_id=1&OwnerTickets.gte=1&limit=2",
    )
    .await;
    assert_eq!(column(&first, "owner_id"), vec![json!(11), json!(12)]);
    let cursor = first["next_cursor"].as_str().expect("another page follows");
    let (status, rest) = support::get(
        &app,
        &format!("/rows?folds=OwnerTickets&org_id=1&OwnerTickets.gte=1&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, 200, "{rest}");
    assert_eq!(column(&rest, "owner_id"), vec![json!(10)]);

    // A timestamp range on a whole-table index.
    let (status, body) = support::get(
        &app,
        "/rows?folds=TicketClosedAt&TicketClosedAt.gte=2000-01-01T00:00:00Z",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(column(&body, "ticket_id"), vec![json!(BOB), json!(ALICE)]);

    for (uri, code, why) in [
        (
            "/rows?folds=OwnerTickets&OwnerTickets.gte=1",
            "unindexed_filter",
            "the lead not pinned",
        ),
        (
            "/rows?folds=OwnerTickets&org_id=1&OwnerAnyClosed=true",
            "unindexed_filter",
            "a fold no index holds",
        ),
        (
            "/rows?folds=OwnerTickets&org_id=1&TicketOpened=true",
            "invalid_input",
            "a fold of other rows",
        ),
        (
            "/rows?folds=OwnerTickets&org_id=1&OwnerTickets=1&OwnerTickets.gte=1",
            "invalid_input",
            "pinned and bounded at once",
        ),
        (
            "/rows?folds=OwnerTickets&org_id=1&OwnerTickets.gte=x",
            "invalid_input",
            "a bound of the wrong type",
        ),
    ] {
        let (status, body) = support::get(&app, uri).await;
        assert_eq!(status, 400, "{why}: {body}");
        assert_eq!(body["error"]["code"], code, "{why}: {body}");
    }

    harness.shutdown();
}

/// A scan cannot filter on a sealed fold the project does not serve, since a filter would
/// learn its value one guess at a time, nor on a collection, which no index orders.
#[tokio::test]
async fn a_scan_does_not_filter_on_a_sealed_fold_or_a_collection() {
    let project = tickets_serving(&[]);
    let harness = Boot::new(project.path())
        .with_master_key()
        .http_status(200)
        .start();
    let app = harness.app();
    for (uri, code) in [
        (
            "/rows?folds=TicketClosedAt&TicketContact=ada@example.com",
            "sealed",
        ),
        (
            "/rows?folds=TicketClosedAt&TicketReporter=x",
            "unindexed_filter",
        ),
    ] {
        let (status, body) = support::get(&app, uri).await;
        assert_eq!(status, 400, "{uri}: {body}");
        assert_eq!(body["error"]["code"], code, "{uri}: {body}");
    }
    harness.shutdown();
}
