//! Subject-scoped encryption end to end: a command emits an event with a
//! subject-encrypted field, so the field is stored as ciphertext (in the tag index,
//! the payload, and the read model), the opaque handle keeps plaintext out of a
//! projector, and the command response never reports the encrypted value.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kiln::context::CommandContext;
use kiln::crypto::MasterKeys;
use kiln::effect::{EffectRuntime, HttpClient, StubHttpClient};
use kiln::loader::LoadedProject;
use kiln::projector::ProjectorSet;
use kiln::runtime::Runtime;
use serde_json::{Value, json};
use tempfile::TempDir;
use tephra::WriteCoordinator;
use uuid::Uuid;

/// A fixed, non-secret master key for the tests.
const MASTER: [u8; 32] = [0x11; 32];

struct Harness {
    rt: Arc<Runtime>,
    coord: WriteCoordinator,
    projectors: ProjectorSet,
    effects: EffectRuntime,
    _data: TempDir,
}

impl Harness {
    fn shutdown(self) {
        self.effects.shutdown_and_join();
        self.projectors.shutdown_and_join();
        self.coord.shutdown();
    }
}

fn write_project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

/// An orders project: `order.placed` carries a customer-scoped `email`, projected
/// into an `orders` read model that stores the ciphertext.
fn orders_project() -> TempDir {
    write_project(&[
        (
            "events/order.star",
            r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": u64_(),
        "email": text(subject = "customer_id", max_length = 100),
    },
)
"#,
        ),
        (
            "commands/place-order.star",
            r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = u64_(), email = text())

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#,
        ),
        (
            "projectors/orders.star",
            r#"
load("events/order.star", "order_placed")

orders = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": u64_(),
        "email": text(subject = "customer_id", max_length = 100),
    },
)

source = [order_placed()]

def handle(event):
    return [put(orders, {
        "order_id": event.data["order_id"],
        "customer_id": event.data["customer_id"],
        "email": event.data["email"],
    })]
"#,
        ),
    ])
}

fn boot(project_dir: &Path, master: Option<MasterKeys>) -> anyhow::Result<Harness> {
    boot_http(project_dir, master, Arc::new(StubHttpClient::status(200)))
}

