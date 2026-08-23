//! Subject-scoped encryption end to end: a command emits an event with a
//! subject-encrypted field, so the field is stored as ciphertext (in the tag index,
//! the payload, and the read model), the opaque handle keeps plaintext out of a
//! projector, and the command response never reports the encrypted value.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use kiln::crypto::{KeyStore, MasterKeys};
use kiln::effect::StubHttpClient;
use kiln::opdb::OpDb;
use kiln::read_api;
use kiln::read_model::ReadModel;
use kiln::runtime::{ExecResult, Runtime};
use serde_json::{Value, json};

mod support;

use support::{
    ALICE, BOB, Boot, Harness, MASTER_KEY, ORDER_EVENTS, PLACE_ORDER, UUID_A as ORDER, UUID_B,
    UUID_C, accounts_project, ctx, orders_project, orders_project_with, orders_with_notify_effect,
    place_order, read_row, wait_position, wait_until, write_project,
};

/// The extra event the read-modify-write projector also sources.
const TOUCHED_EVENT: &str = r#"
touched = event(type = "order.touched", fields = {"order_id": uuid()})
"#;

/// The common shape: a subject-using project, the fixed master key, and an HTTP
/// stub that answers 200.
fn boot(project_dir: &Path) -> Harness {
    Boot::new(project_dir)
        .http_status(200)
        .with_master_key()
        .start()
}

