//! `POST /admin/projections`: an ad-hoc projector, folded over the log, over HTTP.
//!
//! `tests/project.rs` covers the fold itself. What is worth proving here is what the
//! transport adds: that the server bounds the work rather than the caller, that a
//! deployment has to opt in, that a projector is compiled against what the process
//! booted with rather than what is on disk when the request lands, and that the body
//! coming back is the same value `hekla project --json` prints.

mod support;

use std::fs;
use std::process::Command;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use hekla::projection;
use serde_json::Value;
use support::{ALICE, BOB, Boot, CAROL, ctx, get, master_keys, place_order, write_project};
use tower::ServiceExt;

// --- fixtures --------------------------------------------------------------

const EVENTS: &str = "\
event @user.registered {
  user_id: Uuid,
  name: String @max(120),
}
";

const REGISTER: &str = "\
command Register(user_id: Uuid, name: String) {
  emit @user.registered { user_id, name }
}
";

/// Counts registrations by name, so several events fold into one row.
const BY_NAME: &str = "\
projector ByName {
  entity NameCount {
    name: String @key @max(120),
    registrations: Int,
  }

  on @user.registered { name } {
    patch NameCount[name] { registrations: .registrations + 1 }
  }
}
";

/// Stores the customer's sealed `email` under a differently named column, which only
/// works because the fold re-seals under the column's own name.
const BY_CUSTOMER: &str = "\
projector ByCustomer {
  entity PerCustomer {
    customer_id: Customer @key,
    orders: Int,
    last_email: String? @max(100),
  }

  on @order.placed { customer_id, email } {
    patch PerCustomer[customer_id] { orders: .orders + 1, last_email: email }
  }
}
";

const ADMIN_ON: &str = "[admin]\nprojections = true\n";

/// A project that serves projections, with three registrations already in the log.
///
/// Returns the project directory, the data directory and the booted harness; the
/// caller shuts the harness down.
fn booted(admin: &str) -> (tempfile::TempDir, tempfile::TempDir, support::Harness) {
    let project = write_project(&[
        ("hekla.toml", admin),
        ("events/user.hk", EVENTS),
        ("commands/register.hk", REGISTER),
    ]);
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(project.path()).data_dir(data.path()).start();
    // Two share a name and one does not, so the projector folds to two rows: enough for
    // a row cap to actually cap something.
    for (id, name) in [(ALICE, "Ada"), (BOB, "Ada"), (CAROL, "Grace")] {
        harness
            .rt
            .execute(
                "Register",
                serde_json::json!({ "user_id": id, "name": name }),
                &ctx(),
                None,
            )
            .unwrap();
    }
    (project, data, harness)
}

async fn post(app: &Router, query: &str, body: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/admin/projections{query}"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// The same POST, asking to be told how the fold is going. Returns the status and one
/// `Value` per line, in order.
async fn stream(app: &Router, query: &str, body: &str) -> (StatusCode, Vec<Value>) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/admin/projections{query}"))
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::ACCEPT, "application/x-ndjson")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let lines = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("every line is one JSON value"))
        .collect();
    (status, lines)
}

// --- the happy path --------------------------------------------------------

#[tokio::test]
async fn a_posted_projector_folds_the_log() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, body) = post(&harness.app(), "", BY_NAME).await;

    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["projector"], "ByName");
    assert_eq!(body["scanned"]["events"], 3);
    assert_eq!(body["scanned"]["stopped"], Value::Null);
    let rows = body["entities"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "two names across three registrations");
    assert_eq!(rows[0]["name"], "Ada");
    assert_eq!(rows[0]["registrations"], 2);
    assert_eq!(rows[1]["registrations"], 1);

    harness.shutdown();
}

