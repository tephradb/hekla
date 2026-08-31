//! The read-only introspection surface under `/admin`, end to end.
//!
//! These pin the properties the surface exists for: that the log can be paged and
//! filtered, that a correlation id reaches every event of the chain it set off, that a
//! wedged effect's journaled calls are readable, and that a subject-scoped field
//! renders as plaintext, ciphertext or an explicit `erased` marker depending on what
//! is actually knowable rather than silently vanishing.
//!
//! One of the Starlark suite's cases is gone rather than ported. It pinned an
//! `all_events()` subscription rendering as null rather than as `[]`, which heklang has
//! no way to declare: a projector and an effect both select by named arm, so `sources`
//! is what those arms name and an empty one is a module subscribed to nothing. The
//! nullable wire shape went with the case.

use std::sync::Arc;

use hekla::effect::StubHttpClient;
use hekla::http::HttpClient;
use hekla::runtime::Runtime;
use serde_json::{Value, json};

mod support;

use support::{ALICE, BOB, Boot, CAROL, Harness, example_dir, get, post_command, wait_until};

fn boot_with(http: Arc<dyn HttpClient>) -> Harness {
    Boot::example().http(http).start()
}

fn register(id: &str) -> Value {
    support::register_body(id, &format!("{id}@example.com"), "U")
}

/// The example project's effect completes once its HTTP call succeeds and the
/// internal command it invokes commits, which is what moves the log past the
/// command's own append.
fn wait_for_head(rt: &Runtime, target: u64) {
    wait_until(&format!("log head reaches {target}"), || {
        support::log_head(rt) >= target
    });
}

// --- the log ---------------------------------------------------------------

#[tokio::test]
async fn the_log_pages_newest_first_with_no_gap_or_duplicate_at_the_seam() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    for id in [ALICE, BOB, CAROL] {
        post_command(&app, "RegisterUser", register(id), None).await;
    }
    wait_for_head(&harness.rt, 6);

    let (status, first) = get(&app, "/admin/events?limit=2").await;
    assert_eq!(status, 200);
    let head = first["log_head"].as_u64().unwrap();
    let page_one: Vec<u64> = positions(&first);
    assert_eq!(
        page_one,
        vec![head, head - 1],
        "the default direction is newest first"
    );

    let cursor = first["next_cursor"].as_u64().unwrap();
    let (_, second) = get(&app, &format!("/admin/events?limit=2&cursor={cursor}")).await;
    let page_two = positions(&second);
    assert_eq!(
        page_two,
        vec![head - 2, head - 3],
        "the cursor is exclusive, so the seam neither repeats nor skips"
    );

    harness.shutdown();
}

#[tokio::test]
async fn walking_forward_reaches_the_same_events_in_the_other_order() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;
    wait_for_head(&harness.rt, 2);

    let (_, forward) = get(&app, "/admin/events?direction=forward&limit=50").await;
    let mut ascending = positions(&forward);
    let (_, back) = get(&app, "/admin/events?limit=50").await;
    let mut descending = positions(&back);
    descending.reverse();
    assert_eq!(ascending, descending);
    assert_eq!(
        ascending.first(),
        Some(&1),
        "positions are 1-based and dense"
    );
    ascending.dedup();
    assert_eq!(
        ascending.len(),
        descending.len(),
        "no duplicates either way"
    );

    harness.shutdown();
}

#[tokio::test]
async fn types_filter_as_an_or_and_tags_filter_as_an_and() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;
    post_command(&app, "RegisterUser", register(BOB), None).await;
    wait_for_head(&harness.rt, 4);

    let (_, one) = get(&app, "/admin/events?type=user.registered").await;
    assert_eq!(types(&one), vec!["user.registered", "user.registered"]);

    let (_, both) = get(
        &app,
        "/admin/events?type=user.registered&type=user.welcomed",
    )
    .await;
    assert_eq!(both["events"].as_array().unwrap().len(), 4, "types OR");

    // Two tags on one event match; the same two spread across different events do not,
    // which is what makes this an AND rather than a second OR.
    let (_, tagged) = get(
        &app,
        &format!("/admin/events?tag=user_id:{ALICE}&tag=name:U"),
    )
    .await;
    assert_eq!(tagged["events"].as_array().unwrap().len(), 1);
    let (_, impossible) = get(
        &app,
        &format!("/admin/events?tag=user_id:{ALICE}&tag=user_id:{BOB}"),
    )
    .await;
    assert_eq!(impossible["events"].as_array().unwrap().len(), 0);

    harness.shutdown();
}

#[tokio::test]
async fn a_malformed_control_parameter_is_a_400_and_an_oversized_limit_is_clamped() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();

    for uri in [
        "/admin/events?direction=sideways",
        "/admin/events?limit=lots",
        "/admin/events?cursor=soon",
        "/admin/events?decrypt=maybe",
        "/admin/traces/not-a-uuid",
    ] {
        let (status, body) = get(&app, uri).await;
        assert_eq!(status, 400, "{uri} should be rejected");
        assert_eq!(body["error"]["code"], "invalid_input");
    }

    // Clamped rather than rejected, matching every other paged endpoint.
    let (status, _) = get(&app, "/admin/events?limit=100000").await;
    assert_eq!(status, 200);

    harness.shutdown();
}