#[test]
fn boot_without_a_master_key_fails_when_a_project_uses_subjects() {
    let dir = orders_project();
    let err = match Boot::new(dir.path()).http_status(200).try_start() {
        Ok(_) => panic!("expected boot to fail without a master key"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("KILN_MASTER_KEY"),
        "expected a master-key boot error, got: {message}"
    );
}

#[test]
fn a_project_without_subjects_boots_without_a_key() {
    // The example users project has no subjects, so no master key is needed.
    let harness = Boot::example()
        .http_status(200)
        .try_start()
        .expect("boots without a master key");
    harness.shutdown();
}

#[test]
fn the_command_response_omits_the_subject_field_tag() {
    let dir = orders_project();
    let harness = boot(dir.path());
    let body = place_order(&harness.rt, ORDER, 42, "alice@example.com");

    let tags = body["events"][0]["tags"].as_array().unwrap();
    let tag_strings: Vec<&str> = tags.iter().map(|t| t.as_str().unwrap()).collect();
    // Plaintext tags for the non-subject indexed fields are reported.
    assert!(tag_strings.contains(&format!("order_id:{ORDER}").as_str()));
    assert!(tag_strings.contains(&"customer_id:42"));
    // The subject field never appears, in plaintext or ciphertext.
    assert!(
        !tag_strings.iter().any(|t| t.starts_with("email")),
        "the subject field leaked into the response: {tag_strings:?}"
    );
    assert!(
        !tag_strings.iter().any(|t| t.contains("alice@example.com")),
        "plaintext email leaked into the response: {tag_strings:?}"
    );

    harness.shutdown();
}

#[test]
fn a_projector_cannot_turn_a_handle_back_into_plaintext() {
    // The handle is opaque: string-concatenating it (an attempt to derive a
    // plaintext value) is not a supported operation, so the projector's handle()
    // errors and the projector reports failed rather than storing a derivative.
    let dir = orders_project_with(&[(
        "projectors/leaky.star",
        r#"
load("events/order.star", "order_placed")

leaky = entity(key = "order_id", fields = {"order_id": uuid(), "domain": str()})

source = [order_placed()]

def handle(event):
    # Deriving plaintext from the handle: not allowed, so this errors.
    return [put(leaky, {"order_id": event.data.order_id, "domain": event.data.email + "!"})]
"#,
    )]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    wait_until("the projector to fail rather than derive plaintext", || {
        harness.rt.projector("leaky").unwrap().failed()
    });

    harness.shutdown();
}

#[test]
fn the_projector_stores_ciphertext_for_the_subject_column() {
    let dir = orders_project();
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    wait_until("the orders projector to apply the event", || {
        harness.rt.projector("orders").unwrap().position() >= 1
    });

    // Read the read model directly, bypassing the read API's decrypt: the stored
    // email column is ciphertext, never the plaintext.
    let shared = harness.rt.projector("orders").unwrap();
    let model = ReadModel::open_readonly(&shared.db_path).unwrap();
    let entity = shared.entities.iter().find(|e| e.name == "orders").unwrap();
    let row = model.get(entity, ORDER).unwrap().unwrap();
    let stored = row["email"].as_str().unwrap();
    assert_ne!(
        stored, "alice@example.com",
        "the read model must not hold plaintext"
    );
    assert!(!stored.is_empty());
    assert_eq!(row["customer_id"].as_i64(), Some(42));

    harness.shutdown();
}

#[test]
fn the_read_api_decrypts_the_subject_column() {
    let dir = orders_project();
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    let row = read_row(&harness, "orders", "orders", ORDER, 1).expect("a row");
    // The read API decrypts on the way out: the caller sees plaintext, not ciphertext.
    assert_eq!(row["email"], "alice@example.com");
    assert_eq!(row["customer_id"].as_i64(), Some(42));

    harness.shutdown();
}

#[test]
fn erasing_a_subject_shreds_the_read_model_and_the_log() {
    let dir = orders_project();
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    // Before erasure the read API returns the plaintext.
    let row = read_row(&harness, "orders", "orders", ORDER, 1).expect("a row");
    assert_eq!(row["email"], "alice@example.com");

    // Erase customer 42: one key delete.
    let erased = harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();
    assert!(erased);

    // The read model now reads the email as absent (its ciphertext is undecryptable);
    // the order itself and the plaintext customer id remain.
    let row = read_row(&harness, "orders", "orders", ORDER, 1).expect("the order row still exists");
    assert!(
        row.get("email").is_none(),
        "erased email must be absent: {row}"
    );
    assert_eq!(row["customer_id"].as_i64(), Some(42));
    assert_eq!(row["order_id"], ORDER);

    harness.shutdown();
}

#[test]
fn a_handle_into_a_plaintext_column_is_rejected() {
    // Storing a subject handle into a non-subject column would file unreadable
    // ciphertext the read API never decrypts; the projector must fail instead.
    let dir = orders_project_with(&[(
        "projectors/leak.star",
        r#"
load("events/order.star", "order_placed")

leak = entity(key = "order_id", fields = {"order_id": uuid(), "note": str()})

source = [order_placed()]

def handle(event):
    # `note` is a plaintext column; storing the encrypted handle there is rejected.
    return [put(leak, {"order_id": event.data.order_id, "note": event.data.email})]
"#,
    )]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    wait_until(
        "storing a handle in a plaintext column to fail the projector",
        || harness.rt.projector("leak").unwrap().failed(),
    );

    harness.shutdown();
}

#[test]
fn re_emitting_a_folded_subject_value_is_rejected() {
    // A fold sees a subject field as a handle; carrying it into an emit would
    // double-encrypt. The constructor rejects a handle argument.
    let dir = orders_project_with(&[(
        "commands/copy-order.star",
        r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = uint())

# Fold this customer's orders, capturing the (encrypted) email handle into state,
# then try to re-emit it: the constructor must reject the handle.
def query(input):
    return order_placed(customer_id = input.customer_id)

initial = {"email": None}

def fold(state, event):
    return dict(state, email = event.data.email)

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = state["email"],
    )
"#,
    )]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let body = json!({ "order_id": UUID_B, "customer_id": 42 });
    let failed = harness
        .rt
        .execute("copy-order", body, &ctx(), None)
        .is_err();
    assert!(failed, "re-emitting a folded subject handle must fail");
    harness.shutdown();
}

#[test]
fn a_read_does_not_resurrect_an_erased_subject_key() {
    let dir = orders_project();
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let ks = harness.rt.keystore().unwrap();

    // The key exists after the order.
    assert!(
        ks.encrypt_subject_existing("customer_id", "42", "email", "x")
            .unwrap()
            .is_some()
    );
    ks.erase("customer_id", "42").unwrap();
    assert!(
        ks.encrypt_subject_existing("customer_id", "42", "email", "x")
            .unwrap()
            .is_none()
    );
    // A read of the row (the read/query path) must not recreate the key.
    let _ = read_row(&harness, "orders", "orders", ORDER, 1);
    assert!(
        ks.encrypt_subject_existing("customer_id", "42", "email", "x")
            .unwrap()
            .is_none(),
        "the read path must not resurrect an erased subject key"
    );
    harness.shutdown();
}

#[test]
fn fresh_and_recovered_responses_match_for_a_subject_event() {
    // Idempotent replay must return a byte-identical body, including for an event
    // with a subject field (whose tag the response suppresses on both paths).
    let dir = orders_project();
    let harness = boot(dir.path());
    let body = json!({ "order_id": ORDER, "customer_id": 42, "email": "alice@example.com" });

    let fresh = harness
        .rt
        .execute("place-order", body.clone(), &ctx(), Some("idem-1"))
        .unwrap();
    assert_eq!(fresh.status, 200);
    let recovered = harness
        .rt
        .execute("place-order", body, &ctx(), Some("idem-1"))
        .unwrap();
    assert_eq!(recovered.status, 200);
    assert_eq!(
        fresh.body, recovered.body,
        "fresh and recovered responses must be identical"
    );
    harness.shutdown();
}

#[test]
fn an_effect_reveals_the_plaintext_to_act_on_it() {
    let dir = orders_with_notify_effect();
    let stub = Arc::new(StubHttpClient::status(200));
    let harness = Boot::new(dir.path())
        .http(stub.clone())
        .with_master_key()
        .start();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    wait_until("the effect to post", || !stub.calls().is_empty());
    let call = stub.calls().into_iter().next().expect("a posted call");
    let body: Value = serde_json::from_slice(&call.body.expect("a body")).unwrap();
    // reveal() gave the effect the real plaintext to send.
    assert_eq!(body["to"], "alice@example.com");

    harness.shutdown();
}

#[test]
fn a_reveal_on_an_erased_subject_skips_terminally_without_wedging() {
    let dir = orders_with_notify_effect();
    // A persistent 5xx wedges the effect on http.post, which runs after reveal() has
    // already succeeded. That gives a window to erase the customer; each retry re-runs
    // handle from the top, so once the key is gone reveal() fails terminally.
    let harness = Boot::new(dir.path())
        .http_status(500)
        .with_master_key()
        .start();

    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let effect = harness.rt.effect("notify").unwrap().clone();

    // The 5xx wedges the effect: reveal() succeeded this attempt, http.post did not.
    wait_until("the effect to wedge on the 5xx", || {
        effect.consecutive_failures() > 0
    });

    // Erase the customer. The next retry's reveal() can no longer decrypt.
    harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();

    // The terminal skip advances past the position instead of wedging forever.
    wait_until("the terminal skip to advance the effect", || {
        effect.terminal_skips() > 0
    });
    assert_eq!(
        effect.consecutive_failures(),
        0,
        "a terminal skip is not a wedge: consecutive_failures must be unambiguous"
    );
    assert_eq!(
        effect.last_error(),
        None,
        "abandoning a wedged position clears its wedge error"
    );
    assert_eq!(effect.terminal_skips(), 1, "the skip is counted separately");
    assert!(
        effect
            .last_terminal_error()
            .expect("a terminal skip records its message")
            .contains("erased"),
        "the terminal skip records why the position was abandoned"
    );
    // The watermark advances just after the skip is recorded (at the end of the batch).
    wait_until("the effect to advance past the erased event", || {
        effect.position() >= 1
    });

    harness.shutdown();
}

#[test]
fn concurrent_plaintext_uniqueness_admits_only_one() {
    // Control: a plaintext-tag uniqueness boundary under the same concurrent load, to
    // confirm the DCB boundary itself catches concurrent first-writers.
    let dir = write_project(&[
        (
            "events/thing.star",
            r#"registered = event(type = "thing.registered", fields = {"id": uuid(), "email": str(max_length = 100)})
"#,
        ),
        (
            "commands/register.star",
            r#"
load("events/thing.star", "registered")

input = schema(id = uuid(), email = str())

def query(input):
    return registered(email = input.email)

initial = {"taken": False}

def fold(state, event):
    return dict(state, taken = True)

def handle(input, state):
    if state["taken"]:
        return reject("email_taken", "taken")
    return registered(id = input.id, email = input.email)
"#,
        ),
    ]);
    let harness = Boot::new(dir.path()).http_status(200).start();
    let place = |id: &'static str| {
        let rt = harness.rt.clone();
        thread::spawn(move || {
            let body = json!({ "id": id, "email": "race@example.com" });
            rt.execute("register", body, &ctx(), None).unwrap().status
        })
    };
    let a = place(ORDER);
    let b = place(UUID_B);
    let mut statuses = [a.join().unwrap(), b.join().unwrap()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 422], "plaintext control; got {statuses:?}");
    harness.shutdown();
}