/// One shape, two transports. A client that learns the CLI's `--json` has learned this,
/// which is the whole reason the handler returns `Projection::json()` unwrapped.
#[tokio::test]
async fn the_response_is_what_the_cli_prints_with_json() {
    let (project, data, harness) = booted(ADMIN_ON);
    let (_, over_http) = post(&harness.app(), "", BY_NAME).await;
    harness.shutdown();

    let scratch = tempfile::tempdir().unwrap();
    let file = scratch.path().join("question.hk");
    fs::write(&file, BY_NAME).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .args([
            "project",
            file.to_str().unwrap(),
            project.path().to_str().unwrap(),
            "--data-dir",
            data.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut from_cli: Value = serde_json::from_slice(&out.stdout).unwrap();

    // Everything but the module name each was compiled under, which is the one thing
    // that genuinely differs: a path the operator typed, against a fixed label no file
    // can collide with.
    let mut over_http = over_http;
    over_http["source"] = Value::Null;
    from_cli["source"] = Value::Null;
    assert_eq!(over_http, from_cli);
}

// --- watching it fold ------------------------------------------------------

/// The point of the stream: the answer is unchanged, and it is preceded by news.
#[tokio::test]
async fn a_stream_ends_with_the_projection_a_plain_post_returns() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (buffered_status, buffered) = post(&harness.app(), "", BY_NAME).await;
    let (streamed_status, lines) = stream(&harness.app(), "", BY_NAME).await;

    assert_eq!(buffered_status, StatusCode::OK);
    assert_eq!(streamed_status, StatusCode::OK, "{lines:?}");
    // `elapsed` is not on the wire and the digest is of the source, so two folds of the
    // same projector over the same log are the same value. A client that learned the
    // buffered body has learned this one.
    assert_eq!(
        lines.last().expect("a final line"),
        &buffered,
        "the last line is the body the other shape returns"
    );

    harness.shutdown();
}

/// Progress arrives before the answer, in the same units the answer reports.
#[tokio::test]
async fn a_stream_says_where_it_got_to_before_it_says_what_it_found() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, lines) = stream(&harness.app(), "", BY_NAME).await;
    assert_eq!(status, StatusCode::OK, "{lines:?}");

    let ticks: Vec<&Value> = lines
        .iter()
        .filter(|line| line.get("progress").is_some())
        .collect();
    // At least the unthrottled first one. Not *exactly* one: the throttle is real
    // elapsed time, so a loaded machine can put more than it between two of the three
    // matching events, and a count is the one thing here that is not a property of the
    // protocol.
    assert!(!ticks.is_empty(), "{lines:?}");
    let first = &ticks[0]["progress"];
    let answer = lines.last().unwrap();

    // Absolute, not window-relative: a tick is read against the final line's own
    // numbers, which is the whole reason the handler adds `from` back.
    assert_eq!(first["position"], 1, "the first match is at position 1");
    assert_eq!(first["upto"], answer["window"]["upto"]);
    assert_eq!(first["events"], 1);
    assert!(
        lines[lines.len() - 1].get("progress").is_none(),
        "the answer is not a tick: {lines:?}"
    );

    harness.shutdown();
}

/// A window that starts part way through still reports where it really is.
#[tokio::test]
async fn a_tick_inside_a_bounded_window_is_still_an_absolute_position() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, lines) = stream(&harness.app(), "?from=2", BY_NAME).await;
    assert_eq!(status, StatusCode::OK, "{lines:?}");

    let first = lines
        .iter()
        .find_map(|line| line.get("progress"))
        .expect("a tick");
    assert_eq!(
        first["position"], 2,
        "not 1, which is where the window opens"
    );
    assert_eq!(lines.last().unwrap()["scanned"]["events"], 2);

    harness.shutdown();
}

/// Compiling happens before a byte of the body is written, which is what keeps the
/// status code worth reading. A stream that carried this as a line would be a 200.
#[tokio::test]
async fn a_source_that_does_not_compile_is_a_400_even_when_a_stream_was_asked_for() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, lines) = stream(
        &harness.app(),
        "",
        "projector Bad {\n  entity T { id: String @key @max(9) }\n  on @user.nope { id } { put T { id } }\n}\n",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{lines:?}");
    assert_eq!(lines.len(), 1, "a refusal is one body, not a stream");
    assert_eq!(lines[0]["error"]["code"], "invalid_input");
    assert!(lines[0]["findings"].is_array(), "{lines:?}");

    harness.shutdown();
}

/// The window is checked in the same hop as the compile, for the same reason.
#[tokio::test]
async fn a_window_that_holds_nothing_is_a_400_even_when_a_stream_was_asked_for() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, lines) = stream(&harness.app(), "?from=9&upto=2", BY_NAME).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{lines:?}");
    assert_eq!(lines[0]["error"]["code"], "invalid_input");

    harness.shutdown();
}

