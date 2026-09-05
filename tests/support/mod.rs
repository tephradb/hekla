#![allow(dead_code)]
//! The shared harness for the integration suite.
//!
//! Integration tests are separate crates, so every test file includes this module
//! with `mod support;` and uses only the parts it needs (hence the crate-wide
//! `dead_code` allowance above). Everything here is deliberately generic: booting a
//! runtime, writing a throwaway project, waiting on a projector or an effect, and
//! driving the in-process router. A test that needs a genuinely bespoke setup
//! should still build it inline rather than bending a helper here.

pub mod shadow;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use hekla::context::CommandContext;
use hekla::crypto::MasterKeys;
use hekla::effect::{EffectRuntime, StubHttpClient};
use hekla::heklang_host::{HeklaHost, event_from_json};
use hekla::http::HttpClient;
use hekla::loader::{Finding, LoadedProject, Severity};
use hekla::projector::ProjectorSet;
use hekla::read_api;
use hekla::runtime::Runtime;
use hekla::server;
use hekla::store::Store;
use hekla::validate;
use serde_json::{Value, json};
use tempfile::TempDir;
use tephra::{SegmentConfig, SegmentSet, WriteCoordinator, WriterConfig};
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

/// A fixed clock, matching the one `hekla test` pins, for events seeded straight
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

/// The absolute path of a `tests/fixtures/` project.
///
/// A real directory rather than a `write_project` string, so `hek check` and `hek test`
/// run over it and a human can read it. The examples are what hekla teaches; a fixture
/// is what it is tested against, and the two want different things from a project.
pub fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
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

/// Delete `hekla.db` (and its WAL sidecars) while the event log survives. Command
/// idempotency lives entirely in the log, so a reopen after this proves a replay
/// recovers across a restart even with the operational DB gone.
pub fn drop_op_db(data_dir: &Path) {
    for name in ["hekla.db", "hekla.db-wal", "hekla.db-shm"] {
        let path = data_dir.join(name);
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
}

// --- the orders project ---------------------------------------------------

/// `order.placed` carries a customer-scoped `email`, the canonical subject field.
pub const ORDER_EVENTS: &str = r#"
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
  // Optional because an erased subject's column reads back absent, and a type that
  // cannot be absent could not say so.
  email: String? @subject(customer_id) @max(100),
}
"#;

/// The command that emits [`ORDER_EVENTS`]'s `@order.placed`.
pub const PLACE_ORDER: &str = r#"
command PlaceOrder(order_id: Uuid, customer_id: Int, email: String?) {
  emit @order.placed { order_id, customer_id, email }
}
"#;

/// An `Order` read model that stores the subject column as ciphertext.
pub const ORDERS_PROJECTOR: &str = r#"
projector Orders {
  entity Order {
    order_id: Uuid @key,
    customer_id: Int @index,
    email: String? @max(100),
  }

  on @order.placed { order_id, customer_id, email } {
    put Order { order_id, customer_id, email }
  }
}
"#;

/// An effect that `reveal`s the customer email and posts it, exercising the explicit
/// decrypt boundary.
pub const NOTIFY_EFFECT: &str = r#"
effect Notify {
  on @order.placed { email } {
    // `reveal` is the explicit boundary: the effect decrypts the customer email to
    // send it. A projector could not; only an effect has it.
    http.post("https://mail.test/send", { "to": reveal(email) })
  }
}
"#;

/// The orders event module and its `PlaceOrder` command, plus `extra` modules.
pub fn orders_project_with(extra: &[(&str, &str)]) -> TempDir {
    let mut files = vec![
        ("events/order.hk", ORDER_EVENTS),
        ("commands/place-order.hk", PLACE_ORDER),
    ];
    files.extend_from_slice(extra);
    write_project(&files)
}

/// The orders project with the [`ORDERS_PROJECTOR`] read model.
pub fn orders_project() -> TempDir {
    orders_project_with(&[("projectors/orders.hk", ORDERS_PROJECTOR)])
}

/// The orders project with the [`NOTIFY_EFFECT`] instead of a projector.
pub fn orders_with_notify_effect() -> TempDir {
    orders_project_with(&[("effects/notify.hk", NOTIFY_EFFECT)])
}

/// Place an order through `PlaceOrder` and return the response body.
pub fn place_order(rt: &Runtime, order_id: &str, customer_id: u64, email: &str) -> Value {
    let body = json!({ "order_id": order_id, "customer_id": customer_id, "email": email });
    let result = rt.execute("PlaceOrder", body, &ctx(), None).unwrap();
    assert_eq!(result.status, 200, "PlaceOrder failed: {:?}", result.body);
    result.body
}

// --- the accounts project -------------------------------------------------

/// `email` is scoped to the account, so it can be erased.
///
/// The Starlark suite also marked it `unique`, for a global-key tag that made one
/// email match across every account. heklang rejects an equality on sealed content
/// (rule 12), so that boundary cannot be written and the tag is gone with it. What
/// stays is a plaintext handle beside the sealed address, which is what a program can
/// actually fold on.
pub const ACCOUNT_EVENTS: &str = r#"
event @account.registered {
  account_id: Uuid,
  handle: String @max(100),
  email: String? @subject(account_id) @max(100),
}
"#;

/// A boundary over the plaintext handle: one account per handle, across all accounts.
pub const REGISTER_ACCOUNT: &str = r#"
refusal HandleTaken "that handle is already registered"