#[test]
fn concurrent_first_use_of_a_unique_value_admits_only_one() {
    // Two concurrent first-ever writes of the same unique email (distinct accounts).
    // The global-key boundary tag is deterministic even on first use, so the writer
    // that appends second conflicts and is rejected rather than both committing.
    let dir = accounts_project();
    let harness = boot(dir.path());

    let register = |account_id: &'static str| {
        let rt = harness.rt.clone();
        thread::spawn(move || {
            let body = json!({ "account_id": account_id, "email": "race@example.com" });
            rt.execute("register-account", body, &ctx(), None)
                .unwrap()
                .status
        })
    };
    let a = register(ORDER);
    let b = register(UUID_B);
    let mut statuses = [a.join().unwrap(), b.join().unwrap()];
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        [200, 422],
        "exactly one first-writer should win; got {statuses:?}"
    );

    harness.shutdown();
}

#[test]
fn a_projector_can_read_modify_write_a_subject_column() {
    // get() returns subject columns as handles, so a read-modify-write projector can
    // re-store its own row without the encrypted column being rejected.
    let events = format!("{ORDER_EVENTS}{TOUCHED_EVENT}");
    let dir = write_project(&[
        ("events/order.star", events.as_str()),
        ("commands/place-order.star", PLACE_ORDER),
        (
            "commands/touch-order.star",
            r#"
load("events/order.star", "touched")

input = schema(order_id = uuid())

def handle(input, state):
    return touched(order_id = input.order_id)
"#,
        ),
        (
            "projectors/orders.star",
            r#"
load("events/order.star", "order_placed", "touched")

orders = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 100),
        "touches": int(),
    },
)

source = [order_placed(), touched()]

def handle(event):
    if event.type == "order.placed":
        return [put(orders, {
            "order_id": event.data.order_id,
            "customer_id": event.data.customer_id,
            "email": event.data.email,
            "touches": 0,
        })]
    # Read-modify-write: re-store the whole row (carrying the encrypted email handle)
    # with an incremented counter.
    row = get(orders, event.data.order_id)
    if row == None:
        return []
    return [put(orders, {
        "order_id": row["order_id"],
        "customer_id": row["customer_id"],
        "email": row["email"],
        "touches": row["touches"] + 1,
    })]
"#,
        ),
    ]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    harness
        .rt
        .execute("touch-order", json!({ "order_id": ORDER }), &ctx(), None)
        .unwrap();

    wait_until("both events to project", || {
        harness.rt.projector("orders").unwrap().position() >= 2
    });
    assert!(
        !harness.rt.projector("orders").unwrap().failed(),
        "the read-modify-write projector must not fail"
    );
    let row = read_row(&harness, "orders", "orders", ORDER, 2).expect("a row");
    // The re-stored encrypted column still decrypts, and the counter advanced.
    assert_eq!(row["email"], "alice@example.com");
    assert_eq!(row["touches"].as_i64(), Some(1));

    harness.shutdown();
}