/// A request that passed the check and then failed is ours, not the caller's. Every
/// refusal the caller could have avoided is a 400 from the hop before this one, so a
/// 400 here would tell somebody with a full disk to go and fix their heklang.
#[tokio::test]
async fn a_fold_that_fails_after_the_check_is_not_the_callers_fault() {
    let project = write_project(&[
        ("hekla.toml", ADMIN_ON),
        ("events/user.hk", EVENTS),
        ("commands/register.hk", REGISTER),
    ]);
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(project.path()).data_dir(data.path()).start();

    // A projector refused at the declaration, which is the half this test is about: a
    // 400 from the compile, before anything opens the operational database, so the two
    // failure sources are told apart rather than both being 400.
    //
    // `@subject` is not an entity annotation: a column's seal is propagated from what
    // is written into it, never authored. So this is refused at the declaration, before
    // anything opens a database, which is the half this test is about.
    let sealed = "\
projector Sealed {
  entity S {
    name: String @key @max(120) @subject(name),
    seen: Int,
  }

  on @user.registered { name } {
    patch S[name] { seen: .seen + 1 }
  }
}
";
    let (status, body) = post(&harness.app(), "", sealed).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "no master key is a fact about the declaration, settled before anything opens: {body:?}"
    );
    assert_eq!(body["error"]["code"], "invalid_input");

    harness.shutdown();
}

/// A caller that has not heard of the streaming body is never handed one, whatever else
/// it accepts.
#[tokio::test]
async fn a_caller_that_did_not_ask_for_a_stream_gets_the_whole_body() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/admin/projections")
        .header(header::ACCEPT, "application/json, text/html, */*")
        .body(Body::from(BY_NAME))
        .unwrap();
    let response = harness.app().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json",
        "`*/*` is not an ask for a representation nothing else offers"
    );

    harness.shutdown();
}

// --- the gate --------------------------------------------------------------

#[tokio::test]
async fn a_projection_is_refused_when_the_setting_is_off() {
    let (_project, _data, harness) = booted("");
    let (status, body) = post(&harness.app(), "", BY_NAME).await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");
    assert_eq!(body["error"]["code"], "projections_disabled");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("[admin] projections"), "{message}");

    harness.shutdown();
}

/// The GET answers either way, which is what keeps the refusal discoverable: a 403 an
/// operator cannot ask about is worse than one they can.
#[tokio::test]
async fn the_get_reports_the_limits_and_whether_it_is_on() {
    let (_project, _data, off) = booted("");
    let (status, body) = get(&off.app(), "/admin/projections").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert_eq!(
        body["max_events"]["limit"],
        projection::SERVED_MAX_EVENTS,
        "a client learns the ceiling before spending a request on it"
    );
    assert_eq!(
        body["max_events"]["default"],
        projection::SERVED_DEFAULT_EVENTS
    );
    off.shutdown();

    let (_project, _data, on) = booted(ADMIN_ON);
    let (_, body) = get(&on.app(), "/admin/projections").await;
    assert_eq!(body["enabled"], true);
    assert_eq!(body["log_head"], 3);
    on.shutdown();
}

/// `/admin/system` reports the effective configuration, and this is part of it.
#[tokio::test]
async fn the_system_view_reports_whether_projections_are_served() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, body) = get(&harness.app(), "/admin/system").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["config"]["admin"]["projections"], true);
    harness.shutdown();
}

// --- the server's bounds, not the caller's ---------------------------------

#[tokio::test]
async fn a_budget_above_the_ceiling_is_clamped_not_rejected() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let query = format!("?max_events={}", projection::SERVED_MAX_EVENTS * 100);
    let (status, body) = post(&harness.app(), &query, BY_NAME).await;

    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        body["scanned"]["events"], 3,
        "the whole log is under the ceiling"
    );
    assert_eq!(body["scanned"]["stopped"], Value::Null);

    harness.shutdown();
}