#[tokio::test]
async fn an_event_renders_its_envelope_its_payload_and_the_hosts_own_tags() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    let (_, accepted) = post_command(&app, "RegisterUser", register(ALICE), Some("key-1")).await;

    let (status, event) = get(&app, "/admin/events/1").await;
    assert_eq!(status, 200);
    assert_eq!(event["type"], "user.registered");
    assert_eq!(event["declared"], true);
    assert_eq!(event["position"], 1);
    assert_eq!(event["correlation_id"], accepted["correlation_id"]);
    assert_eq!(event["causation_id"], accepted["causation_id"]);
    assert_eq!(event["data"]["user_id"], ALICE);
    assert_eq!(event["data"]["name"], "U");
    assert!(event["timestamp"].is_string());
    assert_eq!(
        event["subjects"].as_object().unwrap().len(),
        0,
        "the example declares no subject-scoped field"
    );

    let hekla_tags = strings(&event["hekla_tags"]);
    let correlation = accepted["correlation_id"].as_str().unwrap();
    assert!(
        hekla_tags.contains(&format!("_hekla_corr:{correlation}")),
        "every event carries its flow's correlation tag: {hekla_tags:?}"
    );
    assert!(
        hekla_tags.iter().any(|tag| tag.starts_with("_hekla_idem:")),
        "a keyed command's events carry its idempotency tag: {hekla_tags:?}"
    );
    assert!(
        strings(&event["tags"])
            .iter()
            .all(|tag| !tag.starts_with("_hekla_")),
        "the author's tags and the host's are reported separately"
    );

    let (status, _) = get(&app, "/admin/events/9999").await;
    assert_eq!(status, 404);

    harness.shutdown();
}

// --- traces ----------------------------------------------------------------

#[tokio::test]
async fn a_trace_spans_the_command_the_effect_and_the_command_the_effect_invoked() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    let (_, accepted) = post_command(&app, "RegisterUser", register(ALICE), None).await;
    let correlation = accepted["correlation_id"].as_str().unwrap().to_owned();
    // `SendWelcome` reacts to the registration and invokes `RecordWelcome`, which
    // appends a second event. Two events means the chain genuinely crossed the effect.
    wait_for_head(&harness.rt, 2);

    let (status, trace) = get(&app, &format!("/admin/traces/{correlation}")).await;
    assert_eq!(status, 200);
    assert_eq!(trace["complete"], true);
    assert_eq!(
        types(&trace),
        vec!["user.registered", "user.welcomed"],
        "the correlation propagates across command -> event -> effect -> command"
    );

    // The two events came from different command executions, which is exactly what
    // separates causation from correlation.
    let events = trace["events"].as_array().unwrap();
    assert_ne!(
        events[0]["causation_id"], events[1]["causation_id"],
        "each hop mints a fresh causation id"
    );
    assert_eq!(events[1]["triggering_event_id"], events[0]["event_id"]);

    // An unrelated flow does not bleed in.
    let (_, other) = post_command(&app, "RegisterUser", register(BOB), None).await;
    let (_, again) = get(&app, &format!("/admin/traces/{correlation}")).await;
    assert_eq!(again["events"].as_array().unwrap().len(), 2);
    assert_ne!(other["correlation_id"], accepted["correlation_id"]);

    harness.shutdown();
}

#[tokio::test]
async fn a_trace_cut_off_by_its_limit_says_so_rather_than_looking_whole() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    let (_, accepted) = post_command(&app, "RegisterUser", register(ALICE), None).await;
    let correlation = accepted["correlation_id"].as_str().unwrap().to_owned();
    wait_for_head(&harness.rt, 2);

    let (_, trace) = get(&app, &format!("/admin/traces/{correlation}?limit=1")).await;
    assert_eq!(trace["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        trace["complete"], false,
        "a partial causal chain has to announce itself"
    );

    harness.shutdown();
}

#[tokio::test]
async fn the_correlation_tag_never_reaches_a_command_response() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    let body = register(ALICE);

    let (_, fresh) = post_command(&app, "RegisterUser", body.clone(), Some("k")).await;
    let (_, replayed) = post_command(&app, "RegisterUser", body, Some("k")).await;
    assert_eq!(fresh, replayed, "the replay reproduces the first outcome");

    for response in [&fresh, &replayed] {
        let tags = strings(&response["events"][0]["tags"]);
        assert!(
            tags.iter().all(|tag| !tag.starts_with("_hekla_")),
            "a host tag is not part of the author's vocabulary: {tags:?}"
        );
    }

    harness.shutdown();
}

// --- effects ---------------------------------------------------------------

