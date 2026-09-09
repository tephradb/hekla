//! The Prometheus scrape at `/metrics`, end to end.
//!
//! Two things are pinned here that a unit test over the recorder cannot reach: that a
//! scrape describes the modules a real project loaded, and that a lane key never
//! becomes a label. The second is the one that matters most and is why this file exists
//! rather than a couple more cases in `src/metrics.rs`: `/status` may name a partition
//! key because it is a live view an erasure passes through, and a scrape may not
//! because it is a copy an erasure cannot reach.
//!
//! **Every case takes [`SERIAL`], and that is not incidental.** A recorder is
//! process-wide and a series is keyed by its labels, so two harnesses booting the same
//! example project in parallel would be writing `hekla_effect_state{name="SendWelcome"}`
//! at each other. That is a property of running several runtimes in one process, which
//! only a test does; a deployment has one. Counters are asserted as a delta across the
//! case rather than as a total, because a counter still carries what earlier cases in
//! this process added to it.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use hekla::effect::StubHttpClient;
use serde_json::json;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

mod support;

use support::{ALICE, Boot, boot_example, get, post_command, register_body, wait_until};

/// Held for the length of every case in this file. See the module docs.
///
/// `tokio`'s rather than `std`'s, because a case holds it across every `await` it makes
/// and a blocking guard there parks the runtime thread.
///
/// It also cannot be poisoned, which is what lets the next case run at all after one
/// panics, though not what makes that next case trustworthy: a panicking case skips its
/// `harness.shutdown()` and `Harness` has no `Drop`, so its projector and effect threads
/// keep running and keep recording. **Read the first failure, not the second.** Fixing
/// that properly means a `Drop` on the shared harness, which is the whole suite's
/// convention and not this file's to change.
static SERIAL: Mutex<()> = Mutex::const_new(());

async fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().await
}

