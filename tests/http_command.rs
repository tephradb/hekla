//! The HTTP command surface: header handling around the idempotency key and the
//! generated OpenAPI document. Drives the in-process router with `oneshot`.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

mod support;

use support::{ALICE, BOB, boot_example, boot_project, post_command, write_project};

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

/// The wire form of a `Timestamp` parameter.
///
/// heklang reads epoch microseconds, and every hekla boundary that *writes* one writes
/// RFC 3339: a read model's column, the read response built from it, and the
/// `date-time` the generated document declares for this very field. So the value a
/// client read out of a row has to post straight back, and both forms have to mean the
/// same instant.
#[tokio::test]
async fn a_timestamp_parameter_takes_rfc_3339_and_epoch_micros() {
    let project = write_project(&[
        (
            "events/slot.hk",
            "event @slot.booked {\n  slot_id: Uuid,\n  at: Timestamp,\n  until: Timestamp?,\n}\n",
        ),
        (
            "commands/book-slot.hk",
            "command BookSlot(slot_id: Uuid, at: Timestamp, until: Timestamp?) {\n  \
             state booked: Bool = fold false\n    on @slot.booked(slot_id) => true\n\n  \
             if booked {\n    return\n  }\n\n  emit @slot.booked { slot_id, at, until }\n}\n",
        ),
    ]);
    let harness = boot_project(project.path());
    let app = harness.app();

    let (status, text_body) = post_command(
        &app,
        "BookSlot",
        json!({ "slot_id": ALICE, "at": "2026-06-01T00:00:00Z" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text_body:?}");

    // An optional parameter takes the text form through its `Opt`, and an offset that
    // is not `Z` is the same instant rather than a second one.
    let (status, micros_body) = post_command(
        &app,
        "BookSlot",
        json!({ "slot_id": BOB, "at": 1_780_272_000_000_000i64, "until": "2026-06-01T02:00:00+02:00" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{micros_body:?}");
    assert_eq!(tag(&micros_body, "until:"), "until:1780272000000000");

    // The derived tag is the value as the log holds it, so equal tags mean the two
    // requests appended one instant rather than two that merely both parsed.
    assert_eq!(tag(&text_body, "at:"), tag(&micros_body, "at:"));
    assert_eq!(tag(&text_body, "at:"), "at:1780272000000000");

    // Text that is not RFC 3339 says so, rather than reporting the string itself as
    // the wrong kind of thing, which is what it used to be.
    let (status, body) = post_command(
        &app,
        "BookSlot",
        json!({ "slot_id": ALICE, "at": "2026-06-01T00:00:00" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");
    assert_eq!(
        body["error"]["message"],
        "`at`: expected Timestamp, stored text that is not RFC 3339"
    );

    harness.shutdown();
}

/// The tag starting with `prefix` on a command response's single emitted event.
fn tag(body: &Value, prefix: &str) -> String {
    body["events"][0]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|found| found.as_str().unwrap().to_owned())
        .find(|found| found.starts_with(prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` tag in {body:?}"))
}
