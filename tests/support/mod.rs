#![allow(dead_code)]
//! The shared harness for the integration suite.
//!
//! Integration tests are separate crates, so every test file includes this module
//! with `mod support;` and uses only the parts it needs (hence the crate-wide
//! `dead_code` allowance above). Everything here is deliberately generic: booting a
//! runtime, writing a throwaway project, waiting on a projector or an effect, and
//! driving the in-process router. A test that needs a genuinely bespoke setup
//! should still build it inline rather than bending a helper here.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use kiln::context::CommandContext;
use kiln::crypto::MasterKeys;
use kiln::dispatch::build_event;
use kiln::effect::{EffectRuntime, HttpClient, StubHttpClient};
use kiln::loader::{Finding, LoadedProject, Severity};
use kiln::projector::ProjectorSet;
use kiln::read_api;
use kiln::runtime::Runtime;
use kiln::server;
use kiln::starlark_builtins::EmittedEvent;
use kiln::validate;
use serde_json::{Value, json};
use tempfile::TempDir;
use tephra::{SegmentConfig, SegmentSet, WriteCoordinator, WriteHandle, WriterConfig};
use tower::ServiceExt;
use uuid::Uuid;

// --- fixed test data ------------------------------------------------------

pub const ALICE: &str = "11111111-1111-1111-1111-111111111111";
pub const BOB: &str = "22222222-2222-2222-2222-222222222222";
pub const CAROL: &str = "33333333-3333-3333-3333-333333333333";
/// A well-formed uuid that no test ever registers, for the not-found cases.
pub const MISSING: &str = "99999999-9999-9999-9999-999999999999";
pub const UUID_A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
pub const UUID_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
pub const UUID_C: &str = "cccccccc-cccc-cccc-cccc-cccccccccccc";

/// A fixed, non-secret master key for the tests.
pub const MASTER_KEY: [u8; 32] = [0x11; 32];

/// A fixed clock, matching the one `kiln test` pins, for events seeded straight
/// into a store.
pub const TEST_NOW: &str = "1970-01-01T00:00:00Z";

/// Segment size for a throwaway store: small, but still clear of the writer's
/// default max batch size.
const TEST_SEGMENT_SIZE: usize = 16 * 1024 * 1024;

/// How long a `wait_*` helper polls before it panics, and how long it sleeps
/// between polls.
///
/// This is a deadlock guard, not a performance assertion, so the budget sits well
/// above what any test needs. The slowest, an effect retrying to five failures under
/// exponential backoff, spends 200+400+800+1600ms in sleeps alone before it can
/// pass. Counting attempts instead of elapsed time made these waits flaky: on a
/// loaded machine the `cond()` call itself dominates, so the attempts ran out long
/// before the work had actually stalled.
const POLL_BUDGET: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The fixed master key, with no retired keys.
pub fn master_keys() -> MasterKeys {
    MasterKeys::new(MASTER_KEY, vec![])
}

/// The absolute path of an `examples/` project, e.g. `example_dir("users")`.
pub fn example_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

// --- the harness ----------------------------------------------------------

/// A live runtime plus the three handles that have to be drained on shutdown.
pub struct Harness {
    pub rt: Arc<Runtime>,
    pub coord: WriteCoordinator,
    pub projectors: ProjectorSet,
    pub effects: EffectRuntime,
    data_dir: PathBuf,
    /// Kept alive for the harness's lifetime when the data directory is ours;
    /// `None` when the caller owns it (so it can reopen the same log).
    _data: Option<TempDir>,
}

impl Harness {
    /// Drain effects, then projectors, then the writer. Every test must end with
    /// this: the order matters, since an effect can still invoke a command.
    pub fn shutdown(self) {
        self.effects.shutdown_and_join();
        self.projectors.shutdown_and_join();
        self.coord.shutdown();
    }

    /// The data directory this runtime was opened against.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The in-process axum router for this runtime.
    pub fn app(&self) -> Router {
        server::app(Arc::clone(&self.rt))
    }
}

/// A runtime boot, configured step by step. Defaults: a throwaway data directory,
/// an HTTP stub that answers 400, and no master keys.
pub struct Boot {
    project_dir: PathBuf,
    data_dir: Option<PathBuf>,
    http: Arc<dyn HttpClient>,
    master: Option<MasterKeys>,
}