#[test]
fn a_stale_row_after_erase_and_reuse_reads_as_absent_not_error() {
    // Erase a customer, then a new order for that same customer mints a fresh key.
    // The old order's ciphertext (under the deleted key) must read as absent, not
    // fail the whole scan.
    let dir = orders_project();
    let harness = boot(dir.path());
    let first = "aaaaaaaa-0000-0000-0000-000000000001";
    let second = "aaaaaaaa-0000-0000-0000-000000000002";
    place_order(&harness.rt, first, 42, "old@example.com");
    wait_until("the first order to project", || {
        harness.rt.projector("orders").unwrap().position() >= 1
    });
    harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();
    // A new order for customer 42 mints a fresh key.
    place_order(&harness.rt, second, 42, "new@example.com");

    // The first order's email is unreadable (its key is gone); the second's is fine.
    let old = read_row(&harness, "orders", "orders", first, 2).expect("first row");
    assert!(
        old.get("email").is_none(),
        "stale email must read as absent: {old}"
    );
    let new = read_row(&harness, "orders", "orders", second, 2).expect("second row");
    assert_eq!(new["email"], "new@example.com");

    harness.shutdown();
}

#[test]
fn unique_enforces_global_uniqueness_across_subjects() {
    let dir = accounts_project();
    let harness = boot(dir.path());

    let register = |account_id: &str, email: &str| {
        let body = json!({ "account_id": account_id, "email": email });
        harness
            .rt
            .execute("register-account", body, &ctx(), None)
            .unwrap()
    };

    // First account with the email succeeds.
    let first = register(ALICE, "shared@example.com");
    assert_eq!(first.status, 200, "first registration: {:?}", first.body);
    // A second, different account with the same email is rejected: the query's
    // global-key tag matched the first account's, across their distinct subject keys.
    let second = register(BOB, "shared@example.com");
    assert_eq!(second.status, 422, "second registration should be rejected");
    assert_eq!(second.body["error"]["code"], "email_taken");
    // A different email on the second account is fine.
    let other = register(BOB, "other@example.com");
    assert_eq!(other.status, 200, "distinct email: {:?}", other.body);

    harness.shutdown();
}

// --- scanning a page of subject rows --------------------------------------

/// Three orders across two customers, so one page mixes subjects.
const ORDER_1: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const ORDER_2: &str = "aaaaaaaa-0000-0000-0000-000000000002";
const ORDER_3: &str = "aaaaaaaa-0000-0000-0000-000000000003";

