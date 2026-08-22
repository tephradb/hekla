//! End-to-end command execution through the runtime: outcome-to-status mapping,
//! echoed correlation/causation, idempotent replay, and the pinned clock. Each
//! test runs the real decision cycle against a fresh temp store and op DB.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use kiln::context::CommandContext;
use kiln::effect::{EffectRuntime, HttpClient, StubHttpClient};
use kiln::loader::LoadedProject;
use kiln::projector::ProjectorSet;
use kiln::runtime::Runtime;
use serde_json::json;
use tempfile::TempDir;
use tephra::WriteCoordinator;
use uuid::Uuid;

type Parts = (Arc<Runtime>, WriteCoordinator, ProjectorSet, EffectRuntime);

/// Open the example project against a throwaway data directory. Effects run with a
/// stub HTTP client, so registering a user fires the welcome effect without
/// touching the network.
fn open() -> (
    Arc<Runtime>,
    WriteCoordinator,
    ProjectorSet,
    EffectRuntime,
    TempDir,
) {
    let data = tempfile::tempdir().unwrap();
    let parts = open_at(data.path());
    (parts.0, parts.1, parts.2, parts.3, data)
}

/// Open the example project against an explicit data directory, so a test can reopen
/// the same event log under a fresh operational DB.
fn open_at(data_dir: &Path) -> Parts {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);
    assert!(
        !project.has_errors(),
        "example project has errors: {:?}",
        project.findings
    );
    let http: Arc<dyn HttpClient> = Arc::new(StubHttpClient::status(400));
    Runtime::open(project, data_dir, http, None).unwrap()
}