command RegisterAccount(account_id: Uuid, handle: String, email: String?) {
  fold taken: Bool = false
    on @account.registered(handle) => true

  if taken {
    return reject HandleTaken
  }

  emit @account.registered { account_id, handle, email }
}
"#;

pub fn accounts_project() -> TempDir {
    write_project(&[
        ("events/account.hk", ACCOUNT_EVENTS),
        ("commands/register-account.hk", REGISTER_ACCOUNT),
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
            "RegisterUser",
            register_body(user_id, email, name),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "RegisterUser failed: {:?}", result.body);
    result.body["positions"]["last"]
        .as_u64()
        .expect("a last position")
}

// --- waiting --------------------------------------------------------------

/// Poll `cond` every [`POLL_INTERVAL`], panicking with `label` once
/// [`POLL_BUDGET`] of wall-clock time has elapsed.
pub fn wait_until<F: FnMut() -> bool>(label: &str, cond: F) {
    if !wait_for(cond) {
        panic!("timed out waiting for {label} after {POLL_BUDGET:?}");
    }
}

/// [`wait_until`] for a caller that has something better to do with a timeout than
/// panic. The model test is the only one: it reports a stall as a divergence, so a
/// planted violation that wedges a lane comes back as a result rather than a panic
/// from inside a helper.
pub fn wait_for<F: FnMut() -> bool>(mut cond: F) -> bool {
    let started = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if started.elapsed() >= POLL_BUDGET {
            return false;
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

/// Block until `effect` has processed everything up to `target`.
pub fn wait_effect_position(rt: &Runtime, effect: &str, target: u64) {
    wait_until(
        &format!("effect `{effect}` to reach position {target}"),
        || rt.effect(effect).unwrap().position() >= target,
    );
}

/// Ask `projector` to rebuild and block until it has.
///
/// The count is read *before* the request, because a rebuild leaves no other trace: it
/// happens into a sibling file and swaps in by rename, so the live position never drops
/// and nothing else says one is in flight. Waiting on the count rather than on a sleep
/// is the difference between a test that is slow and one that is occasionally wrong.
pub fn replay_and_wait(rt: &Runtime, projector: &str) {
    let shared = rt.projector(projector).unwrap();
    let before = shared.replays_completed() + shared.replays_failed();
    shared.request_replay();
    wait_until(
        &format!("projector `{projector}` to finish a replay"),
        || shared.replays_completed() + shared.replays_failed() > before,
    );
}

/// Block until every projector and every effect has caught up to the log head, and the
/// head has stopped moving.
///
/// Two passes, because an effect's `invoke` appends: a single read of the head can be
/// satisfied by components that are about to be handed more work. This is the barrier
/// every assertion about settled state needs, and it panics rather than returning a
/// bool, so a call site cannot quietly skip its assertions when the wait times out.
pub fn quiesce(harness: &Harness) {
    assert!(
        try_quiesce(harness),
        "timed out waiting for the runtime to settle"
    );
}

/// [`quiesce`] that reports a timeout instead of panicking.
pub fn try_quiesce(harness: &Harness) -> bool {
    let rt = &harness.rt;
    let mut previous = u64::MAX;
    wait_for(|| {
        let head = rt.log_head();
        let caught_up = rt
            .projector_handles()
            .iter()
            .all(|handle| handle.position() >= head)
            && rt
                .effect_handles()
                .iter()
                .all(|handle| handle.position() >= head && handle.retry_in_ms().is_none());
        let settled = caught_up && head == previous;
        previous = head;
        settled
    })
}

/// The runtime's current log head, from `/status`.
pub fn log_head(rt: &Runtime) -> u64 {
    rt.status()["log_head"].as_u64().unwrap()
}

/// Run the offline invariant sweep over a stopped data directory, with the fixed master
/// key. The runtime must already be shut down: the sweep takes the directory lock.
pub fn sweep(project_dir: &Path, data_dir: &Path) -> hekla::verify::Report {
    let project = load_ok(project_dir);
    hekla::verify::sweep(&project, data_dir, Some(master_keys())).expect("the sweep should run")
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
pub fn open_store(dir: &Path) -> (WriteCoordinator, Store) {
    let set = SegmentSet::open(dir.join("events"), SegmentConfig::new(TEST_SEGMENT_SIZE)).unwrap();
    let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default()).unwrap();
    (coordinator, Store::writing(store))
}

/// Append one event through the same lowering a command uses, so a seeded event is
/// byte for byte what the runtime would have written.
pub fn seed_event(
    store: &Store,
    project: &LoadedProject,
    ctx: &CommandContext,
    event_type: &str,
    data: Value,
) {
    let event = event_from_json(&project.program, event_type, &data).expect("a declared event");
    let mut host = HeklaHost {
        program: Arc::clone(&project.program),
        events: Arc::clone(&project.events),
        store: store.clone(),
        keystore: None,
        ctx: *ctx,
        now: TEST_NOW.to_owned(),
        idem_tag: None,
        // Only an effect's `invoke` keys an append on a journaled call.
        call: None,
        appended: None,
        emitted: Vec::new(),
        unavailable: None,
        duplicated: false,
        http: None,
        retry_after: None,
        last_transport: None,
        minted: None,
        sealed: false,
    };
    hekla::heklang_host::append_one(&mut host, &event).expect("seeded");
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
