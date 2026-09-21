//! Subject-scoped encryption end to end: a command emits an event with a
//! subject-encrypted field, so the field is stored as ciphertext (in the tag index,
//! the payload, and the read model), sealing keeps plaintext out of a projector, and
//! the command response never reports the encrypted value.
//!
//! Seven of the Starlark suite's cases are gone rather than ported, in three groups.
//!
//! **`unique` is deleted**, so `unique_enforces_global_uniqueness_across_subjects` and
//! the plaintext control beside it have nothing left to test that the ordinary
//! boundary below does not. The feature existed to make one email match across every
//! account through a never-erased global key, and it required an equality on sealed
//! content, which heklang rejects (rule 12) because comparing two ciphertexts leaks
//! whether they hold the same value. What survives is
//! [`erasing_a_subject_does_not_reopen_its_handle`], the property the replacement was
//! chosen to keep.
//!
//! **A misfiled seal is unrepresentable.** `a_handle_into_a_plaintext_column_is_rejected`
//! and `a_handle_filed_under_the_wrong_subject_id_is_rejected` each stored a subject's
//! ciphertext into a column that claimed a different subject, field or scope, and
//! asserted the projector failed. `docs/projectors.md` rule 9 makes a column's subject
//! *propagation rather than declaration*: it is computed from the value written into
//! it, so a column and its content cannot disagree.
//!
//! **A boundary cannot filter on sealed content.** The three scoped-subject-query cases
//! turned on encrypting a filter value under the subject's key and matching it against
//! the tag the emit stored. Rule 12 rejects the equality that would express it, so the
//! encrypt-a-filter path has no caller and neither do the two erased-subject cases that
//! guarded its edges.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine;
use hekla::crypto::{Adopted, KeyStore, MasterKeys};
use hekla::effect::StubHttpClient;
use hekla::opdb::OpDb;
use hekla::read_api;
use hekla::read_model::ReadModel;
use serde_json::{Value, json};

mod support;

use support::{
    ALICE, BOB, Boot, Harness, MASTER_KEY, ORDERS_PROJECTOR, accounts_project, assert_error, ctx,
    orders_project, orders_project_with, orders_with_notify_effect, place_order, read_row,
    wait_position, wait_until, write_project,
};

use support::UUID_A as ORDER;
use support::UUID_B;

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
        message.contains("HEKLA_MASTER_KEY"),
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

/// Rule 12's list of what may be done to sealed content: move it, ask whether it is
/// there, or `reveal` it. Everything else is a compile error, and a projector may not
/// `reveal` at all, so a projector that tries to derive a plaintext from a sealed
/// column never boots.
///
/// The Starlark version drove this to a *runtime* failure and waited for the projector
/// to report `failed`, because an opaque handle could only refuse an operation when the
/// operation ran. A sealed value is typed, so the refusal is static.
#[test]
fn a_projector_cannot_derive_a_plaintext_from_a_sealed_column() {
    assert_error(
        &[
            ("events/order.hk", support::ORDER_EVENTS),
            ("commands/place-order.hk", support::PLACE_ORDER),
            (
                "projectors/leaky.hk",
                r#"
projector Leaky {
  entity Leak {
    order_id: Uuid @key,
    domain: String @max(100),
  }

  on @order.placed { order_id, email } {
    // Deriving a plaintext from sealed content: interpolation reads it.
    put Leak { order_id, domain: "{email}!" }
  }
}
"#,
            ),
        ],
        "cannot be interpolated into a string",
    );
}

#[test]
fn the_projector_stores_ciphertext_for_the_subject_column() {
    let dir = orders_project();
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    wait_position(&harness.rt, "Orders", 1);

    // Read the read model directly, bypassing the read API's decrypt: the stored
    // email column is ciphertext, never the plaintext.
    let shared = harness.rt.projector("Orders").unwrap();
    let model = ReadModel::open_readonly(&shared.db_path).unwrap();
    let entity = shared.entities.iter().find(|e| e.name == "Order").unwrap();
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

    let row = read_row(&harness, "Orders", "Order", ORDER, 1).expect("a row");
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
    let row = read_row(&harness, "Orders", "Order", ORDER, 1).expect("a row");
    assert_eq!(row["email"], "alice@example.com");

    // Erase customer 42: one key delete.
    let erased = harness
        .rt
        .keystore()
        .unwrap()
        .erase("Customer", "42")
        .unwrap();
    assert!(erased);

    // The read model now reads the email as absent (its ciphertext is undecryptable);
    // the order itself and the plaintext customer id remain.
    let row = read_row(&harness, "Orders", "Order", ORDER, 1).expect("the order row still exists");
    assert!(
        row.get("email").is_none(),
        "erased email must be absent: {row}"
    );
    assert_eq!(row["customer_id"].as_i64(), Some(42));
    assert_eq!(row["order_id"], ORDER);

    harness.shutdown();
}