/// One page of an entity read through the read API's `scan`, which shares a single
/// row decryptor (and its secret cache) across every row of the page.
fn scan_rows(harness: &Harness, projector: &str, entity: &str, after: u64) -> Vec<Value> {
    wait_position(&harness.rt, projector, after);
    let shared = harness.rt.projector(projector).unwrap();
    let entity_def = shared
        .entities
        .iter()
        .find(|candidate| candidate.name == entity)
        .unwrap();
    read_api::scan(
        &shared.db_path,
        entity_def,
        None,
        None,
        50,
        harness.rt.keystore(),
    )
    .unwrap()
    .items
}

/// The scanned row whose key column equals `key`.
fn row_for<'a>(rows: &'a [Value], key: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["order_id"] == key)
        .unwrap_or_else(|| panic!("no row for {key} in {rows:?}"))
}

#[test]
fn a_scan_decrypts_each_row_under_its_own_subject_key() {
    // One `RowDecryptor` serves the whole page, caching secrets by subject. A
    // mis-keyed cache would decrypt one customer's ciphertext under another's key.
    let dir = orders_project();
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER_1, 42, "alice@example.com");
    place_order(&harness.rt, ORDER_2, 43, "bob@example.com");
    place_order(&harness.rt, ORDER_3, 42, "alice+two@example.com");

    let rows = scan_rows(&harness, "orders", "orders", 3);
    assert_eq!(rows.len(), 3, "the page holds every order: {rows:?}");
    assert_eq!(row_for(&rows, ORDER_1)["email"], "alice@example.com");
    assert_eq!(row_for(&rows, ORDER_2)["email"], "bob@example.com");
    assert_eq!(row_for(&rows, ORDER_3)["email"], "alice+two@example.com");
    assert_eq!(row_for(&rows, ORDER_2)["customer_id"].as_u64(), Some(43));

    // Erasing one customer blanks only that customer's column, in every row of the
    // page, and leaves the rows themselves (and their plaintext columns) intact.
    harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();
    let rows = scan_rows(&harness, "orders", "orders", 3);
    assert_eq!(rows.len(), 3, "an erasure removes columns, never rows");
    for key in [ORDER_1, ORDER_3] {
        let row = row_for(&rows, key);
        assert!(row.get("email").is_none(), "erased email survived: {row}");
        assert_eq!(row["customer_id"].as_u64(), Some(42));
        assert_eq!(row["order_id"], key);
    }
    let survivor = row_for(&rows, ORDER_2);
    assert_eq!(
        survivor["email"], "bob@example.com",
        "another subject's row must still decrypt: {survivor}"
    );

    harness.shutdown();
}

// --- typed subject columns ------------------------------------------------

/// An event whose subject-encrypted fields are not all text: the read API has to
/// re-type each decrypted string back to its declared kind.
const TYPED_EVENTS: &str = r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 100),
        "order_total": money(subject = "customer_id"),
        "loyalty_points": int(subject = "customer_id"),
    },
)
"#;

const TYPED_PLACE_ORDER: &str = r#"
load("events/order.star", "order_placed")

input = schema(
    order_id = uuid(),
    customer_id = uint(),
    email = str(),
    order_total = money(),
    loyalty_points = int(),
)

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
        order_total = input.order_total,
        loyalty_points = input.loyalty_points,
    )
"#;

const TYPED_PROJECTOR: &str = r#"
load("events/order.star", "order_placed")

orders = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 100),
        "order_total": money(subject = "customer_id"),
        "loyalty_points": int(subject = "customer_id"),
    },
)

source = [order_placed()]

def handle(event):
    return [put(orders, {
        "order_id": event.data.order_id,
        "customer_id": event.data.customer_id,
        "email": event.data.email,
        "order_total": event.data.order_total,
        "loyalty_points": event.data.loyalty_points,
    })]
"#;

