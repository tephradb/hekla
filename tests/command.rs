//! End-to-end command execution through the runtime: outcome-to-status mapping,
//! echoed correlation/causation, idempotent replay, and the pinned clock. Each
//! test runs the real decision cycle against a fresh temp store and op DB.

use std::path::Path;

use kiln::context::CommandContext;
use kiln::loader::LoadedProject;
use kiln::runtime::Runtime;
use serde_json::json;
use tempfile::TempDir;
use tephra::WriteCoordinator;
use uuid::Uuid;

/// Open the example project against a throwaway data directory.
fn open() -> (Runtime, WriteCoordinator, TempDir) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);
    assert!(
        !project.has_errors(),
        "example project has errors: {:?}",
        project.findings
    );
    let data = tempfile::tempdir().unwrap();
    let (runtime, coordinator) = Runtime::open(project, data.path()).unwrap();
    (runtime, coordinator, data)
}

fn ctx() -> CommandContext {
    CommandContext::new(Uuid::new_v4())
}

fn register(user_id: &str, email: &str, name: &str) -> serde_json::Value {
    json!({ "user_id": user_id, "email": email, "name": name })
}

const ALICE: &str = "11111111-1111-1111-1111-111111111111";
const BOB: &str = "22222222-2222-2222-2222-222222222222";

#[test]
fn commits_a_new_registration() {
    let (rt, coord, _data) = open();
    let ctx = ctx();
    let result = rt
        .execute(
            "register-user",
            register(ALICE, "alice@example.com", "Alice"),
            &ctx,
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200);
    assert_eq!(result.body["events"][0]["type"], "user.registered");
    assert_eq!(
        result.body["correlation_id"],
        ctx.correlation_id.to_string()
    );
    assert_eq!(result.body["causation_id"], ctx.causation_id.to_string());
    assert!(result.body["positions"]["first"].is_number());
    coord.shutdown();
}

#[test]
fn rejects_a_taken_email_with_422() {
    let (rt, coord, _data) = open();
    rt.execute(
        "register-user",
        register(ALICE, "dup@example.com", "Alice"),
        &ctx(),
        None,
    )
    .unwrap();
    let result = rt
        .execute(
            "register-user",
            register(BOB, "dup@example.com", "Bob"),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 422);
    assert_eq!(result.body["error"]["code"], "email_taken");
    coord.shutdown();
}

#[test]
fn missing_required_field_is_400() {
    let (rt, coord, _data) = open();
    let result = rt
        .execute(
            "register-user",
            json!({ "user_id": ALICE, "email": "alice@example.com" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 400);
    assert_eq!(result.body["error"]["code"], "invalid_input");
    coord.shutdown();
}

#[test]
fn wrong_typed_field_is_400() {
    let (rt, coord, _data) = open();
    let result = rt
        .execute(
            "register-user",
            json!({ "user_id": ALICE, "email": 42, "name": "Alice" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 400);
    coord.shutdown();
}

#[test]
fn unknown_command_is_404() {
    let (rt, coord, _data) = open();
    let result = rt
        .execute("does-not-exist", json!({}), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 404);
    coord.shutdown();
}

#[test]
fn internal_command_is_not_routed() {
    let (rt, coord, _data) = open();
    let result = rt
        .execute("record-welcome", json!({ "user_id": ALICE }), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 404);
    coord.shutdown();
}

#[test]
fn idempotent_replay_returns_the_original_outcome() {
    let (rt, coord, _data) = open();
    let ctx1 = ctx();
    let body = register(ALICE, "alice@example.com", "Alice");
    let first = rt
        .execute("register-user", body.clone(), &ctx1, Some("k1"))
        .unwrap();
    assert_eq!(first.status, 200);

    // A fresh run of the same request would now reject the duplicate email, but a
    // replay under the same key returns the stored 200, including the original
    // correlation id.
    let ctx2 = ctx();
    let replay = rt
        .execute("register-user", body, &ctx2, Some("k1"))
        .unwrap();
    assert_eq!(replay.status, 200);
    assert_eq!(replay.body, first.body);
    assert_eq!(
        replay.body["correlation_id"],
        ctx1.correlation_id.to_string()
    );
    coord.shutdown();
}

#[test]
fn now_is_available_in_handle() {
    let (rt, coord, _data) = open();
    let result = rt
        .execute(
            "schedule-reminder",
            json!({ "user_id": ALICE }),
            &ctx(),
            None,
        )
        .unwrap();
    // A 200 means now() returned a value the timestamp field accepted and the
    // event committed; had now() errored, the command would have failed.
    assert_eq!(result.status, 200);
    assert_eq!(result.body["events"][0]["type"], "reminder.scheduled");
    coord.shutdown();
}