impl Boot {
    /// Boot the project rooted at `project_dir`.
    pub fn new(project_dir: impl Into<PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
            data_dir: None,
            http: Arc::new(StubHttpClient::status(400)),
            master: None,
        }
    }

    /// Boot `examples/users`.
    pub fn example() -> Self {
        Self::new(example_dir("users"))
    }

    /// Open against an explicit data directory the caller owns, so a later boot can
    /// reopen the same event log.
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// The HTTP client the journaled `http.*` builtins call.
    pub fn http(mut self, http: Arc<dyn HttpClient>) -> Self {
        self.http = http;
        self
    }

    /// The status a default `StubHttpClient` answers with.
    pub fn http_status(self, status: u16) -> Self {
        self.http(Arc::new(StubHttpClient::status(status)))
    }

    /// Boot with the fixed [`master_keys`].
    pub fn with_master_key(self) -> Self {
        self.master(master_keys())
    }

    pub fn master(mut self, master: MasterKeys) -> Self {
        self.master = Some(master);
        self
    }

    /// Boot, returning the error instead of panicking, so a test can assert that
    /// opening the runtime fails.
    pub fn try_start(self) -> anyhow::Result<Harness> {
        let project = load_ok(&self.project_dir);
        let (temp, data_dir) = match self.data_dir {
            Some(dir) => (None, dir),
            None => {
                let temp = tempfile::tempdir().unwrap();
                let dir = temp.path().to_path_buf();
                (Some(temp), dir)
            }
        };
        let (rt, coord, projectors, effects) =
            Runtime::open(project, &data_dir, self.http, self.master)?;
        Ok(Harness {
            rt,
            coord,
            projectors,
            effects,
            data_dir,
            _data: temp,
        })
    }

    /// Boot, panicking if the runtime cannot open.
    pub fn start(self) -> Harness {
        self.try_start().unwrap()
    }
}

/// Boot `examples/users` against a throwaway data directory.
pub fn boot_example() -> Harness {
    Boot::example().start()
}

/// Boot `examples/users` against a data directory the caller owns.
pub fn boot_example_at(data_dir: &Path) -> Harness {
    Boot::example().data_dir(data_dir).start()
}

/// Boot an arbitrary project directory against a throwaway data directory.
pub fn boot_project(project_dir: &Path) -> Harness {
    Boot::new(project_dir).start()
}

/// Load a project and assert it has no load errors.
pub fn load_ok(dir: &Path) -> LoadedProject {
    let project = LoadedProject::load(dir);
    assert!(!project.has_errors(), "{:?}", project.findings);
    project
}

// --- throwaway projects ---------------------------------------------------

/// Write a throwaway project from `(relative path, contents)` pairs. The returned
/// `TempDir` deletes the project when it drops, so callers must keep it bound.
pub fn write_project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

/// Delete `kiln.db` (and its WAL sidecars) while the event log survives. Command
/// idempotency lives entirely in the log, so a reopen after this proves a replay
/// recovers across a restart even with the operational DB gone.
pub fn drop_op_db(data_dir: &Path) {
    for name in ["kiln.db", "kiln.db-wal", "kiln.db-shm"] {
        let path = data_dir.join(name);
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
}

// --- the orders project ---------------------------------------------------

/// `order.placed` carries a customer-scoped `email`, the canonical subject field.
pub const ORDER_EVENTS: &str = r#"
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 100),
    },
)
"#;

/// The command that emits [`ORDER_EVENTS`]'s `order.placed`.
pub const PLACE_ORDER: &str = r#"
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = uint(), email = str())

def handle(input, state):
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#;

/// An `orders` read model that stores the subject column as ciphertext.
pub const ORDERS_PROJECTOR: &str = r#"
load("events/order.star", "order_placed")

orders = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 100),
    },
)

source = [order_placed()]

def handle(event):
    return [put(orders, {
        "order_id": event.data["order_id"],
        "customer_id": event.data["customer_id"],
        "email": event.data["email"],
    })]
"#;

/// An effect that `reveal()`s the customer email and posts it, exercising the
/// explicit decrypt boundary.
pub const NOTIFY_EFFECT: &str = r#"
load("events/order.star", "order_placed")