fn boot_http(
    project_dir: &Path,
    master: Option<MasterKeys>,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<Harness> {
    let project = LoadedProject::load(project_dir);
    assert!(!project.has_errors(), "{:?}", project.findings);
    let data = tempfile::tempdir().unwrap();
    let (rt, coord, projectors, effects) = Runtime::open(project, data.path(), http, master)?;
    Ok(Harness {
        rt,
        coord,
        projectors,
        effects,
        _data: data,
    })
}

/// Read one row from a projector's read model through the read API (which decrypts
/// subject columns), waiting for the projector to catch up first.
fn read_row(harness: &Harness, projector: &str, entity: &str, key: &str) -> Option<Value> {
    for _ in 0..200 {
        if harness.rt.projector(projector).unwrap().position() >= 1 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let shared = harness.rt.projector(projector).unwrap();
    let entity_def = shared.entities.iter().find(|e| e.name == entity).unwrap();
    let (row, _position) =
        kiln::read_api::get_one(&shared.db_path, entity_def, key, harness.rt.keystore()).unwrap();
    row
}

fn place_order(rt: &Runtime, order_id: &str, customer_id: u64, email: &str) -> Value {
    let ctx = CommandContext::new(Uuid::new_v4());
    let body = json!({ "order_id": order_id, "customer_id": customer_id, "email": email });
    let result = rt.execute("place-order", body, &ctx, None).unwrap();
    assert_eq!(result.status, 200, "place-order failed: {:?}", result.body);
    result.body
}

const ORDER: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

#[test]
fn boot_without_a_master_key_fails_when_a_project_uses_subjects() {
    let dir = orders_project();
    let err = match boot(dir.path(), None) {
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let harness = boot(&root, None).expect("boots without a master key");
    harness.shutdown();
}

#[test]
fn the_command_response_omits_the_subject_field_tag() {
    let dir = orders_project();
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    let body = place_order(&harness.rt, ORDER, 42, "alice@example.com");

    let tags = body["events"][0]["tags"].as_array().unwrap();
    let tag_strings: Vec<&str> = tags.iter().map(|t| t.as_str().unwrap()).collect();
    // Plaintext tags for the non-subject indexed fields are reported.
    assert!(
        tag_strings
            .iter()
            .any(|t| *t == format!("order_id:{ORDER}"))
    );
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
    let dir = write_project(&[
        (
            "events/order.star",
            r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": u64_(),
        "email": text(subject = "customer_id", max_length = 100),
    },
)
"#,
        ),
        (
            "commands/place-order.star",
            r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = u64_(), email = text())

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#,
        ),
        (
            "projectors/leaky.star",
            r#"
load("events/order.star", "order_placed")

leaky = entity(key = "order_id", fields = {"order_id": uuid(), "domain": text()})

source = [order_placed()]

def handle(event):
    # Deriving plaintext from the handle: not allowed, so this errors.
    return [put(leaky, {"order_id": event.data["order_id"], "domain": event.data["email"] + "!"})]
"#,
        ),
    ]);
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    let mut failed = false;
    for _ in 0..200 {
        if harness.rt.projector("leaky").unwrap().failed() {
            failed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        failed,
        "the projector should fail rather than derive plaintext"
    );

    harness.shutdown();
}

#[test]
fn the_projector_stores_ciphertext_for_the_subject_column() {
    let dir = orders_project();
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    // Wait for the projector to apply the event.
    for _ in 0..200 {
        if harness.rt.projector("orders").unwrap().position() >= 1 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Read the read model directly: the stored email column is ciphertext, never the
    // plaintext (the read API's decrypt lands in a later phase).
    let shared = harness.rt.projector("orders").unwrap();
    let model = kiln::read_model::ReadModel::open_readonly(&shared.db_path).unwrap();
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
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    let row = read_row(&harness, "orders", "orders", ORDER).expect("a row");
    // The read API decrypts on the way out: the caller sees plaintext, not ciphertext.
    assert_eq!(row["email"], "alice@example.com");
    assert_eq!(row["customer_id"].as_i64(), Some(42));

    harness.shutdown();
}

#[test]
fn erasing_a_subject_shreds_the_read_model_and_the_log() {
    let dir = orders_project();
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    // Before erasure the read API returns the plaintext.
    let row = read_row(&harness, "orders", "orders", ORDER).expect("a row");
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
    let shared = harness.rt.projector("orders").unwrap();
    let entity_def = shared.entities.iter().find(|e| e.name == "orders").unwrap();
    let (row, _) =
        kiln::read_api::get_one(&shared.db_path, entity_def, ORDER, harness.rt.keystore()).unwrap();
    let row = row.expect("the order row still exists");
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
    let dir = write_project(&[
        (
            "events/order.star",
            r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": u64_(),
        "email": text(subject = "customer_id", max_length = 100),
    },
)
"#,
        ),
        (
            "commands/place-order.star",
            r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = u64_(), email = text())

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#,
        ),
        (
            "projectors/leak.star",
            r#"
load("events/order.star", "order_placed")

leak = entity(key = "order_id", fields = {"order_id": uuid(), "note": text()})

source = [order_placed()]

def handle(event):
    # `note` is a plaintext column; storing the encrypted handle there is rejected.
    return [put(leak, {"order_id": event.data["order_id"], "note": event.data["email"]})]
"#,
        ),
    ]);
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let mut failed = false;
    for _ in 0..200 {
        if harness.rt.projector("leak").unwrap().failed() {
            failed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        failed,
        "storing a handle in a plaintext column should fail the projector"
    );
    harness.shutdown();
}

#[test]
fn re_emitting_a_folded_subject_value_is_rejected() {
    // A fold sees a subject field as a handle; carrying it into an emit would
    // double-encrypt. The constructor rejects a handle argument.
    let dir = write_project(&[
        (
            "events/order.star",
            r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": u64_(),
        "email": text(subject = "customer_id", max_length = 100),
    },
)
"#,
        ),
        (
            "commands/place-order.star",
            r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = u64_(), email = text())

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#,
        ),
        (
            "commands/copy-order.star",
            r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = u64_())

# Fold this customer's orders, capturing the (encrypted) email handle into state,
# then try to re-emit it: the constructor must reject the handle.
def query(input):
    return order_placed(customer_id = input.customer_id)

def initial():
    return {"email": None}

def fold(state, event):
    state["email"] = event.data["email"]
    return state

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = state["email"],
    )
