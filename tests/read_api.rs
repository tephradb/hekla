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
use kiln::loader::LoadedProject;
use kiln::projector::ProjectorSet;
use kiln::runtime::Runtime;
use kiln::server;
use serde_json::{Value, json};
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
    _data: TempDir,
}

impl Harness {
    fn shutdown(self) {
        self.projectors.shutdown_and_join();
        self.coord.shutdown();
    }
}

fn boot() -> Harness {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);
    assert!(!project.has_errors(), "{:?}", project.findings);
    let data = tempfile::tempdir().unwrap();
    let (runtime, coord, projectors) = Runtime::open(project, data.path()).unwrap();
    Harness {
        rt: Arc::new(runtime),
        coord,
        projectors,
        _data: data,
    }
}

fn register(rt: &Runtime, user_id: &str, email: &str, name: &str) {
    let ctx = CommandContext::new(Uuid::new_v4());
    let body = json!({ "user_id": user_id, "email": email, "name": name });
    let result = rt.execute("register-user", body, &ctx, None).unwrap();
    assert_eq!(result.status, 200, "register failed: {:?}", result.body);
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

    harness.shutdown();
}
