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
            (
                "contacted_at",
                "owner_id",
                "10",
                "1700040000000000".to_owned(),
            ),
            ("reporter", "owner_id", "10", older.to_string()),
            ("watchers", "org_id", "1", watchers().to_string()),
            ("labels", "org_id", "1", labels().to_string()),
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
            .encrypt_subject("owner_id", "10", "reporter", "Ada Lovelace")
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
                subject: "owner_id".to_owned(),
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

    harness
        .rt
        .keystore()
        .unwrap()
        .erase("owner_id", "10")
        .unwrap();

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
    harness.rt.keystore().unwrap().erase("org_id", "1").unwrap();
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