source = [order_placed()]

def handle(event):
    # reveal() is the explicit boundary: the effect decrypts the customer email to
    # send it. A projector could not; only an effect has reveal().
    email = reveal(event.data["email"])
    http.post(url = "https://mail.test/send", body = {"to": email})
"#;

/// The orders event module and its `place-order` command, plus `extra` modules.
pub fn orders_project_with(extra: &[(&str, &str)]) -> TempDir {
    let mut files = vec![
        ("events/order.star", ORDER_EVENTS),
        ("commands/place-order.star", PLACE_ORDER),
    ];
    files.extend_from_slice(extra);
    write_project(&files)
}

/// The orders project with the [`ORDERS_PROJECTOR`] read model.
pub fn orders_project() -> TempDir {
    orders_project_with(&[("projectors/orders.star", ORDERS_PROJECTOR)])
}

/// The orders project with the [`NOTIFY_EFFECT`] instead of a projector.
pub fn orders_with_notify_effect() -> TempDir {
    orders_project_with(&[("effects/notify.star", NOTIFY_EFFECT)])
}

/// Place an order through `place-order` and return the response body.
pub fn place_order(rt: &Runtime, order_id: &str, customer_id: u64, email: &str) -> Value {
    let body = json!({ "order_id": order_id, "customer_id": customer_id, "email": email });
    let result = rt.execute("place-order", body, &ctx(), None).unwrap();
    assert_eq!(result.status, 200, "place-order failed: {:?}", result.body);
    result.body
}

// --- the accounts project -------------------------------------------------

/// `email` is scoped to the account (for erasure) and `unique`, so a global-key tag
/// enforces uniqueness across accounts.
pub const ACCOUNT_EVENTS: &str = r#"
account_registered = event(
    type = "account.registered",
    fields = {
        "account_id": uuid(),
        "email": str(subject = "account_id", unique = True, max_length = 100),
    },
)
"#;

/// A boundary that queries the unique email, resolving to the global key.
pub const REGISTER_ACCOUNT: &str = r#"
load("events/account.star", "account_registered")

input = schema(account_id = uuid(), email = str())

# Uniqueness across all accounts: constrain only the unique field, which resolves to
# the global-key tag (a per-account scoped tag could not match across accounts).
def query(input):
    return account_registered(email = input.email)

initial = {"taken": False}

def fold(state, event):
    return dict(state, taken = True)

def handle(input, state):
    if state["taken"]:
        return reject("email_taken", "that email is already registered")
    return account_registered(account_id = input.account_id, email = input.email)
"#;

pub fn accounts_project() -> TempDir {
    write_project(&[
        ("events/account.star", ACCOUNT_EVENTS),
        ("commands/register-account.star", REGISTER_ACCOUNT),
    ])
}

// --- driving commands -----------------------------------------------------

/// A fresh command context with its own correlation id.
pub fn ctx() -> CommandContext {
    CommandContext::new(Uuid::new_v4())
}

/// The `register-user` request body for `examples/users`.
pub fn register_body(user_id: &str, email: &str, name: &str) -> Value {
    json!({ "user_id": user_id, "email": email, "name": name })
}

/// Register a user through `examples/users`' `register-user` and return the log
/// position of the appended `user.registered`, the value a client would pass back
/// as `?after=` for read-your-writes.
pub fn register_user(rt: &Runtime, user_id: &str, email: &str, name: &str) -> u64 {
    let result = rt
        .execute(
            "register-user",
            register_body(user_id, email, name),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "register failed: {:?}", result.body);
    result.body["positions"]["last"]
        .as_u64()
        .expect("a last position")
}

// --- waiting --------------------------------------------------------------