/// Delete `kiln.db` (and its WAL sidecars) while the event log survives, then reopen.
/// Command idempotency lives entirely in the log, so this proves a replay recovers
/// across a restart even with the operational DB gone (it holds only effect state).
fn drop_op_db(data_dir: &Path) {
    for name in ["kiln.db", "kiln.db-wal", "kiln.db-shm"] {
        let path = data_dir.join(name);
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
}

/// Drain effects, then projectors, then the writer.
fn shutdown(effects: EffectRuntime, projectors: ProjectorSet, coord: WriteCoordinator) {
    effects.shutdown_and_join();
    projectors.shutdown_and_join();
    coord.shutdown();
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
    let (rt, coord, projectors, effects, _data) = open();
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
    shutdown(effects, projectors, coord);
}

#[test]
fn rejects_a_taken_email_with_422() {
    let (rt, coord, projectors, effects, _data) = open();
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
    shutdown(effects, projectors, coord);
}

#[test]
fn missing_required_field_is_400() {
    let (rt, coord, projectors, effects, _data) = open();
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
    shutdown(effects, projectors, coord);
}

#[test]
fn wrong_typed_field_is_400() {
    let (rt, coord, projectors, effects, _data) = open();
    let result = rt
        .execute(
            "register-user",
            json!({ "user_id": ALICE, "email": 42, "name": "Alice" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 400);
    shutdown(effects, projectors, coord);
}

#[test]
fn unknown_command_is_404() {
    let (rt, coord, projectors, effects, _data) = open();
    let result = rt
        .execute("does-not-exist", json!({}), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 404);
    shutdown(effects, projectors, coord);
}

#[test]
fn internal_command_is_not_routed() {
    let (rt, coord, projectors, effects, _data) = open();
    let result = rt
        .execute("record-welcome", json!({ "user_id": ALICE }), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 404);
    shutdown(effects, projectors, coord);
}

#[test]
fn idempotent_replay_returns_the_original_outcome() {
    let (rt, coord, projectors, effects, _data) = open();
    let ctx1 = ctx();
    let body = register(ALICE, "alice@example.com", "Alice");
    let first = rt
        .execute("register-user", body.clone(), &ctx1, Some("k1"))
        .unwrap();
    assert_eq!(first.status, 200);

    // A fresh run of the same request would now reject the duplicate email, but a
    // replay under the same key recovers the original 200 from the log, including the
    // original correlation id.
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
    shutdown(effects, projectors, coord);
}

#[test]
fn now_is_available_in_handle() {
    let (rt, coord, projectors, effects, _data) = open();
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
    shutdown(effects, projectors, coord);
}

#[test]
fn boundaryless_command_recovers_from_the_log_across_a_restart() {
    let data = tempfile::tempdir().unwrap();

    // First run: a boundaryless keyed command commits.
    let (rt, coord, projectors, effects) = open_at(data.path());
    let ctx1 = ctx();
    let first = rt
        .execute(
            "schedule-reminder",
            json!({ "user_id": ALICE }),
            &ctx1,
            Some("k1"),
        )
        .unwrap();
    assert_eq!(first.status, 200);
    shutdown(effects, projectors, coord);

    // Restart with the operational DB gone: only the event log survives.
    drop_op_db(data.path());

    // Reopen over the same log and replay the same key. The outcome is recovered from
    // the log, byte-identical (original ids, positions, and the original
    // `now()`-derived event); the re-run's own emitted event never lands because the
    // append's existence clause rejects it.
    let (rt, coord, projectors, effects) = open_at(data.path());
    let replay = rt
        .execute(
            "schedule-reminder",
            json!({ "user_id": ALICE }),
            &ctx(),
            Some("k1"),
        )
        .unwrap();
    assert_eq!(replay.status, 200);
    assert_eq!(
        replay.body, first.body,
        "replay must recover the original outcome"
    );
    assert_eq!(
        replay.body["correlation_id"],
        ctx1.correlation_id.to_string(),
        "recovery uses the original request's identity, not the replay's"
    );

    // And no duplicate was appended: a fresh key lands right after the single
    // original event, proving the replay wrote nothing.
    let last = first.body["positions"]["last"].as_u64().unwrap();
    let fresh = rt
        .execute(
            "schedule-reminder",
            json!({ "user_id": BOB }),
            &ctx(),
            Some("k2"),
        )
        .unwrap();
    assert_eq!(fresh.body["positions"]["first"].as_u64(), Some(last + 1));
    shutdown(effects, projectors, coord);
}

#[test]
fn boundaried_command_recovers_instead_of_re_rejecting_across_a_restart() {
    let data = tempfile::tempdir().unwrap();

    // First run: a command with a real uniqueness boundary commits under a key.
    let (rt, coord, projectors, effects) = open_at(data.path());
    let first = rt
        .execute(
            "register-user",
            register(ALICE, "alice@example.com", "Alice"),
            &ctx(),
            Some("k1"),
        )
        .unwrap();
    assert_eq!(first.status, 200);
    shutdown(effects, projectors, coord);

    // Restart with the operational DB gone, then reopen over the same log.
    drop_op_db(data.path());
    let (rt, coord, projectors, effects) = open_at(data.path());

    // Replaying the key must recover the original 200 from the log. The replay
    // re-folds, sees the email already taken, and `handle` rejects; the reject arm's
    // tag re-read finds the prior commit and recovers its 200 instead of returning a
    // spurious 422 for a request that had succeeded.
    let replay = rt
        .execute(
            "register-user",
            register(ALICE, "alice@example.com", "Alice"),
            &ctx(),
            Some("k1"),
        )
        .unwrap();
    assert_eq!(
        replay.status, 200,
        "expected recovery, got {:?}",
        replay.body
    );
    assert_eq!(replay.body["positions"], first.body["positions"]);
    shutdown(effects, projectors, coord);
}

#[test]
fn concurrent_same_key_requests_commit_once_and_all_recover() {
    let (rt, coord, projectors, effects, _data) = open();
    let body = register(ALICE, "concurrent@example.com", "Alice");

    // Fire many same-key requests at a boundaried command at once. The append's
    // existence clause serializes them atomically: exactly one commits, and every
    // other loses either at the append (existence conflict) or at a re-fold reject,
    // and recovers the winner's outcome. No double-commit (#1), no spurious 422 (#2).
    let outcomes: Vec<(u16, serde_json::Value)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let rt = &rt;
                let body = body.clone();
                scope.spawn(move || {
                    let result = rt
                        .execute("register-user", body, &ctx(), Some("dup"))
                        .unwrap();
                    (result.status, result.body)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let (_, ref winner) = outcomes[0];
    for (status, body) in &outcomes {
        assert_eq!(
            *status, 200,
            "every same-key request returns 200, got {body:?}"
        );
        // Identical positions across all requests means a single physical commit: a
        // second commit would have carried a distinct position range.
        assert_eq!(body["positions"], winner["positions"]);
    }
    shutdown(effects, projectors, coord);
}