#[tokio::test]
async fn a_completed_invocation_lists_every_call_it_journaled() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;
    wait_for_head(&harness.rt, 2);
    wait_until("the invocation completes", || {
        harness.rt.effect("SendWelcome").unwrap().position() >= 1
    });

    let (status, page) = get(&app, "/admin/effects/SendWelcome/invocations").await;
    assert_eq!(status, 200);
    let invocations = page["invocations"].as_array().unwrap();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0]["position"], 1);
    assert_eq!(invocations[0]["status"], "terminal");
    assert!(invocations[0]["completed_at"].is_string());

    let (status, detail) = get(&app, "/admin/effects/SendWelcome/invocations/1").await;
    assert_eq!(status, 200);
    let calls = detail["calls"].as_array().unwrap();
    let kinds: Vec<&str> = calls
        .iter()
        .map(|call| call["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["http", "invoke"],
        "in the order the handler made them"
    );
    assert_eq!(calls[0]["seq"], 0);
    assert_eq!(
        calls[0]["result"]["status"], 200,
        "the recorded result is returned parsed, not as an escaped blob"
    );
    // An `invoke` records an outcome rather than a status and a body: rule 6 cuts
    // the retryable cases out of the type, so the three that reach a journal are `ok`
    // and, when it refused, the code and message it refused with.
    assert_eq!(calls[1]["result"]["ok"], true);
    assert!(
        calls[0]["call_hash"].as_str().unwrap().len() == 64,
        "the call is identified by its content hash; the arguments are not stored"
    );
    assert!(
        detail["next_cursor"].is_null(),
        "a list that fits is the whole sequence"
    );

    let (status, _) = get(&app, "/admin/effects/SendWelcome/invocations/999").await;
    assert_eq!(status, 404);
    let (status, _) = get(&app, "/admin/effects/nope/invocations").await;
    assert_eq!(status, 404);

    harness.shutdown();
}

#[tokio::test]
async fn a_wedged_effect_shows_a_running_invocation_and_a_failure_count() {
    // A 500 is absorbed by the runtime and retried forever, so the invocation stays
    // `running` and the failure count climbs. Nothing is journaled, because the bail
    // happens before the journal write, which is what lets a retry re-send.
    let harness = boot_with(Arc::new(StubHttpClient::status(500)));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;
    wait_until("the effect wedges", || {
        harness
            .rt
            .effect("SendWelcome")
            .unwrap()
            .consecutive_failures()
            >= 2
    });

    let (status, listed) = get(&app, "/admin/effects").await;
    assert_eq!(status, 200);
    let effect = &listed["effects"][0];
    assert_eq!(effect["name"], "SendWelcome");
    assert_eq!(effect["sources"][0], "user.registered");
    assert!(effect["consecutive_failures"].as_u64().unwrap() >= 2);
    assert!(effect["last_error"].is_string());
    assert_eq!(effect["quarantined"], false);
    assert!(effect["quarantine"].is_null());
    assert!(
        effect["watermark"].is_null(),
        "null is `never ran`, which the driver flattens to zero but an operator should not"
    );

    let (_, one) = get(&app, "/admin/effects/SendWelcome").await;
    assert_eq!(one["name"], "SendWelcome");

    let (_, detail) = get(&app, "/admin/effects/SendWelcome/invocations/1").await;
    assert_eq!(detail["status"], "running");
    assert_eq!(
        detail["calls"].as_array().unwrap().len(),
        0,
        "a retryable status bails before the journal write, so nothing is recorded"
    );

    let (status, _) = get(&app, "/admin/effects/nope").await;
    assert_eq!(status, 404);

    harness.shutdown();
}

// --- projectors, schema, system --------------------------------------------

#[tokio::test]
async fn a_projector_reports_its_entities_and_counts_them_only_when_asked() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;
    support::wait_position_async(&harness.rt, "Users", 1).await;

    let (status, listed) = get(&app, "/admin/projectors").await;
    assert_eq!(status, 200);
    let names: Vec<&str> = listed["projectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["UserStats", "Users"]);
    assert!(
        listed["projectors"][0]["definition_hash"].is_null(),
        "the list endpoint opens no database"
    );
    assert!(listed["projectors"][0]["entities"][0]["rows"].is_null());

    let (status, one) = get(&app, "/admin/projectors/Users").await;
    assert_eq!(status, 200);
    assert_eq!(one["readiness"], "ready");
    assert_eq!(one["sources"][0], "user.registered");
    assert!(
        one["definition_hash"].is_string(),
        "read out of the model itself, so it is what the rows were built from"
    );
    let entity = &one["entities"][0];
    assert_eq!(entity["name"], "User");
    assert_eq!(entity["key"], "user_id");
    assert_eq!(entity["key_kind"], "Uuid");
    assert_eq!(entity["indexes"][0]["name"], "by_email");
    assert_eq!(strings(&entity["filterable"]), vec!["email", "user_id"]);
    assert!(entity["rows"].is_null(), "counts are opt-in");

    let (_, counted) = get(&app, "/admin/projectors/Users?counts=true").await;
    assert_eq!(counted["entities"][0]["rows"], 1);

    let (status, _) = get(&app, "/admin/projectors/nope").await;
    assert_eq!(status, 404);
    let (status, _) = get(&app, "/admin/projectors/Users?counts=perhaps").await;
    assert_eq!(status, 400);

    harness.shutdown();
}