#[test]
fn a_scanned_page_decrypts_typed_subject_columns_and_skips_erased_rows() {
    let dir = write_project(&[
        ("events/order.star", TYPED_EVENTS),
        ("commands/place-order.star", TYPED_PLACE_ORDER),
        ("projectors/orders.star", TYPED_PROJECTOR),
    ]);
    let harness = boot(dir.path());
    let place = |order_id: &str, customer_id: u64, total: &str, points: i64| {
        let body = json!({
            "order_id": order_id,
            "customer_id": customer_id,
            "email": "buyer@example.com",
            "order_total": total,
            "loyalty_points": points,
        });
        let result = harness
            .rt
            .execute("place-order", body, &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "place-order failed: {:?}", result.body);
    };
    place(ORDER_1, 42, "19.99", 250);
    place(ORDER_2, 99, "7.50", -3);
    place(ORDER_3, 42, "100.00", 0);

    let rows = scan_rows(&harness, "orders", "orders", 3);
    let survivor = row_for(&rows, ORDER_2);
    // Money stays a decimal string (its wire form); an integer comes back a number.
    assert_eq!(survivor["order_total"], Value::String("7.50".to_owned()));
    assert_eq!(survivor["loyalty_points"].as_i64(), Some(-3));
    assert!(
        survivor["loyalty_points"].is_number(),
        "an encrypted i64 must re-type as a number: {survivor}"
    );
    assert_eq!(survivor["email"], "buyer@example.com");

    harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();
    let rows = scan_rows(&harness, "orders", "orders", 3);
    assert_eq!(rows.len(), 3, "an erased subject drops columns, not rows");
    for key in [ORDER_1, ORDER_3] {
        let row = row_for(&rows, key);
        for column in ["email", "order_total", "loyalty_points"] {
            assert!(
                row.get(column).is_none(),
                "erased column `{column}` survived: {row}"
            );
        }
        assert_eq!(row["customer_id"].as_u64(), Some(42));
    }
    let survivor = row_for(&rows, ORDER_2);
    assert_eq!(survivor["order_total"], Value::String("7.50".to_owned()));
    assert_eq!(survivor["loyalty_points"].as_i64(), Some(-3));

    harness.shutdown();
}

// --- scoped subject queries -----------------------------------------------

/// A boundary that constrains the subject id *and* the subject-encrypted field, so
/// the filter value is encrypted under that customer's existing key and has to match
/// the ciphertext tag the emit path stored.
const REORDER_COMMAND: &str = r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = uint(), email = str())

def query(input):
    return order_placed(customer_id = input.customer_id, email = input.email)

initial = {"seen": False}

def fold(state, event):
    return dict(state, seen = True)

def handle(input, state):
    if state["seen"]:
        return reject("already_ordered", "that customer already ordered under that email")
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#;

fn reorder(rt: &Runtime, order_id: &str, customer_id: u64, email: &str) -> ExecResult {
    let body = json!({ "order_id": order_id, "customer_id": customer_id, "email": email });
    rt.execute("reorder", body, &ctx(), None).unwrap()
}

#[test]
fn a_scoped_subject_query_matches_only_its_own_subject() {
    let dir = orders_project_with(&[("commands/reorder.star", REORDER_COMMAND)]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER_1, 42, "alice@example.com");
    // Give customer 99 a key of its own, so the cross-subject case below is a
    // key-that-exists-but-differs case, not a missing-key one.
    place_order(&harness.rt, ORDER_2, 99, "carol@example.com");

    // Same subject, same value: the encrypted filter matches the stored tag.
    let same = reorder(&harness.rt, ORDER_3, 42, "alice@example.com");
    assert_eq!(same.status, 422, "the boundary must match: {:?}", same.body);
    assert_eq!(same.body["error"]["code"], "already_ordered");

    // Same value, a different subject: encrypted under 99's key, so a different
    // ciphertext, so no match.
    let cross = reorder(&harness.rt, ORDER_3, 99, "alice@example.com");
    assert_eq!(
        cross.status, 200,
        "one subject's tag must not match another's: {:?}",
        cross.body
    );
    // Same subject, a different value: also no match.
    let other_value = reorder(&harness.rt, UUID_B, 42, "carol@example.com");
    assert_eq!(
        other_value.status, 200,
        "a different plaintext must not match: {:?}",
        other_value.body
    );
    // The event the cross-subject reorder appended is now itself matchable, which
    // proves the emit and the query lower the same subject to the same tag.
    let repeat = reorder(&harness.rt, UUID_C, 99, "alice@example.com");
    assert_eq!(repeat.status, 422, "the appended tag must be matchable");
    assert_eq!(repeat.body["error"]["code"], "already_ordered");

    harness.shutdown();
}

#[test]
fn a_query_over_an_erased_subject_matches_nothing_and_still_appends() {
    // With the subject key gone the clause cannot be lowered, so it is made
    // deliberately unmatchable. Dropping it instead would widen the boundary to every
    // `order.placed` (another subject's events folding into this command's state);
    // erroring instead would 500 every command touching an erased customer.
    let dir = orders_project_with(&[("commands/reorder.star", REORDER_COMMAND)]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER_1, 42, "alice@example.com");
    // A second customer's event stays in the log: if the erased clause degraded to
    // "every order.placed", the fold would see this one too.
    place_order(&harness.rt, ORDER_2, 99, "carol@example.com");

    let blocked = reorder(&harness.rt, ORDER_3, 42, "alice@example.com");
    assert_eq!(blocked.status, 422, "control: the guard fires while keyed");

    harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();

    let after = reorder(&harness.rt, ORDER_3, 42, "alice@example.com");
    assert_eq!(
        after.status, 200,
        "an erased subject's clause must match nothing, not error or widen: {:?}",
        after.body
    );

    harness.shutdown();
}

