//! The HTTP command surface: header handling around the idempotency key and the
//! generated OpenAPI document. Drives the in-process router with `oneshot`.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

mod support;

use support::{ALICE, BOB, boot_example, post_command};

#[tokio::test]
async fn blank_idempotency_key_is_not_a_shared_key() {
    let harness = boot_example();
    let app = harness.app();

    // A present-but-empty header must not be treated as a real key. Two unrelated
    // requests carrying an empty key must not collide: the first commits, the second
    // (a different user, same email) is rejected on state grounds, not replayed as
    // the first request's cached 200 or refused as an in-flight duplicate.
    let (first, first_body) = post_command(
        &app,
        "RegisterUser",
        json!({ "user_id": ALICE, "email": "dup@example.com", "name": "Alice" }),
        Some("   "),
    )
    .await;
    assert_eq!(first, StatusCode::OK, "{first_body:?}");

    let (second, second_body) = post_command(
        &app,
        "RegisterUser",
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
    let harness = boot_example();
    let app = harness.app();

    // A retry whose Idempotency-Key picked up surrounding whitespace (proxies do
    // this) must hash to the same tag, so the second request recovers the first's
    // outcome rather than re-running and rejecting the duplicate email.
    let (first, first_body) = post_command(
        &app,
        "RegisterUser",
        json!({ "user_id": ALICE, "email": "dup@example.com", "name": "Alice" }),
        Some("k1"),
    )
    .await;
    assert_eq!(first, StatusCode::OK, "{first_body:?}");

    let (second, second_body) = post_command(
        &app,
        "RegisterUser",
        json!({ "user_id": BOB, "email": "dup@example.com", "name": "Bob" }),
        Some("  k1  "),
    )
    .await;
    assert_eq!(second, StatusCode::OK, "{second_body:?}");
    assert_eq!(second_body["positions"], first_body["positions"]);

    harness.shutdown();
}

#[tokio::test]
async fn openapi_is_served_as_json() {
    let harness = boot_example();
    let app = harness.app();

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
        doc["paths"]["/commands/RegisterUser"].is_object(),
        "the public command is documented: {doc}"
    );

    harness.shutdown();
}

/// POST a raw (possibly non-JSON) body to a command, bypassing the serialization
/// `post_command` does, so the body contract itself can be exercised.
async fn post_raw(app: &Router, name: &str, body: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/commands/{name}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn a_non_object_or_malformed_body_is_a_400() {
    let harness = boot_example();
    let app = harness.app();

    // A JSON array is well-formed JSON but not a request body. It has to be refused
    // before dispatch: reaching input allocation would surface as an opaque 500.
    let (status, body) = post_raw(&app, "RegisterUser", "[1, 2]").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");
    assert_eq!(body["error"]["message"], "body must be a JSON object");

    // Malformed JSON is likewise a client error, with the parse failure explained.
    let (status, body) = post_raw(&app, "RegisterUser", "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("body is not valid JSON"),
        "the 400 explains the parse failure: {message}"
    );

    // An empty body becomes `{}` (so a command with no fields needs no payload) and
    // reaches schema validation, which is what reports the missing fields. A 500
    // here would mean the empty body was handed to dispatch as a non-object.
    let (status, body) = post_raw(&app, "RegisterUser", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("missing required field"),
        "an empty body reaches the input schema: {message}"
    );

    harness.shutdown();
}
