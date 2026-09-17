//! End-to-end read API: register a user through the command path, wait for the
//! projector to catch up, then read it back over HTTP through the in-process
//! router. Exercises point reads, the indexed filter, the unindexed-filter 400,
//! cursor-carrying responses, the projector position, and the replay route. Also
//! covers limit clamping, bad cursors, filtering on an index prefix and ranging over
//! the column after it, filtered pagination, an integer-keyed entity, entities whose
//! identifiers are SQL keywords, and what the read surface still serves once a
//! projector has died.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hekla::runtime::Runtime;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

mod support;

use support::{
    ALICE, BOB, Harness, MISSING, UUID_A, UUID_B, UUID_C, boot_example, boot_project, ctx, get,
    register_user, send, wait_position_async, write_project,
};

#[tokio::test]
async fn reads_a_row_and_the_indexed_filter_through_http() {
    let harness = boot_example();
    register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position_async(&harness.rt, "Users", 1).await;
    let app = harness.app();

    let (status, body) = get(&app, &format!("/read/Users/User/{ALICE}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item"]["email"], "alice@example.com");
    assert_eq!(body["item"]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= 1);

    // Indexed filter on email returns the same row.
    let (status, body) = get(&app, "/read/Users/User?email=alice@example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= 1);

    // A get() projector maintained the running count.
    let (status, body) = get(&app, "/read/UserStats/Totals/all").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item"]["count"].as_i64(), Some(1));

    harness.shutdown();
}

#[tokio::test]
async fn unindexed_filter_and_missing_targets_are_rejected() {
    let harness = boot_example();
    register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position_async(&harness.rt, "Users", 1).await;
    let app = harness.app();

    // `name` is not indexed: a 400, never a table scan.
    let (status, body) = get(&app, "/read/Users/User?name=Alice").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "unindexed_filter");

    // Missing row, unknown entity, and unknown projector are all 404.
    let (status, _) = get(&app, &format!("/read/Users/User/{MISSING}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/read/Users/Ghost/x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/read/Ghost/User/x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    harness.shutdown();
}

#[tokio::test]
async fn scan_paginates_with_a_cursor() {
    let harness = boot_example();
    for i in 0..5 {
        let id = format!("00000000-0000-0000-0000-00000000000{i}");
        register_user(&harness.rt, &id, &format!("user{i}@example.com"), "User");
    }
    wait_position_async(&harness.rt, "Users", 5).await;
    let app = harness.app();

    let mut seen = Vec::new();
    let mut uri = "/read/Users/User?limit=2".to_owned();
    loop {
        let (status, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::OK);
        for item in body["items"].as_array().unwrap() {
            seen.push(item["user_id"].as_str().unwrap().to_owned());
        }
        match body["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/read/Users/User?limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5);
    assert!(
        seen.is_sorted(),
        "cursor pages come back ordered by key: {seen:?}"
    );
    let mut deduped = seen.clone();
    deduped.dedup();
    assert_eq!(deduped.len(), 5, "every row exactly once");

    harness.shutdown();
}

#[tokio::test]
async fn replay_route_is_accepted() {
    let harness = boot_example();
    register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position_async(&harness.rt, "Users", 1).await;
    let app = harness.app();

    let (status, body) = send(&app, Method::POST, "/projectors/Users/replay").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "replay_scheduled");

    let (status, _) = send(&app, Method::POST, "/projectors/ghost/replay").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    harness.shutdown();
}

#[tokio::test]
async fn after_waits_for_the_projector_then_reads_your_write() {
    let harness = boot_example();
    // Deliberately no wait_position: the read must block until the projector
    // catches up on its own, which is the whole point of `?after=`.
    let pos = register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    let app = harness.app();

    let (status, body) = get(&app, &format!("/read/Users/User/{ALICE}?after={pos}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item"]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= pos);

    harness.shutdown();
}

#[tokio::test]
async fn after_waits_for_the_projector_on_a_scan() {
    let harness = boot_example();
    let pos = register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    let app = harness.app();

    let (status, body) = get(&app, &format!("/read/Users/User?after={pos}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["user_id"], ALICE);
    assert!(body["position"].as_u64().unwrap() >= pos);

    harness.shutdown();
}

#[tokio::test]
async fn after_reserves_its_slot_and_does_not_shadow_a_filter() {
    let harness = boot_example();
    let pos = register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    let app = harness.app();

    // `after` is a reserved param, not a filter field; the email filter still binds.
    let (status, body) = get(
        &app,
        &format!("/read/Users/User?email=alice@example.com&after={pos}"),
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
    let harness = boot_example();
    register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    let rename = harness
        .rt
        .execute(
            "RenameUser",
            json!({ "user_id": ALICE, "name": "Alicia" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(rename.status, 200, "rename failed: {:?}", rename.body);
    let pos = rename.body["positions"]["last"].as_u64().unwrap();
    let app = harness.app();

    let (status, body) = get(
        &app,
        &format!("/read/UserStats/Totals/all?after={pos}&timeout_ms=2000"),
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
    let harness = boot_example();
    let pos = register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position_async(&harness.rt, "Users", pos).await;
    let app = harness.app();

    // Already caught up: a 0ms wait still succeeds on the first check.
    let (status, _) = get(
        &app,
        &format!("/read/Users/User/{ALICE}?after={pos}&timeout_ms=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Not caught up: a 0ms wait fails closed immediately rather than blocking.
    let unreachable = pos + 1000;
    let (status, body) = get(
        &app,
        &format!("/read/Users/User/{ALICE}?after={unreachable}&timeout_ms=0"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "not_caught_up");

    harness.shutdown();
}

#[tokio::test]
async fn a_non_numeric_after_is_a_400() {
    let harness = boot_example();
    register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position_async(&harness.rt, "Users", 1).await;
    let app = harness.app();

    let (status, body) = get(&app, &format!("/read/Users/User/{ALICE}?after=abc")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_after");

    harness.shutdown();
}

#[tokio::test]
async fn after_beyond_the_log_times_out_with_503_and_retry_after() {
    let harness = boot_example();
    let pos = register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position_async(&harness.rt, "Users", pos).await;
    let app = harness.app();

    // A position the projector can never reach, with a short budget so the wait
    // gives up fast and fails closed.
    let unreachable = pos + 1000;
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "/read/Users/User/{ALICE}?after={unreachable}&timeout_ms=100"
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        response.headers().contains_key(header::RETRY_AFTER),
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
    let harness = boot_example();
    register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position_async(&harness.rt, "Users", 1).await;
    let app = harness.app();

    let (status, body) = get(&app, "/status").await;
    assert_eq!(status, StatusCode::OK);
    let projectors = body["projectors"].as_array().unwrap();
    let users = projectors
        .iter()
        .find(|entry| entry["name"] == "Users")
        .expect("users projector in status");
    assert!(users["position"].as_u64().unwrap() >= 1);
    assert!(users["lag"].is_number());
    assert_eq!(users["failed"], json!(false));
    assert_eq!(users["last_error"], Value::Null);

    harness.shutdown();
}

/// The event a failing projector chokes on, and one it does not.
///
/// A projector has no `fail`: it cannot refuse, because a rebuild has to reach the
/// same rows every time. What it can still do is hit a real store error, and the one
/// an author actually meets is a read model narrower than the value reaching it. The
/// column below takes three characters and the event field takes fifty.
///
/// The write is an interpolation rather than the field itself, because heklang's `@max`
/// invariant now rejects the plain form statically (`heklang/docs/projectors.md`): a
/// column bounding an event field tighter than the event declares it is a schema bug it
/// catches at `hek check`. An interpolation has no second declaration to compare
/// against, so it is what the runtime error is still for, which is what this exercises.
const BOOM_EVENTS: &str = r#"
event @boom.happened { id: Uuid, label: String @max(50) }
"#;

const EMIT_BOOM: &str = r#"
command EmitBoom(id: Uuid, label: String) {
  emit @boom.happened { id, label }
}
"#;

const NARROW_PROJECTOR: &str = r#"
projector Watch {
  entity Thing {
    id: Uuid @key,
    label: String @max(3),
  }

  on @boom.happened { id, label } {
    put Thing { id, label: "{label}" }
  }
}
"#;

#[tokio::test]
async fn status_reports_a_failed_projector() {
    // A projector that cannot apply an event: the thread dies rather than silently
    // freezing the read model, and /status must surface it as failed with the error,
    // not merely as lagging behind head.
    let dir = write_project(&[
        ("events/e.hk", BOOM_EVENTS),
        ("commands/emit-boom.hk", EMIT_BOOM),
        ("projectors/watch.hk", NARROW_PROJECTOR),
    ]);
    let harness = boot_project(dir.path());

    let result = harness
        .rt
        .execute(
            "EmitBoom",
            json!({ "id": ALICE, "label": "far too long for the column" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "emit failed: {:?}", result.body);

    let app = harness.app();
    let watch = wait_failed(&app, "Watch").await;
    assert!(
        watch["last_error"].as_str().unwrap().contains("label"),
        "the error names the column that would not take the value: {watch:?}"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_filter_value_of_the_wrong_type_is_a_400() {
    // A value that cannot be the indexed column's type is a 400 up front, not a scan
    // that binds it as text and silently matches nothing.
    let dir = write_project(&[
        ("events/e.hk", "event @scored { id: Uuid, n: Int }\n"),
        (
            "projectors/scores.hk",
            r#"
projector Scores {
  entity Score {
    id: Uuid @key,
    n: Int @index,
  }

  on @scored { id, n } {
    put Score { id, n }
  }
}
"#,
        ),
    ]);
    let harness = boot_project(dir.path());
    let app = harness.app();

    let (status, body) = get(&app, "/read/Scores/Score?n=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");

    // A well-typed value is accepted (no rows yet, so it just scans empty).
    let (ok, _) = get(&app, "/read/Scores/Score?n=5").await;
    assert_eq!(ok, StatusCode::OK);

    harness.shutdown();
}

/// Poll `/status` until `projector` reports `failed`, then return its entry.
async fn wait_failed(app: &Router, projector: &str) -> Value {
    for _ in 0..500 {
        let (_, body) = get(app, "/status").await;
        let entry = body["projectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == projector)
            .cloned();
        if let Some(entry) = entry
            && entry["failed"] == json!(true)
        {
            return entry;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("projector `{projector}` never reported failed in /status");
}

#[tokio::test]
async fn a_failed_projector_still_serves_its_last_good_rows() {
    // A dead projector freezes its read model; it must not take the read surface with
    // it. Rows written before the failure stay readable, and a `?after=` past the
    // frozen position fails closed with a 503 rather than pinning the request forever.
    let dir = write_project(&[
        ("events/e.hk", BOOM_EVENTS),
        ("commands/emit-boom.hk", EMIT_BOOM),
        ("projectors/watch.hk", NARROW_PROJECTOR),
    ]);
    let harness = boot_project(dir.path());
    let app = harness.app();

    let ok = harness
        .rt
        .execute(
            "EmitBoom",
            json!({ "id": ALICE, "label": "ok" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(ok.status, 200, "the first emit failed: {:?}", ok.body);
    let ok_pos = ok.body["positions"]["last"].as_u64().unwrap();
    wait_position_async(&harness.rt, "Watch", ok_pos).await;

    let bad = harness
        .rt
        .execute(
            "EmitBoom",
            json!({ "id": BOB, "label": "far too long for the column" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(bad.status, 200, "the second emit failed: {:?}", bad.body);
    let watch = wait_failed(&app, "Watch").await;
    let frozen = watch["position"].as_u64().unwrap();
    assert!(
        frozen >= ok_pos,
        "the frozen position keeps the pre-failure batch: {watch:?}"
    );

    // The pre-failure row is still served, by point read and by scan.
    let (status, body) = get(&app, &format!("/read/Watch/Thing/{ALICE}")).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["item"]["id"], ALICE);

    let (status, body) = get(&app, "/read/Watch/Thing").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], ALICE);

    // A row the dead projector never got to is a plain 404, not a 500.
    let (status, _) = get(&app, &format!("/read/Watch/Thing/{BOB}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // `?after=` past the frozen position gives up rather than blocking forever.
    let unreachable = frozen + 10;
    let (status, body) = get(
        &app,
        &format!("/read/Watch/Thing/{ALICE}?after={unreachable}&timeout_ms=100"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    assert_eq!(body["error"]["code"], "not_caught_up");

    harness.shutdown();
}

#[tokio::test]
async fn scan_limit_is_clamped_and_validated() {
    let harness = boot_example();
    for i in 0..3 {
        let id = format!("00000000-0000-0000-0000-00000000000{i}");
        register_user(&harness.rt, &id, &format!("user{i}@example.com"), "User");
    }
    wait_position_async(&harness.rt, "Users", 3).await;
    let app = harness.app();

    // `limit=0` clamps up to 1: a literal LIMIT 0 would return an empty page while
    // still handing back a forward cursor, so a paginating client would spin.
    let (status, body) = get(&app, "/read/Users/User?limit=0").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert!(
        body["next_cursor"].is_string(),
        "a clamped page still advances: {body:?}"
    );

    // A limit above MAX_LIMIT is clamped down, not rejected.
    let (status, body) = get(&app, "/read/Users/User?limit=100000").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
    assert_eq!(body["next_cursor"], Value::Null);

    for bad in ["abc", "-1", "1.5", ""] {
        let (status, body) = get(&app, &format!("/read/Users/User?limit={bad}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "limit={bad}: {body:?}");
        assert_eq!(body["error"]["code"], "invalid_input", "limit={bad}");
    }

    harness.shutdown();
}

#[tokio::test]
async fn a_bad_cursor_and_a_filter_spanning_two_indexes_are_rejected() {
    let harness = boot_example();
    register_user(&harness.rt, ALICE, "alice@example.com", "Alice");
    wait_position_async(&harness.rt, "Users", 1).await;
    let app = harness.app();

    // Not base64url at all.
    let (status, body) = get(&app, "/read/Users/User?cursor=***not-base64").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");

    // Valid base64url over bytes that are not UTF-8 (`__g` decodes to 0xff 0xfe).
    let (status, body) = get(&app, "/read/Users/User?cursor=__g").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");

    // Two filter fields that are not a prefix of one index: refused outright, never one
    // arbitrary field applied and the other silently dropped (which would over-return
    // rows and look like success). `Users` declares `index (email)` and keys on
    // `user_id`, so the pair is two separate access paths rather than one.
    let (status, body) = get(
        &app,
        &format!("/read/Users/User?email=alice@example.com&user_id={ALICE}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "unindexed_filter");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("not a prefix of any declared index") && message.contains("index (email)"),
        "the message names the rule and what is declared: {body:?}"
    );

    // A filter on the key column itself is allowed: it is filterable without an index.
    let (status, body) = get(&app, &format!("/read/Users/User?user_id={ALICE}")).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["user_id"], ALICE);

    // And it really filters, rather than returning everything.
    let (status, body) = get(&app, &format!("/read/Users/User?user_id={MISSING}")).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(body["items"].as_array().unwrap().is_empty());

    harness.shutdown();
}

/// Follow `next_cursor` from `uri` (which must already carry its own `limit`),
/// collecting the value of `field` from every row. Returns the ids and how many
/// requests it took, so an extra empty round trip is visible to the caller.
async fn page_through(app: &Router, uri: &str, field: &str) -> (Vec<String>, usize) {
    let mut seen = Vec::new();
    let mut requests = 0;
    let mut next = uri.to_owned();
    loop {
        let (status, body) = get(app, &next).await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        requests += 1;
        for item in body["items"].as_array().unwrap() {
            seen.push(item[field].to_string().trim_matches('"').to_owned());
        }
        match body["next_cursor"].as_str() {
            Some(cursor) => next = format!("{uri}&cursor={cursor}"),
            None => return (seen, requests),
        }
    }
}

fn catalog_project() -> TempDir {
    write_project(&[
        (
            "events/item.hk",
            "event @item.added { id: Uuid, bucket: String @max(20), shelf: String @max(20), rank: Int, price: Money(2) }\n",
        ),
        (
            "commands/add-item.hk",
            r#"
command AddItem(id: Uuid, bucket: String, shelf: String, rank: Int, price: Money(2)) {
  emit @item.added { id, bucket, shelf, rank, price }
}
"#,
        ),
        (
            "projectors/catalog.hk",
            r#"
projector Catalog {
  entity Item {
    id: Uuid @key,
    bucket: String @max(20) @index,
    shelf: String @max(20),
    rank: Int,
    // Money is stored as its decimal string, so it takes equality and refuses a range.
    // In no index at all, which also makes it the unfilterable column here.
    price: Money(2),

    // Three columns, so a filter can be a prefix of one, two or all three of them,
    // and `rank` can carry a range under the two before it.
    index (bucket, shelf, rank)
  }

  on @item.added { id, bucket, shelf, rank, price } {
    put Item { id, bucket, shelf, rank, price }
  }
}
"#,
        ),
    ])
}

/// Add one row to a `catalog_project` harness, returning the log position it reached.
fn add_item(harness: &Harness, id: &str, bucket: &str, shelf: &str, rank: i64) -> u64 {
    let result = harness
        .rt
        .execute(
            "AddItem",
            json!({
                "id": id,
                "bucket": bucket,
                "shelf": shelf,
                "rank": rank,
                "price": "1.00",
            }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "AddItem failed: {:?}", result.body);
    result.body["positions"]["last"].as_u64().unwrap()
}

/// The `id`s a scan of `uri` returns, which must be a single page.
async fn scanned_ids(app: &Router, uri: &str) -> Vec<String> {
    let (status, body) = get(app, uri).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(body["next_cursor"].is_null(), "expected one page: {body:?}");
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The `unindexed_filter`/`invalid_input` message a refused scan of `uri` carries.
async fn refused(app: &Router, uri: &str, code: &str) -> String {
    let (status, body) = get(app, uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body:?}");
    assert_eq!(body["error"]["code"], code, "{uri}: {body:?}");
    body["error"]["message"].as_str().unwrap().to_owned()
}

/// One row per id, spread so that every prefix of `index (bucket, shelf, rank)` picks
/// out a different set and no two sets are accidentally equal.
async fn seeded_catalog(harness: &Harness) {
    let mut last = 0;
    for (id, bucket, shelf, rank) in [
        ("00000000-0000-0000-0000-000000000001", "a", "top", 1),
        ("00000000-0000-0000-0000-000000000002", "a", "top", 5),
        ("00000000-0000-0000-0000-000000000003", "a", "low", 5),
        ("00000000-0000-0000-0000-000000000004", "b", "top", 5),
        ("00000000-0000-0000-0000-000000000005", "a", "top", 9),
    ] {
        last = add_item(harness, id, bucket, shelf, rank);
    }
    wait_position_async(&harness.rt, "Catalog", last).await;
}

#[tokio::test]
async fn a_scan_filters_on_any_prefix_of_a_declared_index() {
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    seeded_catalog(&harness).await;
    let app = harness.app();

    let one = scanned_ids(&app, "/read/Catalog/Item?bucket=a").await;
    assert_eq!(one.len(), 4, "every bucket-a row: {one:?}");
    let two = scanned_ids(&app, "/read/Catalog/Item?bucket=a&shelf=top").await;
    assert_eq!(two.len(), 3, "{two:?}");
    let three = scanned_ids(&app, "/read/Catalog/Item?bucket=a&shelf=top&rank=5").await;
    assert_eq!(three, vec!["00000000-0000-0000-0000-000000000002"]);

    // A query string carries no order, so the same three columns in any order are the
    // same question. This is the assertion that would fail if the prefix match compared
    // sequences rather than sets.
    let shuffled = scanned_ids(&app, "/read/Catalog/Item?rank=5&bucket=a&shelf=top").await;
    assert_eq!(shuffled, three);

    harness.shutdown();
}

#[tokio::test]
async fn a_scan_that_skips_an_index_column_is_refused_rather_than_scanned() {
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    seeded_catalog(&harness).await;
    let app = harness.app();

    // `shelf` is the index's second column. Reaching these rows means visiting every
    // bucket, which is the table scan the read API exists to refuse. The `rank` pair
    // skips a column in the middle, which is the same refusal for the same reason.
    for uri in [
        "/read/Catalog/Item?shelf=top",
        "/read/Catalog/Item?bucket=a&rank=5",
    ] {
        let message = refused(&app, uri, "unindexed_filter").await;
        assert!(
            message.contains("not a prefix of any declared index")
                && message.contains("index (bucket, shelf, rank)"),
            "{uri}: the refusal names the rule and what is declared: {message}"
        );
    }

    // A column in no index at all is the other shape of the same 400, and it is worth
    // keeping separate: this one is answered before any index is consulted.
    let message = refused(&app, "/read/Catalog/Item?price=1.00", "unindexed_filter").await;
    assert!(message.contains("declare an index on it"), "{message}");

    let message = refused(&app, "/read/Catalog/Item?nonesuch=1", "unindexed_filter").await;
    assert!(
        message.contains("is not a column of entity `Item`"),
        "{message}"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_scan_ranges_over_the_column_after_its_equality_prefix() {
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    seeded_catalog(&harness).await;
    let app = harness.app();

    let at_least = scanned_ids(&app, "/read/Catalog/Item?bucket=a&shelf=top&rank.gte=5").await;
    assert_eq!(
        at_least,
        vec![
            "00000000-0000-0000-0000-000000000002",
            "00000000-0000-0000-0000-000000000005",
        ]
    );
    // Both ends at once, and the exclusive upper is what keeps rank 9 out: a bounded
    // range is two clauses over one column rather than two range columns.
    let between = scanned_ids(
        &app,
        "/read/Catalog/Item?bucket=a&shelf=top&rank.gt=1&rank.lt=9",
    )
    .await;
    assert_eq!(between, vec!["00000000-0000-0000-0000-000000000002"]);

    // The leading column of an index takes a range with no equality before it, because
    // an empty prefix is still a prefix.
    let leading = scanned_ids(&app, "/read/Catalog/Item?bucket.gte=b").await;
    assert_eq!(leading, vec!["00000000-0000-0000-0000-000000000004"]);

    harness.shutdown();
}

#[tokio::test]
async fn a_range_is_refused_where_it_could_not_mean_what_it_says() {
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    seeded_catalog(&harness).await;
    let app = harness.app();

    // Money is stored as its decimal string, so `>=` would sort "2" above "10". The
    // column filters for equality; it is the *range* that has no answer.
    let message = refused(&app, "/read/Catalog/Item?price.gte=2", "unindexed_filter").await;
    assert!(message.contains("declare an index on it"), "{message}");

    // `rank` is an Int and ranges fine, but not two columns at once: an index orders
    // one column after its equality prefix, and a second range would mean sorting.
    let message = refused(
        &app,
        "/read/Catalog/Item?bucket.gte=a&rank.lte=5",
        "unindexed_filter",
    )
    .await;
    assert!(message.contains("ranges over one column"), "{message}");

    // A misspelled operator names the four that exist rather than being read as a
    // column called `rank.between`.
    let message = refused(&app, "/read/Catalog/Item?rank.between=5", "invalid_input").await;
    assert!(
        message.contains("unknown filter operator `.between`") && message.contains(".gte"),
        "{message}"
    );

    // A range bound still has to parse as its column's type, the same as an equality.
    let message = refused(
        &app,
        "/read/Catalog/Item?bucket=a&shelf=top&rank.gte=abc",
        "invalid_input",
    )
    .await;
    assert!(message.contains("expected an integer"), "{message}");

    harness.shutdown();
}

#[tokio::test]
async fn a_scan_orders_by_a_declared_index_in_either_direction() {
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    seeded_catalog(&harness).await;
    let app = harness.app();

    // `(bucket, shelf, rank)` is a different order from the key's, so this is not the
    // default dressed up: rows 3 and 4 swap places against key order.
    let ordered = scanned_ids(&app, "/read/Catalog/Item?order_by=by_bucket_shelf_rank").await;
    assert_eq!(
        ordered,
        vec![
            "00000000-0000-0000-0000-000000000003",
            "00000000-0000-0000-0000-000000000001",
            "00000000-0000-0000-0000-000000000002",
            "00000000-0000-0000-0000-000000000005",
            "00000000-0000-0000-0000-000000000004",
        ]
    );

    let reversed = scanned_ids(&app, "/read/Catalog/Item?order_by=-by_bucket_shelf_rank").await;
    let mut backwards = ordered.clone();
    backwards.reverse();
    assert_eq!(
        reversed, backwards,
        "reversing turns the whole tuple around, not its first column"
    );

    // The key is nameable so that it can be reversed, which is the common "newest
    // first" request and has no other spelling.
    let newest = scanned_ids(&app, "/read/Catalog/Item?order_by=-id").await;
    assert_eq!(newest[0], "00000000-0000-0000-0000-000000000005");

    // An ordering composes with a filter, as long as the filter is a prefix of the same
    // index the ordering names.
    let filtered = scanned_ids(
        &app,
        "/read/Catalog/Item?bucket=a&order_by=-by_bucket_shelf_rank",
    )
    .await;
    assert_eq!(filtered.len(), 4, "{filtered:?}");
    assert_eq!(filtered[0], "00000000-0000-0000-0000-000000000005");

    harness.shutdown();
}

#[tokio::test]
async fn an_ordered_scan_pages_without_dropping_or_repeating_a_row() {
    // Every seeded row shares `(bucket, shelf)` with another and two share `rank`, so
    // the index tuple is not unique and page boundaries fall inside ties. The key is the
    // last term of every ordering for exactly that reason.
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    let mut last = 0;
    for (n, bucket, shelf, rank) in [
        (1, "a", "top", 5),
        (2, "a", "top", 5),
        (3, "a", "top", 5),
        (4, "a", "low", 1),
        (5, "b", "top", 1),
        (6, "b", "top", 1),
    ] {
        let id = format!("00000000-0000-0000-0000-00000000000{n}");
        last = add_item(&harness, &id, bucket, shelf, rank);
    }
    wait_position_async(&harness.rt, "Catalog", last).await;
    let app = harness.app();

    for order in ["by_bucket_shelf_rank", "-by_bucket_shelf_rank"] {
        let (seen, _) = page_through(
            &app,
            &format!("/read/Catalog/Item?order_by={order}&limit=2"),
            "id",
        )
        .await;
        assert_eq!(seen.len(), 6, "{order}: {seen:?}");
        let mut deduped = seen.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            6,
            "{order}: every row exactly once: {seen:?}"
        );
    }

    harness.shutdown();
}

#[tokio::test]
async fn an_ordering_that_cannot_be_served_is_refused_rather_than_sorted() {
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    seeded_catalog(&harness).await;
    let app = harness.app();

    let message = refused(
        &app,
        "/read/Catalog/Item?order_by=by_nothing",
        "invalid_input",
    )
    .await;
    assert!(
        message.contains("is not an ordering of entity `Item`")
            && message.contains("by_bucket_shelf_rank"),
        "the refusal lists what can be ordered by: {message}"
    );

    // The filter is a prefix of `by_bucket`, but the ordering names the wider index, and
    // `shelf` is not a prefix of it. One index has to serve both or the rows come out of
    // one and get sorted for the other.
    let message = refused(
        &app,
        "/read/Catalog/Item?shelf=top&order_by=by_bucket_shelf_rank",
        "unindexed_filter",
    )
    .await;
    assert!(
        message.contains("asks for the rows in that index's order"),
        "{message}"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_cursor_taken_over_one_ordering_is_refused_under_another() {
    // When a cursor was a bare key this could not arise: every scan was in key order, so
    // carrying one across queries was nearly harmless. A tuple read under a different
    // ordering compares the wrong columns, which is a page of the wrong rows rather than
    // an error, so it has to be caught here.
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    seeded_catalog(&harness).await;
    let app = harness.app();

    let (status, body) = get(
        &app,
        "/read/Catalog/Item?order_by=by_bucket_shelf_rank&limit=2",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let cursor = body["next_cursor"].as_str().expect("a second page follows");
    assert!(
        !cursor.contains("by_bucket"),
        "a cursor stays opaque: {cursor}"
    );

    // The same cursor under the same ordering resumes.
    let (status, body) = get(
        &app,
        &format!("/read/Catalog/Item?order_by=by_bucket_shelf_rank&limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    for other in ["-by_bucket_shelf_rank", "id"] {
        let message = refused(
            &app,
            &format!("/read/Catalog/Item?order_by={other}&limit=2&cursor={cursor}"),
            "invalid_input",
        )
        .await;
        assert!(
            message.contains("was taken over `order_by=by_bucket_shelf_rank`"),
            "{other}: {message}"
        );
    }

    // Dropping `order_by` entirely is the default key ordering, which is another
    // ordering: a cursor is not portable just because the parameter is absent.
    let message = refused(
        &app,
        &format!("/read/Catalog/Item?limit=2&cursor={cursor}"),
        "invalid_input",
    )
    .await;
    assert!(message.contains("`order_by=id`"), "{message}");

    harness.shutdown();
}

#[tokio::test]
async fn an_index_that_cannot_order_rows_still_filters_them() {
    // `shelf` is optional here, so `by_shelf` cannot back an ordering: a row-value
    // comparison against NULL matches nothing, and pagination would drop every row whose
    // `shelf` is absent at a page boundary. It stays a perfectly good filter.
    let dir = write_project(&[
        (
            "events/stocked.hk",
            "event @stocked { id: Uuid, shelf: String? @max(20) }\n",
        ),
        (
            "commands/stock.hk",
            "command Stock(id: Uuid, shelf: String?) {\n  emit @stocked { id, shelf }\n}\n",
        ),
        (
            "projectors/stock.hk",
            r#"
projector Stock {
  entity Item {
    id: Uuid @key,
    shelf: String? @max(20) @index,
  }

  on @stocked { id, shelf } {
    put Item { id, shelf }
  }
}
"#,
        ),
    ]);
    let harness = boot_project(dir.path());
    let mut last = 0;
    for (n, shelf) in [(1, json!("top")), (2, Value::Null)] {
        let id = format!("00000000-0000-0000-0000-00000000000{n}");
        let result = harness
            .rt
            .execute("Stock", json!({ "id": id, "shelf": shelf }), &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "Stock failed: {:?}", result.body);
        last = result.body["positions"]["last"].as_u64().unwrap();
    }
    wait_position_async(&harness.rt, "Stock", last).await;
    let app = harness.app();

    let filtered = scanned_ids(&app, "/read/Stock/Item?shelf=top").await;
    assert_eq!(filtered, vec!["00000000-0000-0000-0000-000000000001"]);

    // And it ranges. A range over a nullable column is well defined: `>=` does not match
    // NULL, which is the answer someone asking for one wants. Only the *ordering* breaks,
    // because a row-value comparison against NULL loses rows at a page boundary instead
    // of excluding them from an answer.
    let ranged = scanned_ids(&app, "/read/Stock/Item?shelf.gte=a").await;
    assert_eq!(ranged, vec!["00000000-0000-0000-0000-000000000001"]);
    let above = scanned_ids(&app, "/read/Stock/Item?shelf.gt=top").await;
    assert!(above.is_empty(), "{above:?}");

    // The document agrees with the runtime: the bounds are declared, the ordering is not.
    let (status, doc) = get(&app, "/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    let params: Vec<&str> = doc["paths"]["/read/Stock/Item"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|param| param["name"].as_str().unwrap())
        .collect();
    assert!(params.contains(&"shelf.gte"), "{params:?}");
    let orderings = doc["paths"]["/read/Stock/Item"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|param| param["name"] == "order_by")
        .map(|param| param["schema"]["enum"].clone())
        .unwrap();
    assert_eq!(
        orderings,
        json!(["id", "-id"]),
        "an index that cannot sort is left out of the orderings rather than listed and refused"
    );

    let message = refused(&app, "/read/Stock/Item?order_by=by_shelf", "invalid_input").await;
    assert!(
        message.contains("`shelf` is optional") && message.contains("stays filterable"),
        "{message}"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_cursor_whose_ordering_changed_width_is_refused_rather_than_looping() {
    // The ordering *token* and its *width* move independently: an index already
    // containing the key is not widened by it, so moving `@key` turns `index (a, k)` from
    // a two-column ordering into a three-column one while `by_a_k` still spells it. The
    // token check would pass. Left to the model, the tuple is dropped and every request
    // returns page one with the same `next_cursor`, which is a client following cursors
    // in a circle with nothing reporting a problem.
    //
    // Hand-built rather than reached by editing a project mid-flight, because it is the
    // handler's arithmetic under test and a deploy in the middle of a scan is not.
    let dir = catalog_project();
    let harness = boot_project(dir.path());
    seeded_catalog(&harness).await;
    let app = harness.app();

    let wide = URL_SAFE_NO_PAD.encode(br#"{"o":"id","v":["a","b"]}"#);
    let message = refused(
        &app,
        &format!("/read/Catalog/Item?limit=2&cursor={wide}"),
        "invalid_input",
    )
    .await;
    assert!(
        message.contains("carries 2 value(s)") && message.contains("orders on 1"),
        "{message}"
    );

    let narrow = URL_SAFE_NO_PAD.encode(br#"{"o":"by_bucket_shelf_rank","v":["a"]}"#);
    let message = refused(
        &app,
        &format!("/read/Catalog/Item?order_by=by_bucket_shelf_rank&limit=2&cursor={narrow}"),
        "invalid_input",
    )
    .await;
    assert!(
        message.contains("carries 1 value(s)") && message.contains("orders on 4"),
        "{message}"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_money_column_ranges_only_where_an_index_would_have_admitted_it() {
    // The refusal above is the *unindexed* one, because `price` is in no index, so it
    // never reaches the comparability check. A column that is indexed and still cannot
    // carry a range is the case that check exists for, and it needs its own entity.
    let dir = write_project(&[
        (
            "events/priced.hk",
            "event @priced { id: Uuid, amount: Money(2) }\n",
        ),
        (
            "commands/price.hk",
            "command Price(id: Uuid, amount: Money(2)) {\n  emit @priced { id, amount }\n}\n",
        ),
        (
            "projectors/prices.hk",
            r#"
projector Prices {
  entity Priced {
    id: Uuid @key,
    amount: Money(2) @index,
  }

  on @priced { id, amount } {
    put Priced { id, amount }
  }
}
"#,
        ),
    ]);
    let harness = boot_project(dir.path());
    let result = harness
        .rt
        .execute(
            "Price",
            json!({ "id": "00000000-0000-0000-0000-000000000001", "amount": "10.00" }),
            &ctx(),
            None,
        )
        .unwrap();
    let last = result.body["positions"]["last"].as_u64().unwrap();
    wait_position_async(&harness.rt, "Prices", last).await;
    let app = harness.app();

    // Equality works: the column is indexed and the comparison is exact.
    let matched = scanned_ids(&app, "/read/Prices/Priced?amount=10.00").await;
    assert_eq!(matched.len(), 1, "{matched:?}");

    let message = refused(&app, "/read/Prices/Priced?amount.gte=2", "invalid_input").await;
    assert!(
        message.contains("Money(2)") && message.contains("no order a range could use"),
        "the refusal names the declared type and why: {message}"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_filtered_scan_paginates_without_dropping_or_repeating_rows() {
    // A filter and a cursor AND together in the WHERE clause; a bind-order or
    // clause-composition bug only shows up on the second page of a filtered scan.
    // The row counts are exact multiples of the page size, which is where the
    // over-fetch-one logic has to choose between a terminal page and either an extra
    // empty round trip or a cursor that never terminates.
    let dir = catalog_project();
    let harness = boot_project(dir.path());

    // Interleaved so the bucket-b rows fall between the bucket-a rows in key order.
    let rows = [
        ("00000000-0000-0000-0000-000000000001", "a"),
        ("00000000-0000-0000-0000-000000000002", "b"),
        ("00000000-0000-0000-0000-000000000003", "a"),
        ("00000000-0000-0000-0000-000000000004", "b"),
        ("00000000-0000-0000-0000-000000000005", "a"),
        ("00000000-0000-0000-0000-000000000006", "a"),
    ];
    let mut last = 0;
    for (id, bucket) in rows {
        last = add_item(&harness, id, bucket, "top", 1);
    }
    wait_position_async(&harness.rt, "Catalog", last).await;
    let app = harness.app();

    let (seen, requests) = page_through(&app, "/read/Catalog/Item?bucket=a&limit=2", "id").await;
    assert_eq!(
        seen,
        vec![
            "00000000-0000-0000-0000-000000000001",
            "00000000-0000-0000-0000-000000000003",
            "00000000-0000-0000-0000-000000000005",
            "00000000-0000-0000-0000-000000000006",
        ],
        "every bucket-a row exactly once, in key order, with no bucket-b row"
    );
    assert_eq!(
        requests, 2,
        "4 rows at limit 2 terminate on the second page, with no extra empty round trip"
    );

    // The same boundary unfiltered: 6 rows at limit 3.
    let (seen, requests) = page_through(&app, "/read/Catalog/Item?limit=3", "id").await;
    assert_eq!(seen.len(), 6);
    let mut deduped = seen.clone();
    deduped.dedup();
    assert_eq!(deduped.len(), 6, "every row exactly once");
    assert!(seen.is_sorted(), "pages come back ordered by key: {seen:?}");
    assert_eq!(
        requests, 2,
        "6 rows at limit 3 terminate on the second page"
    );

    harness.shutdown();
}

#[tokio::test]
async fn an_integer_keyed_entity_reads_and_paginates_by_key() {
    // Every other read test uses a text or uuid key, so the `Value::Number` cursor
    // branch and the INTEGER key binding are otherwise unexercised. The values are
    // picked so numeric and lexicographic order disagree: under a text binding the
    // pages would come back in the wrong order, or repeat.
    let dir = write_project(&[
        (
            "events/count.hk",
            "event @counted { n: Int, label: String @max(20) }\n",
        ),
        (
            "commands/count.hk",
            r#"
command Count(n: Int, label: String) {
  emit @counted { n, label }
}
"#,
        ),
        (
            "projectors/tally.hk",
            r#"
projector Tally {
  entity Counter {
    n: Int @key,
    label: String @max(20),
  }

  on @counted { n, label } {
    put Counter { n, label }
  }
}
"#,
        ),
    ]);
    let harness = boot_project(dir.path());

    let mut last = 0;
    for n in [1u64, 2, 10, 20] {
        let result = harness
            .rt
            .execute(
                "Count",
                json!({ "n": n, "label": format!("row-{n}") }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "Count failed: {:?}", result.body);
        last = result.body["positions"]["last"].as_u64().unwrap();
    }
    wait_position_async(&harness.rt, "Tally", last).await;
    let app = harness.app();

    // The path segment coerces to INTEGER, so the row is found by its numeric key.
    let (status, body) = get(&app, "/read/Tally/Counter/10").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["item"]["n"], json!(10));
    assert_eq!(body["item"]["label"], "row-10");

    let (seen, requests) = page_through(&app, "/read/Tally/Counter?limit=2", "n").await;
    assert_eq!(
        seen,
        vec!["1", "2", "10", "20"],
        "cursor pages follow numeric key order, each row exactly once"
    );
    assert_eq!(
        requests, 2,
        "4 rows at limit 2 terminate on the second page"
    );

    harness.shutdown();
}

#[tokio::test]
async fn an_entity_field_named_a_sql_keyword_reads_and_writes_end_to_end() {
    // `group` is a SQLite keyword and a perfectly reasonable field name. Every
    // generated identifier is quoted, so the runtime boots and the column round trips
    // through the projector's INSERT and the read API's SELECT. Unquoted, the CREATE
    // TABLE was a syntax error that killed the whole runtime at boot.
    let dir = write_project(&[
        (
            "events/item.hk",
            "event @item.added { id: Uuid, group: String @max(20) }\n",
        ),
        (
            "commands/add-item.hk",
            r#"
command AddItem(id: Uuid, group: String) {
  emit @item.added { id, group }
}
"#,
        ),
        (
            "projectors/catalog.hk",
            r#"
projector Catalog {
  entity Item {
    id: Uuid @key,
    group: String @max(20),
  }

  on @item.added { id, group } {
    put Item { id, group }
  }
}
"#,
        ),
    ]);
    let harness = boot_project(dir.path());

    let result = harness
        .rt
        .execute(
            "AddItem",
            json!({ "id": UUID_A, "group": "widgets" }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "AddItem failed: {:?}", result.body);
    let last = result.body["positions"]["last"].as_u64().unwrap();
    wait_position_async(&harness.rt, "Catalog", last).await;
    let app = harness.app();

    let (status, body) = get(&app, &format!("/read/Catalog/Item/{UUID_A}")).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["item"]["id"], UUID_A);
    assert_eq!(body["item"]["group"], "widgets");

    // The scan selects the same column list, so a missed quote there would 500 here.
    let (status, body) = get(&app, "/read/Catalog/Item").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["items"][0]["group"], "widgets");

    harness.shutdown();
}

/// An entity whose key, indexed column and plain column are all SQLite keywords, so
/// every generated statement (DDL, index DDL, INSERT, SELECT, WHERE, ORDER BY) has
/// to quote its identifiers.
fn keyword_columns_project() -> TempDir {
    write_project(&[
        (
            "events/row.hk",
            r#"
event @row.added {
  order: Uuid,
  select: String @max(20),
  group: String @max(20),
}
"#,
        ),
        (
            "commands/add-row.hk",
            r#"
command AddRow(order: Uuid, select: String, group: String) {
  emit @row.added { order, select, group }
}
"#,
        ),
        (
            "projectors/keys.hk",
            r#"
projector Keys {
  entity Row {
    order: Uuid @key,
    select: String @max(20) @index,
    group: String @max(20),
  }

  on @row.added { order, select, group } {
    put Row { order, select, group }
  }
}
"#,
        ),
    ])
}

/// Add every row through `add-row` and return the last log position.
async fn add_keyword_rows(rt: &Runtime, rows: &[(&str, &str)]) -> u64 {
    let mut last = 0;
    for (order, select) in rows {
        let result = rt
            .execute(
                "AddRow",
                json!({ "order": order, "select": select, "group": "g" }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "AddRow failed: {:?}", result.body);
        last = result.body["positions"]["last"].as_u64().unwrap();
    }
    last
}

#[tokio::test]
async fn a_sql_keyword_as_the_entity_key_reads_by_key_and_paginates() {
    // The key lands in a point read's WHERE, the cursor's `key > ?` and the ORDER BY,
    // none of which quoting the DDL alone would cover.
    let dir = keyword_columns_project();
    let harness = boot_project(dir.path());

    let last = add_keyword_rows(&harness.rt, &[(UUID_A, "a"), (UUID_B, "b"), (UUID_C, "a")]).await;
    wait_position_async(&harness.rt, "Keys", last).await;
    let app = harness.app();

    let (status, body) = get(&app, &format!("/read/Keys/Row/{UUID_B}")).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["item"]["order"], UUID_B);
    assert_eq!(body["item"]["select"], "b");

    let (seen, requests) = page_through(&app, "/read/Keys/Row?limit=2", "order").await;
    assert_eq!(
        seen,
        vec![UUID_A, UUID_B, UUID_C],
        "cursor pages follow key order, each row exactly once"
    );
    assert_eq!(
        requests, 2,
        "3 rows at limit 2 terminate on the second page"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_sql_keyword_column_filters_through_its_index() {
    // An indexed filter puts the column in both the index DDL and the scan's WHERE.
    let dir = keyword_columns_project();
    let harness = boot_project(dir.path());

    let last = add_keyword_rows(&harness.rt, &[(UUID_A, "a"), (UUID_B, "b"), (UUID_C, "a")]).await;
    wait_position_async(&harness.rt, "Keys", last).await;
    let app = harness.app();

    let (status, body) = get(&app, "/read/Keys/Row?select=a").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let matched: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["order"].as_str().unwrap())
        .collect();
    assert_eq!(
        matched,
        vec![UUID_A, UUID_C],
        "only the `a` rows, in key order"
    );

    // The filter is still an equality match, not a substring or a no-op.
    let (status, body) = get(&app, "/read/Keys/Row?select=b").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["order"], UUID_B);

    harness.shutdown();
}
