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

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use hekla::crypto::{KeyStore, MasterKeys};
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
        .erase("customer_id", "42")
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
command CopyOrder(order_id: Uuid, customer_id: Int) {
  // Folds this customer's own address. The variable is sealed under `customer_id`,
  // and the emit below writes it into a field sealed under the same subject, so it
  // moves without ever being read.
  state email: String? = fold none
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
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
  shop_id: Int,
  email: String? @subject(customer_id) @max(100),
  contact: String? @subject(shop_id) @max(100),
}
"#,
            ),
            (
                "commands/copy-order.hk",
                r#"
command CopyOrder(order_id: Uuid, customer_id: Int, shop_id: Int) {
  state email: String? = fold none
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
    let _ = read_row(&harness, "Orders", "Order", ORDER, 1);
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
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
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
    customer_id: Int @index,
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
        .erase("customer_id", "42")
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
        ks.erase("account_id", ALICE).unwrap(),
        "the subject key must exist to be erased"
    );
    assert!(
        ks.encrypt_subject_existing("account_id", ALICE, "email", "alice@example.com")
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
        .erase("customer_id", "42")
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
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
  email: String? @subject(customer_id) @max(100),
  order_total: Money(2)? @subject(customer_id),
  loyalty_points: Int? @subject(customer_id),
}
"#;

const TYPED_PLACE_ORDER: &str = r#"
command PlaceOrder(
  order_id: Uuid,
  customer_id: Int,
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
    customer_id: Int @index,
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
        .erase("customer_id", "42")
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
  on @order.placed { customer_id } {
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
  on @order.placed { customer_id } {
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