"#,
        ),
    ]);
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let ctx = CommandContext::new(Uuid::new_v4());
    let body = json!({ "order_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "customer_id": 42 });
    let failed = harness.rt.execute("copy-order", body, &ctx, None).is_err();
    assert!(failed, "re-emitting a folded subject handle must fail");
    harness.shutdown();
}

#[test]
fn a_read_does_not_resurrect_an_erased_subject_key() {
    let dir = orders_project();
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let ks = harness.rt.keystore().unwrap();

    // The key exists after the order.
    assert!(
        ks.encrypt_subject_existing("customer_id", "42", "email", "x")
            .unwrap()
            .is_some()
    );
    // Erase it.
    ks.erase("customer_id", "42").unwrap();
    assert!(
        ks.encrypt_subject_existing("customer_id", "42", "email", "x")
            .unwrap()
            .is_none()
    );
    // A read of the row (the read/query path) must not recreate the key.
    let _ = read_row(&harness, "orders", "orders", ORDER);
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
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    let body = json!({ "order_id": ORDER, "customer_id": 42, "email": "alice@example.com" });

    let ctx = CommandContext::new(Uuid::new_v4());
    let fresh = harness
        .rt
        .execute("place-order", body.clone(), &ctx, Some("idem-1"))
        .unwrap();
    assert_eq!(fresh.status, 200);
    let ctx2 = CommandContext::new(Uuid::new_v4());
    let recovered = harness
        .rt
        .execute("place-order", body, &ctx2, Some("idem-1"))
        .unwrap();
    assert_eq!(recovered.status, 200);
    assert_eq!(
        fresh.body, recovered.body,
        "fresh and recovered responses must be identical"
    );
    harness.shutdown();
}

/// An accounts project: `email` is scoped to the account (for erasure) and `unique`
/// (so a global-key tag enforces uniqueness across accounts). register-account's
/// boundary queries the email, resolving to the global key.
fn accounts_project() -> TempDir {
    write_project(&[
        (
            "events/account.star",
            r#"
account_registered = event(
    type = "account.registered",
    fields = {
        "account_id": uuid(),
        "email": text(subject = "account_id", unique = True, max_length = 100),
    },
)
"#,
        ),
        (
            "commands/register-account.star",
            r#"
load("events/account.star", "account_registered")

input = schema(account_id = uuid(), email = text())

# Uniqueness across all accounts: constrain only the unique field, which resolves to
# the global-key tag (a per-account scoped tag could not match across accounts).
def query(input):
    return account_registered(email = input.email)

def initial():
    return {"taken": False}

def fold(state, event):
    state["taken"] = True
    return state

def handle(input, state):
    if state["taken"]:
        return reject("email_taken", "that email is already registered")
    return account_registered(account_id = input.account_id, email = input.email)
"#,
        ),
    ])
}

/// An orders project whose effect reveals the customer email and posts it, to
/// exercise the explicit `reveal()` decrypt boundary.
fn orders_with_notify_effect() -> TempDir {
    write_project(&[
        (
            "events/order.star",
            r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": u64_(),
        "email": text(subject = "customer_id", max_length = 100),
    },
)
"#,
        ),
        (
            "commands/place-order.star",
            r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = u64_(), email = text())

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#,
        ),
        (
            "effects/notify.star",
            r#"
load("events/order.star", "order_placed")

source = [order_placed()]

def handle(event):
    # reveal() is the explicit boundary: the effect decrypts the customer email to
    # send it. A projector could not; only an effect has reveal().
    email = reveal(event.data["email"])
    http.post(url = "https://mail.test/send", body = {"to": email})
"#,
        ),
    ])
}

#[test]
fn an_effect_reveals_the_plaintext_to_act_on_it() {
    let dir = orders_with_notify_effect();
    let stub = Arc::new(StubHttpClient::status(200));
    let harness = boot_http(
        dir.path(),
        Some(MasterKeys::new(MASTER, vec![])),
        stub.clone(),
    )
    .unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");

    // Wait for the effect to make its call.
    let mut posted = None;
    for _ in 0..300 {
        if let Some(call) = stub.calls().into_iter().next() {
            posted = Some(call);
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let call = posted.expect("the effect should have posted");
    let body: Value = serde_json::from_slice(&call.body.expect("a body")).unwrap();
    // reveal() gave the effect the real plaintext to send.
    assert_eq!(body["to"], "alice@example.com");

    harness.shutdown();
}

#[test]
fn concurrent_plaintext_uniqueness_admits_only_one() {
    // Control: a plaintext-tag uniqueness boundary under the same concurrent load, to
    // confirm the DCB boundary itself catches concurrent first-writers.
    let dir = write_project(&[
        (
            "events/thing.star",
            r#"registered = event(type = "thing.registered", fields = {"id": uuid(), "email": text(max_length = 100)})
"#,
        ),
        (
            "commands/register.star",
            r#"
load("events/thing.star", "registered")

input = schema(id = uuid(), email = text())

def query(input):
    return registered(email = input.email)

def initial():
    return {"taken": False}

def fold(state, event):
    state["taken"] = True
    return state

def handle(input, state):
    if state["taken"]:
        return reject("email_taken", "taken")
    return registered(id = input.id, email = input.email)
"#,
        ),
    ]);
    let harness = boot(dir.path(), None).unwrap();
    let place = |id: &'static str| {
        let rt = harness.rt.clone();
        thread::spawn(move || {
            let ctx = CommandContext::new(Uuid::new_v4());
            let body = json!({ "id": id, "email": "race@example.com" });
            rt.execute("register", body, &ctx, None).unwrap().status
        })
    };
    let a = place("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let b = place("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
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
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();

    let register = |account_id: &'static str| {
        let rt = harness.rt.clone();
        thread::spawn(move || {
            let ctx = CommandContext::new(Uuid::new_v4());
            let body = json!({ "account_id": account_id, "email": "race@example.com" });
            rt.execute("register-account", body, &ctx, None)
                .unwrap()
                .status
        })
    };
    let a = register("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let b = register("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
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
    let dir = write_project(&[
        (
            "events/order.star",
            r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": u64_(),
        "email": text(subject = "customer_id", max_length = 100),
    },
)
touched = event(type = "order.touched", fields = {"order_id": uuid()})
"#,
        ),
        (
            "commands/place-order.star",
            r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = u64_(), email = text())

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#,
        ),
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
        "customer_id": u64_(),
        "email": text(subject = "customer_id", max_length = 100),
        "touches": i64_(),
    },
)