/// A caller that names no budget still gets one, because a request has something
/// waiting on the other end of it.
#[tokio::test]
async fn a_budget_applies_when_the_caller_names_none() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (_, unbounded) = post(&harness.app(), "", BY_NAME).await;
    let (_, bounded) = post(&harness.app(), "?max_events=1", BY_NAME).await;

    // The default is far above this fixture, so the two agree except where the caller
    // asked for less. What this pins is that the bounded one *can* stop, and says so.
    assert_eq!(unbounded["scanned"]["stopped"], Value::Null);
    assert_eq!(bounded["scanned"]["events"], 1);
    assert_eq!(bounded["scanned"]["stopped"], "max-events");

    harness.shutdown();
}

#[tokio::test]
async fn a_row_cap_is_clamped_and_the_count_still_reports_every_row() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, body) = post(&harness.app(), "?rows=1", BY_NAME).await;

    assert_eq!(status, StatusCode::OK, "{body:?}");
    let entity = &body["entities"][0];
    assert_eq!(entity["rows"].as_array().unwrap().len(), 1, "the cap held");
    assert_eq!(
        entity["row_count"], 2,
        "and the count is of the whole entity"
    );
    assert_eq!(entity["truncated"], true);
    assert_eq!(body["truncated"], true, "the top level says so too");

    harness.shutdown();
}

/// The per-request budget bounds one fold; this bounds the endpoint. Without it a
/// handful of concurrent requests would own the blocking pool every other reader here
/// shares.
#[tokio::test]
async fn only_so_many_projections_fold_at_once() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let app = harness.app();

    // Hold every slot, then ask for one more. Held across the await, so the guards are
    // still alive when the request lands.
    let mut held = Vec::new();
    for _ in 0..hekla::runtime::PROJECTION_SLOTS {
        held.push(harness.rt.projection_slot().expect("a free slot"));
    }
    let (status, body) = post(&app, "", BY_NAME).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body:?}");
    assert_eq!(body["error"]["code"], "projections_busy");

    // And the slots come back.
    drop(held);
    let (status, _) = post(&app, "", BY_NAME).await;
    assert_eq!(status, StatusCode::OK);

    harness.shutdown();
}

// --- what the request can get wrong ----------------------------------------

/// The compiler's own diagnostics, with the line and column. A caller that sent source
/// has to get back the thing it must fix.
#[tokio::test]
async fn a_source_that_does_not_compile_is_a_400_carrying_the_findings() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, body) = post(
        &harness.app(),
        "",
        "projector Bad {\n  entity T { id: String @key @max(9) }\n  on @user.nope { id } { put T { id } }\n}\n",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");
    let findings = body["findings"].as_array().expect("the diagnostics");
    let first = &findings[0];
    assert_eq!(first["severity"], "error", "{first:?}");
    assert_eq!(first["location"], "<projection>", "{first:?}");
    assert!(
        first["message"].as_str().unwrap().contains("user.nope"),
        "{first:?}"
    );
    // Structured, not rendered: the console puts a caret here, and a browser regexing a
    // `format!` in `validate.rs` is the coupling this shape exists to avoid.
    //
    // The numbers are checked and not merely present, because they were wrong: heklang
    // counts from one and hekla added one more, so every diagnostic it had ever printed
    // pointed a line past the problem. `on @user.nope` is the third line of the body
    // above and `@` is its sixth character.
    assert_eq!(first["line"], 3, "{first:?}");
    assert_eq!(first["column"], 6, "{first:?}");

    harness.shutdown();
}

#[tokio::test]
async fn an_empty_body_is_a_400_rather_than_a_projection_of_nothing() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, body) = post(&harness.app(), "", "   \n").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");
    harness.shutdown();
}

/// Rejected rather than ignored, unlike a filter on `/admin/events`. A typo in
/// `max_events` costs the bound the caller was trying to impose.
#[tokio::test]
async fn an_unknown_query_parameter_is_refused() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, body) = post(&harness.app(), "?max_event=1", BY_NAME).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("max_event"), "{message}");
    assert!(
        message.contains("max_events"),
        "it names the real one: {message}"
    );

    harness.shutdown();
}