#[tokio::test]
async fn the_schema_endpoint_reports_internal_commands_the_document_omits() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();

    let (status, schema) = get(&app, "/admin/schema").await;
    assert_eq!(status, 200);

    let commands = schema["commands"].as_array().unwrap();
    let internal: Vec<&str> = commands
        .iter()
        .filter(|c| c["internal"] == json!(true))
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        internal,
        vec!["RecordWelcome"],
        "an internal command is unrouted, not invisible: an effect can still invoke it"
    );

    let register = commands
        .iter()
        .find(|c| c["name"] == json!("RegisterUser"))
        .unwrap();
    assert!(register["source_hash"].as_str().unwrap().len() == 64);
    let input: Vec<&str> = register["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(input, vec!["user_id", "email", "name"]);
    assert_eq!(register["input"][0]["kind"], "Uuid");

    let events: Vec<&str> = schema["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        events,
        vec![
            "reminder.scheduled",
            "user.registered",
            "user.renamed",
            "user.welcomed"
        ]
    );

    // Every module is recorded at boot, which is the only place a projector's or
    // effect's source hash survives the units moving into their threads.
    let modules: Vec<(&str, &str)> = schema["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["kind"].as_str().unwrap(), m["name"].as_str().unwrap()))
        .collect();
    assert!(modules.contains(&("effect", "SendWelcome")));
    assert!(modules.contains(&("projector", "Users")));
    assert!(modules.iter().all(|(_, name)| !name.is_empty()));

    harness.shutdown();
}

#[tokio::test]
async fn the_system_endpoint_reports_the_projects_configuration_not_the_defaults() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();

    let (status, system) = get(&app, "/admin/system").await;
    assert_eq!(status, 200);
    assert_eq!(system["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(system["opdb_schema_version"], hekla::opdb::SCHEMA_VERSION);
    assert_eq!(system["verify"], false);
    assert_eq!(system["keystore"]["configured"], false);
    assert!(system["data_dir"].is_string());

    // `examples/users/hekla.toml` sets both away from their defaults (16 and 7), so
    // these values prove the effective config is retained rather than reconstructed.
    assert_eq!(system["config"]["effects"]["pool_size"], 8);
    assert_eq!(system["config"]["retention"]["effect_journal_days"], 14);
    assert_eq!(system["config"]["projectors"]["auto_rebuild"], true);

    harness.shutdown();
}

#[tokio::test]
async fn the_index_lists_the_endpoints_it_serves() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();

    let (status, index) = get(&app, "/admin").await;
    assert_eq!(status, 200);
    let paths: Vec<&str> = index["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    for expected in [
        "/admin/events",
        "/admin/traces/{correlation_id}",
        "/admin/effects/{name}/invocations/{position}",
        "/admin/schema",
        "/admin/system",
    ] {
        assert!(paths.contains(&expected), "missing {expected}");
    }

    harness.shutdown();
}

// --- subject-scoped fields -------------------------------------------------

fn order_body() -> Value {
    json!({
        "order_id": support::UUID_A,
        "customer_id": 42,
        "shop_id": 7,
        "email": "ari@example.com",
        "shipping_address": "1 Test St",
        "order_total": "42.00",
        "notes": "n",
    })
}

#[tokio::test]
async fn a_subject_field_decrypts_by_default_and_stays_ciphertext_when_asked() {
    let harness = Boot::new(example_dir("orders"))
        .with_master_key()
        .http(Arc::new(StubHttpClient::ok()))
        .start();
    let app = harness.app();
    let (status, _) = post_command(&app, "PlaceOrder", order_body(), None).await;
    assert_eq!(status, 200);

    let (_, event) = get(&app, "/admin/events/1").await;
    assert_eq!(event["data"]["email"], "ari@example.com");
    assert_eq!(
        event["data"]["order_total"], "42.00",
        "a decrypted value is re-typed to its declared kind"
    );
    assert_eq!(event["subjects"]["email"]["state"], "decrypted");
    assert_eq!(event["subjects"]["email"]["subject"], "customer_id");
    assert_eq!(event["subjects"]["email"]["subject_value"], "42");
    assert_eq!(event["subjects"]["order_total"]["subject"], "shop_id");
    assert_eq!(
        event["data"]["customer_id"], 42,
        "a subject id is not itself encrypted; subjects do not chain"
    );

    let (_, opted_out) = get(&app, "/admin/events/1?decrypt=false").await;
    assert_eq!(opted_out["subjects"]["email"]["state"], "encrypted");
    assert_ne!(opted_out["data"]["email"], "ari@example.com");
    assert!(
        !opted_out["data"]["email"].as_str().unwrap().is_empty(),
        "the stored ciphertext is shown rather than the field being dropped"
    );

    harness.shutdown();
}

#[tokio::test]
async fn an_erased_subject_is_marked_erased_rather_than_silently_vanishing() {
    let harness = Boot::new(example_dir("orders"))
        .with_master_key()
        .http(Arc::new(StubHttpClient::ok()))
        .start();
    let app = harness.app();
    post_command(&app, "PlaceOrder", order_body(), None).await;

    let (_, before) = get(&app, "/admin/subjects/customer_id/42").await;
    assert_eq!(before["state"], "live");

    harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();

    let (_, event) = get(&app, "/admin/events/1").await;
    assert_eq!(
        event["subjects"]["email"]["state"], "erased",
        "a read model drops an unreadable column; an operator must not have to infer it"
    );
    assert_ne!(event["data"]["email"], "ari@example.com");
    assert!(
        event["data"].as_object().unwrap().contains_key("email"),
        "the field stays present, holding the ciphertext it will always hold now"
    );
    assert_eq!(
        event["subjects"]["order_total"]["state"], "decrypted",
        "erasing one subject leaves another subject's fields readable"
    );

    let (_, after) = get(&app, "/admin/subjects/customer_id/42").await;
    assert_eq!(
        after["state"], "absent",
        "erasure deletes the row, so `erased` and `never existed` are one state on disk"
    );

    harness.shutdown();
}

#[tokio::test]
async fn the_subject_inventory_counts_live_keys_without_exposing_key_material() {
    let harness = Boot::new(example_dir("orders"))
        .with_master_key()
        .http(Arc::new(StubHttpClient::ok()))
        .start();
    let app = harness.app();
    post_command(&app, "PlaceOrder", order_body(), None).await;

    let (status, subjects) = get(&app, "/admin/subjects").await;
    assert_eq!(status, 200);
    let counts: Vec<(&str, u64)> = subjects["counts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["subject_field"].as_str().unwrap(),
                c["live_keys"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        counts,
        vec![("customer_id", 1), ("shop_id", 1)],
        "the reserved global uniqueness secret is not a subject and is excluded"
    );

    for entry in subjects["subjects"].as_array().unwrap() {
        let keys: Vec<&String> = entry.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "created_at",
                "master_key_id",
                "subject_field",
                "subject_value"
            ],
            "no key material, wrapped or otherwise"
        );
    }

    // Only one half of the cursor is a client error, not a silently ignored parameter.
    let (status, _) = get(&app, "/admin/subjects?after_field=customer_id").await;
    assert_eq!(status, 400);

    harness.shutdown();
}