/// A boundaried command that never emits, so nothing on the append path can mint a
/// subject key and the query is the only suspect.
const CHECK_ORDER_COMMAND: &str = r#"
load("events/order.star", "order_placed")

input = schema(customer_id = uint(), email = str())

def query(input):
    return order_placed(customer_id = input.customer_id, email = input.email)

initial = {"found": False}

def fold(state, event):
    return dict(state, found = True)

def handle(input, state):
    return []
"#;

#[test]
fn a_query_scoped_to_an_erased_subject_matches_nothing_and_mints_no_key() {
    let dir = orders_project_with(&[("commands/check-order.star", CHECK_ORDER_COMMAND)]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER_1, 42, "alice@example.com");
    let ks = harness.rt.keystore().unwrap();
    ks.erase("customer_id", "42").unwrap();

    let body = json!({ "customer_id": 42, "email": "alice@example.com" });
    let result = harness
        .rt
        .execute("check-order", body, &ctx(), None)
        .unwrap();
    assert_eq!(
        result.status, 200,
        "a query over an erased subject must not error: {:?}",
        result.body
    );
    assert!(
        ks.encrypt_subject_existing("customer_id", "42", "email", "x")
            .unwrap()
            .is_none(),
        "lowering a query must not resurrect an erased subject key"
    );

    harness.shutdown();
}

// --- uniqueness across erasure --------------------------------------------

#[test]
fn unique_still_rejects_after_the_subject_is_erased() {
    // The `unique` tag is minted under the never-erased global key precisely so
    // uniqueness still fires once the subject's own key is shredded. If it were
    // subject-scoped, erasing an account would silently re-open its email for reuse.
    let dir = accounts_project();
    let harness = boot(dir.path());
    let register = |account_id: &str, email: &str| {
        let body = json!({ "account_id": account_id, "email": email });
        harness
            .rt
            .execute("register-account", body, &ctx(), None)
            .unwrap()
    };

    let first = register(ALICE, "shared@example.com");
    assert_eq!(first.status, 200, "first registration: {:?}", first.body);

    let ks = harness.rt.keystore().unwrap();
    assert!(
        ks.erase("account_id", ALICE).unwrap(),
        "the subject key must exist to be erased"
    );
    assert!(
        ks.encrypt_subject_existing("account_id", ALICE, "email", "shared@example.com")
            .unwrap()
            .is_none(),
        "control: the erased account's scoped key is really gone"
    );

    let reuse = register(BOB, "shared@example.com");
    assert_eq!(
        reuse.status, 422,
        "erasing a subject must not re-open its unique value: {:?}",
        reuse.body
    );
    assert_eq!(reuse.body["error"]["code"], "email_taken");

    harness.shutdown();
}

// --- master key rotation --------------------------------------------------

/// The master a rotation moves the store onto.
const NEXT_MASTER_KEY: [u8; 32] = [0x22; 32];
/// A master that never wrapped anything here, for the boot-guard case.
const WRONG_MASTER_KEY: [u8; 32] = [0x99; 32];

#[test]
fn rotating_the_master_survives_a_restart_and_a_wrong_master_fails_boot() {
    let dir = orders_project();
    let data = tempfile::tempdir().unwrap();
    let boot_at = |master: MasterKeys| {
        Boot::new(dir.path())
            .data_dir(data.path())
            .http_status(200)
            .master(master)
            .try_start()
    };

    let harness = boot_at(MasterKeys::new(MASTER_KEY, vec![])).expect("the first boot");
    place_order(&harness.rt, ORDER_1, 42, "alice@example.com");
    let row = read_row(&harness, "orders", "orders", ORDER_1, 1).expect("a row");
    assert_eq!(row["email"], "alice@example.com");
    harness.shutdown();

    // Rotate offline, keeping the old master so the stored wrapping can be unwrapped.
    {
        let opdb = Arc::new(Mutex::new(
            OpDb::open(&data.path().join("kiln.db")).unwrap(),
        ));
        let keystore = KeyStore::new(opdb, MasterKeys::new(NEXT_MASTER_KEY, vec![MASTER_KEY]));
        assert_eq!(
            keystore.rotate().unwrap(),
            1,
            "the customer's subject key is rewrapped"
        );
        assert_eq!(keystore.rotate().unwrap(), 0, "a second pass is a no-op");
    }

    // The old master is gone now: only the rewrapping keeps the data readable.
    let harness = boot_at(MasterKeys::new(NEXT_MASTER_KEY, vec![])).expect("the rotated boot");
    let row = read_row(&harness, "orders", "orders", ORDER_1, 1).expect("a row after rotation");
    assert_eq!(
        row["email"], "alice@example.com",
        "rotation rewraps the key without touching the ciphertext"
    );
    harness.shutdown();

    // A master that never wrapped this data must fail fast at boot rather than blank
    // every personal column at read time.
    let err = match boot_at(MasterKeys::new(WRONG_MASTER_KEY, vec![])) {
        Ok(_) => panic!("booting under a master that wrapped nothing must fail"),
        Err(err) => format!("{err:#}"),
    };
    assert!(
        err.contains("KILN_MASTER_KEY"),
        "the boot guard names the key to set: {err}"
    );
}

