//! The HTTP command surface: header handling around the idempotency key and the
//! generated OpenAPI document. Drives the in-process router with `oneshot`.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use kiln::effect::{EffectRuntime, HttpClient, StubHttpClient};
use kiln::loader::LoadedProject;
use kiln::projector::ProjectorSet;
use kiln::runtime::Runtime;
use kiln::server;
use serde_json::{Value, json};
use tempfile::TempDir;
use tephra::WriteCoordinator;
use tower::ServiceExt;

const ALICE: &str = "11111111-1111-1111-1111-111111111111";
const BOB: &str = "22222222-2222-2222-2222-222222222222";

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

fn boot() -> Harness {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);
    assert!(!project.has_errors(), "{:?}", project.findings);
    let data = tempfile::tempdir().unwrap();
    let http: Arc<dyn HttpClient> = Arc::new(StubHttpClient::status(400));
    let (rt, coord, projectors, effects) = Runtime::open(project, data.path(), http).unwrap();
    Harness {
        rt,
        coord,
        projectors,
        effects,
        _data: data,
    }
}

/// POST a command with an optional `Idempotency-Key` header.
async fn post_command(
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

#[tokio::test]
async fn blank_idempotency_key_is_not_a_shared_key() {
    let harness = boot();
    let app = server::app(Arc::clone(&harness.rt));

    // A present-but-empty header must not be treated as a real key. Two unrelated
    // requests carrying an empty key must not collide: the first commits, the second
    // (a different user, same email) is rejected on state grounds, not replayed as
    // the first request's cached 200 or refused as an in-flight duplicate.
    let (first, first_body) = post_command(
        &app,
        "register-user",
        json!({ "user_id": ALICE, "email": "dup@example.com", "name": "Alice" }),
        Some("   "),
    )
    .await;
    assert_eq!(first, StatusCode::OK, "{first_body:?}");

    let (second, second_body) = post_command(
        &app,
        "register-user",
        json!({ "user_id": BOB, "email": "dup@example.com", "name": "Bob" }),
        Some(""),
    )
    .await;
    assert_eq!(second, StatusCode::UNPROCESSABLE_ENTITY, "{second_body:?}");
    assert_eq!(second_body["error"]["code"], "email_taken");

    harness.shutdown();
}

#[tokio::test]
async fn whitespace_only_differs_in_the_key_still_dedupes() {
    let harness = boot();
    let app = server::app(Arc::clone(&harness.rt));

    // A retry whose Idempotency-Key picked up surrounding whitespace (proxies do
    // this) must hash to the same tag, so the second request recovers the first's
    // outcome rather than re-running and rejecting the duplicate email.
    let (first, first_body) = post_command(
        &app,
        "register-user",
        json!({ "user_id": ALICE, "email": "dup@example.com", "name": "Alice" }),
        Some("k1"),
    )
    .await;
    assert_eq!(first, StatusCode::OK, "{first_body:?}");

    let (second, second_body) = post_command(
        &app,
        "register-user",
        json!({ "user_id": BOB, "email": "dup@example.com", "name": "Bob" }),
        Some("  k1  "),
    )
    .await;
    assert_eq!(second, StatusCode::OK, "{second_body:?}");
    assert_eq!(second_body["positions"], first_body["positions"]);
}

#[tokio::test]
async fn openapi_is_served_as_json() {
    let harness = boot();
    let app = server::app(Arc::clone(&harness.rt));

    let request = Request::builder()
        .method(Method::GET)
        .uri("/openapi.json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["openapi"], "3.1.0");
    assert!(
        doc["paths"]["/commands/register-user"].is_object(),
        "the public command is documented: {doc}"
    );

    harness.shutdown();
}