// --- paging at the boundary ------------------------------------------------

/// Seed `count` events sharing one correlation into a data directory, then boot on it.
///
/// Appended through the same envelope seam a command uses, so the events are
/// byte-identical to real ones, including the correlation tag a trace is found by.
///
/// Booted against a stub that always fails, so `SendWelcome` wedges on the first
/// seeded registration and never invokes `RecordWelcome`. Otherwise the effect would
/// append `user.welcomed` events carrying the same propagated correlation, and the log
/// these tests count would grow underneath them.
fn boot_with_seeded_log(count: usize) -> (Harness, String) {
    let data = tempfile::tempdir().unwrap();
    let project = support::load_ok(&example_dir("users"));
    let ctx = support::ctx();
    {
        let (coordinator, store) = support::open_store(data.path());
        for index in 0..count {
            support::seed_event(
                &store,
                &project,
                &ctx,
                "user.registered",
                json!({
                    "user_id": ALICE,
                    "email": format!("u{index}@example.com"),
                    "name": "U",
                }),
            );
        }
        coordinator.shutdown();
    }
    let harness = Boot::example()
        .data_dir(data.path())
        .http(Arc::new(StubHttpClient::status(500)))
        .start();
    // The tempdir has to outlive the harness, which holds the data directory open.
    std::mem::forget(data);
    (harness, ctx.correlation_id.to_string())
}

