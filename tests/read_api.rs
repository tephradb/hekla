//! End-to-end read API: register a user through the command path, wait for the
//! projector to catch up, then read it back over HTTP through the in-process
//! router. Exercises point reads, the indexed filter, the unindexed-filter 400,
//! cursor-carrying responses, the projector position, and the replay route.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use kiln::context::CommandContext;
use kiln::effect::{EffectRuntime, HttpClient, StubHttpClient};
use kiln::loader::LoadedProject;
use kiln::projector::ProjectorSet;
use kiln::runtime::Runtime;
use kiln::server;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use tempfile::TempDir;
use tephra::WriteCoordinator;
use tower::ServiceExt;
use uuid::Uuid;

const ALICE: &str = "11111111-1111-1111-1111-111111111111";
const MISSING: &str = "99999999-9999-9999-9999-999999999999";

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

/// Write a throwaway project from `(relative path, contents)` pairs, for a case
/// that needs a bespoke project rather than the shared example.
fn write_project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

fn boot() -> Harness {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);
    assert!(!project.has_errors(), "{:?}", project.findings);
    let data = tempfile::tempdir().unwrap();
    let http: Arc<dyn HttpClient> = Arc::new(StubHttpClient::status(400));
    let (rt, coord, projectors, effects) = Runtime::open(project, data.path(), http, None).unwrap();
    Harness {
        rt,
        coord,
        projectors,
        effects,
        _data: data,
    }
}

fn register(rt: &Runtime, user_id: &str, email: &str, name: &str) {
    register_at(rt, user_id, email, name);
}

/// Register a user and return the log position of the appended `user.registered`,
/// the value a client would pass back as `?after=` for read-your-writes.
fn register_at(rt: &Runtime, user_id: &str, email: &str, name: &str) -> u64 {
    let ctx = CommandContext::new(Uuid::new_v4());
    let body = json!({ "user_id": user_id, "email": email, "name": name });
    let result = rt.execute("register-user", body, &ctx, None).unwrap();
    assert_eq!(result.status, 200, "register failed: {:?}", result.body);
    result.body["positions"]["last"]
        .as_u64()
        .expect("a last position")
}