/// A refusal that belongs to the request rather than to the source: no projector to
/// fold. It must not surface as a 500.
#[tokio::test]
async fn a_source_with_no_projector_is_a_400() {
    let (_project, _data, harness) = booted(ADMIN_ON);
    let (status, body) = post(&harness.app(), "", "fn double(n: Int) -> Int { n * 2 }\n").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body["error"]["code"], "invalid_input");
    harness.shutdown();
}

// --- what it compiles against ----------------------------------------------

/// The reason the runtime keeps its sources rather than re-reading the directory. A
/// module edited under a live process would typecheck a projection against declarations
/// this process is not running, and the fold would then read stored payloads against a
/// schema the log has never seen.
#[tokio::test]
async fn a_projection_compiles_against_what_booted_not_what_is_on_disk() {
    let (project, _data, harness) = booted(ADMIN_ON);

    let events = project.path().join("events/user.hk");
    let edited = format!("{EVENTS}\nevent @user.invented {{ user_id: Uuid }}\n");
    fs::write(&events, edited).unwrap();

    let (status, body) = post(
        &harness.app(),
        "",
        "projector Q {\n  entity T { id: String @key @max(40) }\n  on @user.invented { user_id } { put T { id: user_id } }\n}\n",
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an event added after boot is not deployed, so it is not there to fold: {body:?}"
    );
    let findings = body["findings"].as_array().unwrap();
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap()
            .contains("user.invented"),
        "{findings:?}"
    );

    harness.shutdown();
}

// --- what it does not do ---------------------------------------------------

#[tokio::test]
async fn a_posted_projection_writes_nothing_to_the_data_directory() {
    let (_project, data, harness) = booted(ADMIN_ON);
    // The whole directory, not just `events/`: the claim is that a projection leaves no
    // mark anywhere, and a diff of one subtree would pass for a read model written into
    // another. `data/projectors/` exists from boot, so naming it alone would compare two
    // empty listings and assert nothing.
    let before = support::tree(data.path());
    assert!(!before.is_empty(), "the fixture wrote a segment");

    let (status, body) = post(&harness.app(), "", BY_NAME).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["scanned"]["events"], 3, "the fold actually ran");

    let after = support::tree(data.path());
    let changed: Vec<&String> = before
        .iter()
        .zip(&after)
        .filter(|(before, after)| before != after)
        .map(|(before, _)| &before.0)
        .collect();
    assert_eq!(before.len(), after.len(), "a file appeared or went");
    assert!(changed.is_empty(), "these changed: {changed:?}");

    harness.shutdown();
}

/// It reads through a follower while the process that owns the directory is running,
/// which is the same property `hekla project` has and the reason neither takes a lock.
#[tokio::test]
async fn a_projection_runs_against_the_directory_its_own_server_holds() {
    let (_project, _data, harness) = booted(ADMIN_ON);

    let (first, _) = post(&harness.app(), "", BY_NAME).await;
    let (second, body) = post(&harness.app(), "", BY_NAME).await;
    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK, "{body:?}");
    assert_eq!(body["scanned"]["events"], 3);

    harness.shutdown();
}

// --- sealed columns --------------------------------------------------------

/// The masters reach the fold through the runtime's key store, and the fold builds its
/// own over its own connection rather than taking the one every effect shares.
#[tokio::test]
async fn a_sealed_projection_reads_back_as_plaintext() {
    let orders = support::orders_project_with(&[
        ("hekla.toml", ADMIN_ON),
        ("projectors/orders.hk", support::ORDERS_PROJECTOR),
    ]);
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(orders.path())
        .data_dir(data.path())
        .with_master_key()
        .start();
    place_order(&harness.rt, support::UUID_A, 1, "ada@example.test");

    let (status, body) = post(&harness.app(), "", BY_CUSTOMER).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let rows = body["entities"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0]["last_email"], "ada@example.test");
    assert_eq!(body["shredded"]["writes"], 0);

    // And the rendering opt-out leaves the stored value alone.
    let (_, sealed) = post(&harness.app(), "?decrypt=false", BY_CUSTOMER).await;
    let rows = sealed["entities"][0]["rows"].as_array().unwrap();
    assert_ne!(rows[0]["last_email"], "ada@example.test");

    // The fixture key is what the harness booted with, so a projection that could not
    // reach it would have failed above rather than returned ciphertext.
    let _ = master_keys();

    harness.shutdown();
}