#[tokio::test]
async fn a_full_page_at_the_maximum_limit_still_offers_a_cursor() {
    // Exactly one more event than the largest page. The over-fetch probe asks for
    // MAX_LIMIT + 1, so anything that clamps the probe back to MAX_LIMIT makes a full
    // page look like the last one and a client stops paging on a log it has not read.
    let (harness, _) = boot_with_seeded_log(hekla::introspect::MAX_LIMIT + 1);
    let app = harness.app();

    let uri = format!("/admin/events?limit={}", hekla::introspect::MAX_LIMIT);
    let (status, page) = get(&app, &uri).await;
    assert_eq!(status, 200);
    assert_eq!(
        page["events"].as_array().unwrap().len(),
        hekla::introspect::MAX_LIMIT
    );
    let cursor = page["next_cursor"]
        .as_u64()
        .expect("a full page is not evidence that the log ended");

    let (_, rest) = get(&app, &format!("/admin/events?cursor={cursor}")).await;
    assert_eq!(
        rest["events"].as_array().unwrap().len(),
        1,
        "the event the first page could not fit"
    );
    assert!(
        rest["next_cursor"].is_null(),
        "and now it really is the end"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_trace_truncated_at_the_maximum_limit_still_reports_itself_incomplete() {
    // `complete` exists so a partial causal chain announces itself, so it is the one
    // field that must not be wrong at the page boundary.
    let (harness, correlation) = boot_with_seeded_log(hekla::introspect::MAX_LIMIT + 1);
    let app = harness.app();

    let uri = format!(
        "/admin/traces/{correlation}?limit={}",
        hekla::introspect::MAX_LIMIT
    );
    let (status, trace) = get(&app, &uri).await;
    assert_eq!(status, 200);
    assert_eq!(
        trace["events"].as_array().unwrap().len(),
        hekla::introspect::MAX_LIMIT
    );
    assert_eq!(
        trace["complete"], false,
        "a chain cut off at exactly the page size is still cut off"
    );

    harness.shutdown();
}

#[tokio::test]
async fn the_journaled_call_list_pages_rather_than_truncating_silently() {
    // `SendWelcome` journals two calls. Asking for one must not look like an
    // invocation that only ever made one, because this endpoint's whole use is
    // "the first call missing is the one it is stuck on".
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;
    wait_for_head(&harness.rt, 2);
    wait_until("the invocation completes", || {
        harness.rt.effect("SendWelcome").unwrap().position() >= 1
    });

    let (_, first) = get(&app, "/admin/effects/SendWelcome/invocations/1?limit=1").await;
    let calls = first["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["seq"], 0);
    assert_eq!(calls[0]["kind"], "http");
    let cursor = first["next_cursor"]
        .as_u64()
        .expect("a truncated call list has to say so");

    let (_, second) = get(
        &app,
        &format!("/admin/effects/SendWelcome/invocations/1?limit=1&cursor={cursor}"),
    )
    .await;
    let calls = second["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["seq"], 1,
        "seq counts the whole sequence, not the page"
    );
    assert_eq!(calls[0]["kind"], "invoke");
    assert!(second["next_cursor"].is_null());

    harness.shutdown();
}

// --- contract shapes -------------------------------------------------------

#[tokio::test]
async fn a_command_input_field_reports_only_what_an_input_schema_carries() {
    // `schema()` rejects `subject` and `unique` outright and carries no `indexed`, so
    // describing command input as a full field declaration would promise four
    // properties that do not exist and fail every validator.
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();

    let (_, schema) = get(&app, "/admin/schema").await;
    let register = schema["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!("RegisterUser"))
        .unwrap();
    for field in register["input"].as_array().unwrap() {
        let keys: Vec<&String> = field.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["kind", "name"]);
    }

    harness.shutdown();
}

#[tokio::test]
async fn a_bad_typed_path_segment_is_the_plain_text_400_the_document_declares() {
    // Rejected by the routing layer before the handler runs, so it is not the JSON
    // error envelope. A client that parses `error.code` on every 4xx has to be told.
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();

    for uri in [
        "/admin/events/abc",
        "/admin/effects/SendWelcome/invocations/abc",
    ] {
        let (status, content_type, body) = raw_get(&app, uri).await;
        assert_eq!(status, 400, "{uri}");
        assert!(
            content_type.starts_with("text/plain"),
            "{uri} returned {content_type}"
        );
        assert!(
            serde_json::from_slice::<Value>(&body).is_err(),
            "{uri} is not the JSON envelope, which is exactly what the document says"
        );
    }

    harness.shutdown();
}

#[tokio::test]
async fn a_key_that_cannot_be_unwrapped_marks_one_field_instead_of_failing_the_page() {
    // The read API fails the whole request when a key cannot be obtained, so a
    // misconfigured master is loud. Here that would take the log offline: one bad row
    // would 500 every page overlapping it, and the cursor is a position a caller
    // cannot discover without a page.
    let data = tempfile::tempdir().unwrap();
    {
        let harness = Boot::new(example_dir("orders"))
            .with_master_key()
            .data_dir(data.path())
            .http(Arc::new(StubHttpClient::ok()))
            .start();
        let app = harness.app();
        post_command(&app, "PlaceOrder", order_body(), None).await;
        harness.shutdown();
    }

    // Corrupt one subject's wrapped key. The ciphertext is untouched, so this is the
    // "key cannot be obtained" case rather than the "will not decrypt" one.
    {
        let conn = rusqlite::Connection::open(data.path().join("hekla.db")).unwrap();
        conn.execute(
            "UPDATE subject_key SET wrapped_key = ?1 WHERE subject_field = 'customer_id'",
            rusqlite::params![vec![0u8; 8]],
        )
        .unwrap();
    }

    let harness = Boot::new(example_dir("orders"))
        .with_master_key()
        .data_dir(data.path())
        .http(Arc::new(StubHttpClient::ok()))
        .start();
    let app = harness.app();

    let (status, event) = get(&app, "/admin/events/1").await;
    assert_eq!(status, 200, "one unreadable key must not brick the log");
    assert_eq!(event["subjects"]["email"]["state"], "unreadable");
    assert_eq!(
        event["subjects"]["order_total"]["state"], "decrypted",
        "a different subject's key is unaffected"
    );

    let (status, page) = get(&app, "/admin/events").await;
    assert_eq!(status, 200);
    assert_eq!(page["events"].as_array().unwrap().len(), 1);

    harness.shutdown();
}

#[tokio::test]
async fn a_subject_erased_and_then_recreated_reports_stale_rather_than_erased() {
    // `Ok(None)` from the decryptor covers two situations. Reporting both as `erased`
    // tells an operator the data was irreversibly shredded, when here the key is live
    // and it is this one value that was written under the superseded one.
    let harness = Boot::new(example_dir("orders"))
        .with_master_key()
        .http(Arc::new(StubHttpClient::ok()))
        .start();
    let app = harness.app();
    post_command(&app, "PlaceOrder", order_body(), None).await;

    harness
        .rt
        .keystore()
        .unwrap()
        .erase("customer_id", "42")
        .unwrap();

    // A second order for the same customer mints a fresh key for that subject. A
    // different email, because the command's boundary is one order per address.
    let mut second = order_body();
    second["order_id"] = json!(support::UUID_B);
    second["email"] = json!("second@example.com");
    let (status, _) = post_command(&app, "PlaceOrder", second, None).await;
    assert_eq!(status, 200);

    let (_, live) = get(&app, "/admin/subjects/customer_id/42").await;
    assert_eq!(live["state"], "live", "the second order recreated the key");

    let (_, first_event) = get(&app, "/admin/events/1").await;
    assert_eq!(
        first_event["subjects"]["email"]["state"], "stale",
        "the key is present; this value simply predates it"
    );
    let (_, second_event) = get(&app, "/admin/events/2").await;
    assert_eq!(
        second_event["subjects"]["email"]["state"], "decrypted",
        "and the value written under the current key still reads"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_trace_longer_than_one_page_can_be_read_to_the_end() {
    // Saying a causal chain was cut off without offering a way to finish it leaves the
    // rest of the chain unreachable, which is worse than not saying it.
    let (harness, correlation) = boot_with_seeded_log(hekla::introspect::MAX_LIMIT + 1);
    let app = harness.app();

    let uri = format!(
        "/admin/traces/{correlation}?limit={}",
        hekla::introspect::MAX_LIMIT
    );
    let (_, first) = get(&app, &uri).await;
    assert_eq!(first["complete"], false);
    let cursor = first["next_cursor"]
        .as_u64()
        .expect("an incomplete chain has to be continuable");

    let (_, rest) = get(
        &app,
        &format!("/admin/traces/{correlation}?cursor={cursor}"),
    )
    .await;
    assert_eq!(rest["events"].as_array().unwrap().len(), 1);
    assert_eq!(rest["complete"], true);
    assert!(rest["next_cursor"].is_null());

    harness.shutdown();
}

#[tokio::test]
async fn the_reserved_global_secret_is_not_a_subject_to_either_reader() {
    // The listing hides it because it is not a subject and cannot be erased. A point
    // lookup that reported it would make the two readers disagree about what a subject
    // is, over the same table.
    let harness = Boot::new(example_dir("orders"))
        .with_master_key()
        .http(Arc::new(StubHttpClient::ok()))
        .start();
    let app = harness.app();
    // `email` is `unique`, so appending mints the global uniqueness secret.
    post_command(&app, "PlaceOrder", order_body(), None).await;

    let (_, listed) = get(&app, "/admin/subjects").await;
    let fields: Vec<&str> = listed["counts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["subject_field"].as_str().unwrap())
        .collect();
    assert!(!fields.iter().any(|field| field.starts_with("_hekla_")));

    let (status, global) = get(&app, "/admin/subjects/_hekla_global/global").await;
    assert_eq!(status, 200);
    assert_eq!(
        global["state"], "absent",
        "the row exists, but it is not a subject and the surface says so consistently"
    );

    harness.shutdown();
}

/// GET `uri` without assuming the body is JSON, for the responses that are not.
async fn raw_get(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, String, Vec<u8>) {
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri(uri)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, content_type, bytes.to_vec())
}

// --- helpers ---------------------------------------------------------------

fn positions(page: &Value) -> Vec<u64> {
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["position"].as_u64().unwrap())
        .collect()
}

fn types(page: &Value) -> Vec<&str> {
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

// --- effect state, retry deadline, and the trace-to-invocation join --------

/// The one-word state and the retry deadline are the two things a dashboard needs
/// that the raw counters do not give: whether this effect is stuck, and how long
/// until it tries again. Both must come from the runtime rather than be re-derived,
/// so this also pins that `/status` and `/admin/effects` cannot disagree.
#[tokio::test]
async fn a_wedged_effect_reports_the_state_and_when_the_next_attempt_is_due() {
    let harness = boot_with(Arc::new(StubHttpClient::status(500)));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;

    // Wait for a backoff long enough that it is still pending when the request below
    // lands. The wedge backoff doubles from 200ms, so this is true from the third
    // attempt on and only grows after that.
    wait_until(
        "the effect is waiting out a backoff of at least a second",
        || {
            harness
                .rt
                .effect("SendWelcome")
                .unwrap()
                .retry_in_ms()
                .is_some_and(|remaining| remaining > 1_000)
        },
    );

    let (status, listed) = get(&app, "/admin/effects").await;
    assert_eq!(status, 200);
    let effect = &listed["effects"][0];
    assert_eq!(effect["name"], "SendWelcome");
    assert_eq!(
        effect["state"], "wedged",
        "a non-zero failure count is a wedge, not lag: a terminal skip never touches it"
    );
    let retry_in = effect["retry_in_ms"]
        .as_u64()
        .expect("a wedged effect says how long until it tries again");
    assert!(
        retry_in > 0,
        "a duration, not an instant: a reader counts down without its clock having to \
         agree with the servers"
    );

    let (_, status_body) = get(&app, "/status").await;
    assert_eq!(
        status_body["effects"][0]["state"], "wedged",
        "`/status` and `/admin/effects` derive the state from one function, so they \
         cannot report different words for the same effect"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_caught_up_effect_is_healthy_and_names_no_deadline() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;
    wait_until("the effect catches up", || {
        let head = support::log_head(&harness.rt);
        harness.rt.effect("SendWelcome").unwrap().state(head) == "healthy"
    });

    let (status, listed) = get(&app, "/admin/effects").await;
    assert_eq!(status, 200);
    let effect = &listed["effects"][0];
    assert_eq!(effect["state"], "healthy");
    assert!(
        effect["retry_in_ms"].is_null(),
        "nothing is waiting, so there is no countdown to report"
    );

    harness.shutdown();
}

/// The envelope records that *an* effect produced an event, never which one. The
/// journal is keyed by effect and position, so the trace joins it and answers exactly.
#[tokio::test]
async fn a_trace_names_the_effect_that_ran_on_each_of_its_events() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    let response = post_command(&app, "RegisterUser", register(ALICE), None).await;
    let correlation = response.1["correlation_id"].as_str().unwrap().to_owned();
    // The head reaching 2 only means the effect's `invoke` committed; the
    // invocation is marked terminal after the handler returns. Waiting on the head
    // alone would sometimes read the row while it is still `running`.
    wait_until("the effect finishes the invocation", || {
        harness.rt.effect("SendWelcome").unwrap().position() >= 1
    });

    let (status, trace) = get(&app, &format!("/admin/traces/{correlation}")).await;
    assert_eq!(status, 200);
    let invocations = trace["invocations"].as_array().unwrap();
    assert_eq!(
        invocations.len(),
        1,
        "one effect subscribes to `user.registered`, so exactly one invocation ran \
         over this chain: {invocations:?}"
    );
    assert_eq!(invocations[0]["effect"], "SendWelcome");
    assert_eq!(
        invocations[0]["position"], 1,
        "the invocation is keyed by the position of the event that triggered it, \
         which is the first event of the chain rather than the one it appended"
    );
    assert_eq!(invocations[0]["status"], "terminal");

    harness.shutdown();
}

/// The contrast that keeps the test above from passing vacuously: a chain no effect
/// subscribes to joins nothing, rather than every invocation in the journal.
#[tokio::test]
async fn a_chain_no_effect_subscribes_to_joins_no_invocations() {
    let harness = boot_with(Arc::new(StubHttpClient::ok()));
    let app = harness.app();
    post_command(&app, "RegisterUser", register(ALICE), None).await;
    wait_for_head(&harness.rt, 2);

    // `schedule-reminder` appends `reminder.scheduled`, which `SendWelcome` does not
    // subscribe to, so this second chain has no invocation over it even though the
    // journal now holds one from the first.
    let response = post_command(&app, "ScheduleReminder", json!({ "user_id": ALICE }), None).await;
    let correlation = response.1["correlation_id"].as_str().unwrap().to_owned();

    let (status, trace) = get(&app, &format!("/admin/traces/{correlation}")).await;
    assert_eq!(status, 200);
    assert_eq!(trace["events"].as_array().unwrap().len(), 1);
    assert!(
        trace["invocations"].as_array().unwrap().is_empty(),
        "the join is by position, so an unrelated invocation must not leak in: {:?}",
        trace["invocations"]
    );

    harness.shutdown();
}
