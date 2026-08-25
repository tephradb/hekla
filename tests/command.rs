//! End-to-end command execution through the runtime: outcome-to-status mapping,
//! echoed correlation/causation, idempotent replay, and the pinned clock. Each
//! test runs the real decision cycle against a fresh temp store and op DB.

use std::thread;

use serde_json::{Value, json};

mod support;

use support::{
    ALICE, BOB, Boot, Harness, boot_example, boot_example_at, ctx, drop_op_db, log_head,
    register_body, write_project,
};

#[test]
fn commits_a_new_registration() {
    let harness = boot_example();
    let ctx = ctx();
    let result = harness
        .rt
        .execute(
            "register-user",
            register_body(ALICE, "alice@example.com", "Alice"),
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
    harness.shutdown();
}

#[test]
fn rejects_a_taken_email_with_422() {
    let harness = boot_example();
    harness
        .rt
        .execute(
            "register-user",
            register_body(ALICE, "dup@example.com", "Alice"),
            &ctx(),
            None,
        )
        .unwrap();
    let result = harness
        .rt
        .execute(
            "register-user",
            register_body(BOB, "dup@example.com", "Bob"),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 422);
    assert_eq!(result.body["error"]["code"], "email_taken");
    harness.shutdown();
}

#[test]
fn missing_required_field_is_400() {
    let harness = boot_example();
    let result = harness
        .rt
        .execute(
            "register-user",
            json!({ "user_id": ALICE, "email": "alice@example.com" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 400);
    assert_eq!(result.body["error"]["code"], "invalid_input");
    harness.shutdown();
}

#[test]
fn wrong_typed_field_is_400() {
    let harness = boot_example();
    let result = harness
        .rt
        .execute(
            "register-user",
            json!({ "user_id": ALICE, "email": 42, "name": "Alice" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 400);
    harness.shutdown();
}

#[test]
fn unknown_command_is_404() {
    let harness = boot_example();
    let result = harness
        .rt
        .execute("does-not-exist", json!({}), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 404);
    harness.shutdown();
}

#[test]
fn internal_command_is_not_routed() {
    let harness = boot_example();
    let result = harness
        .rt
        .execute("record-welcome", json!({ "user_id": ALICE }), &ctx(), None)
        .unwrap();
    assert_eq!(result.status, 404);
    harness.shutdown();
}

#[test]
fn idempotent_replay_returns_the_original_outcome() {
    let harness = boot_example();
    let ctx1 = ctx();
    let body = register_body(ALICE, "alice@example.com", "Alice");
    let first = harness
        .rt
        .execute("register-user", body.clone(), &ctx1, Some("k1"))
        .unwrap();
    assert_eq!(first.status, 200);

    // A fresh run of the same request would now reject the duplicate email, but a
    // replay under the same key recovers the original 200 from the log, including the
    // original correlation id.
    let replay = harness
        .rt
        .execute("register-user", body, &ctx(), Some("k1"))
        .unwrap();
    assert_eq!(replay.status, 200);
    assert_eq!(replay.body, first.body);
    assert_eq!(
        replay.body["correlation_id"],
        ctx1.correlation_id.to_string()
    );
    harness.shutdown();
}

#[test]
fn now_is_available_in_handle() {
    let harness = boot_example();
    let result = harness
        .rt
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
    harness.shutdown();
}

#[test]
fn boundaryless_command_recovers_from_the_log_across_a_restart() {
    let data = tempfile::tempdir().unwrap();

    // First run: a boundaryless keyed command commits.
    let harness = boot_example_at(data.path());
    let ctx1 = ctx();
    let first = harness
        .rt
        .execute(
            "schedule-reminder",
            json!({ "user_id": ALICE }),
            &ctx1,
            Some("k1"),
        )
        .unwrap();
    assert_eq!(first.status, 200);
    harness.shutdown();

    // Restart with the operational DB gone: only the event log survives.
    drop_op_db(data.path());

    // Reopen over the same log and replay the same key. The outcome is recovered from
    // the log, byte-identical (original ids, positions, and the original
    // `now()`-derived event); the re-run's own emitted event never lands because the
    // append's existence clause rejects it.
    let harness = boot_example_at(data.path());
    let replay = harness
        .rt
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
    let fresh = harness
        .rt
        .execute(
            "schedule-reminder",
            json!({ "user_id": BOB }),
            &ctx(),
            Some("k2"),
        )
        .unwrap();
    assert_eq!(fresh.body["positions"]["first"].as_u64(), Some(last + 1));
    harness.shutdown();
}

#[test]
fn boundaried_command_recovers_instead_of_re_rejecting_across_a_restart() {
    let data = tempfile::tempdir().unwrap();

    // First run: a command with a real uniqueness boundary commits under a key.
    let harness = boot_example_at(data.path());
    let first = harness
        .rt
        .execute(
            "register-user",
            register_body(ALICE, "alice@example.com", "Alice"),
            &ctx(),
            Some("k1"),
        )
        .unwrap();
    assert_eq!(first.status, 200);
    harness.shutdown();

    // Restart with the operational DB gone, then reopen over the same log.
    drop_op_db(data.path());
    let harness = boot_example_at(data.path());

    // Replaying the key must recover the original 200 from the log. The replay
    // re-folds, sees the email already taken, and `handle` rejects; the reject arm's
    // tag re-read finds the prior commit and recovers its 200 instead of returning a
    // spurious 422 for a request that had succeeded.
    let replay = harness
        .rt
        .execute(
            "register-user",
            register_body(ALICE, "alice@example.com", "Alice"),
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
    harness.shutdown();
}

#[test]
fn concurrent_same_key_requests_commit_once_and_all_recover() {
    let harness = boot_example();
    let body = register_body(ALICE, "concurrent@example.com", "Alice");

    // Fire many same-key requests at a boundaried command at once. The append's
    // existence clause serializes them atomically: exactly one commits, and every
    // other loses either at the append (existence conflict) or at a re-fold reject,
    // and recovers the winner's outcome. No double-commit (#1), no spurious 422 (#2).
    let outcomes: Vec<(u16, serde_json::Value)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let rt = &harness.rt;
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

    let winner = &outcomes[0].1;
    for (status, body) in &outcomes {
        assert_eq!(
            *status, 200,
            "every same-key request returns 200, got {body:?}"
        );
        // Identical positions across all requests means a single physical commit: a
        // second commit would have carried a distinct position range.
        assert_eq!(body["positions"], winner["positions"]);
    }
    harness.shutdown();
}

/// A minimal project whose only command is idempotent by construction: once the
/// account is closed, `handle` returns no events rather than rejecting.
const CLOSE_ACCOUNT_EVENTS: &str = r#"
account_closed = event(
    type = "account.closed",
    fields = {"account_id": uuid()},
)
"#;

const CLOSE_ACCOUNT: &str = r#"
load("events/e.star", "account_closed")

input = schema(account_id = uuid())

def query(input):
    return account_closed(account_id = input.account_id)

initial = False

def fold_event(state, event):
    return True

fold = {all_events(): fold_event}

def handle(input, state):
    if state:
        return []
    return account_closed(account_id = input.account_id)
"#;

#[test]
fn empty_emit_replay_recovers_the_original_outcome() {
    let project = write_project(&[
        ("events/e.star", CLOSE_ACCOUNT_EVENTS),
        ("commands/close-account.star", CLOSE_ACCOUNT),
    ]);
    let harness = Boot::new(project.path()).http_status(200).start();

    let body = json!({ "account_id": ALICE });
    let ctx1 = ctx();
    let first = harness
        .rt
        .execute("close-account", body.clone(), &ctx1, Some("k1"))
        .unwrap();
    assert_eq!(first.status, 200);
    assert_eq!(first.body["events"][0]["type"], "account.closed");

    // The replay folds the committed event and `handle` emits nothing, so no append
    // happens and the existence clause never fires. Recovery has to come from the tag
    // re-read; without it the client gets an empty 200 where the original returned the
    // committed events and positions.
    let replay = harness
        .rt
        .execute("close-account", body, &ctx(), Some("k1"))
        .unwrap();
    assert_eq!(replay.status, 200);
    assert_eq!(
        replay.body, first.body,
        "an empty emit must replay the original outcome"
    );
    assert_eq!(
        replay.body["correlation_id"],
        ctx1.correlation_id.to_string(),
        "recovery uses the original request's identity, not the replay's"
    );
    harness.shutdown();
}

#[test]
fn concurrent_renames_of_the_same_user_all_commit_after_retrying() {
    let harness = boot_example();
    let registered = harness
        .rt
        .execute(
            "register-user",
            register_body(ALICE, "alice@example.com", "Alice"),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(registered.status, 200, "{:?}", registered.body);

    // `rename-user`'s boundary is this user's whole history, so four unkeyed renames
    // of the same user all collide on it. Each loser must re-read and retry: unlike
    // the same-key case there is no idempotency tag to recover from, so a boundary
    // conflict that stopped being retried would surface as a 409.
    let outcomes: Vec<(u16, Value)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let rt = &harness.rt;
                scope.spawn(move || {
                    let result = rt
                        .execute(
                            "rename-user",
                            json!({ "user_id": ALICE, "name": format!("n{i}") }),
                            &ctx(),
                            None,
                        )
                        .unwrap();
                    (result.status, result.body)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut firsts: Vec<u64> = Vec::new();
    for (status, body) in &outcomes {
        assert_eq!(
            *status, 200,
            "a boundary conflict is retried, not surfaced: {body:?}"
        );
        firsts.push(body["positions"]["first"].as_u64().unwrap());
    }
    // Distinct start positions mean four separate physical commits: a retry that
    // reused stale folded state, or an attempt appended twice, would repeat one.
    firsts.sort_unstable();
    firsts.dedup();
    assert_eq!(
        firsts.len(),
        4,
        "each rename commits exactly once: {firsts:?}"
    );

    // One registration plus four renames, and nothing else: the welcome effect's
    // stubbed POST answers 400, so no `user.welcomed` lands.
    assert_eq!(log_head(&harness.rt), 5);
    harness.shutdown();
}

#[test]
fn a_drained_write_coordinator_returns_503_unavailable() {
    let data = tempfile::tempdir().unwrap();
    // Drain by hand rather than through `Harness::shutdown`, which consumes the
    // runtime along with the coordinator. Once `WriteCoordinator::shutdown` has
    // joined the writer thread, any later append sends on a disconnected channel.
    let Harness {
        rt,
        coord,
        projectors,
        effects,
        ..
    } = boot_example_at(data.path());
    effects.shutdown_and_join();
    projectors.shutdown_and_join();
    coord.shutdown();

    // A boundaryless command reads nothing, so the only thing that can fail is the
    // append itself.
    let result = rt
        .execute(
            "schedule-reminder",
            json!({ "user_id": ALICE }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 503, "{:?}", result.body);
    assert_eq!(result.body["error"]["code"], "unavailable");
    let message = result.body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("retry"),
        "a write that never landed must read as retryable: {message}"
    );
}

#[test]
fn the_same_idempotency_key_on_two_commands_does_not_collide() {
    let harness = boot_example();

    // A client reusing one key across a workflow's two calls is normal. The tag
    // hashes the command name alongside the key, so the second call must decide for
    // itself rather than replay the first call's outcome.
    let first = harness
        .rt
        .execute(
            "register-user",
            register_body(ALICE, "alice@example.com", "Alice"),
            &ctx(),
            Some("shared-key"),
        )
        .unwrap();
    assert_eq!(first.status, 200, "{:?}", first.body);
    assert_eq!(first.body["events"][0]["type"], "user.registered");

    let second = harness
        .rt
        .execute(
            "schedule-reminder",
            json!({ "user_id": ALICE }),
            &ctx(),
            Some("shared-key"),
        )
        .unwrap();
    assert_eq!(second.status, 200, "{:?}", second.body);
    assert_eq!(
        second.body["events"][0]["type"], "reminder.scheduled",
        "the second command must not replay the first's event"
    );
    assert_ne!(
        second.body["positions"]["first"], first.body["positions"]["first"],
        "a recovered outcome would carry the first command's positions"
    );
    assert_eq!(log_head(&harness.rt), 2, "both commands actually appended");
    harness.shutdown();
}

#[test]
fn a_keyed_request_that_rejects_returns_422_and_does_not_burn_the_key() {
    let harness = boot_example();
    let committed = harness
        .rt
        .execute(
            "register-user",
            register_body(ALICE, "dup@example.com", "Alice"),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(committed.status, 200, "{:?}", committed.body);

    // A keyed request that genuinely should be rejected: the reject arm re-reads the
    // tag, finds no prior commit under this key, and surfaces the rejection.
    let rejected = harness
        .rt
        .execute(
            "register-user",
            register_body(BOB, "dup@example.com", "Bob"),
            &ctx(),
            Some("k9"),
        )
        .unwrap();
    assert_eq!(rejected.status, 422, "{:?}", rejected.body);
    assert_eq!(rejected.body["error"]["code"], "email_taken");
    assert_eq!(
        log_head(&harness.rt),
        1,
        "a rejection appends nothing, so it writes no tag either"
    );

    // The key was therefore never burned: the same key on a body that can succeed
    // commits for real instead of replaying the rejection or a phantom 200.
    let retried = harness
        .rt
        .execute(
            "register-user",
            register_body(BOB, "bob@example.com", "Bob"),
            &ctx(),
            Some("k9"),
        )
        .unwrap();
    assert_eq!(retried.status, 200, "{:?}", retried.body);
    assert_eq!(retried.body["events"][0]["type"], "user.registered");
    assert_eq!(
        retried.body["positions"]["first"].as_u64(),
        Some(committed.body["positions"]["last"].as_u64().unwrap() + 1),
        "the retry lands as a fresh append"
    );
    harness.shutdown();
}

#[test]
fn an_unknown_field_in_the_body_is_rejected_with_400() {
    let harness = boot_example();
    let result = harness
        .rt
        .execute(
            "register-user",
            json!({
                "user_id": ALICE,
                "email": "alice@example.com",
                "name": "Alice",
                "nickname": "Al",
            }),
            &ctx(),
            None,
        )
        .unwrap();
    // The input schema is strict: a typo'd or stale field is a client error, not a
    // silently dropped one that lets the command run against a body it never got.
    assert_eq!(result.status, 400, "{:?}", result.body);
    assert_eq!(result.body["error"]["code"], "invalid_input");
    let message = result.body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("unknown field `nickname`"),
        "the error names the offending field: {message}"
    );
    assert_eq!(
        log_head(&harness.rt),
        0,
        "input validation happens before any append"
    );
    harness.shutdown();
}