/// Rule 12 splits what the Starlark version treated as one prohibition. Moving sealed
/// content into a field sealed under the *same* subject is legal, because moving is not
/// reading; folding it under one subject and emitting it under another is not, because
/// then one value would need two keys.
///
/// The Starlark version asserted only the refusal, and refused both: a handle was
/// opaque to the constructor whatever it was being written into, so a command could not
/// carry a customer's own address forward at all.
#[test]
fn a_folded_subject_value_may_be_re_emitted_under_its_own_subject_and_no_other() {
    let dir = orders_project_with(&[
        ("projectors/orders.hk", ORDERS_PROJECTOR),
        (
            "commands/copy-order.hk",
            r#"
command CopyOrder(order_id: Uuid, customer_id: Customer) {
  // Folds this customer's own address. The variable is sealed under `customer_id`,
  // and the emit below writes it into a field sealed under the same subject, so it
  // moves without ever being read.
  fold email: String? = none
    on @order.placed(customer_id) { email } => email

  emit @order.placed { order_id, customer_id, email }
}
"#,
        ),
    ]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    let copied = harness
        .rt
        .execute(
            "CopyOrder",
            json!({ "order_id": UUID_B, "customer_id": 42 }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(copied.status, 200, "{:?}", copied.body);

    // It really moved: the copy decrypts to the same address, under the same key.
    let row = read_row(&harness, "Orders", "Order", UUID_B, 2).expect("the copied row");
    assert_eq!(row["email"], "alice@example.com");
    harness.shutdown();

    // The other half: a second subject on the same event, and a fold that tries to
    // carry the customer's address into the shop's field.
    assert_error(
        &[
            (
                "events/order.hk",
                r#"
subject Customer(Int)
subject Shop(Int)

event @order.placed {
  order_id: Uuid,
  customer_id: Customer,
  shop_id: Shop,
  email: String? @subject(customer_id) @max(100),
  contact: String? @subject(shop_id) @max(100),
}
"#,
            ),
            (
                "commands/copy-order.hk",
                r#"
command CopyOrder(order_id: Uuid, customer_id: Customer, shop_id: Shop) {
  fold email: String? = none
    on @order.placed(customer_id) { email } => email

  emit @order.placed { order_id, customer_id, shop_id, email: none, contact: email }
}
"#,
            ),
        ],
        "subject",
    );
}

#[test]
fn a_read_does_not_resurrect_an_erased_subject_key() {
    let dir = orders_project();
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let ks = harness.rt.keystore().unwrap();

    // The key exists after the order.
    assert!(
        ks.encrypt_subject_existing("Customer", "42", "email", "x")
            .unwrap()
            .is_some()
    );
    ks.erase("Customer", "42").unwrap();
    assert!(
        ks.encrypt_subject_existing("Customer", "42", "email", "x")
            .unwrap()
            .is_none()
    );
    // A read of the row (the read/query path) must not recreate the key.
    let _ = read_row(&harness, "Orders", "Order", ORDER, 1);
    assert!(
        ks.encrypt_subject_existing("Customer", "42", "email", "x")
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
        .execute("PlaceOrder", body.clone(), &ctx(), Some("idem-1"))
        .unwrap();
    assert_eq!(fresh.status, 200);
    let recovered = harness
        .rt
        .execute("PlaceOrder", body, &ctx(), Some("idem-1"))
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
    // `reveal` gave the effect the real plaintext to send.
    assert_eq!(body["to"], "alice@example.com");

    harness.shutdown();
}

#[test]
fn a_reveal_on_an_erased_subject_skips_terminally_without_wedging() {
    let dir = orders_with_notify_effect();
    // A persistent 5xx wedges the effect on http.post, which runs after `reveal` has
    // already succeeded. That gives a window to erase the customer; each retry re-runs
    // the arm from the top, so once the key is gone `reveal` fails terminally.
    let harness = Boot::new(dir.path())
        .http_status(500)
        .with_master_key()
        .start();

    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let effect = harness.rt.effect("Notify").unwrap().clone();

    // The 5xx wedges the effect: `reveal` succeeded this attempt, http.post did not.
    wait_until("the effect to wedge on the 5xx", || {
        effect.consecutive_failures() > 0
    });

    // Erase the customer. The next retry's `reveal` can no longer decrypt.
    harness
        .rt
        .keystore()
        .unwrap()
        .erase("Customer", "42")
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
fn concurrent_first_use_of_a_boundaried_value_admits_only_one() {
    // Two concurrent first-ever writes of the same handle, on distinct accounts. The
    // slice is in both commands' append conditions, so the writer that appends second
    // conflicts, re-folds against the winner's event and rejects rather than both
    // committing.
    let dir = accounts_project();
    let harness = boot(dir.path());

    let register = |account_id: &'static str| {
        let rt = harness.rt.clone();
        thread::spawn(move || {
            let body = json!({
                "account_id": account_id,
                "handle": "race",
                "email": "race@example.com",
            });
            rt.execute("RegisterAccount", body, &ctx(), None)
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

/// A `patch` reads the row it writes, sealed column included, so a projector can carry
/// a credential it may never `reveal` across an update. That is rule 9's propagation
/// seen from the store: the column is sealed because sealed content was written into
/// it, and it stays sealed when it is written back.
#[test]
fn a_projector_can_read_modify_write_a_subject_column() {
    let dir = write_project(&[
        (
            "events/order.hk",
            r#"
subject Customer(Int)

event @order.placed {
  order_id: Uuid,
  customer_id: Customer,
  email: String? @subject(customer_id) @max(100),
}

event @order.touched { order_id: Uuid }
"#,
        ),
        ("commands/place-order.hk", support::PLACE_ORDER),
        (
            "commands/touch-order.hk",
            r#"
command TouchOrder(order_id: Uuid) {
  emit @order.touched { order_id }
}
"#,
        ),
        (
            "projectors/orders.hk",
            r#"
projector Orders {
  entity Order {
    order_id: Uuid @key,
    customer_id: Customer @index,
    email: String? @max(100),
    touches: Int,
  }

  on @order.placed { order_id, customer_id, email } {
    put Order { order_id, customer_id, email, touches: 0 }
  }

  // Read-modify-write: the stored counter is loaded before the value expression
  // runs, and the sealed column rides through untouched.
  on @order.touched { order_id } {
    update Order[order_id] { touches: .touches + 1 }
  }
}
"#,
        ),
    ]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    harness
        .rt
        .execute("TouchOrder", json!({ "order_id": ORDER }), &ctx(), None)
        .unwrap();

    wait_position(&harness.rt, "Orders", 2);
    assert!(
        !harness.rt.projector("Orders").unwrap().failed(),
        "the read-modify-write projector must not fail"
    );
    let row = read_row(&harness, "Orders", "Order", ORDER, 2).expect("a row");
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
    wait_position(&harness.rt, "Orders", 1);
    harness
        .rt
        .keystore()
        .unwrap()
        .erase("Customer", "42")
        .unwrap();
    // A new order for customer 42 mints a fresh key.
    place_order(&harness.rt, second, 42, "new@example.com");

    // The first order's email is unreadable (its key is gone); the second's is fine.
    let old = read_row(&harness, "Orders", "Order", first, 2).expect("first row");
    assert!(
        old.get("email").is_none(),
        "stale email must read as absent: {old}"
    );
    let new = read_row(&harness, "Orders", "Order", second, 2).expect("second row");
    assert_eq!(new["email"], "new@example.com");

    harness.shutdown();
}

/// What the `unique` replacement was chosen to preserve. The handle is plaintext, so
/// the slice that enforces it is untouched by a shred: erasing an account takes its
/// address and leaves the name it registered under claimed.
///
/// Under `unique` this worked through a never-erased global key, and the argument for
/// it was exactly this case. The plaintext boundary reaches the same place with no key
/// at all, which is why the feature was not replaced with another one.
#[test]
fn erasing_a_subject_does_not_reopen_its_handle() {
    let dir = accounts_project();
    let harness = boot(dir.path());
    let register = |account_id: &str, handle: &str, email: &str| {
        let body = json!({ "account_id": account_id, "handle": handle, "email": email });
        harness
            .rt
            .execute("RegisterAccount", body, &ctx(), None)
            .unwrap()
    };

    let first = register(ALICE, "shared", "alice@example.com");
    assert_eq!(first.status, 200, "first registration: {:?}", first.body);
    // A different account taking the same handle is refused while the first is live.
    let second = register(BOB, "shared", "bob@example.com");
    assert_eq!(second.status, 422, "{:?}", second.body);
    assert_eq!(second.body["error"]["code"], "handle_taken");
    // A different handle on the same account is fine, so the rule is the handle and
    // not the account.
    let other = register(BOB, "other", "bob@example.com");
    assert_eq!(other.status, 200, "distinct handle: {:?}", other.body);

    let ks = harness.rt.keystore().unwrap();
    assert!(
        ks.erase("Account", ALICE).unwrap(),
        "the subject key must exist to be erased"
    );
    assert!(
        ks.encrypt_subject_existing("Account", ALICE, "email", "alice@example.com")
            .unwrap()
            .is_none(),
        "control: the erased account's scoped key is really gone"
    );

    let reuse = register(BOB, "shared", "bob@example.com");
    assert_eq!(
        reuse.status, 422,
        "erasing a subject must not re-open the handle it claimed: {:?}",
        reuse.body
    );
    assert_eq!(reuse.body["error"]["code"], "handle_taken");

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
        &read_api::Query::all(50),
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

    let rows = scan_rows(&harness, "Orders", "Order", 3);
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
        .erase("Customer", "42")
        .unwrap();
    let rows = scan_rows(&harness, "Orders", "Order", 3);
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
///
/// Each is optional, which is forced rather than incidental: an erased subject's
/// column reads back *absent*, and a type that cannot be absent could not say so.
const TYPED_EVENTS: &str = r#"
subject Customer(Int)

event @order.placed {
  order_id: Uuid,
  customer_id: Customer,
  email: String? @subject(customer_id) @max(100),
  order_total: Money(2)? @subject(customer_id),
  loyalty_points: Int? @subject(customer_id),
}
"#;

const TYPED_PLACE_ORDER: &str = r#"
command PlaceOrder(
  order_id: Uuid,
  customer_id: Customer,
  email: String?,
  order_total: Money(2)?,
  loyalty_points: Int?,
) {
  emit @order.placed { order_id, customer_id, email, order_total, loyalty_points }
}
"#;

const TYPED_PROJECTOR: &str = r#"
projector Orders {
  entity Order {
    order_id: Uuid @key,
    customer_id: Customer @index,
    email: String? @max(100),
    order_total: Money(2)?,
    loyalty_points: Int?,
  }

  on @order.placed { order_id, customer_id, email, order_total, loyalty_points } {
    put Order { order_id, customer_id, email, order_total, loyalty_points }
  }
}
"#;

#[test]
fn a_scanned_page_decrypts_typed_subject_columns_and_skips_erased_rows() {
    let dir = write_project(&[
        ("events/order.hk", TYPED_EVENTS),
        ("commands/place-order.hk", TYPED_PLACE_ORDER),
        ("projectors/orders.hk", TYPED_PROJECTOR),
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
            .execute("PlaceOrder", body, &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "PlaceOrder failed: {:?}", result.body);
    };
    place(ORDER_1, 42, "19.99", 250);
    place(ORDER_2, 99, "7.50", -3);
    place(ORDER_3, 42, "100.00", 0);

    let rows = scan_rows(&harness, "Orders", "Order", 3);
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
        .erase("Customer", "42")
        .unwrap();
    let rows = scan_rows(&harness, "Orders", "Order", 3);
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
    let row = read_row(&harness, "Orders", "Order", ORDER_1, 1).expect("a row");
    assert_eq!(row["email"], "alice@example.com");
    harness.shutdown();

    // Rotate offline, keeping the old master so the stored wrapping can be unwrapped.
    {
        let opdb = Arc::new(Mutex::new(
            OpDb::open(&data.path().join("hekla.db")).unwrap(),
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
    let row = read_row(&harness, "Orders", "Order", ORDER_1, 1).expect("a row after rotation");
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
        err.contains("HEKLA_MASTER_KEY"),
        "the boot guard names the key to set: {err}"
    );
}

// --- erasing from an effect -----------------------------------------------

/// An effect that shreds the customer it was told to, the shape a GDPR redact handler
/// takes: the subject id it erases comes from a plaintext field, not from a value
/// scoped to the key it is about to destroy.
///
/// The Starlark version put the "only customer 42" condition in the handler's clause.
/// Rule 1 makes an event select exactly one arm, so an arm carries no filter and the
/// condition is an ordinary `if` in the body.
const SHRED_EFFECT: &str = r#"
effect Shred {
  on @order.placed { @key customer_id } {
    if customer_id == 42 {
      erase(customer_id)
    }
  }
}
"#;

#[test]
fn an_effect_erases_a_subject_and_shreds_its_data() {
    let dir = orders_project_with(&[
        ("effects/shred.hk", SHRED_EFFECT),
        ("projectors/orders.hk", ORDERS_PROJECTOR),
    ]);
    let harness = boot(dir.path());
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    let effect = harness.rt.effect("Shred").unwrap().clone();
    wait_until("the effect to erase the first customer", || {
        effect.position() >= 1
    });

    // The same shred `hekla erase` performs, reached from an arm: the read model's
    // ciphertext no longer decrypts, while the plaintext ids stay.
    let row = read_row(&harness, "Orders", "Order", ORDER, 1).expect("the order row survives");
    assert!(
        row.get("email").is_none(),
        "the erased email must be absent: {row}"
    );
    assert_eq!(row["customer_id"].as_i64(), Some(42));

    // Scoped to the subject it named, not a blanket decrypt failure. The arm's guard
    // admits only customer 42, so 43 is never erased.
    place_order(&harness.rt, UUID_B, 43, "bob@example.com");
    let row = read_row(&harness, "Orders", "Order", UUID_B, 2).expect("bob's row");
    assert_eq!(row["email"], "bob@example.com");

    // Erasing is not a failure: the invocation completed rather than wedging or
    // recording a terminal skip.
    assert_eq!(effect.consecutive_failures(), 0);
    assert_eq!(effect.terminal_skips(), 0);
    harness.shutdown();
}

/// Erases the same subject twice in one invocation. Identical calls get successive
/// ordinals, so both are journaled separately and a replay skips both.
const DOUBLE_SHRED_EFFECT: &str = r#"
effect Shred {
  on @order.placed { @key customer_id } {
    erase(customer_id)
    erase(customer_id)
  }
}
"#;

/// The Starlark version read the two journaled results back and asserted `true` then
/// `false`, because `erase` returned whether a key was really deleted. heklang
/// deliberately drops that result: it is a race that is always already lost, and an
/// author reading it would branch on whether someone else got there first. So what is
/// left to pin is the ordinal channel itself, on a call that has no result at all.
#[test]
fn an_effect_erase_is_journaled_under_successive_ordinals() {
    let dir = orders_project_with(&[("effects/shred.hk", DOUBLE_SHRED_EFFECT)]);
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(dir.path())
        .data_dir(data.path())
        .http_status(200)
        .with_master_key()
        .start();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    let effect = harness.rt.effect("Shred").unwrap().clone();
    wait_until("the effect to complete", || effect.position() >= 1);
    harness.shutdown();

    let db = rusqlite::Connection::open(data.path().join("hekla.db")).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT kind, disambiguator, call_hash FROM effect_journal \
             WHERE effect = 'Shred' ORDER BY disambiguator",
        )
        .unwrap();
    let rows: Vec<(String, i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 2, "both calls are journaled: {rows:?}");
    assert_eq!(rows[0].0, "erase");
    assert_eq!((rows[0].1, rows[1].1), (0, 1));
    assert_eq!(
        rows[0].2, rows[1].2,
        "identical calls share a key, so only the ordinal separates them"
    );
}

// --- a subject deleted with its tenant -------------------------------------

/// A tenant and the people under it: `Member` declares `under Tenant`, so a member's key
/// is wrapped under its tenant's rather than under the master.
const NESTED_EVENTS: &str = r#"
subject Tenant(Int)
subject Member(Int) under Tenant

event @member.joined {
  member_id: Member,
  tenant_id: Tenant,
  // Optional because an erased subject's column reads back absent, and a type that
  // cannot be absent could not say so.
  email: String? @subject(member_id) @max(100),
  // The tenant's own sealed value, so a test can tell "the tenant was shredded" from
  // "everything was shredded".
  plan: String? @subject(tenant_id) @max(100),
}
"#;

const NESTED_COMMAND: &str = r#"
command Join(member_id: Member, tenant_id: Tenant, email: String?, plan: String?) {
  emit @member.joined { member_id, tenant_id, email, plan }
}
"#;

const NESTED_PROJECTOR: &str = r#"
projector Members {
  entity Member {
    member_id: Member @key,
    tenant_id: Tenant @index,
    email: String? @max(100),
    plan: String? @max(100),
  }

  on @member.joined { member_id, tenant_id, email, plan } {
    put Member { member_id, tenant_id, email, plan }
  }
}
"#;

fn nested_project() -> tempfile::TempDir {
    write_project(&[
        ("events/member.hk", NESTED_EVENTS),
        ("commands/join.hk", NESTED_COMMAND),
        ("projectors/members.hk", NESTED_PROJECTOR),
    ])
}

fn join(harness: &Harness, member: u64, tenant: u64, email: &str) {
    let body = json!({
        "member_id": member,
        "tenant_id": tenant,
        "email": email,
        "plan": "pro",
    });
    harness
        .rt
        .execute("Join", body, &ctx(), None)
        .unwrap_or_else(|err| panic!("joining member {member}: {err:#}"));
}

fn member(harness: &Harness, id: u64, after: u64) -> Value {
    read_row(harness, "Members", "Member", &id.to_string(), after).expect("the member row")
}

/// The whole of Phase 39, in one assertion: **one row delete shreds every key beneath it.**
///
/// Deleting the tenant's row is O(1) and touches nothing else. Every member's key is
/// wrapped under a key derived from the tenant's secret, so destroying that secret makes
/// them underivable at the same instant: no walk, no second write, and no enumeration
/// projector to tell the runtime who the members were.
///
/// It asserts the member rows are *still there* on purpose. Unreachable is the guarantee;
/// reclaiming the bytes is the sweeper's job and a separate one.
#[test]
fn erasing_a_tenant_shreds_every_member_beneath_it() {
    let dir = nested_project();
    let harness = boot(dir.path());
    join(&harness, 1, 7, "ada@example.com");
    join(&harness, 2, 7, "grace@example.com");
    // A member of a different tenant, which must survive: the cascade follows the
    // declared hierarchy rather than shredding every subject of that kind.
    join(&harness, 3, 9, "alan@example.com");

    assert_eq!(member(&harness, 1, 3)["email"], "ada@example.com");
    assert_eq!(member(&harness, 3, 3)["email"], "alan@example.com");

    let keystore = harness.rt.keystore().unwrap();
    assert!(keystore.erase("Tenant", "7").unwrap(), "one row delete");

    let ada = member(&harness, 1, 3);
    assert!(
        ada.get("email").is_none(),
        "the member's key was wrapped under the tenant's, so it went with it: {ada}"
    );
    assert!(
        ada.get("plan").is_none(),
        "the tenant's own sealed column went too: {ada}"
    );
    assert_eq!(ada["tenant_id"].as_u64(), Some(7), "plaintext ids remain");

    let grace = member(&harness, 2, 3);
    assert!(grace.get("email").is_none(), "every member, not just one");

    let alan = member(&harness, 3, 3);
    assert_eq!(
        alan["email"], "alan@example.com",
        "another tenant's member is untouched: {alan}"
    );

    // Unreachable, not deleted. The rows survive until the sweep reclaims them, which is
    // the storage half and deliberately not what the erase does.
    assert!(
        harness.rt.subject_key_exists("Member", "1").unwrap(),
        "the child row survives its parent's deletion"
    );
    assert!(
        !harness.rt.subject_key_exists("Tenant", "7").unwrap(),
        "the parent row is the one that went"
    );

    harness.shutdown();
}

/// A member written *after* its tenant was erased gets a fresh tenant key and is readable,
/// which is the point-in-time shred every subject already had, applied one level up.
///
/// The members from before stay shredded, because they are wrapped under the secret that
/// was destroyed rather than under the one minted now.
#[test]
fn a_tenant_written_to_after_an_erasure_shelters_only_what_came_after() {
    let dir = nested_project();
    let harness = boot(dir.path());
    join(&harness, 1, 7, "ada@example.com");

    harness.rt.keystore().unwrap().erase("Tenant", "7").unwrap();
    join(&harness, 2, 7, "grace@example.com");

    let ada = member(&harness, 1, 2);
    assert!(
        ada.get("email").is_none(),
        "written under the destroyed tenant secret, so still shredded: {ada}"
    );
    let grace = member(&harness, 2, 2);
    assert_eq!(
        grace["email"], "grace@example.com",
        "written under the fresh one, so readable: {grace}"
    );

    harness.shutdown();
}

/// An unreachable child row reads as absent and is *replaced*, rather than failing the
/// write that meets it.
///
/// Member 1's row outlives its tenant's erasure, so its wrapped key can never be opened
/// again. Writing member 1 again must mint a fresh secret over that row rather than
/// erroring: the old ciphertext is already unrecoverable, so refusing protects nothing and
/// would wedge the write path for good.
#[test]
fn an_unwrappable_child_row_is_replaced_rather_than_failing_the_write() {
    let dir = nested_project();
    let harness = boot(dir.path());
    join(&harness, 1, 7, "ada@example.com");

    harness.rt.keystore().unwrap().erase("Tenant", "7").unwrap();
    assert!(
        harness.rt.subject_key_exists("Member", "1").unwrap(),
        "the stale child row is still on disk, which is the case this is about"
    );

    // The same member again. Its row is there and unopenable; this must not error.
    join(&harness, 1, 7, "ada2@example.com");
    let ada = member(&harness, 1, 2);
    assert_eq!(
        ada["email"], "ada2@example.com",
        "the replaced key reads back: {ada}"
    );

    harness.shutdown();
}

/// A rotation rewraps the roots and leaves the children alone, and both still read.
///
/// A child's wrapping key is derived from its parent's *secret*, which a rotation does not
/// change: it rewraps that secret under a new master and the secret itself is untouched.
/// So a child needs no rewrap at all, and the count reports only the roots. That is the
/// whole of what a hierarchy costs a rotation, and it is a saving rather than a cost.
#[test]
fn a_rotation_rewraps_the_roots_and_leaves_the_children_readable() {
    let dir = nested_project();
    let data = tempfile::tempdir().unwrap();
    let boot_at = |master: MasterKeys| {
        Boot::new(dir.path())
            .data_dir(data.path())
            .http_status(200)
            .master(master)
            .try_start()
    };

    let harness = boot_at(MasterKeys::new(MASTER_KEY, vec![])).expect("the first boot");
    join(&harness, 1, 7, "ada@example.com");
    join(&harness, 2, 7, "grace@example.com");
    harness.shutdown();

    {
        let opdb = Arc::new(Mutex::new(
            OpDb::open(&data.path().join("hekla.db")).unwrap(),
        ));
        let keystore = KeyStore::new(opdb, MasterKeys::new(NEXT_MASTER_KEY, vec![MASTER_KEY]));
        assert_eq!(
            keystore.rotate().unwrap(),
            1,
            "one tenant root; its two members are wrapped under it and need no rewrap"
        );
        assert_eq!(keystore.rotate().unwrap(), 0, "a second pass is a no-op");
    }

    // The old master is gone. Both members still decrypt, each through its parent.
    let harness = boot_at(MasterKeys::new(NEXT_MASTER_KEY, vec![])).expect("the rotated boot");
    assert_eq!(member(&harness, 1, 2)["email"], "ada@example.com");
    assert_eq!(member(&harness, 2, 2)["email"], "grace@example.com");

    harness.shutdown();
}

/// hekla refuses at load what it could not file a key under at write time.
///
/// heklang refuses this too, at the annotation's span, which is the better message and
/// the one an author sees first. hekla's copy is about a *deployment*: a directory
/// reaches this runtime without going through `hek check`, and the failure it prevents is
/// a write that cannot mint a key, in production, on the path that must not fail.
#[test]
fn an_event_sealing_under_a_child_without_its_parent_is_refused_at_load() {
    assert_error(
        &[(
            "events/member.hk",
            r#"
subject Tenant(Int)
subject Member(Int) under Tenant

event @member.noted {
  member_id: Member,
  note: String? @subject(member_id) @max(100),
}
"#,
        )],
        "carries no `Tenant` field",
    );
}

/// The storage half: unreadable rows are reclaimed, and only the unreachable ones.
///
/// Separate from the erase on purpose. Erasing a tenant is one row delete however many
/// customers it has, which is the whole point; reclaiming their rows afterwards is
/// bounded, chunked background work that no request waits on.
#[test]
fn the_sweep_reclaims_orphaned_keys_and_leaves_reachable_ones() {
    let dir = nested_project();
    let harness = boot(dir.path());
    join(&harness, 1, 7, "ada@example.com");
    join(&harness, 2, 7, "grace@example.com");
    join(&harness, 3, 9, "alan@example.com");

    harness.rt.keystore().unwrap().erase("Tenant", "7").unwrap();
    assert!(
        harness.rt.subject_key_exists("Member", "1").unwrap(),
        "still on disk until the sweep runs"
    );

    hekla::effect::sweep_orphan_keys_now(&harness.rt).unwrap();

    assert!(
        !harness.rt.subject_key_exists("Member", "1").unwrap(),
        "an orphan is reclaimed"
    );
    assert!(
        !harness.rt.subject_key_exists("Member", "2").unwrap(),
        "every orphan, not just one"
    );
    assert!(
        harness.rt.subject_key_exists("Member", "3").unwrap(),
        "a member whose tenant is alive is reachable and stays"
    );
    assert!(
        harness.rt.subject_key_exists("Tenant", "9").unwrap(),
        "a root is never an orphan"
    );

    harness.shutdown();
}

/// An orphaned key reads as absent from the moment its parent goes, not from whenever the
/// sweeper happens to run.
///
/// The row is still on disk for that whole window, so answering from row existence would
/// report `live` for a subject whose data is permanently unreadable, and would change its
/// answer on a background timer. Reachability is the question every other erasure surface
/// answers, and this is the one that used to disagree with them.
#[test]
fn a_subject_whose_parent_was_erased_reads_as_absent_before_the_sweep() {
    let dir = nested_project();
    let harness = boot(dir.path());
    join(&harness, 1, 7, "ada@example.com");

    assert!(harness.rt.subject_key_reachable("Member", "1").unwrap());
    harness.rt.keystore().unwrap().erase("Tenant", "7").unwrap();

    assert!(
        harness.rt.subject_key_exists("Member", "1").unwrap(),
        "the row is still on disk, which is exactly the window this is about"
    );
    assert!(
        !harness.rt.subject_key_reachable("Member", "1").unwrap(),
        "and nothing can open it, which is what `absent` has to mean"
    );

    // The sweep changes what is on disk and must not change the answer.
    hekla::effect::sweep_orphan_keys_now(&harness.rt).unwrap();
    assert!(
        !harness.rt.subject_key_reachable("Member", "1").unwrap(),
        "the same answer before and after the sweep"
    );

    harness.shutdown();
}

/// Two fields of the ancestor's type is refused, because nothing says which one the key
/// hangs from.
///
/// `@subject(buyer)` disambiguates its *own* subject by naming a sibling; an ancestor gets
/// no such syntax. Taking the first declared would make which shop a customer's key hangs
/// from depend on field order, and an `erase` of the other shop would then leave that
/// customer readable with nothing anywhere saying why.
#[test]
fn an_event_carrying_two_of_an_ancestors_type_is_refused_rather_than_guessed() {
    assert_error(
        &[(
            "events/transfer.hk",
            r#"
subject Shop(Int)
subject Customer(Int) under Shop

event @transfer.made {
  buyer: Customer,
  from_shop: Shop,
  to_shop: Shop,
  note: String? @subject(buyer) @max(100),
}
"#,
        )],
        "nothing says which one its key hangs from",
    );
}

/// A break two levels up names the whole hierarchy, not one link the author never wrote.
///
/// `Customer` sits under `Shop`, and `Shop` under `Market`. Reporting "`Customer`, which
/// sits under `Market`" would teach the reader a relationship that does not exist, which
/// is the thing a diagnostic about a hierarchy least wants to do.
#[test]
fn a_missing_grandparent_is_reported_against_the_whole_chain() {
    assert_error(
        &[(
            "events/member.hk",
            r#"
subject Market(Int)
subject Shop(Int) under Market
subject Customer(Int) under Shop

event @member.noted {
  buyer: Customer,
  shop: Shop,
  note: String? @subject(buyer) @max(100),
}
"#,
        )],
        "which sits under `Shop` under `Market`, but carries no `Market` field",
    );
}

// --- the hierarchy, proved at the key store ---------------------------------

/// A key store over its own in-memory opdb, for the cases that are about wrapping rather
/// than about a project.
fn keystore() -> KeyStore {
    let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
    KeyStore::new(opdb, MasterKeys::new(MASTER_KEY, vec![]))
}

/// Three levels, not two. `Customer under Shop under Market`: minting the customer's key
/// needs the shop's secret, and if the shop has no row yet that needs the market's, so
/// both the mint and the unwrap recurse.
///
/// Depth one is the case where "wrap under the parent" and "wrap under the root" coincide,
/// so it cannot tell a recursive implementation from a one-level one. This can: erasing the
/// **market** has to reach the customer two links away, through a shop whose own row is
/// untouched.
#[test]
fn a_grandparent_erasure_reaches_two_levels_down() {
    let ks = keystore();
    let chain = [("Customer", "88"), ("Shop", "7"), ("Market", "1")];
    let sealed = ks
        .encrypt_subject_in(&chain, "email", "ada@example.com")
        .unwrap();
    assert_eq!(
        ks.decrypt_subject("Customer", "88", "email", &sealed)
            .unwrap()
            .as_deref(),
        Some("ada@example.com"),
        "a two-link chain reads back"
    );

    // The middle row is untouched, and the bottom one still exists. Only the top goes.
    assert!(ks.erase("Market", "1").unwrap(), "one row delete");

    assert_eq!(
        ks.decrypt_subject("Customer", "88", "email", &sealed)
            .unwrap(),
        None,
        "the customer is two links below the market and goes with it"
    );
    assert!(
        ks.decrypt_subject("Shop", "7", "name", "not-a-real-ciphertext")
            .unwrap()
            .is_none(),
        "so does the shop in between"
    );
}

/// Two writers racing to replace one unreachable row must agree on a single key.
///
/// This is the case the replacement compare-and-set exists for, and keying it on the
/// parent instead of the wrapped bytes made it wrong in the worst way: the replacement
/// hangs from the same parent as the row it replaced, so each writer would have matched
/// the *other's* fresh row and deleted a key already sealed under, leaving ciphertext
/// nobody could read and no error anywhere.
///
/// The assertion is the one that would have caught it: whatever key each thread was
/// handed, both have to open both ciphertexts afterwards.
#[test]
fn two_writers_replacing_one_unreachable_row_agree_on_a_key() {
    let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
    let masters = MasterKeys::new(MASTER_KEY, vec![]);
    let seed = KeyStore::new(Arc::clone(&opdb), masters.clone());
    let chain = [("Member", "1"), ("Tenant", "7")];
    seed.encrypt_subject_in(&chain, "email", "before").unwrap();

    // Erase the tenant. Member 1's row survives and nothing can ever open it again.
    seed.erase("Tenant", "7").unwrap();
    assert!(
        opdb.lock()
            .unwrap()
            .subject_key_exists("Member", "1")
            .unwrap(),
        "the stale row is what both writers are about to replace"
    );

    let a = KeyStore::new(Arc::clone(&opdb), masters.clone());
    let b = KeyStore::new(Arc::clone(&opdb), masters);
    let (from_a, from_b) = thread::scope(|scope| {
        let one = scope.spawn(|| a.encrypt_subject_in(&chain, "email", "ada").unwrap());
        let two = scope.spawn(|| b.encrypt_subject_in(&chain, "email", "grace").unwrap());
        (one.join().unwrap(), two.join().unwrap())
    });

    // One key persisted, so both ciphertexts open under it. If either writer's key was
    // deleted by the other, the value it sealed is gone for good.
    let reader = KeyStore::new(opdb, MasterKeys::new(MASTER_KEY, vec![]));
    assert_eq!(
        reader
            .decrypt_subject("Member", "1", "email", &from_a)
            .unwrap()
            .as_deref(),
        Some("ada"),
        "the first writer's value survived"
    );
    assert_eq!(
        reader
            .decrypt_subject("Member", "1", "email", &from_b)
            .unwrap()
            .as_deref(),
        Some("grace"),
        "and so did the second's"
    );
}

/// A wrapped child key moved to another row does not hand that row the other subject's
/// secret.
///
/// Both rows hang from the same tenant, so the derived wrapping key is identical and the
/// stolen bytes would unwrap cleanly on their own. What stops it is the associated data,
/// which binds each child's own identity into its wrapping.
///
/// The assertion is deliberately about *reading another subject's content*, not about
/// getting an error. A failed child unwrap reads as absent by design, so "it did not open"
/// is indistinguishable from an ordinary shred and would pass with no binding at all.
/// Without the binding the key opens, yields member 1's secret, and member 1's ciphertext
/// then reads back in full under member 2's identity. That is the whole attack, and it is
/// what this catches.
#[test]
fn a_child_key_moved_to_another_row_does_not_open() {
    // File-backed so a second connection can reach the rows: moving bytes between them is
    // the whole point, and no API does it because nothing should.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hekla.db");
    let opdb = Arc::new(Mutex::new(OpDb::open(&path).unwrap()));
    let ks = KeyStore::new(Arc::clone(&opdb), MasterKeys::new(MASTER_KEY, vec![]));
    let ada = ks
        .encrypt_subject_in(&[("Member", "1"), ("Tenant", "7")], "email", "ada")
        .unwrap();
    let grace = ks
        .encrypt_subject_in(&[("Member", "2"), ("Tenant", "7")], "email", "grace")
        .unwrap();

    // Member 1's wrapped key onto member 2's row, under the same live tenant.
    let stolen = opdb
        .lock()
        .unwrap()
        .get_subject_key("Member", "1")
        .unwrap()
        .unwrap()
        .wrapped;
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE subject_key SET wrapped_key = ?1 WHERE subject = 'Member' AND subject_value = '2'",
        rusqlite::params![stolen],
    )
    .unwrap();

    // Member 1's own ciphertext, offered under member 2's identity. If the stolen key
    // opens, this reads back as "ada" and one subject has been read as another.
    let stolen = ks.decrypt_subject("Member", "2", "email", &ada);
    assert!(
        !matches!(&stolen, Ok(Some(text)) if text == "ada"),
        "a relocated wrapping must not open one subject's content under another's name: {stolen:?}"
    );
    // And it is reported as tampering rather than as a shred: the parent generation still
    // matches, so the key this row was wrapped under has not moved and somebody wrote to
    // the store. Telling an operator "erased" here would hide that.
    assert!(stolen.is_err(), "{stolen:?}");
    assert!(ks.decrypt_subject("Member", "2", "email", &grace).is_err());
}

/// A member whose tenant was erased and then written to again reads as **absent**, not as
/// an error.
///
/// This is the root case (`a_stale_ciphertext_under_a_superseded_key_reads_as_none`) one
/// level up, and it fails at a different layer. For a root, the superseded key still
/// unwraps and it is the *data* that will not decrypt. For a child, the parent's secret is
/// what its key was wrapped under, so a recreated parent breaks the **key** unwrap. If that
/// reports `Err`, the read API 500s instead of omitting the column, and the write path
/// hard-fails instead of replacing the dead row.
#[test]
fn a_member_whose_tenant_was_recreated_reads_as_absent_not_an_error() {
    let ks = keystore();
    let chain = [("Member", "1"), ("Tenant", "7")];
    let sealed = ks
        .encrypt_subject_in(&chain, "email", "ada@example.com")
        .unwrap();

    ks.erase("Tenant", "7").unwrap();
    // Any write under the tenant mints it again, with a new secret.
    ks.encrypt_subject_in(&[("Member", "2"), ("Tenant", "7")], "email", "grace")
        .unwrap();

    let read = ks.decrypt_subject("Member", "1", "email", &sealed);
    assert!(
        matches!(read, Ok(None)),
        "unrecoverable data is absent, which is what every erasure surface expects: {read:?}"
    );

    // And the dead row is replaceable, rather than wedging every future write to it.
    let again = ks
        .encrypt_subject_in(&chain, "email", "ada2@example.com")
        .unwrap();
    assert_eq!(
        ks.decrypt_subject("Member", "1", "email", &again)
            .unwrap()
            .as_deref(),
        Some("ada2@example.com")
    );
}

// --- a key store edited from outside -----------------------------------------

/// A file-backed store plus a second connection to it, for the cases that are only
/// reachable by writing rows hekla would never write.
fn tamperable() -> (tempfile::TempDir, Arc<Mutex<OpDb>>, rusqlite::Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hekla.db");
    let opdb = Arc::new(Mutex::new(OpDb::open(&path).unwrap()));
    let conn = rusqlite::Connection::open(&path).unwrap();
    (dir, opdb, conn)
}

/// A cycle in the parent pointers is refused, and refused *quickly*.
///
/// heklang checks the declared graph acyclic, so no project can produce this; a key store
/// somebody edited can. Both walks over it have to terminate: the recursive unwrap in Rust,
/// which would otherwise recurse until the stack went and take the process with it, and the
/// reachability query in SQL, which would otherwise be an unbounded `UNION ALL` and hang the
/// request holding the lock.
///
/// A hang is the worst of the three possible answers, because nothing reports it.
#[test]
fn a_cycle_in_the_parent_pointers_is_refused_rather_than_walked() {
    let (_dir, opdb, conn) = tamperable();
    let ks = KeyStore::new(Arc::clone(&opdb), MasterKeys::new(MASTER_KEY, vec![]));
    let sealed = ks
        .encrypt_subject_in(&[("Member", "1"), ("Tenant", "7")], "email", "ada")
        .unwrap();

    // Point the tenant at its own child. Nothing in hekla writes this.
    conn.execute(
        "UPDATE subject_key SET master_key_id = NULL, parent_subject = 'Member', \
         parent_value = '1', parent_fingerprint = 'forged' \
         WHERE subject = 'Tenant' AND subject_value = '7'",
        [],
    )
    .unwrap();

    let read = ks.decrypt_subject("Member", "1", "email", &sealed);
    assert!(
        read.is_err(),
        "a cycle is a broken store and has to say so: {read:?}"
    );
    assert!(
        format!("{:#}", read.unwrap_err()).contains("cycle"),
        "and name what is wrong with it"
    );

    // The SQL walks terminate too, and answer in the safe direction.
    let db = opdb.lock().unwrap();
    assert!(!db.subject_key_reachable("Member", "1").unwrap());
    assert!(db.descendant_key_count("Tenant", "7").unwrap() <= 2);
}

/// The schema refuses a row wrapped under neither a master nor a parent, and one wrapped
/// under both.
///
/// `Wrapping::Neither` exists in the code as a value rather than a panic, for an operator
/// who got a row into that state. This is the check that says they cannot: the constraint
/// is the database's rather than a rule this module remembers, so no path in or out of the
/// store has to re-establish it.
#[test]
fn a_key_row_is_wrapped_under_exactly_one_thing() {
    let (_dir, opdb, conn) = tamperable();
    let ks = KeyStore::new(Arc::clone(&opdb), MasterKeys::new(MASTER_KEY, vec![]));
    ks.encrypt_subject_in(&[("Member", "1"), ("Tenant", "7")], "email", "ada")
        .unwrap();

    let neither = conn.execute(
        "UPDATE subject_key SET parent_subject = NULL, parent_value = NULL, \
         parent_fingerprint = NULL WHERE subject = 'Member'",
        [],
    );
    assert!(neither.is_err(), "a row under nothing is unopenable");

    let both = conn.execute(
        "UPDATE subject_key SET master_key_id = 'm' WHERE subject = 'Member'",
        [],
    );
    assert!(both.is_err(), "a row under two things is ambiguous");

    let half = conn.execute(
        "UPDATE subject_key SET parent_fingerprint = NULL WHERE subject = 'Member'",
        [],
    );
    assert!(
        half.is_err(),
        "a parent without its generation is half a pointer"
    );
}

/// The sweep reaches a grandchild, which one pass cannot.
///
/// A grandchild is not an orphan while its parent's row is still there, so it only becomes
/// reclaimable once that parent has been swept. The loop repeats until a pass finds
/// nothing for exactly this reason, and a version that swept once would leave the deeper
/// rows on disk for ever.
#[test]
fn the_sweep_converges_on_a_grandchild() {
    let dir = tempfile::tempdir().unwrap();
    let opdb = Arc::new(Mutex::new(
        OpDb::open(&dir.path().join("hekla.db")).unwrap(),
    ));
    let ks = KeyStore::new(Arc::clone(&opdb), MasterKeys::new(MASTER_KEY, vec![]));
    ks.encrypt_subject_in(
        &[("Customer", "88"), ("Shop", "7"), ("Market", "1")],
        "email",
        "ada",
    )
    .unwrap();
    ks.erase("Market", "1").unwrap();

    // One call, however many passes it takes inside.
    let mut passes = 0;
    loop {
        let deleted = opdb
            .lock()
            .unwrap()
            .sweep_orphan_subject_keys(1000)
            .unwrap();
        passes += 1;
        if deleted == 0 {
            break;
        }
    }
    assert!(
        passes > 2,
        "the grandchild is only reachable after its parent is swept, so this needs more \
         than one pass to have proved anything: {passes}"
    );

    let db = opdb.lock().unwrap();
    assert!(
        !db.subject_key_exists("Shop", "7").unwrap(),
        "the child went"
    );
    assert!(
        !db.subject_key_exists("Customer", "88").unwrap(),
        "and so did the grandchild, which one pass could not have reached"
    );
}

/// Many writers, one hierarchy, erases landing in the middle of it.
///
/// Every individual race here has its own test; this is the one that runs them together and
/// asserts the property that matters at the end: whatever each writer was handed, the value
/// it sealed is either readable or the subject was erased under it. What must never happen
/// is a write reporting success over a key another thread then destroyed, which is
/// unrecoverable and silent.
#[test]
fn concurrent_writers_and_erasers_never_lose_a_key_they_reported_success_for() {
    let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
    let masters = MasterKeys::new(MASTER_KEY, vec![]);
    let sealed: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    thread::scope(|scope| {
        for worker in 0..12u32 {
            let opdb = Arc::clone(&opdb);
            let masters = masters.clone();
            let sealed = &sealed;
            scope.spawn(move || {
                let ks = KeyStore::new(opdb, masters);
                for round in 0..60u32 {
                    // Four members over two tenants, so writers collide constantly.
                    let member = ((worker + round) % 4).to_string();
                    let tenant = (round % 2).to_string();
                    let chain = [("Member", member.as_str()), ("Tenant", tenant.as_str())];
                    let text = format!("w{worker}r{round}");
                    if round % 7 == 3 {
                        // An erase in the middle of everyone else's writes.
                        ks.erase("Tenant", &tenant).unwrap();
                        continue;
                    }
                    let content = ks.encrypt_subject_in(&chain, "email", &text).unwrap();
                    sealed.lock().unwrap().push((member, content));
                }
            });
        }
    });

    // Nothing panicked and nothing errored, which is most of it. The rest: every ciphertext
    // either reads back or its subject is gone, and no read is an error.
    let ks = KeyStore::new(opdb, masters);
    let (mut readable, mut shredded) = (0, 0);
    for (member, content) in sealed.into_inner().unwrap() {
        match ks.decrypt_subject("Member", &member, "email", &content) {
            Ok(Some(_)) => readable += 1,
            Ok(None) => shredded += 1,
            Err(err) => {
                panic!("a concurrent erase must read as absent, never as a broken store: {err:#}")
            }
        }
    }
    // Both outcomes actually happened, so the run exercised the races rather than
    // serialising past them and asserting nothing.
    assert!(
        readable > 0,
        "no write survived, so nothing was really tested"
    );
    assert!(shredded > 0, "no erase landed, so nothing was really raced");
}

// --- a parent declared onto rows that already exist --------------------------

/// The load-bearing property: adoption keeps the secret, so everything already sealed
/// under the subject still reads.
///
/// A subject's secret is what its values are encrypted with. Adoption changes which key
/// that secret is *wrapped* under and must not touch the secret itself, so the ciphertext
/// written before the parent was declared has to come back byte-identical afterwards. The
/// implementation that gets this wrong is the one that mints a fresh key and reports
/// success, which shreds every value it claims to have protected.
#[test]
fn an_adoption_keeps_the_secret_so_everything_already_sealed_still_reads() {
    let ks = keystore();
    // Written while `Customer` was flat: a one-entry chain, so the row is a root.
    let before = ks
        .encrypt_subject_in(&[("Customer", "88")], "email", "ada@example.com")
        .unwrap();
    let also = ks
        .encrypt_subject_in(&[("Customer", "88")], "address", "12 Bell Lane")
        .unwrap();

    // `under Shop` is declared, and the fold reports customer 88 belongs to shop 7.
    assert_eq!(
        ks.adopt_in(&[("Customer", "88"), ("Shop", "7")]).unwrap(),
        Adopted::Moved,
        "a root of a now-parented subject moves"
    );

    assert_eq!(
        ks.decrypt_subject("Customer", "88", "email", &before)
            .unwrap()
            .as_deref(),
        Some("ada@example.com"),
        "the very bytes written before the declaration changed still open"
    );
    assert_eq!(
        ks.decrypt_subject("Customer", "88", "address", &also)
            .unwrap()
            .as_deref(),
        Some("12 Bell Lane"),
        "every field of it, not just the one that happened to be checked"
    );
}

/// And the point of the exercise: once adopted, the tenant's erase reaches it.
///
/// This is the failure the whole phase exists to remove. Before adoption, erasing shop 7
/// leaves customer 88 perfectly readable and reports success, which is a deletion that
/// quietly did less than it said.
#[test]
fn an_adopted_subject_is_shredded_by_the_tenant_that_now_owns_it() {
    let ks = keystore();
    let sealed = ks
        .encrypt_subject_in(&[("Customer", "88")], "email", "ada@example.com")
        .unwrap();

    // The gap, demonstrated first: a flat root is untouched by its future tenant.
    ks.erase("Shop", "7").unwrap();
    assert_eq!(
        ks.decrypt_subject("Customer", "88", "email", &sealed)
            .unwrap()
            .as_deref(),
        Some("ada@example.com"),
        "an unadopted root survives its tenant's erasure, which is the bug"
    );

    ks.adopt_in(&[("Customer", "88"), ("Shop", "7")]).unwrap();
    assert!(ks.erase("Shop", "7").unwrap(), "one row delete");
    assert_eq!(
        ks.decrypt_subject("Customer", "88", "email", &sealed)
            .unwrap(),
        None,
        "and now the tenant takes its customer with it"
    );
}

/// Adoption runs twice without doing anything the second time, and never touches a row
/// that is already a child.
///
/// Idempotence is what lets it run on every boot. The second half is the direction rule:
/// a row wrapped under shop 9 while the chain says shop 7 stays under shop 9, because a
/// parent that disagrees with the declaration is the documented "whichever event arrived
/// first" case and re-parenting it would fight that rule on every write.
#[test]
fn an_adoption_is_idempotent_and_never_repoints_an_existing_child() {
    let ks = keystore();
    ks.encrypt_subject_in(&[("Customer", "88")], "email", "ada")
        .unwrap();
    assert_eq!(
        ks.adopt_in(&[("Customer", "88"), ("Shop", "7")]).unwrap(),
        Adopted::Moved
    );
    assert_eq!(
        ks.adopt_in(&[("Customer", "88"), ("Shop", "7")]).unwrap(),
        Adopted::Settled,
        "the second pass finds a child and leaves it alone"
    );

    // Minted under shop 9 by a write, then offered shop 7 by a later event.
    let sealed = ks
        .encrypt_subject_in(&[("Customer", "99"), ("Shop", "9")], "email", "grace")
        .unwrap();
    assert_eq!(
        ks.adopt_in(&[("Customer", "99"), ("Shop", "7")]).unwrap(),
        Adopted::Settled,
        "a child is where it is, whatever a later chain says"
    );
    assert!(ks.erase("Shop", "7").unwrap());
    assert_eq!(
        ks.decrypt_subject("Customer", "99", "email", &sealed)
            .unwrap()
            .as_deref(),
        Some("grace"),
        "erasing the shop it was never under does not touch it"
    );
}

/// Adoption of a subject with no row does nothing, rather than creating one.
///
/// A key row is created by a write. Adoption is a repair of rows that already exist, and
/// minting here would resurrect a subject that was erased between the fold that listed it
/// and the write that acts on it.
#[test]
fn adopting_a_subject_with_no_key_creates_nothing() {
    let ks = keystore();
    assert_eq!(
        ks.adopt_in(&[("Customer", "404"), ("Shop", "7")]).unwrap(),
        Adopted::Settled,
        "nothing to adopt"
    );
    assert!(
        ks.decrypt_subject("Customer", "404", "email", "not-a-ciphertext")
            .unwrap()
            .is_none(),
        "and no key was conjured for it"
    );
}

/// The same project before `under` was declared. The event already carries the tenant id,
/// because something else on it is sealed under the tenant, so declaring the parent later
/// needs no new field at all: this is the migration as it actually looks.
const FLAT_EVENTS: &str = r#"
subject Tenant(Int)
subject Member(Int)

event @member.joined {
  member_id: Member,
  tenant_id: Tenant,
  email: String? @subject(member_id) @max(100),
  plan: String? @subject(tenant_id) @max(100),
}
"#;

fn flat_project() -> tempfile::TempDir {
    write_project(&[
        ("events/member.hk", FLAT_EVENTS),
        ("commands/join.hk", NESTED_COMMAND),
        ("projectors/members.hk", NESTED_PROJECTOR),
    ])
}

fn boot_at(project_dir: &Path, data_dir: &Path) -> Harness {
    Boot::new(project_dir)
        .http_status(200)
        .with_master_key()
        .data_dir(data_dir)
        .start()
}

/// Declaring a parent onto a project that already has key rows moves those rows, at boot,
/// before anything can read or write one.
///
/// This is the gap Phase 39 left open, end to end. Members written while `Member` was its
/// own root are wrapped under the master; after the declaration changes they hang from
/// their tenant, and erasing the tenant reaches them. Without the adoption the erase
/// reports success and every one of these rows stays readable, which is the failure a
/// deletion feature cannot have.
#[test]
fn declaring_a_parent_onto_a_running_project_adopts_the_keys_already_there() {
    let data = tempfile::tempdir().unwrap();

    // Boot one: `Member` is flat, so every member's key is a root under the master.
    let flat = flat_project();
    let harness = boot_at(flat.path(), data.path());
    join(&harness, 1, 7, "ada@example.com");
    join(&harness, 2, 7, "grace@example.com");
    // A member of a different tenant, so the cascade is shown to follow the hierarchy
    // rather than shredding every subject of that kind.
    join(&harness, 3, 9, "alan@example.com");
    assert_eq!(member(&harness, 1, 3)["email"], "ada@example.com");
    assert!(
        harness.rt.subject_key_exists("Member", "1").unwrap(),
        "the row exists, and at this point it is a root"
    );
    harness.shutdown();

    // Boot two: the same data directory, and now `Member under Tenant`.
    let nested = nested_project();
    let harness = boot_at(nested.path(), data.path());

    // Everything written before the declaration changed still reads. The secret moved
    // container, not value, which is what makes the adoption safe to do at all.
    assert_eq!(
        member(&harness, 1, 3)["email"],
        "ada@example.com",
        "a rewrap keeps the key, so nothing already sealed is lost"
    );
    assert_eq!(member(&harness, 3, 3)["email"], "alan@example.com");

    // And the tenant now owns them, which it did not an instant ago.
    let keystore = harness.rt.keystore().unwrap();
    assert!(keystore.erase("Tenant", "7").unwrap(), "one row delete");
    assert!(
        member(&harness, 1, 3).get("email").is_none(),
        "a member minted before the parent was declared is shredded with its tenant"
    );
    assert!(
        member(&harness, 2, 3).get("email").is_none(),
        "every one of them, not just the first"
    );
    assert_eq!(
        member(&harness, 3, 3)["email"],
        "alan@example.com",
        "and the other tenant's member is untouched"
    );
    harness.shutdown();
}

/// The harder migration: the parent's id was not on the event at all, and `@absent` is
/// what puts it there.
///
/// heklang refuses an event that seals under a child without carrying its ancestor, and
/// hekla refuses a boot that adds a required field to an event type with stored instances
/// unless it answers for the payloads already written. Together they mean the only way to
/// declare `under` onto a log is to say what the old events' parent is, which is exactly
/// what makes those rows adoptable rather than lost.
const UNPARENTED_EVENTS: &str = r#"
subject Member(Int)

event @member.joined {
  member_id: Member,
  email: String? @subject(member_id) @max(100),
}
"#;

const UNPARENTED_COMMAND: &str = r#"
command Join(member_id: Member, email: String?) {
  emit @member.joined { member_id, email }
}
"#;

const ADOPTED_EVENTS: &str = r#"
subject Tenant(Int)
subject Member(Int) under Tenant

event @member.joined {
  member_id: Member,
  tenant_id: Tenant @absent(1),
  email: String? @subject(member_id) @max(100),
}
"#;

const ADOPTED_COMMAND: &str = r#"
command Join(member_id: Member, tenant_id: Tenant, email: String?) {
  emit @member.joined { member_id, tenant_id, email }
}
"#;

const SIMPLE_PROJECTOR: &str = r#"
projector Members {
  entity Member {
    member_id: Member @key,
    email: String? @max(100),
  }

  on @member.joined { member_id, email } {
    put Member { member_id, email }
  }
}
"#;

fn unparented_project() -> tempfile::TempDir {
    write_project(&[
        ("events/member.hk", UNPARENTED_EVENTS),
        ("commands/join.hk", UNPARENTED_COMMAND),
        ("projectors/members.hk", SIMPLE_PROJECTOR),
    ])
}

fn adopted_project(events: &str) -> tempfile::TempDir {
    write_project(&[
        ("events/member.hk", events),
        ("commands/join.hk", ADOPTED_COMMAND),
        ("projectors/members.hk", SIMPLE_PROJECTOR),
    ])
}

#[test]
fn a_parent_id_the_old_events_never_carried_is_read_from_its_absent_value() {
    let data = tempfile::tempdir().unwrap();

    let before = unparented_project();
    let harness = boot_at(before.path(), data.path());
    harness
        .rt
        .execute(
            "Join",
            json!({ "member_id": 1, "email": "ada@example.com" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(member(&harness, 1, 1)["email"], "ada@example.com");
    harness.shutdown();

    // The same log, now read by a program that says those members belong to tenant 1.
    let after = adopted_project(ADOPTED_EVENTS);
    let harness = boot_at(after.path(), data.path());
    assert_eq!(
        member(&harness, 1, 1)["email"],
        "ada@example.com",
        "the value survives the move"
    );

    let keystore = harness.rt.keystore().unwrap();
    assert!(
        keystore.erase("Tenant", "1").unwrap(),
        "the tenant the absent value named"
    );
    assert!(
        member(&harness, 1, 1).get("email").is_none(),
        "and it reaches a member whose event never carried a tenant id"
    );
    harness.shutdown();
}

/// Without `@absent`, the boot refuses rather than adopting under a guess.
///
/// The load-bearing half of the design: adoption never invents a parent, because it never
/// has to. A declaration that does not say what the old payloads mean is refused before
/// any of this runs, by a check that was already there.
#[test]
fn declaring_a_parent_without_answering_for_the_old_payloads_is_refused() {
    let data = tempfile::tempdir().unwrap();

    let before = unparented_project();
    let harness = boot_at(before.path(), data.path());
    harness
        .rt
        .execute(
            "Join",
            json!({ "member_id": 1, "email": "ada@example.com" }),
            &ctx(),
            None,
        )
        .unwrap();
    harness.shutdown();

    let unanswered = ADOPTED_EVENTS.replace(" @absent(1)", "");
    let after = adopted_project(&unanswered);
    let err = Boot::new(after.path())
        .http_status(200)
        .with_master_key()
        .data_dir(data.path())
        .try_start()
        .err()
        .expect("a required field no stored payload carries is refused");
    let message = format!("{err:#}");
    assert!(
        message.contains("cannot read events that are already in the log"),
        "refused for the reason that makes adoption possible: {message}"
    );
    assert!(
        message.contains("tenant_id"),
        "and it names the field: {message}"
    );
}

/// The parent is the one the **earliest** event named, which is the rule a write would
/// have followed.
///
/// A key is minted once, under whichever event arrived first; `encryption.md` says so,
/// and an adoption that reconstructs that decision has to agree. Taking the latest
/// instead would file the key under a tenant whose erasure the runtime never promised.
#[test]
fn an_adoption_takes_the_parent_the_earliest_event_named() {
    let data = tempfile::tempdir().unwrap();
    let flat = flat_project();
    let harness = boot_at(flat.path(), data.path());
    // Member 1 twice, naming two different tenants. The rest are here so the fold still
    // has work to do when it reads the second one.
    join(&harness, 1, 7, "ada@example.com");
    join(&harness, 1, 9, "ada@example.com");
    join(&harness, 2, 7, "grace@example.com");
    join(&harness, 3, 7, "alan@example.com");
    harness.shutdown();

    let nested = nested_project();
    let harness = boot_at(nested.path(), data.path());
    let keystore = harness.rt.keystore().unwrap();

    keystore.erase("Tenant", "9").unwrap();
    assert_eq!(
        member(&harness, 1, 4)["email"],
        "ada@example.com",
        "the tenant a later event named does not hold this key"
    );
    keystore.erase("Tenant", "7").unwrap();
    assert!(
        member(&harness, 1, 4).get("email").is_none(),
        "the one the first event named does"
    );
    harness.shutdown();
}

fn adopt_cli(project: &Path, data: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("adopt")
        .arg(project)
        .arg("--data-dir")
        .arg(data)
        .arg("--no-progress")
        .env(
            "HEKLA_MASTER_KEY",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(MASTER_KEY),
        )
        .output()
        .unwrap()
}

/// `hekla adopt` does the same work ahead of a deploy, and says what it noticed on the
/// way.
///
/// The disagreement report is the only thing anywhere that looks at whether two events
/// agree about a subject's parent. It is a warning rather than a refusal because the
/// state is legal and documented, and because it is the expected shape of a migration:
/// members that existed before the change take the absent tenant, and any of them written
/// to afterwards name a real one too.
#[test]
fn the_adopt_command_moves_the_keys_and_reports_a_disagreement() {
    let data = tempfile::tempdir().unwrap();
    let flat = flat_project();
    let harness = boot_at(flat.path(), data.path());
    join(&harness, 1, 7, "ada@example.com");
    join(&harness, 1, 9, "ada@example.com");
    join(&harness, 2, 7, "grace@example.com");
    join(&harness, 3, 7, "alan@example.com");
    harness.shutdown();

    let nested = nested_project();
    let first = adopt_cli(nested.path(), data.path());
    let out = String::from_utf8_lossy(&first.stdout).into_owned();
    let err = String::from_utf8_lossy(&first.stderr).into_owned();
    assert!(first.status.success(), "adopt failed: {out}{err}");
    assert!(
        out.contains("adopted 3 subject key(s)"),
        "every member moved: {out}"
    );
    assert!(
        err.contains("sits under `Tenant` = `7`") && err.contains("under `Tenant` = `9`"),
        "the disagreement is named, both sides of it: {err}"
    );

    // Run again: nothing left, and nothing to say about it.
    let second = adopt_cli(nested.path(), data.path());
    let out = String::from_utf8_lossy(&second.stdout).into_owned();
    assert!(
        out.contains("already under its declared parent"),
        "the second pass has nothing to do: {out}"
    );

    // And the boot that follows agrees, which is the point of running it early.
    let harness = boot_at(nested.path(), data.path());
    assert_eq!(member(&harness, 1, 4)["email"], "ada@example.com");
    harness.rt.keystore().unwrap().erase("Tenant", "7").unwrap();
    assert!(member(&harness, 1, 4).get("email").is_none());
    harness.shutdown();
}

/// A root the log cannot account for refuses the boot rather than being served around.
///
/// Unreachable through the declarations: heklang requires the parent id on every event
/// that seals under a child, so a key row exists only because some event minted it and
/// that event carries the ancestor. The refusal is here because "unreachable" is a claim
/// about today's checks, and a key silently left under the master is exactly the failure
/// this phase exists to remove. Reached here by writing the row hekla would not write.
#[test]
fn a_root_no_event_accounts_for_refuses_the_boot() {
    let data = tempfile::tempdir().unwrap();
    let nested = nested_project();
    let harness = boot_at(nested.path(), data.path());
    join(&harness, 1, 7, "ada@example.com");
    harness.shutdown();

    // A `Member` root with no event naming its tenant. Nothing in hekla writes this.
    {
        let opdb = OpDb::open(&data.path().join("hekla.db")).unwrap();
        let masters = MasterKeys::new(MASTER_KEY, vec![]);
        let keystore = KeyStore::new(Arc::new(Mutex::new(opdb)), masters);
        keystore
            .encrypt_subject("Member", "404", "email", "nobody@example.com")
            .unwrap();
    }

    let err = Boot::new(nested.path())
        .http_status(200)
        .with_master_key()
        .data_dir(data.path())
        .try_start()
        .err()
        .expect("a key the hierarchy does not hold must not be served around");
    let message = format!("{err:#}");
    assert!(
        message.contains("no event in the log says which one"),
        "refused for the right reason: {message}"
    );
    assert!(
        message.contains("`Member` = `404`"),
        "and it names the row: {message}"
    );
}

/// Adoption running beside live writers and erasers never reports a broken store.
///
/// The realistic race, because `hekla adopt` is meant to run against a live server: the
/// deployment still serving the old declaration keeps minting roots while this moves
/// them. Three things then contend for one row, a writer that may mint it, an adopter
/// that moves it, and an eraser destroying the tenant it is being moved under, and each
/// has its own test above. This is the one that runs them together.
///
/// **The property is that no read is ever an `Err`**, which is a read API answering 500
/// and a projector wedging. What each ciphertext ends up as depends on an interleaving
/// this cannot pin, so the counts are not asserted: that an adopted key is shredded by
/// its tenant is [`an_adopted_subject_is_shredded_by_the_tenant_that_now_owns_it`]'s job,
/// where it is deterministic. What is asserted here is that the race really ran.
#[test]
fn adopting_beside_writers_and_erasers_never_reports_a_broken_store() {
    let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
    let masters = MasterKeys::new(MASTER_KEY, vec![]);
    let sealed: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    let moved = Mutex::new(0usize);

    // Seeded before the threads start, so an adopter has work from the first instant
    // rather than racing the writers for something to do. Without this the erasers drain
    // long before the first key moves and the run exercises nothing.
    {
        let seed = KeyStore::new(Arc::clone(&opdb), masters.clone());
        for member in 0..6u32 {
            let member = member.to_string();
            let content = seed
                .encrypt_subject_in(&[("Member", member.as_str())], "email", "seeded")
                .unwrap();
            sealed.lock().unwrap().push((member, content));
        }
    }

    thread::scope(|scope| {
        // Writers on the old declaration: a one-entry chain, so they mint roots.
        for worker in 0..4u32 {
            let opdb = Arc::clone(&opdb);
            let masters = masters.clone();
            let sealed = &sealed;
            scope.spawn(move || {
                let ks = KeyStore::new(opdb, masters);
                for round in 0..30u32 {
                    let member = ((worker + round) % 6).to_string();
                    let text = format!("w{worker}r{round}");
                    let content = ks
                        .encrypt_subject_in(&[("Member", member.as_str())], "email", &text)
                        .unwrap();
                    sealed.lock().unwrap().push((member, content));
                }
            });
        }
        // Adopters on the new one, moving those roots under their tenants.
        for _ in 0..2 {
            let opdb = Arc::clone(&opdb);
            let masters = masters.clone();
            let moved = &moved;
            scope.spawn(move || {
                let ks = KeyStore::new(opdb, masters);
                for round in 0..30u32 {
                    let member = (round % 6).to_string();
                    let tenant = (round % 2).to_string();
                    if ks
                        .adopt_in(&[("Member", member.as_str()), ("Tenant", tenant.as_str())])
                        .unwrap()
                        == Adopted::Moved
                    {
                        *moved.lock().unwrap() += 1;
                    }
                }
            });
        }
        // And an eraser destroying the tenants underneath all of it.
        {
            let opdb = Arc::clone(&opdb);
            let masters = masters.clone();
            scope.spawn(move || {
                let ks = KeyStore::new(opdb, masters);
                for round in 0..30u32 {
                    ks.erase("Tenant", &(round % 2).to_string()).unwrap();
                }
            });
        }
    });

    let ks = KeyStore::new(Arc::clone(&opdb), masters);
    for (member, content) in sealed.into_inner().unwrap() {
        if let Err(err) = ks.decrypt_subject("Member", &member, "email", &content) {
            panic!("an adoption racing an erase must never break a read: {err:#}");
        }
    }
    // The race really ran: keys moved, and a write after all of it still round-trips, so
    // the store is not merely quiet because everything in it is broken.
    assert!(
        *moved.lock().unwrap() > 0,
        "no adoption succeeded, so the thing under test never ran"
    );
    let after = ks
        .encrypt_subject_in(&[("Member", "9"), ("Tenant", "9")], "email", "after")
        .unwrap();
    assert_eq!(
        ks.decrypt_subject("Member", "9", "email", &after)
            .unwrap()
            .as_deref(),
        Some("after"),
        "the store still works when the race is over"
    );
}

/// The witness is an event that actually **minted** the key, not merely one that mentions
/// the subject.
///
/// Rule 12: an absent optional was never encrypted, so a sealed field holding nothing
/// mints no key and `HeklaHost::lower` skips it. An event like that names a parent while
/// having filed nothing under it, so treating it as the witness files the key under a
/// tenant the write path never chose, and the fold then stops before ever reading the
/// event that did the minting. Erasing the real tenant leaves the content readable, with
/// nothing anywhere saying so, which is the failure this whole phase exists to remove.
#[test]
fn an_event_that_sealed_nothing_does_not_decide_where_the_key_goes() {
    let data = tempfile::tempdir().unwrap();
    let flat = flat_project();
    let harness = boot_at(flat.path(), data.path());
    // Mentions tenant 7 and seals nothing under member 1, so member 1 gets no key here.
    harness
        .rt
        .execute(
            "Join",
            json!({ "member_id": 1, "tenant_id": 7, "email": null, "plan": "pro" }),
            &ctx(),
            None,
        )
        .unwrap();
    // This one mints it, under tenant 9.
    join(&harness, 1, 9, "ada@example.com");
    harness.shutdown();

    let nested = nested_project();
    let harness = boot_at(nested.path(), data.path());
    let keystore = harness.rt.keystore().unwrap();

    keystore.erase("Tenant", "7").unwrap();
    assert_eq!(
        member(&harness, 1, 2)["email"],
        "ada@example.com",
        "the tenant named by an event that sealed nothing does not hold this key"
    );
    keystore.erase("Tenant", "9").unwrap();
    assert!(
        member(&harness, 1, 2).get("email").is_none(),
        "the tenant named by the event that minted it does"
    );
    harness.shutdown();
}

/// `hekla plan` counts the keys a deploy would move, before it moves them.
///
/// The adoption is work the boot does to stored data and cannot be skipped, so a gate
/// that reads plans wants it in front of the deploy rather than in a log afterwards. It
/// also has to make the plan non-empty: "nothing would change" about a boot that is about
/// to rewrap rows is the one answer this must not give.
#[test]
fn a_plan_counts_the_keys_a_deploy_would_move() {
    let data = tempfile::tempdir().unwrap();
    let flat = flat_project();
    let harness = boot_at(flat.path(), data.path());
    join(&harness, 1, 7, "ada@example.com");
    join(&harness, 2, 7, "grace@example.com");
    harness.shutdown();

    let nested = nested_project();
    let project = hekla::loader::LoadedProject::load(nested.path());
    let plan = hekla::plan::compute_with(&project, data.path(), hekla::plan::Replay::Off).unwrap();
    assert_eq!(plan.adoptions, 2, "both members are waiting");
    assert!(
        !plan.is_empty(),
        "a deploy that rewraps rows is not 'nothing would change'"
    );
    assert_eq!(
        plan.json()["adoptions"],
        2,
        "and a gate reading the json sees it too"
    );
    assert!(
        format!("{plan}").contains("would move under the parent"),
        "and so does an operator reading it: {plan}"
    );

    // Once they have moved, the same plan says nothing about them.
    adopt_cli(nested.path(), data.path());
    let after = hekla::plan::compute_with(&project, data.path(), hekla::plan::Replay::Off).unwrap();
    assert_eq!(after.adoptions, 0);
    assert!(!format!("{after}").contains("would move under the parent"));
}

/// `hekla verify` reports keys that have not moved, and never moves them.
///
/// An audit that advances state cannot be re-run to check its own answer, which is why
/// `open_quiescent` starts no threads either. The violation has to survive the sweep.
#[test]
fn verify_reports_keys_that_have_not_moved_without_moving_them() {
    let data = tempfile::tempdir().unwrap();
    let flat = flat_project();
    let harness = boot_at(flat.path(), data.path());
    join(&harness, 1, 7, "ada@example.com");
    harness.shutdown();

    let nested = nested_project();
    let project = hekla::loader::LoadedProject::load(nested.path());
    let report = hekla::verify::sweep(
        &project,
        data.path(),
        Some(MasterKeys::new(MASTER_KEY, vec![])),
    )
    .unwrap();
    let named = report
        .violations
        .iter()
        .map(|violation| violation.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        named.contains("wrapped under the master although their subject declares a parent"),
        "the sweep reports the unadopted key: {named}"
    );
    assert!(
        named.contains("`Member`"),
        "and names the subject to look at: {named}"
    );

    // Still there, so the audit reported rather than repaired.
    let harness = boot_at(nested.path(), data.path());
    assert_eq!(member(&harness, 1, 1)["email"], "ada@example.com");
    harness.shutdown();
}

/// The audit names the subjects that actually have rows waiting, not every subject that
/// declares a parent.
///
/// An operator reading a verify failure has to know where to look. Listing every parented
/// subject sends them to ones that are fine, and makes the count-to-name ratio meaningless.
#[test]
fn the_audit_names_only_the_subjects_with_keys_waiting() {
    let dir = write_project(&[(
        "events/thing.hk",
        r#"
subject Tenant(Int)
subject Member(Int) under Tenant
subject Device(Int) under Tenant

event @member.joined {
  member_id: Member,
  device_id: Device,
  tenant_id: Tenant,
  email: String? @subject(member_id) @max(100),
  serial: String? @subject(device_id) @max(100),
}
"#,
    )]);
    let project = hekla::loader::LoadedProject::load(dir.path());
    let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
    let keystore = KeyStore::new(opdb, MasterKeys::new(MASTER_KEY, vec![]));
    // A root for one of the two parented subjects, and a healthy child for the other.
    keystore
        .encrypt_subject("Member", "1", "email", "ada@example.com")
        .unwrap();
    keystore
        .encrypt_subject_in(&[("Device", "9"), ("Tenant", "7")], "serial", "sn-1")
        .unwrap();

    let waiting = hekla::adopt::waiting_by_subject(&project.program, &keystore).unwrap();
    assert_eq!(
        waiting,
        vec![("Member".to_owned(), 1)],
        "only the subject with a root, and `Device` is not dragged in with it"
    );
}

/// A run that resolved everything and moved nothing is **not** settled.
///
/// The predicate was the bug: it asked its own bookkeeping instead of the store.
/// `adopt_in` legitimately declines rows (a compare-and-set lost, a row erased since the
/// scan, an adoption undone because its parent went mid-flight), and a declined subject is
/// gone from `unresolved` while still sitting under the master. Reading that as "done"
/// serves, or exits zero, on exactly the state this phase exists to prevent.
#[test]
fn a_run_that_left_rows_under_the_master_is_not_settled() {
    let moved_nothing = hekla::adopt::Adoption {
        adopted: 0,
        waiting: 3,
        remaining: 3,
        ..Default::default()
    };
    assert!(
        !moved_nothing.settled(),
        "rows are still under the master, whatever the fold resolved"
    );
    let unfinished = hekla::adopt::unfinished(&moved_nothing).expect("it has to say so");
    assert!(
        matches!(unfinished, hekla::adopt::Unfinished::Contended(3)),
        "and say which of the two endings it is, because they need different answers: {unfinished:?}"
    );
    assert!(
        unfinished
            .to_string()
            .contains("still wrapped under the master")
    );

    let unaccounted = hekla::adopt::Adoption {
        unresolved: vec![("Member".to_owned(), "1".to_owned())],
        // Still a root in the store. Without this the run is settled whatever the fold
        // could not place, which is the point of asking the store first.
        remaining: 1,
        ..Default::default()
    };
    let unfinished = hekla::adopt::unfinished(&unaccounted).expect("this one too");
    assert!(matches!(
        unfinished,
        hekla::adopt::Unfinished::Unaccounted { .. }
    ));
    let said = unfinished.to_string();
    assert!(
        said.contains("no event in the log says which one") && said.contains("`Member` = `1`"),
        "the other ending names the row and the repair: {said}"
    );

    assert!(hekla::adopt::Adoption::default().settled());
    assert!(hekla::adopt::unfinished(&hekla::adopt::Adoption::default()).is_none());
}

/// Adoption reaches through a whole chain, not just one link.
///
/// `Customer under Shop under Market`: moving the customer needs the shop's secret, and
/// if the shop has no row yet that needs the market's, so the adoption mints both on the
/// way. Depth one is the case where "wrap under the parent" and "wrap under the root"
/// coincide and cannot tell a recursive implementation from a one-level one. This can:
/// erasing the **market** has to reach a customer two links away.
#[test]
fn an_adoption_reaches_through_a_whole_chain() {
    let ks = keystore();
    // Written while `Customer` was flat, so its row is a root under the master.
    let sealed = ks
        .encrypt_subject_in(&[("Customer", "88")], "email", "ada@example.com")
        .unwrap();
    assert_eq!(
        ks.adopt_in(&[("Customer", "88"), ("Shop", "7"), ("Market", "1")])
            .unwrap(),
        Adopted::Moved,
        "a two-link chain is adopted in one move"
    );
    assert_eq!(
        ks.decrypt_subject("Customer", "88", "email", &sealed)
            .unwrap()
            .as_deref(),
        Some("ada@example.com"),
        "and the secret came with it"
    );

    // The shop was minted on the way, under the market, so one delete at the top reaches
    // both. If the adoption had wrapped the customer under a root shop, this would leave
    // the customer readable.
    assert!(ks.erase("Market", "1").unwrap(), "one row delete");
    assert_eq!(
        ks.decrypt_subject("Customer", "88", "email", &sealed)
            .unwrap(),
        None,
        "the customer is two links below the market and goes with it"
    );
}

/// The disagreement report is one line per subject, and bounded.
///
/// Two events naming different parents is the *expected* shape of a migration, not a rare
/// fault: rows from before the change take the `@absent` tenant and anything written after
/// names a real one. So a report that grew with the events rather than with the subjects
/// would put one warning line on an operator's screen per event, and a subject argued over
/// ten times would be named ten times. Both of those were true when this was first
/// written.
///
/// The events are interleaved per member on purpose. The scan stops once every waiting key
/// has a parent, so a run of "first events" followed by a run of "second events" would
/// resolve everything and halt before noticing any disagreement at all.
#[test]
fn the_disagreement_report_names_each_subject_once_and_stops() {
    let data = tempfile::tempdir().unwrap();
    let flat = flat_project();
    let harness = boot_at(flat.path(), data.path());
    // Member 0 is argued over three times, so a report that did not deduplicate would
    // name it twice.
    join(&harness, 0, 1, "m0@example.com");
    join(&harness, 0, 2, "m0@example.com");
    join(&harness, 0, 3, "m0@example.com");
    for member in 1..25u64 {
        join(&harness, member, 1, &format!("m{member}@example.com"));
        join(&harness, member, 2, &format!("m{member}@example.com"));
    }
    harness.shutdown();

    let nested = nested_project();
    let run = adopt_cli(nested.path(), data.path());
    let err = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        run.status.success(),
        "a disagreement is a warning, not a refusal: {err}"
    );

    let warnings: Vec<&str> = err
        .lines()
        .filter(|line| line.contains("sits under"))
        .collect();
    assert!(
        !warnings.is_empty(),
        "the run has to have noticed some, or this asserts nothing: {err}"
    );
    assert!(
        warnings.len() <= 20,
        "the list is capped, so a migration-shaped log cannot flood the terminal: {} lines",
        warnings.len()
    );

    let named: Vec<&str> = warnings
        .iter()
        .filter_map(|line| line.split("` = `").nth(1))
        .filter_map(|rest| rest.split('`').next())
        .collect();
    let distinct: BTreeSet<&&str> = named.iter().collect();
    assert_eq!(
        named.len(),
        distinct.len(),
        "each subject is named once however many events argue over it: {named:?}"
    );
}

/// An adoption whose ancestor is erased under it puts the row back, rather than leaving
/// it hanging from a generation that no longer exists.
///
/// The window is between minting the ancestors and committing the move: the row lands
/// wrapped under a parent that has just been destroyed, so everything already sealed under
/// that subject becomes unreadable, and it was readable a moment earlier. That is data
/// lost to an erase nobody asked to reach it. The secret is still in hand at that point,
/// so the move is undoable, and undoing it is the only answer that cannot lose anything.
///
/// Raced rather than contrived, because nothing outside the function can reach that
/// window. The chain is two links deep on purpose: erasing the **root** is wrong for the
/// whole span between minting it and the commit, where erasing the immediate parent is
/// only wrong for the tail of it, so a round that is raced at all lands in the window
/// almost every time rather than by luck.
///
/// **The eraser runs until the adopter stops**, rather than for a round count of its own.
/// Matching counts read as fair and were not: erasing a subject that is not there costs
/// one lookup, so the eraser spent all 3000 of its rounds while the adopter got through
/// about thirty, and the window it exists to hold open was shut for the rest of the run.
/// Losing that start by a scheduling hair shut the window before the first round, and the
/// test then failed for want of a race rather than for a bug, which is how it read in CI.
#[test]
fn an_adoption_whose_ancestor_is_erased_under_it_puts_the_row_back() {
    let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
    let masters = MasterKeys::new(MASTER_KEY, vec![]);
    let taken_back: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    // Several undos rather than one, so a pass says the path runs rather than that it once
    // did. The rounds are the adopter's patience rather than its workload: with the window
    // held open the fifth lands inside the first few hundred, even with both threads on one
    // core.
    let wanted = 5usize;
    let rounds = 3000u32;
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        {
            let opdb = Arc::clone(&opdb);
            let masters = masters.clone();
            let stop = &stop;
            scope.spawn(move || {
                let ks = KeyStore::new(opdb, masters);
                while !stop.load(Ordering::Relaxed) {
                    // One lookup here against a round of minting, wrapping and reading back
                    // over there, so a runner with a single core to give would spend most
                    // of it in this loop without the yield.
                    thread::yield_now();
                    ks.erase("Region", "1").unwrap();
                }
            });
        }
        {
            let opdb = Arc::clone(&opdb);
            let masters = masters.clone();
            let taken_back = &taken_back;
            let stop = &stop;
            scope.spawn(move || {
                let ks = KeyStore::new(opdb, masters);
                for round in 0..rounds {
                    // A fresh root each round, written while the subject was still flat.
                    let member = round.to_string();
                    let sealed = ks
                        .encrypt_subject_in(&[("Member", member.as_str())], "email", "ada")
                        .unwrap();
                    let outcome = ks
                        .adopt_in(&[
                            ("Member", member.as_str()),
                            ("Tenant", "7"),
                            ("Region", "1"),
                        ])
                        .unwrap();
                    if outcome == Adopted::Undone {
                        let mut taken_back = taken_back.lock().unwrap();
                        taken_back.push((member, sealed));
                        if taken_back.len() >= wanted {
                            break;
                        }
                    }
                }
                // However the loop ended, the eraser is waiting on this to go home.
                stop.store(true, Ordering::Relaxed);
            });
        }
    });

    let taken_back = taken_back.into_inner().unwrap();
    assert!(
        !taken_back.is_empty(),
        "no adoption was undone in {rounds} rounds, so the path under test never ran"
    );
    // The whole point: what the adoption took back is still readable. Left hanging from
    // the erased region instead, every one of these would read as shredded, by an erase
    // that was never asked to reach them.
    let ks = KeyStore::new(opdb, masters);
    for (member, sealed) in &taken_back {
        assert_eq!(
            ks.decrypt_subject("Member", member, "email", sealed)
                .unwrap()
                .as_deref(),
            Some("ada"),
            "`Member` = `{member}` was put back under the master, so its content survived"
        );
    }
}

/// A run against a directory something else is writing to either settles it or says it
/// did not, and a run with nothing else writing settles it.
///
/// This is the contract the pass loop exists for. One pass can move less than it
/// resolved, because an eraser can take the tenant out from under a row between the scan
/// and the write, so the run looks again. Bounded, so a directory under continuous erasure
/// reports what is left rather than spinning; and what is left is the refusal, never a
/// quiet success. The disjunction is asserted rather than the branch, because which one a
/// run takes depends on an interleaving no test can pin.
#[test]
fn adopting_beside_an_eraser_either_settles_or_says_it_did_not() {
    let data = tempfile::tempdir().unwrap();
    let flat = flat_project();
    let harness = boot_at(flat.path(), data.path());
    for member in 1..8u64 {
        join(&harness, member, 7, &format!("m{member}@example.com"));
    }
    harness.shutdown();

    let nested = nested_project();
    let stop = Arc::new(Mutex::new(false));
    let erasing = {
        let stop = Arc::clone(&stop);
        let path = data.path().join("hekla.db");
        thread::spawn(move || {
            let opdb = Arc::new(Mutex::new(OpDb::open(&path).unwrap()));
            let ks = KeyStore::new(opdb, MasterKeys::new(MASTER_KEY, vec![]));
            while !*stop.lock().unwrap() {
                ks.erase("Tenant", "7").unwrap();
            }
        })
    };

    let contended = adopt_cli(nested.path(), data.path());
    *stop.lock().unwrap() = true;
    erasing.join().unwrap();

    let err = String::from_utf8_lossy(&contended.stderr).into_owned();
    assert!(
        contended.status.success(),
        "contention is the normal condition of a pre-flight, not a failure: {err}"
    );

    // Nothing writing now, so this one has to settle it.
    let quiet = adopt_cli(nested.path(), data.path());
    let out = String::from_utf8_lossy(&quiet.stdout).into_owned();
    let err = String::from_utf8_lossy(&quiet.stderr).into_owned();
    assert!(quiet.status.success(), "{out}{err}");
    let settled = adopt_cli(nested.path(), data.path());
    assert!(
        String::from_utf8_lossy(&settled.stdout).contains("already under its declared parent"),
        "and leaves nothing behind for the next one: {}",
        String::from_utf8_lossy(&settled.stdout)
    );

    // Asked of the store rather than of the command's own report, because "settled" is a
    // claim about where the keys are: the tenant now reaches every member, which is the
    // thing the whole run was for and the thing a clean exit would otherwise only assert
    // about itself.
    let harness = boot_at(nested.path(), data.path());
    let readable = (1..8u64)
        .filter(|id| member(&harness, *id, 7).get("email").is_some())
        .count();
    harness.rt.keystore().unwrap().erase("Tenant", "7").unwrap();
    let survivors = (1..8u64)
        .filter(|id| member(&harness, *id, 7).get("email").is_some())
        .count();
    assert_eq!(
        survivors, 0,
        "after the tenant goes, none of the {readable} readable members is left"
    );
    harness.shutdown();
}

/// A middle subject is adopted even when nothing seals directly under it.
///
/// `Member under Tenant` already deployed, so tenant rows exist as roots minted on the way
/// to a member's key. Declaring `Tenant under Region` makes those roots the ones waiting,
/// and the events that would say where they belong seal under **Member**, not under
/// Tenant: their chain is `[Member, Tenant, Region]` and the tenant is a link in the
/// middle of it. A fold that only reads the head of a chain never resolves them, and the
/// boot then refuses for ever with advice the author has already followed.
const MEMBER_ONLY_EVENTS: &str = r#"
subject Tenant(Int)
subject Member(Int) under Tenant

event @member.joined {
  member_id: Member,
  tenant_id: Tenant,
  email: String? @subject(member_id) @max(100),
}
"#;

const REGION_EVENTS: &str = r#"
subject Region(Int)
subject Tenant(Int) under Region
subject Member(Int) under Tenant

event @member.joined {
  member_id: Member,
  tenant_id: Tenant,
  region_id: Region @absent(1),
  email: String? @subject(member_id) @max(100),
}
"#;

const MEMBER_ONLY_COMMAND: &str = r#"
command Join(member_id: Member, tenant_id: Tenant, email: String?) {
  emit @member.joined { member_id, tenant_id, email }
}
"#;

const REGION_COMMAND: &str = r#"
command Join(member_id: Member, tenant_id: Tenant, region_id: Region, email: String?) {
  emit @member.joined { member_id, tenant_id, region_id, email }
}
"#;

fn middle_project(events: &str, command: &str) -> tempfile::TempDir {
    write_project(&[
        ("events/member.hk", events),
        ("commands/join.hk", command),
        ("projectors/members.hk", SIMPLE_PROJECTOR),
    ])
}

#[test]
fn a_subject_that_only_ever_appears_as_an_ancestor_is_still_adopted() {
    let data = tempfile::tempdir().unwrap();
    let before = middle_project(MEMBER_ONLY_EVENTS, MEMBER_ONLY_COMMAND);
    let harness = boot_at(before.path(), data.path());
    harness
        .rt
        .execute(
            "Join",
            json!({ "member_id": 1, "tenant_id": 7, "email": "ada@example.com" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(member(&harness, 1, 1)["email"], "ada@example.com");
    harness.shutdown();

    // A region above the tenant. Nothing seals under `Tenant`, so the only events that
    // name a region carry it as the *last* link of a member's chain.
    let after = middle_project(REGION_EVENTS, REGION_COMMAND);
    let harness = boot_at(after.path(), data.path());
    assert_eq!(
        member(&harness, 1, 1)["email"],
        "ada@example.com",
        "the boot adopted the tenant rather than refusing"
    );

    let keystore = harness.rt.keystore().unwrap();
    assert!(keystore.erase("Region", "1").unwrap(), "one row delete");
    assert!(
        member(&harness, 1, 1).get("email").is_none(),
        "and the region now reaches a member two links below it"
    );
    harness.shutdown();
}

/// A run the key store could not act on says so, rather than reading as contention.
///
/// The three endings send an operator to three different places, and two of them are
/// faults. A run where every row errored leaves `remaining` high exactly as a contended
/// one does, so collapsing them told whoever ran `hekla adopt` to go looking for a second
/// process, and exited zero while doing it: a deploy gate went green over a store that
/// could not be adopted at all. Worse, the error text only ever reached a `tracing::warn!`
/// that this command installs no subscriber for, so it went nowhere.
#[test]
fn a_run_that_could_not_move_a_row_is_not_reported_as_contention() {
    let failed = hekla::adopt::Adoption {
        waiting: 3,
        remaining: 3,
        failed: 3,
        first_failure: Some("the key store is a wreck".to_owned()),
        ..Default::default()
    };
    let unfinished = hekla::adopt::unfinished(&failed).expect("it has to say so");
    assert!(
        matches!(unfinished, hekla::adopt::Unfinished::Failed { .. }),
        "a failure is not a race: {unfinished:?}"
    );
    let said = unfinished.to_string();
    assert!(
        said.contains("the key store is a wreck"),
        "and it names what actually went wrong: {said}"
    );
    assert!(
        !said.contains("Something else is writing"),
        "rather than sending them after a writer that is not there: {said}"
    );

    // Contention still reads as contention when nothing errored.
    let contended = hekla::adopt::Adoption {
        waiting: 3,
        remaining: 3,
        ..Default::default()
    };
    assert!(matches!(
        hekla::adopt::unfinished(&contended),
        Some(hekla::adopt::Unfinished::Contended(3))
    ));
}

/// A row that failed once and moved on the next look is not a fault.
///
/// The bookkeeping and the store can disagree, and the store is right. A pass that errored
/// on a row it later adopted left `failed` set for the whole run, so a fully adopted store
/// reported a fault: `hekla adopt` exited non-zero and, worse, the **boot refused to
/// start** on a store where every key was exactly where the declaration said. Asking
/// `remaining` first is what makes `settled()`'s own doc ("asked of the store, not of the
/// bookkeeping") true, after two rounds of it not being.
#[test]
fn a_failure_that_the_next_pass_resolved_is_not_reported() {
    let recovered = hekla::adopt::Adoption {
        waiting: 10,
        adopted: 10,
        remaining: 0,
        failed: 1,
        first_failure: Some("lost a race in the first pass".to_owned()),
        ..Default::default()
    };
    assert!(
        recovered.settled(),
        "nothing is left under the master, so the run is done"
    );
    assert!(
        hekla::adopt::unfinished(&recovered).is_none(),
        "and it must not refuse a boot over a pass that has since been made good"
    );

    // Still left, and a failure is still not a race.
    let stuck = hekla::adopt::Adoption {
        remaining: 1,
        failed: 1,
        first_failure: Some("the key store is a wreck".to_owned()),
        ..Default::default()
    };
    assert!(matches!(
        hekla::adopt::unfinished(&stuck),
        Some(hekla::adopt::Unfinished::Failed { .. })
    ));
}