async fn wait_position(rt: &Runtime, projector: &str, target: u64) {
    for _ in 0..200 {
        if rt.projector(projector).unwrap().position() >= target {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("projector `{projector}` did not reach position {target}");
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    send(app, Method::GET, uri).await
}

async fn send(app: &Router, method: Method, uri: &str) -> (StatusCode, Value) {
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

#[tokio::test]
async fn reads_a_row_and_the_indexed_filter_through_http() {
    let harness = boot();
    register(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position(&harness.rt, "users", 1).await;
    let app = server::app(Arc::clone(&harness.rt));

    // Point read by key.
    let (status, body) = get(&app, &format!("/read/users/users/{ALICE}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item"]["email"], "alice@example.com");
    assert_eq!(body["item"]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= 1);

    // Indexed filter on email returns the same row.
    let (status, body) = get(&app, "/read/users/users?email=alice@example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= 1);

    // A get() projector maintained the running count.
    let (status, body) = get(&app, "/read/user-stats/totals/all").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item"]["count"].as_i64(), Some(1));

    harness.shutdown();
}

#[tokio::test]
async fn unindexed_filter_and_missing_targets_are_rejected() {
    let harness = boot();
    register(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position(&harness.rt, "users", 1).await;
    let app = server::app(Arc::clone(&harness.rt));

    // `name` is not indexed: a 400, never a table scan.
    let (status, body) = get(&app, "/read/users/users?name=Alice").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "unindexed_filter");

    // Missing row, unknown entity, and unknown projector are all 404.
    let (status, _) = get(&app, &format!("/read/users/users/{MISSING}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/read/users/ghosts/x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/read/ghost/users/x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    harness.shutdown();
}

#[tokio::test]
async fn scan_paginates_with_a_cursor() {
    let harness = boot();
    for i in 0..5 {
        let id = format!("00000000-0000-0000-0000-00000000000{i}");
        register(&harness.rt, &id, &format!("user{i}@example.com"), "User");
    }
    wait_position(&harness.rt, "users", 5).await;
    let app = server::app(Arc::clone(&harness.rt));

    let mut seen = Vec::new();
    let mut uri = "/read/users/users?limit=2".to_owned();
    loop {
        let (status, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::OK);
        for item in body["items"].as_array().unwrap() {
            seen.push(item["user_id"].as_str().unwrap().to_owned());
        }
        match body["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/read/users/users?limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5);
    // Ordered by key, every row exactly once.
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 5);

    harness.shutdown();
}

#[tokio::test]
async fn replay_route_is_accepted() {
    let harness = boot();
    register(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position(&harness.rt, "users", 1).await;
    let app = server::app(Arc::clone(&harness.rt));

    let (status, body) = send(&app, Method::POST, "/projectors/users/replay").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "replay_scheduled");

    let (status, _) = send(&app, Method::POST, "/projectors/ghost/replay").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    harness.shutdown();
}

#[tokio::test]
async fn after_waits_for_the_projector_then_reads_your_write() {
    let harness = boot();
    // Deliberately no wait_position: the read must block until the projector
    // catches up on its own, which is the whole point of `?after=`.
    let pos = register_at(&harness.rt, ALICE, "alice@example.com", "Alice");
    let app = server::app(Arc::clone(&harness.rt));

    let (status, body) = get(&app, &format!("/read/users/users/{ALICE}?after={pos}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item"]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= pos);

    harness.shutdown();
}

#[tokio::test]
async fn after_waits_for_the_projector_on_a_scan() {
    let harness = boot();
    let pos = register_at(&harness.rt, ALICE, "alice@example.com", "Alice");
    let app = server::app(Arc::clone(&harness.rt));

    let (status, body) = get(&app, &format!("/read/users/users?after={pos}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= pos);

    harness.shutdown();
}

#[tokio::test]
async fn after_reserves_its_slot_and_does_not_shadow_a_filter() {
    let harness = boot();
    let pos = register_at(&harness.rt, ALICE, "alice@example.com", "Alice");
    let app = server::app(Arc::clone(&harness.rt));

    // `after` is a reserved param, not a filter field; the email filter still binds.
    let (status, body) = get(
        &app,
        &format!("/read/users/users?email=alice@example.com&after={pos}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= pos);

    harness.shutdown();
}

#[tokio::test]
async fn after_resolves_on_a_selective_projector_past_a_non_matching_tail() {
    // `user-stats` sources only user.registered, so a later user.renamed is a
    // non-matching tail for it. Its watermark (hence reported position) must still
    // advance to head, or `?after=<rename position>` would spuriously time out even
    // though the data it wants is already visible.
    let harness = boot();
    register(&harness.rt, ALICE, "alice@example.com", "Alice");
    let ctx = CommandContext::new(Uuid::new_v4());
    let rename = harness
        .rt
        .execute(
            "rename-user",
            json!({ "user_id": ALICE, "name": "Alicia" }),
            &ctx,
            None,
        )
        .unwrap();
    assert_eq!(rename.status, 200, "rename failed: {:?}", rename.body);
    let pos = rename.body["positions"]["last"].as_u64().unwrap();
    let app = server::app(Arc::clone(&harness.rt));

    let (status, body) = get(
        &app,
        &format!("/read/user-stats/totals/all?after={pos}&timeout_ms=2000"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "selective projector must report caught up: {body:?}"
    );
    assert_eq!(body["item"]["count"].as_i64(), Some(1));
    assert!(body["position"].as_u64().unwrap() >= pos);

    harness.shutdown();
}

#[tokio::test]
async fn timeout_ms_zero_is_an_immediate_check() {
    let harness = boot();
    let pos = register_at(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position(&harness.rt, "users", pos).await;
    let app = server::app(Arc::clone(&harness.rt));

    // Already caught up: a 0ms wait still succeeds on the first check.
    let (status, _) = get(
        &app,
        &format!("/read/users/users/{ALICE}?after={pos}&timeout_ms=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Not caught up: a 0ms wait fails closed immediately rather than blocking.
    let (status, body) = get(
        &app,
        &format!(
            "/read/users/users/{ALICE}?after={}&timeout_ms=0",
            pos + 1000
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "not_caught_up");

    harness.shutdown();
}

#[tokio::test]
async fn a_non_numeric_after_is_a_400() {
    let harness = boot();
    register(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position(&harness.rt, "users", 1).await;
    let app = server::app(Arc::clone(&harness.rt));

    let (status, body) = get(&app, &format!("/read/users/users/{ALICE}?after=abc")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_after");

    harness.shutdown();
}

#[tokio::test]
async fn after_beyond_the_log_times_out_with_503_and_retry_after() {
    let harness = boot();
    let pos = register_at(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position(&harness.rt, "users", pos).await;
    let app = server::app(Arc::clone(&harness.rt));

    // A position the projector can never reach, with a short budget so the wait
    // gives up fast and fails closed.
    let unreachable = pos + 1000;
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "/read/users/users/{ALICE}?after={unreachable}&timeout_ms=100"
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        response
            .headers()
            .contains_key(axum::http::header::RETRY_AFTER),
        "a 503 carries Retry-After"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "not_caught_up");

    harness.shutdown();
}

#[tokio::test]
async fn status_reports_projector_position_and_lag() {
    let harness = boot();
    register(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position(&harness.rt, "users", 1).await;
    let app = server::app(Arc::clone(&harness.rt));

    let (status, body) = get(&app, "/status").await;
    assert_eq!(status, StatusCode::OK);
    let projectors = body["projectors"].as_array().unwrap();
    let users = projectors
        .iter()
        .find(|entry| entry["name"] == "users")
        .expect("users projector in status");
    assert!(users["position"].as_u64().unwrap() >= 1);
    assert!(users["lag"].is_number());
    // A healthy projector reports no failure.
    assert_eq!(users["failed"], json!(false));
    assert_eq!(users["last_error"], Value::Null);

    harness.shutdown();
}

#[tokio::test]
async fn status_reports_a_failed_projector() {
    // A projector whose handle fails on its first event: the thread dies rather
    // than silently freezing the read model, and /status must surface it as failed
    // with the error, not merely as lagging behind head.
    let dir = write_project(&[
        (
            "events/e.star",
            r#"boom = event(type = "boom.happened", fields = {"id": uuid()})
"#,
        ),
        (
            "commands/emit-boom.star",
            r#"
load("events/e.star", "boom")

input = schema(id = uuid())

def handle(input, state):
    return emit(boom(id = input.id))
"#,
        ),
        (
            "projectors/watch.star",
            r#"
load("events/e.star", "boom")

things = entity(key = "id", fields = {"id": uuid()})

source = [boom()]

def handle(event):
    fail("projector boom")
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    assert!(!project.has_errors(), "{:?}", project.findings);
    let data = tempfile::tempdir().unwrap();
    let http: Arc<dyn HttpClient> = Arc::new(StubHttpClient::status(400));
    let (rt, coord, projectors, effects) = Runtime::open(project, data.path(), http, None).unwrap();

    let ctx = CommandContext::new(Uuid::new_v4());
    let result = rt
        .execute("emit-boom", json!({ "id": ALICE }), &ctx, None)
        .unwrap();
    assert_eq!(result.status, 200, "emit failed: {:?}", result.body);

    let app = server::app(Arc::clone(&rt));
    let mut failed = None;
    for _ in 0..200 {
        let (_, body) = get(&app, "/status").await;
        let watch = body["projectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "watch")
            .cloned();
        if let Some(entry) = watch
            && entry["failed"] == json!(true)
        {
            failed = Some(entry);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let watch = failed.expect("watch projector should report failed in /status");
    assert!(
        watch["last_error"].as_str().unwrap().contains("boom"),
        "expected the handle error in last_error: {watch:?}"
    );

    effects.shutdown_and_join();
    projectors.shutdown_and_join();
    coord.shutdown();
}

#[tokio::test]
async fn a_filter_value_of_the_wrong_type_is_a_400() {
    // A value that cannot be the indexed column's type is a 400 up front, not a scan
    // that binds it as text and silently matches nothing.
    let dir = write_project(&[
        (
            "events/e.star",
            r#"scored = event(type = "scored", fields = {"id": uuid(), "n": i64_()})
"#,
        ),
        (
            "projectors/nums.star",
            r#"
load("events/e.star", "scored")

scores = entity(
    key = "id",
    fields = {"id": uuid(), "n": i64_()},
    indexes = [index("by_n", ["n"])],
)

source = [scored()]

def handle(event):
    return [put(scores, {"id": event.data["id"], "n": event.data["n"]})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    assert!(!project.has_errors(), "{:?}", project.findings);
    let data = tempfile::tempdir().unwrap();
    let http: Arc<dyn HttpClient> = Arc::new(StubHttpClient::status(400));
    let (rt, coord, projectors, effects) = Runtime::open(project, data.path(), http, None).unwrap();
    let app = server::app(Arc::clone(&rt));

    let (status, body) = get(&app, "/read/nums/scores?n=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");

    // A well-typed value is accepted (no rows yet, so it just scans empty).
    let (ok, _) = get(&app, "/read/nums/scores?n=5").await;
    assert_eq!(ok, StatusCode::OK);

    effects.shutdown_and_join();
    projectors.shutdown_and_join();
    coord.shutdown();
}