/// Scrape `/metrics`, returning the status, the content type and the body. Not
/// `support::get`, which decodes JSON: this endpoint answers a line protocol.
async fn scrape(app: &Router) -> (StatusCode, String, String) {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        content_type,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

/// Just the body, for the cases that only read samples out of it.
async fn render(app: &Router) -> String {
    scrape(app).await.2
}

/// The first sample whose line names `metric` and contains every one of `parts`.
///
/// The metric name is matched up to its delimiter rather than by prefix, so an
/// assertion cannot pass by matching a longer name that happens to start the same way.
fn sample(render: &str, metric: &str, parts: &[&str]) -> Option<f64> {
    render
        .lines()
        .filter(|line| !line.starts_with('#') && line.split(['{', ' ']).next() == Some(metric))
        .find(|line| parts.iter().all(|part| line.contains(part)))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

/// A sample's value, or `0.0` when the series is absent. What a counter delta wants: an
/// absent series and a zero reading mean the same thing to `increase()`.
fn value(render: &str, metric: &str, parts: &[&str]) -> f64 {
    sample(render, metric, parts).unwrap_or(0.0)
}

#[tokio::test]
async fn a_scrape_answers_the_text_format_with_help_text() {
    let _serial = serial().await;
    let harness = boot_example();
    let (status, content_type, body) = scrape(&harness.app()).await;

    assert_eq!(status, 200);
    assert_eq!(
        content_type, "text/plain; version=0.0.4",
        "the version is the exposition format's, not hekla's"
    );
    assert!(
        body.contains("# HELP hekla_log_head_position"),
        "a described metric carries its help text: {body}"
    );
    assert!(
        body.contains("# TYPE hekla_commands_total counter"),
        "{body}"
    );
    harness.shutdown();
}

/// The inventory is there before anything has happened, which is what makes an alert on
/// a module going missing possible: a series that only appeared once a module had done
/// something could never say that one had stopped.
#[tokio::test]
async fn every_loaded_module_is_described_before_anything_runs() {
    let _serial = serial().await;
    let harness = boot_example();
    let body = render(&harness.app()).await;

    for (kind, name) in [
        ("command", "RegisterUser"),
        ("command", "RecordWelcome"),
        ("projector", "Users"),
        ("projector", "UserStats"),
        ("effect", "SendWelcome"),
    ] {
        assert!(
            body.lines()
                .any(|line| line.starts_with("hekla_module_info")
                    && line.contains(&format!(r#"kind="{kind}""#))
                    && line.contains(&format!(r#"name="{name}""#))
                    && line.contains(r#"hash=""#)),
            "no `hekla_module_info` for the {kind} `{name}`: {body}"
        );
    }
    assert!(
        body.contains(r#"kind="command",name="RecordWelcome""#),
        "an internal command is loaded and running, so it is in the inventory even \
         though `POST /commands/RecordWelcome` is a 404: {body}"
    );

    // Present rather than zero: this process may have run other cases first. That a
    // never-fired counter reads exactly 0 is asserted in `src/metrics.rs`, against a
    // recorder local to that test.
    for (metric, parts) in [
        (
            "hekla_commands_total",
            vec![r#"command="ScheduleReminder""#, r#"outcome="committed""#],
        ),
        // The 500 path lives in the handler rather than in `run_with_retry`, so this
        // series exists only if `execute` was wired up as well as enumerated. A critical
        // alert reads it, and a counter nothing can ever increment is worse than none.
        (
            "hekla_commands_total",
            vec![r#"command="RegisterUser""#, r#"outcome="error""#],
        ),
        (
            "hekla_reads_total",
            vec![
                r#"projector="Users""#,
                r#"entity="User""#,
                r#"outcome="not_found""#,
            ],
        ),
        (
            "hekla_read_waits_total",
            vec![r#"projector="Users""#, r#"outcome="timeout""#],
        ),
    ] {
        assert!(
            sample(&body, metric, &parts).is_some(),
            "`{metric}{parts:?}` has no series before it fires, so `rate()` does not \
             work from the first scrape: {body}"
        );
    }
    harness.shutdown();
}

/// A malformed query is the caller's mistake, not hekla's, and was the one part of the
/// read surface with no series at all: both names are already declarations by the time a
/// 400 is decided, so the cardinality argument that keeps the two 404s uncounted does not
/// reach it.
#[tokio::test]
async fn a_malformed_read_is_counted_as_invalid_input() {
    let _serial = serial().await;
    let harness = boot_example();
    let app = harness.app();
    let before = render(&app).await;

    let (status, _) = get(&app, "/read/Users/User?limit=nope").await;
    assert_eq!(status, 400);
    let (status, _) = get(&app, "/read/Users/User?cursor=not-base64!!").await;
    assert_eq!(status, 400);
    let after = render(&app).await;

    let invalid = [
        r#"projector="Users""#,
        r#"entity="User""#,
        r#"outcome="invalid_input""#,
    ];
    assert_eq!(
        value(&after, "hekla_reads_total", &invalid)
            - value(&before, "hekla_reads_total", &invalid),
        2.0,
        "{after}"
    );
    harness.shutdown();
}

#[tokio::test]
async fn a_command_moves_its_outcome_and_its_appended_events() {
    let _serial = serial().await;
    let harness = boot_example();
    let app = harness.app();
    let before = render(&app).await;

    let (status, _) = post_command(
        &app,
        "RegisterUser",
        register_body(ALICE, "alice@example.com", "Alice"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let after = render(&app).await;

    let committed = [r#"command="RegisterUser""#, r#"outcome="committed""#];
    assert_eq!(
        value(&after, "hekla_commands_total", &committed)
            - value(&before, "hekla_commands_total", &committed),
        1.0,
        "{after}"
    );
    let registered = [r#"event="user.registered""#];
    assert_eq!(
        value(&after, "hekla_events_appended_total", &registered)
            - value(&before, "hekla_events_appended_total", &registered),
        1.0,
        "{after}"
    );
    harness.shutdown();
}

/// A refusal is the command working, so it must not read as an error anywhere: the
/// `rejected` outcome moves, the declared code appears, and `error` does not move.
#[tokio::test]
async fn a_refusal_reports_its_declared_code_and_is_not_an_error() {
    let _serial = serial().await;
    let harness = boot_example();
    let app = harness.app();

    post_command(
        &app,
        "RegisterUser",
        register_body(ALICE, "taken@example.com", "Alice"),
        None,
    )
    .await;
    let before = render(&app).await;

    let (status, body) = post_command(
        &app,
        "RegisterUser",
        json!({
            "user_id": "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
            "email": "taken@example.com",
            "name": "Someone Else",
        }),
        None,
    )
    .await;
    assert_eq!(status, 422, "the second registration is refused");
    assert_eq!(body["error"]["code"], "email_taken");
    let after = render(&app).await;

    let rejected = [r#"command="RegisterUser""#, r#"outcome="rejected""#];
    assert_eq!(
        value(&after, "hekla_commands_total", &rejected)
            - value(&before, "hekla_commands_total", &rejected),
        1.0,
        "{after}"
    );
    let code = [r#"command="RegisterUser""#, r#"code="email_taken""#];
    assert_eq!(
        value(&after, "hekla_command_refusals_total", &code)
            - value(&before, "hekla_command_refusals_total", &code),
        1.0,
        "the code is the one the response carried, derived from a declared `refusal`: \
         {after}"
    );
    let errored = [r#"command="RegisterUser""#, r#"outcome="error""#];
    assert_eq!(
        value(&after, "hekla_commands_total", &errored)
            - value(&before, "hekla_commands_total", &errored),
        0.0,
        "a refusal is the command working, not an error: {after}"
    );
    harness.shutdown();
}

#[tokio::test]
async fn a_caught_up_projector_reports_no_lag_and_a_ready_state() {
    let _serial = serial().await;
    let harness = boot_example();
    let app = harness.app();
    let before = render(&app).await;

    post_command(
        &app,
        "RegisterUser",
        register_body(ALICE, "alice@example.com", "Alice"),
        None,
    )
    .await;
    support::wait_position(&harness.rt, "Users", 1);
    let after = render(&app).await;

    let users = [r#"name="Users""#];
    assert_eq!(value(&after, "hekla_projector_lag", &users), 0.0, "{after}");
    assert_eq!(value(&after, "hekla_projector_up", &users), 1.0, "{after}");
    assert_eq!(
        value(
            &after,
            "hekla_projector_readiness",
            &[r#"name="Users""#, r#"state="ready""#],
        ),
        1.0,
        "{after}"
    );
    assert_eq!(
        value(
            &after,
            "hekla_projector_readiness",
            &[r#"name="Users""#, r#"state="stale""#],
        ),
        0.0,
        "a state set puts 1 on the current state and 0 on every other: {after}"
    );
    assert!(
        value(&after, "hekla_projector_events_total", &users)
            - value(&before, "hekla_projector_events_total", &users)
            >= 1.0,
        "{after}"
    );
    harness.shutdown();
}

#[tokio::test]
async fn a_wedged_effect_reports_its_wedged_lane_count_and_state() {
    let _serial = serial().await;
    let harness = Boot::example()
        .http(Arc::new(StubHttpClient::status(500)))
        .start();
    let app = harness.app();
    let before = render(&app).await;

    post_command(
        &app,
        "RegisterUser",
        register_body(ALICE, "alice@example.com", "Alice"),
        None,
    )
    .await;
    wait_until("the effect wedges on the failing endpoint", || {
        harness.rt.effect("SendWelcome").unwrap().wedged_lanes() >= 1
    });
    let after = render(&app).await;

    let effect = [r#"name="SendWelcome""#];
    assert!(
        value(&after, "hekla_effect_wedged_lanes", &effect) >= 1.0,
        "a count and never a key: {after}"
    );
    assert_eq!(
        value(
            &after,
            "hekla_effect_state",
            &[r#"name="SendWelcome""#, r#"state="wedged""#],
        ),
        1.0,
        "{after}"
    );
    assert_eq!(
        value(
            &after,
            "hekla_effect_state",
            &[r#"name="SendWelcome""#, r#"state="healthy""#],
        ),
        0.0,
        "{after}"
    );
    assert!(
        value(&after, "hekla_effect_consecutive_failures", &effect) >= 1.0,
        "{after}"
    );

    let five_hundreds = [r#"outcome="5xx""#];
    assert!(
        value(&after, "hekla_effect_http_requests_total", &five_hundreds)
            - value(&before, "hekla_effect_http_requests_total", &five_hundreds)
            >= 1.0,
        "the failing endpoint is visible as a status class: {after}"
    );

    let (_, status_body) = get(&app, "/status").await;
    assert_eq!(
        status_body["effects"][0]["state"], "wedged",
        "the scrape and `/status` read the same function, so they cannot report \
         different words for one effect"
    );
    harness.shutdown();
}

/// The rule `hekla::metrics` states, as a check on the bytes rather than on the intent.
/// `/status` may name the pinning lane, because it is a live view an erasure passes
/// through; a scrape may not, because it is a copy taken into a database `hekla erase`
/// cannot reach.
#[tokio::test]
async fn a_lane_key_never_reaches_the_scrape() {
    let _serial = serial().await;
    let harness = Boot::example()
        .http(Arc::new(StubHttpClient::status(500)))
        .start();
    let app = harness.app();

    post_command(
        &app,
        "RegisterUser",
        register_body(ALICE, "alice@example.com", "Alice"),
        None,
    )
    .await;
    wait_until("the effect names the lane holding its mark down", || {
        harness
            .rt
            .effect("SendWelcome")
            .unwrap()
            .pinning()
            .is_some()
    });

    let (lane, _) = harness.rt.effect("SendWelcome").unwrap().pinning().unwrap();
    assert!(
        lane.contains(ALICE),
        "this case is only meaningful while the lane carries the `@key user_id`: {lane}"
    );

    let (_, status_body) = get(&app, "/status").await;
    assert_eq!(
        status_body["effects"][0]["pinning_key"], lane,
        "`/status` names it, and may: an erased subject stops appearing there"
    );

    let body = render(&app).await;
    assert!(
        !body.contains(&lane),
        "the lane key `{lane}` reached the scrape: {body}"
    );
    assert!(
        !body.contains(ALICE),
        "the partition value reached the scrape by another route: {body}"
    );
    harness.shutdown();
}