/// Poll `cond` every [`POLL_INTERVAL`], panicking with `label` once
/// [`POLL_BUDGET`] of wall-clock time has elapsed.
pub fn wait_until<F: Fn() -> bool>(label: &str, cond: F) {
    let started = Instant::now();
    loop {
        if cond() {
            return;
        }
        if started.elapsed() >= POLL_BUDGET {
            panic!(
                "timed out waiting for {label} after {:?}",
                started.elapsed()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Block until `projector` has applied everything up to `target`.
pub fn wait_position(rt: &Runtime, projector: &str, target: u64) {
    wait_until(
        &format!("projector `{projector}` to reach position {target}"),
        || rt.projector(projector).unwrap().position() >= target,
    );
}

/// [`wait_position`] for an async test: yields to the tokio runtime between polls
/// rather than blocking its worker thread.
pub async fn wait_position_async(rt: &Runtime, projector: &str, target: u64) {
    let started = Instant::now();
    loop {
        if rt.projector(projector).unwrap().position() >= target {
            return;
        }
        if started.elapsed() >= POLL_BUDGET {
            panic!(
                "projector `{projector}` did not reach position {target} after {:?}",
                started.elapsed()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The runtime's current log head, from `/status`.
pub fn log_head(rt: &Runtime) -> u64 {
    rt.status()["log_head"].as_u64().unwrap()
}

// --- reading the read model ----------------------------------------------

/// One row from a projector's read model, read through the read API (which decrypts
/// subject columns), after waiting for the projector to reach `after`.
pub fn read_row(
    harness: &Harness,
    projector: &str,
    entity: &str,
    key: &str,
    after: u64,
) -> Option<Value> {
    wait_position(&harness.rt, projector, after);
    let shared = harness.rt.projector(projector).unwrap();
    let entity_def = shared
        .entities
        .iter()
        .find(|candidate| candidate.name == entity)
        .unwrap();
    let (row, _position) =
        read_api::get_one(&shared.db_path, entity_def, key, harness.rt.keystore()).unwrap();
    row
}

// --- seeding a store directly ---------------------------------------------

/// Open a throwaway event store under `dir`. The caller keeps `dir` alive, so the
/// store outlives it.
pub fn open_store(dir: &Path) -> (WriteCoordinator, WriteHandle) {
    let set = SegmentSet::open(dir.join("events"), SegmentConfig::new(TEST_SEGMENT_SIZE)).unwrap();
    WriteCoordinator::start(set, WriterConfig::default()).unwrap()
}

/// Append `emitted` through the same envelope seam a command uses, so the seeded
/// event is byte-identical to one the runtime would write.
pub fn seed_event(
    store: &WriteHandle,
    project: &LoadedProject,
    ctx: &CommandContext,
    emitted: EmittedEvent,
) {
    let event = build_event(
        &emitted,
        project.events.by_type.get(&emitted.event_type),
        None,
        ctx,
        TEST_NOW,
        None,
    )
    .unwrap();
    store.append(vec![event], None).unwrap();
}

// --- the loader and validation pass ---------------------------------------

/// Every finding the loader and the validation pass produce together.
pub fn findings(project: &LoadedProject) -> Vec<Finding> {
    let mut all = project.findings.clone();
    all.extend(validate::check(project));
    all
}

/// The error-severity findings, rendered as `location: message`.
pub fn errors(project: &LoadedProject) -> Vec<String> {
    findings(project)
        .into_iter()
        .filter(|finding| finding.severity == Severity::Error)
        .map(|finding| format!("{}: {}", finding.location, finding.message))
        .collect()
}

/// Load a throwaway project and assert some error message contains `needle`.
pub fn assert_error(files: &[(&str, &str)], needle: &str) {
    let dir = write_project(files);
    let errs = errors(&LoadedProject::load(dir.path()));
    assert!(
        errs.iter().any(|err| err.contains(needle)),
        "expected an error containing `{needle}`, got {errs:?}"
    );
}

/// Load a throwaway project and assert it produces no errors.
pub fn assert_clean(files: &[(&str, &str)]) {
    let dir = write_project(files);
    let errs = errors(&LoadedProject::load(dir.path()));
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");
}

// --- driving the router ---------------------------------------------------

/// Send a bodiless request and decode the JSON response (an empty body is `null`).
pub async fn send(app: &Router, method: Method, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

/// GET `uri` and decode the JSON response.
pub async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    send(app, Method::GET, uri).await
}

/// POST a command with an optional `Idempotency-Key` header.
pub async fn post_command(
    app: &Router,
    name: &str,
    body: Value,
    idem_key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(format!("/commands/{name}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = idem_key {
        builder = builder.header("idempotency-key", key);
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