// --- misfiled handles -----------------------------------------------------

/// `order.placed` carries two ids, so a projector can file the customer-scoped email
/// under the wrong one.
const TWO_ID_EVENTS: &str = r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "shop_id": uint(),
        "email": str(subject = "customer_id", max_length = 100),
    },
)
"#;

const TWO_ID_PLACE_ORDER: &str = r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = uint(), shop_id = uint(), email = str())

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        shop_id = input.shop_id,
        email = input.email,
    )
"#;

/// The row's `customer_id` is the shop's id, so the handle's subject value disagrees
/// with the row it would be filed under.
const WRONG_ID_PROJECTOR: &str = r#"
load("events/order.star", "order_placed")

misfiled = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 100),
    },
)

source = [order_placed()]

def handle(event):
    return [put(misfiled, {
        "order_id": event.data.order_id,
        "customer_id": event.data.shop_id,
        "email": event.data.email,
    })]
"#;

/// The handle is encrypted for `email`, but stored into a column named `note`.
const WRONG_FIELD_PROJECTOR: &str = r#"
load("events/order.star", "order_placed")

misnamed = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "note": str(subject = "customer_id", max_length = 100),
    },
)

source = [order_placed()]

def handle(event):
    return [put(misnamed, {
        "order_id": event.data.order_id,
        "customer_id": event.data.customer_id,
        "note": event.data.email,
    })]
"#;

/// The column is scoped to `shop_id`, but the handle is scoped to `customer_id`.
const WRONG_SCOPE_PROJECTOR: &str = r#"
load("events/order.star", "order_placed")

misscoped = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "shop_id": uint(),
        "email": str(subject = "shop_id", max_length = 100),
    },
)

source = [order_placed()]

def handle(event):
    return [put(misscoped, {
        "order_id": event.data.order_id,
        "shop_id": event.data.shop_id,
        "email": event.data.email,
    })]
"#;

#[test]
fn a_handle_filed_under_the_wrong_subject_id_is_rejected() {
    // Each of these would file one subject's ciphertext under a row that claims a
    // different subject, a field, or a different scope: erasing the real subject would
    // leave a permanently undecryptable row, and erasing the claimed one would fail to
    // shred anything. All three must fail the projector instead of storing the row.
    let dir = write_project(&[
        ("events/order.star", TWO_ID_EVENTS),
        ("commands/place-order.star", TWO_ID_PLACE_ORDER),
        ("projectors/wrong-id.star", WRONG_ID_PROJECTOR),
        ("projectors/wrong-field.star", WRONG_FIELD_PROJECTOR),
        ("projectors/wrong-scope.star", WRONG_SCOPE_PROJECTOR),
    ]);
    let harness = boot(dir.path());
    let body = json!({
        "order_id": ORDER_1,
        "customer_id": 42,
        "shop_id": 7,
        "email": "alice@example.com",
    });
    let result = harness
        .rt
        .execute("place-order", body, &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 200, "place-order failed: {:?}", result.body);

    for (projector, needle) in [
        ("wrong-id", "holds data for"),
        ("wrong-field", "encrypted for field `email`"),
        ("wrong-scope", "is scoped to subject `shop_id`"),
    ] {
        wait_until(&format!("projector `{projector}` to fail"), || {
            harness.rt.projector(projector).unwrap().failed()
        });
        let message = harness
            .rt
            .projector(projector)
            .unwrap()
            .last_error()
            .expect("a failed projector records its error");
        assert!(
            message.contains(needle),
            "projector `{projector}` failed for the wrong reason: {message}"
        );
        assert_eq!(
            harness.rt.projector(projector).unwrap().position(),
            0,
            "a rejected row must not be checkpointed"
        );
    }

    harness.shutdown();
}