source = [order_placed(), touched()]

def handle(event):
    if event.type == "order.placed":
        return [put(orders, {
            "order_id": event.data["order_id"],
            "customer_id": event.data["customer_id"],
            "email": event.data["email"],
            "touches": 0,
        })]
    # Read-modify-write: re-store the whole row (carrying the encrypted email handle)
    # with an incremented counter.
    row = get(orders, event.data["order_id"])
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
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    place_order(&harness.rt, ORDER, 42, "alice@example.com");
    let ctx = CommandContext::new(Uuid::new_v4());
    harness
        .rt
        .execute("touch-order", json!({ "order_id": ORDER }), &ctx, None)
        .unwrap();

    // Wait for both events to project.
    for _ in 0..200 {
        if harness.rt.projector("orders").unwrap().position() >= 2 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !harness.rt.projector("orders").unwrap().failed(),
        "the read-modify-write projector must not fail"
    );
    let row = read_row(&harness, "orders", "orders", ORDER).expect("a row");
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
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();
    let first = "aaaaaaaa-0000-0000-0000-000000000001";
    let second = "aaaaaaaa-0000-0000-0000-000000000002";
    place_order(&harness.rt, first, 42, "old@example.com");
    // Wait for the first to project, then erase customer 42.
    for _ in 0..200 {
        if harness.rt.projector("orders").unwrap().position() >= 1 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();
    // A new order for customer 42 mints a fresh key.
    place_order(&harness.rt, second, 42, "new@example.com");
    for _ in 0..200 {
        if harness.rt.projector("orders").unwrap().position() >= 2 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // The first order's email is unreadable (its key is gone); the second's is fine.
    let old = read_row(&harness, "orders", "orders", first).expect("first row");
    assert!(
        old.get("email").is_none(),
        "stale email must read as absent: {old}"
    );
    let new = read_row(&harness, "orders", "orders", second).expect("second row");
    assert_eq!(new["email"], "new@example.com");

    harness.shutdown();
}

#[test]
fn unique_enforces_global_uniqueness_across_subjects() {
    let dir = accounts_project();
    let harness = boot(dir.path(), Some(MasterKeys::new(MASTER, vec![]))).unwrap();

    let register = |account_id: &str, email: &str| {
        let ctx = CommandContext::new(Uuid::new_v4());
        let body = json!({ "account_id": account_id, "email": email });
        harness
            .rt
            .execute("register-account", body, &ctx, None)
            .unwrap()
    };

    let a = "11111111-1111-1111-1111-111111111111";
    let b = "22222222-2222-2222-2222-222222222222";
    // First account with the email succeeds.
    let first = register(a, "shared@example.com");
    assert_eq!(first.status, 200, "first registration: {:?}", first.body);
    // A second, different account with the same email is rejected: the query's
    // global-key tag matched the first account's, across their distinct subject keys.
    let second = register(b, "shared@example.com");
    assert_eq!(second.status, 422, "second registration should be rejected");
    assert_eq!(second.body["error"]["code"], "email_taken");
    // A different email on the second account is fine.
    let other = register(b, "other@example.com");
    assert_eq!(other.status, 200, "distinct email: {:?}", other.body);

    harness.shutdown();
}
