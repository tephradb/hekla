//! The HTTP surface: `POST /commands/{name}`, the generated read API
//! (`GET /read/{projector}/{entity}[/{key}]`), `POST /projectors/{name}/replay`,
//! `POST /effects/{name}/skip/{position}`, `GET /status`, `GET /health`, the
//! generated `GET /openapi.json`, a Scalar reference UI over it at `GET /docs`, and
//! the read-only introspection surface under `GET /admin`.
//!
//! Nothing here is authenticated, and that is not specific to `/admin`: a caller who
//! can reach this port can already append events and skip an effect's work. The bind
//! address is the boundary, and it defaults to loopback (`crate::cli`). One prefix for
//! everything read-only is what lets a deployment that binds wider block it in a proxy.
//!
//! Handlers are thin. They pull the correlation id and idempotency key from
//! headers, mint a per-request [`CommandContext`], and run the (synchronous)
//! command cycle on a blocking thread. The [`Runtime`] owns the outcome-to-status
//! mapping, so the server only turns an [`ExecResult`] into a JSON response.

use std::collections::HashMap;
use std::future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{MethodRouter, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tephra::WriteCoordinator;
use tokio::net::TcpListener;
use tokio::signal;
use uuid::Uuid;

use crate::context::CommandContext;
use crate::effect::EffectRuntime;
use crate::introspect;
use crate::projector::{ProjectorSet, ProjectorShared, Readiness};
use crate::read_api;
use crate::runtime::{Runtime, error_body};
use crate::schema::{EntityDef, EventDef, ModuleDef};
use crate::ui;

type Shared = Arc<Runtime>;

/// Serve `runtime` on `addr` until a shutdown signal, then drain in order: the
/// effects first (they append through commands, and finish any in-flight
/// invocation) while the writer is still live, then the projectors (so their read
/// models reflect what the effects just wrote), and finally the writer through
/// `coordinator`.
pub async fn serve(
    runtime: Arc<Runtime>,
    coordinator: WriteCoordinator,
    projectors: ProjectorSet,
    effects: EffectRuntime,
    addr: SocketAddr,
) -> anyhow::Result<()> {
    let service = app(runtime);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("hekla listening on http://{addr}");
    // The console and the introspection API are one URL, told apart by `Accept`, so
    // this is both lines at once: a browser opens the console, curl gets the JSON.
    tracing::info!("  admin console   http://{addr}{ADMIN_ROUTE}");
    tracing::info!("  api reference   http://{addr}{DOCS_ROUTE}");
    axum::serve(listener, service)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving http")?;
    tracing::info!("draining effects, then projectors, then shutting down the writer");
    effects.shutdown_and_join();
    projectors.shutdown_and_join();
    coordinator.shutdown();
    Ok(())
}

/// Every path template [`app`] registers, named once and used from the table below.
/// axum exposes no route table of its own, so [`routes`] is what the OpenAPI drift
/// test reads to check that every served path is described.
pub const COMMAND_ROUTE: &str = "/commands/{name}";
pub const READ_ONE_ROUTE: &str = "/read/{projector}/{entity}/{key}";
pub const READ_SCAN_ROUTE: &str = "/read/{projector}/{entity}";
pub const REPLAY_ROUTE: &str = "/projectors/{name}/replay";
pub const SKIP_ROUTE: &str = "/effects/{name}/skip/{position}";
pub const STATUS_ROUTE: &str = "/status";
pub const HEALTH_ROUTE: &str = "/health";
pub const OPENAPI_ROUTE: &str = "/openapi.json";
pub const DOCS_ROUTE: &str = "/docs";

/// The read-only introspection surface. One prefix, so a deployment that binds
/// beyond loopback can deny it wholesale without hekla's cooperation.
pub const ADMIN_ROUTE: &str = "/admin";
pub const ADMIN_EVENTS_ROUTE: &str = "/admin/events";
pub const ADMIN_EVENT_ROUTE: &str = "/admin/events/{position}";
pub const ADMIN_TRACE_ROUTE: &str = "/admin/traces/{correlation_id}";
pub const ADMIN_EFFECTS_ROUTE: &str = "/admin/effects";
pub const ADMIN_EFFECT_ROUTE: &str = "/admin/effects/{name}";
pub const ADMIN_INVOCATIONS_ROUTE: &str = "/admin/effects/{name}/invocations";
pub const ADMIN_INVOCATION_ROUTE: &str = "/admin/effects/{name}/invocations/{position}";
pub const ADMIN_PROJECTORS_ROUTE: &str = "/admin/projectors";
pub const ADMIN_PROJECTOR_ROUTE: &str = "/admin/projectors/{name}";
pub const ADMIN_SCHEMA_ROUTE: &str = "/admin/schema";
pub const ADMIN_SYSTEM_ROUTE: &str = "/admin/system";
pub const ADMIN_SUBJECTS_ROUTE: &str = "/admin/subjects";
pub const ADMIN_SUBJECT_ROUTE: &str = "/admin/subjects/{field}/{value}";

/// The admin console's own files. Flat, because `{file}` captures a single segment: a
/// nested asset would be unroutable, and worse, undescribable, since the drift test's
/// matcher compares segment counts exactly. It is the one `/admin` route that is not
/// content-negotiated, since it serves the console rather than being a view of it.
pub const ADMIN_ASSETS_ROUTE: &str = "/admin/assets/{file}";

/// Every route, as one table.
///
/// [`app`] folds this into a `Router` and [`routes`] projects out the paths, so a route
/// this process serves and a route the generated OpenAPI is checked against are the
/// same list by construction. A second hand-maintained copy would catch a typo but not
/// the failure that matters: adding a route and forgetting to declare it, leaving a
/// public endpoint with no spec and a drift test that still passes.
fn route_table() -> Vec<(&'static str, MethodRouter<Shared>)> {
    vec![
        (COMMAND_ROUTE, post(execute)),
        (READ_ONE_ROUTE, get(read_one)),
        (READ_SCAN_ROUTE, get(read_scan)),
        (REPLAY_ROUTE, post(replay)),
        (SKIP_ROUTE, post(skip)),
        (STATUS_ROUTE, get(status)),
        (HEALTH_ROUTE, get(health)),
        (OPENAPI_ROUTE, get(openapi_doc)),
        (DOCS_ROUTE, get(docs)),
        (ADMIN_ROUTE, get(admin_index)),
        (ADMIN_EVENTS_ROUTE, get(admin_events)),
        (ADMIN_EVENT_ROUTE, get(admin_event)),
        (ADMIN_TRACE_ROUTE, get(admin_trace)),
        (ADMIN_EFFECTS_ROUTE, get(admin_effects)),
        (ADMIN_EFFECT_ROUTE, get(admin_effect)),
        (ADMIN_INVOCATIONS_ROUTE, get(admin_invocations)),
        (ADMIN_INVOCATION_ROUTE, get(admin_invocation)),
        (ADMIN_PROJECTORS_ROUTE, get(admin_projectors)),
        (ADMIN_PROJECTOR_ROUTE, get(admin_projector)),
        (ADMIN_SCHEMA_ROUTE, get(admin_schema)),
        (ADMIN_SYSTEM_ROUTE, get(admin_system)),
        (ADMIN_SUBJECTS_ROUTE, get(admin_subjects)),
        (ADMIN_SUBJECT_ROUTE, get(admin_subject)),
        (ADMIN_ASSETS_ROUTE, get(admin_asset)),
    ]
}

/// Every path template [`app`] registers, derived from the same table it registers.
pub fn routes() -> Vec<&'static str> {
    route_table().into_iter().map(|(path, _)| path).collect()
}

/// The HTTP application over an already-built runtime, for in-process testing
/// (drive it with `tower::ServiceExt::oneshot`).
pub fn app(shared: Shared) -> Router {
    route_table()
        .into_iter()
        .fold(Router::new(), |router, (path, handler)| {
            // Content negotiation is attached per route, from the same table the
            // routes come from, so an `/admin` route added later gets a deep link
            // without anyone remembering to. Layering the whole `Router` instead would
            // run the check on `/commands` and `/read`, and would turn every unrouted
            // `/admin/...` 404 into a 200 page, which is a worse answer than the 404.
            let handler = if path.starts_with(ADMIN_ROUTE) && path != ADMIN_ASSETS_ROUTE {
                // `route_layer`, not `layer`: `MethodRouter::layer` wraps the
                // method-not-allowed fallback too, so a `POST` here would short-circuit
                // into the console shell and answer 200 instead of 405.
                handler.route_layer(middleware::from_fn(ui::negotiate))
            } else {
                handler
            };
            router.route(path, handler)
        })
        .with_state(shared)
}

/// A Scalar API reference over the generated spec. The page loads Scalar from a
/// CDN and points it at `/openapi.json`, so it needs a network connection but no
/// bundled assets.
const SCALAR_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <title>hekla API reference</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
  </head>
  <body>
    <div id="app"></div>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
    <script>
      Scalar.createApiReference('#app', { url: '/openapi.json' })
    </script>
  </body>
</html>
"#;

async fn execute(
    State(runtime): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ctx = CommandContext::new(correlation_id(&headers));
    // Normalize the key that gets hashed into the idempotency tag: trim surrounding
    // whitespace (proxies add it, and `"k"` vs `"k "` must dedupe) and treat a blank
    // header as no key at all.
    let idem_key = header_string(&headers, "idempotency-key")
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty());

    let value = match parse_body(&body) {
        Ok(value) => value,
        Err(message) => {
            return json_response(400, error_body(&ctx, "invalid_input", &message));
        }
    };

    let task = tokio::task::spawn_blocking(move || {
        runtime.execute(&name, value, &ctx, idem_key.as_deref())
    });
    match task.await {
        Ok(Ok(result)) => json_response(result.status, result.body),
        Ok(Err(err)) => {
            tracing::error!("command execution failed: {err:#}");
            json_response(500, error_body(&ctx, "internal", "internal error"))
        }
        Err(err) => {
            tracing::error!("command task panicked: {err}");
            json_response(500, error_body(&ctx, "internal", "internal error"))
        }
    }
}

async fn status(State(runtime): State<Shared>) -> Json<Value> {
    Json(runtime.status())
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn openapi_doc(State(runtime): State<Shared>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        runtime.openapi_json().to_owned(),
    )
        .into_response()
}

async fn docs() -> Html<&'static str> {
    Html(SCALAR_HTML)
}

/// `GET /read/{projector}/{entity}/{key}`: one row by key, with the projector's
/// log position. 404 for an unknown projector, entity, or missing row. An optional
/// `?after=<pos>` first waits for the projector to reach that position (503 if it
/// cannot within `timeout_ms`, default 5s), for read-your-writes.
async fn read_one(
    State(runtime): State<Shared>,
    Path((projector, entity, key)): Path<(String, String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (shared, entity_def) = match resolve_entity(&runtime, &projector, &entity) {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let wait = match parse_wait(&params) {
        Ok(wait) => wait,
        Err(response) => return *response,
    };
    if let Some(response) = honor_wait(&shared, &projector, wait).await {
        return response;
    }
    let db_path = shared.db_path.clone();
    let keystore = runtime.keystore().cloned();
    let task = tokio::task::spawn_blocking(move || {
        read_api::get_one(&db_path, &entity_def, &key, keystore.as_ref())
    });
    match task.await {
        Ok(Ok((Some(item), position))) => {
            json_response(200, json!({ "item": item, "position": position }))
        }
        Ok(Ok((None, _))) => json_response(404, read_error("not_found", "no such row")),
        Ok(Err(err)) => read_failed(err),
        Err(err) => task_panicked(err),
    }
}

/// `GET /read/{projector}/{entity}?<field>=<value>&limit=&cursor=`: an ordered,
/// cursor-paginated scan. A filter on anything but the key or a declared index is
/// a 400, never a table scan. An optional `?after=<pos>` first waits for the
/// projector to reach that position (503 if it cannot within `timeout_ms`, default
/// 5s), for read-your-writes.
async fn read_scan(
    State(runtime): State<Shared>,
    Path((projector, entity)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (shared, entity_def) = match resolve_entity(&runtime, &projector, &entity) {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    // Parse the wait up front (cheap), but honor it only after the scan params are
    // validated below, so a malformed scan fails fast instead of blocking first.
    let wait = match parse_wait(&params) {
        Ok(wait) => wait,
        Err(response) => return *response,
    };

    let mut filters: Vec<(String, String)> = Vec::new();
    let mut limit = read_api::DEFAULT_LIMIT;
    let mut cursor: Option<String> = None;
    for (name, value) in params {
        match name.as_str() {
            "limit" => match value.parse::<usize>() {
                Ok(n) => limit = n.clamp(1, read_api::MAX_LIMIT),
                Err(_) => {
                    return json_response(
                        400,
                        read_error("invalid_input", "limit must be a positive integer"),
                    );
                }
            },
            "cursor" => cursor = Some(value),
            // `after`/`timeout_ms` are consumed by parse_wait, so never a filter.
            _ if read_api::RESERVED_QUERY_PARAMS.contains(&name.as_str()) => {}
            _ => filters.push((name, value)),
        }
    }
    if filters.len() > 1 {
        return json_response(
            400,
            read_error(
                "unindexed_filter",
                "only a single indexed filter field is supported",
            ),
        );
    }
    let filter = filters.into_iter().next();
    if let Some((field, value)) = &filter {
        if !read_api::is_filterable(&entity_def, field) {
            return json_response(
                400,
                read_error(
                    "unindexed_filter",
                    &format!("filter field `{field}` is not indexed; declare an index on it"),
                ),
            );
        }
        if let Err(err) = read_api::check_filter(&entity_def, field, value) {
            return json_response(
                400,
                read_error("invalid_input", &format!("filter `{field}`: {err}")),
            );
        }
    }
    let after_key = match &cursor {
        Some(raw) => match read_api::decode_cursor(raw) {
            Ok(key) => Some(key),
            Err(_) => {
                return json_response(400, read_error("invalid_input", "cursor is not valid"));
            }
        },
        None => None,
    };

    if let Some(response) = honor_wait(&shared, &projector, wait).await {
        return response;
    }
    let db_path = shared.db_path.clone();
    let keystore = runtime.keystore().cloned();
    let task = tokio::task::spawn_blocking(move || {
        let filter = filter
            .as_ref()
            .map(|(field, value)| (field.as_str(), value.as_str()));
        read_api::scan(
            &db_path,
            &entity_def,
            filter,
            after_key.as_deref(),
            limit,
            keystore.as_ref(),
        )
    });
    match task.await {
        Ok(Ok(page)) => json_response(
            200,
            json!({
                "items": page.items,
                "next_cursor": page.next_cursor,
                "position": page.position,
            }),
        ),
        Ok(Err(err)) => read_failed(err),
        Err(err) => task_panicked(err),
    }
}

/// `POST /projectors/{name}/replay`: schedule a rebuild-and-swap. Returns 202; the
/// projector picks it up between batches and callers watch `/status` for lag. A
/// projector whose thread has stopped gets a 503 instead: the request is only a flag,
/// and nothing is left to act on it.
async fn replay(State(runtime): State<Shared>, Path(name): Path<String>) -> Response {
    let Some(shared) = runtime.projector(&name) else {
        return json_response(
            404,
            read_error("not_found", &format!("no projector `{name}`")),
        );
    };
    if !shared.running() {
        return json_response(
            503,
            read_error(
                "not_running",
                &format!(
                    "projector `{name}` has stopped; see its `last_error` in /status, then restart the server"
                ),
            ),
        );
    }
    shared.request_replay();
    json_response(
        202,
        json!({ "status": "replay_scheduled", "projector": name }),
    )
}

/// `POST /effects/{name}/skip/{position}`: an explicit, manual operator action to
/// advance a wedged effect past a genuinely unprocessable event. Never automatic.
/// Returns 202; the driver marks the position terminal at its next backoff check.
async fn skip(
    State(runtime): State<Shared>,
    Path((name, position)): Path<(String, u64)>,
) -> Response {
    match runtime.effect(&name) {
        Some(effect) => {
            effect.request_skip(position);
            json_response(
                202,
                json!({ "status": "skip_scheduled", "effect": name, "position": position }),
            )
        }
        None => json_response(404, read_error("not_found", &format!("no effect `{name}`"))),
    }
}

/// Resolve a projector and one of its entities to the projector's shared handle and
/// the entity definition. The error variant (boxed, since it is the rare variant of
/// a hot path's result) is a 404 for an unknown projector or entity, or a 503 when
/// the projector's read model cannot be served at the current definition. Handlers
/// reuse the handle for both the read-your-writes wait and the read model's path,
/// avoiding a second lookup.
fn resolve_entity(
    runtime: &Runtime,
    projector: &str,
    entity: &str,
) -> Result<(Arc<ProjectorShared>, EntityDef), Box<Response>> {
    let Some(shared) = runtime.projector(projector) else {
        return Err(Box::new(json_response(
            404,
            read_error("not_found", &format!("no projector `{projector}`")),
        )));
    };
    let Some(entity_def) = read_api::find_entity(&shared.entities, entity) else {
        return Err(Box::new(json_response(
            404,
            read_error(
                "not_found",
                &format!("no entity `{entity}` in projector `{projector}`"),
            ),
        )));
    };
    // The entity definition above is the current one, but the database may still be
    // the shape a previous definition built. Querying across that mismatch fails on a
    // missing column, so say so plainly instead of leaking a SQLite error as a 500.
    if let Some(response) = not_servable(projector, shared.readiness()) {
        return Err(Box::new(response));
    }
    Ok((Arc::clone(shared), entity_def.clone()))
}

/// A 503 while a projector's read model does not match the definition the read API
/// would serve it at. `rebuilding` resolves on its own, so it carries a `Retry-After`;
/// `stale` needs an operator, so it does not.
fn not_servable(projector: &str, readiness: Readiness) -> Option<Response> {
    let (code, message, retry) = match readiness {
        Readiness::Ready => return None,
        Readiness::Rebuilding => (
            "rebuilding",
            format!("projector `{projector}` is rebuilding its read model"),
            true,
        ),
        Readiness::Stale => (
            "stale",
            format!(
                "projector `{projector}` was built from a different definition and auto-rebuild is off; POST /projectors/{projector}/replay to rebuild it"
            ),
            false,
        ),
        Readiness::Failed => (
            "rebuild_failed",
            format!(
                "projector `{projector}` could not rebuild its read model; see its `last_error` in /status, then POST /projectors/{projector}/replay to retry"
            ),
            false,
        ),
        // Served as unavailable rather than stale-but-readable. A quarantined
        // projector failed an invariant check, so its rows and its position are
        // exactly what cannot be vouched for, and a read-your-writes wait against a
        // position that moved backwards would resolve on a lie.
        Readiness::Quarantined => (
            "quarantined",
            format!(
                "projector `{projector}` failed an invariant check and stopped advancing; see its `last_error` in /status"
            ),
            false,
        ),
    };
    let mut response = json_response(503, read_error(code, &message));
    if retry {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    Some(response)
}

fn read_error(code: &str, message: &str) -> Value {
    json!({ "error": { "code": code, "message": message } })
}

fn read_failed(err: anyhow::Error) -> Response {
    tracing::error!("read failed: {err:#}");
    json_response(500, read_error("internal", "internal error"))
}

fn task_panicked(err: tokio::task::JoinError) -> Response {
    tracing::error!("read task panicked: {err}");
    json_response(500, read_error("internal", "internal error"))
}

/// How often the read-your-writes wait re-checks the projector's position.
const READ_WAIT_TICK: Duration = Duration::from_millis(10);
/// The wait budget when a read passes `after` without a `timeout_ms`.
pub(crate) const READ_WAIT_DEFAULT: Duration = Duration::from_millis(5_000);
/// The ceiling on a client-supplied `timeout_ms`, so one read cannot pin a request
/// for longer than this.
pub(crate) const READ_WAIT_MAX: Duration = Duration::from_millis(30_000);

/// A read-your-writes wait parsed off the query string: block until the projector
/// reaches `after`, giving up after `timeout`.
struct Wait {
    after: u64,
    timeout: Duration,
}

/// Parse the optional `after` / `timeout_ms` read-your-writes params. No `after`
/// means no wait (`Ok(None)`); a malformed value is a 400 (the `Err` response,
/// boxed since it is the rare variant of a hot path's result).
fn parse_wait(params: &HashMap<String, String>) -> Result<Option<Wait>, Box<Response>> {
    let Some(raw) = params.get("after") else {
        return Ok(None);
    };
    let after = raw.parse::<u64>().map_err(|_| {
        Box::new(json_response(
            400,
            read_error("invalid_after", "after must be a non-negative integer"),
        ))
    })?;
    let timeout = match params.get("timeout_ms") {
        Some(raw) => {
            let ms = raw.parse::<u64>().map_err(|_| {
                Box::new(json_response(
                    400,
                    read_error("invalid_input", "timeout_ms must be a non-negative integer"),
                ))
            })?;
            // 0 means "check once, do not wait"; the ceiling bounds a held request.
            Duration::from_millis(ms.min(READ_WAIT_MAX.as_millis() as u64))
        }
        None => READ_WAIT_DEFAULT,
    };
    Ok(Some(Wait { after, timeout }))
}

/// Honor a parsed read-your-writes wait against an already-resolved projector.
/// Returns `None` to proceed with the read, or `Some(503)` when the projector did
/// not catch up in time. No wait (`None`) proceeds immediately.
async fn honor_wait(
    shared: &ProjectorShared,
    projector: &str,
    wait: Option<Wait>,
) -> Option<Response> {
    let wait = wait?;
    if await_position(shared, wait.after, wait.timeout).await {
        None
    } else {
        Some(not_caught_up(projector, wait.after, wait.timeout))
    }
}

/// Wait until the projector's committed position reaches `after`, or `timeout`
/// elapses; returns whether it was reached. The projector publishes its in-memory
/// position only after committing its batch and checkpoint, so a satisfied wait
/// means a fresh snapshot read sees the data.
async fn await_position(shared: &ProjectorShared, after: u64, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, async {
        while shared.position() < after {
            tokio::time::sleep(READ_WAIT_TICK).await;
        }
    })
    .await
    .is_ok()
}

/// A 503 for a read-your-writes wait that timed out, with a `Retry-After` so a
/// client backs off rather than hammering a lagging projector.
fn not_caught_up(projector: &str, after: u64, timeout: Duration) -> Response {
    let body = read_error(
        "not_caught_up",
        &format!(
            "projector `{projector}` did not reach position {after} within {}ms",
            timeout.as_millis()
        ),
    );
    let mut response = json_response(503, body);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

/// The request's correlation id: the `x-correlation-id` header when it is a valid
/// uuid, otherwise a fresh one.
fn correlation_id(headers: &HeaderMap) -> Uuid {
    header_string(headers, "x-correlation-id")
        .and_then(|raw| Uuid::parse_str(&raw).ok())
        .unwrap_or_else(Uuid::new_v4)
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Parse the request body as a JSON object. An empty body is an empty object, so
/// a command with no fields needs no payload.
fn parse_body(body: &Bytes) -> Result<Value, String> {
    if body.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|err| format!("body is not valid JSON: {err}"))?;
    if !value.is_object() {
        return Err("body must be a JSON object".to_owned());
    }
    Ok(value)
}

fn json_response(status: u16, body: Value) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, Json(body)).into_response()
}

async fn shutdown_signal() {
    on_ctrl_c(signal::ctrl_c().await).await;
}

/// Report the ctrl-c handler's outcome to `with_graceful_shutdown`. A handler
/// that could not be installed never resolves: returning would drain a
/// freshly started server as if a signal had arrived, so the process has to be
/// killed externally instead.
async fn on_ctrl_c(installed: io::Result<()>) {
    if installed.is_err() {
        tracing::error!("failed to install the ctrl-c handler; shutdown must be forced");
        future::pending::<()>().await;
    }
    tracing::info!("shutdown signal received");
}

// --- introspection ---------------------------------------------------------
//
// Every route below is a `GET` and none of them writes. They share one shape: parse
// and validate the query string (cheap, on the async thread), then do the reading on
// a blocking thread, because both the event log and SQLite run on the caller's.

/// A query parameter that may repeat, collected in the order it was given.
fn multi(params: &[(String, String)], key: &str) -> Vec<String> {
    params
        .iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
        .collect()
}

/// The last value for a parameter, or `None`. Last rather than first so a hand-edited
/// URL behaves the way a browser address bar does.
fn single<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Boxed for the same reason [`resolve_entity`]'s error is: it is the rare variant of
/// a `Result` the happy path returns everywhere, and a `Response` is large.
fn bad_request(message: &str) -> Box<Response> {
    Box::new(json_response(400, read_error("invalid_input", message)))
}

fn not_found(message: &str) -> Response {
    json_response(404, read_error("not_found", message))
}

/// The page size, clamped rather than rejected, as everywhere else in the API.
fn parse_limit(params: &[(String, String)]) -> Result<usize, Box<Response>> {
    match single(params, "limit") {
        None => Ok(introspect::DEFAULT_LIMIT),
        Some(raw) => raw
            .parse::<usize>()
            .map(|limit| limit.clamp(1, introspect::MAX_LIMIT))
            .map_err(|_| bad_request("limit must be a non-negative integer")),
    }
}

fn parse_u64(params: &[(String, String)], key: &str) -> Result<Option<u64>, Box<Response>> {
    match single(params, key) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<u64>()
            .map(Some)
            .map_err(|_| bad_request(&format!("{key} must be a non-negative integer"))),
    }
}

fn parse_flag(
    params: &[(String, String)],
    key: &str,
    default: bool,
) -> Result<bool, Box<Response>> {
    match single(params, key) {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(_) => Err(bad_request(&format!("{key} must be `true` or `false`"))),
    }
}

fn parse_direction(params: &[(String, String)]) -> Result<introspect::Direction, Box<Response>> {
    match single(params, "direction") {
        None => Ok(introspect::Direction::Back),
        Some(raw) => introspect::Direction::parse(raw)
            .ok_or_else(|| bad_request("direction must be `back` or `forward`")),
    }
}

/// Run a blocking read and render its result, mapping both failure modes the same way
/// every other read endpoint does.
async fn blocking_json<F>(work: F) -> Response
where
    F: FnOnce() -> anyhow::Result<Value> + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(body)) => json_response(200, body),
        Ok(Err(err)) => read_failed(err),
        Err(err) => task_panicked(err),
    }
}

/// `GET /admin/assets/{file}`: one file of the bundled console.
async fn admin_asset(headers: HeaderMap, Path(file): Path<String>) -> Response {
    // Resolution is over the compiled-in table and nothing else, so a name carrying
    // `..` simply does not match an entry. The development override then substitutes
    // the content of a name that already resolved, never a path from the request.
    let Some(asset) = ui::asset(&file) else {
        // The same envelope every other 404 here answers with, and what the generated
        // document promises for this one: a client that deserializes the documented
        // error body must not be handed an empty response instead.
        return not_found(&format!("no console asset named `{file}`"));
    };
    // Only the development override reads from disk. With none configured this is a
    // table lookup and a refcount bump, so a pool hop per asset would cost more than
    // the work it moves off the runtime, ~25 times per page load.
    if ui::override_dir().is_none() {
        return ui::serve(asset, &headers);
    }
    match tokio::task::spawn_blocking(move || ui::serve(asset, &headers)).await {
        Ok(response) => response,
        Err(err) => {
            tracing::error!("asset task panicked: {err}");
            ui::empty(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /admin`: what is under this prefix.
///
/// The startup line points here, so it has to be a useful landing page rather than a
/// 404. Static: it describes the shape of the surface, not the state of the process.
async fn admin_index() -> Json<Value> {
    Json(json!({
        // A browser asking this same URL for `text/html` gets the console instead, so
        // point a reader who found the JSON at the other representation.
        "console": ADMIN_ROUTE,
        "endpoints": [
            { "path": ADMIN_EVENTS_ROUTE, "description": "page the event log, newest first" },
            { "path": ADMIN_EVENT_ROUTE, "description": "one event, with its payload and subject states" },
            { "path": ADMIN_TRACE_ROUTE, "description": "every event of one correlated flow" },
            { "path": ADMIN_EFFECTS_ROUTE, "description": "every effect and its durable state" },
            { "path": ADMIN_EFFECT_ROUTE, "description": "one effect" },
            { "path": ADMIN_INVOCATIONS_ROUTE, "description": "an effect's invocations, newest first" },
            { "path": ADMIN_INVOCATION_ROUTE, "description": "one invocation and every call it journaled" },
            { "path": ADMIN_PROJECTORS_ROUTE, "description": "every projector and its readiness" },
            { "path": ADMIN_PROJECTOR_ROUTE, "description": "one projector, its entities and their shapes" },
            { "path": ADMIN_SCHEMA_ROUTE, "description": "the loaded project: events, commands, projectors, effects" },
            { "path": ADMIN_SYSTEM_ROUTE, "description": "version, uptime, configuration and storage" },
            { "path": ADMIN_SUBJECTS_ROUTE, "description": "the subject-key inventory" },
            { "path": ADMIN_SUBJECT_ROUTE, "description": "whether one subject still has a key" },
        ]
    }))
}

/// `GET /admin/events`: a page of the log.
///
/// `type` and `tag` may each repeat: types OR together and tags AND together, which is
/// exactly a tephra query item, so nothing is reinterpreted on the way through.
async fn admin_events(
    State(runtime): State<Shared>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let limit = match parse_limit(&params) {
        Ok(limit) => limit,
        Err(response) => return *response,
    };
    let direction = match parse_direction(&params) {
        Ok(direction) => direction,
        Err(response) => return *response,
    };
    let cursor = match parse_u64(&params, "cursor") {
        Ok(cursor) => cursor,
        Err(response) => return *response,
    };
    let decrypt = match parse_flag(&params, "decrypt", true) {
        Ok(decrypt) => decrypt,
        Err(response) => return *response,
    };
    let query = match introspect::build_query(&multi(&params, "type"), &multi(&params, "tag")) {
        Ok(query) => query,
        Err(err) => return *bad_request(&err.to_string()),
    };
    blocking_json(move || {
        // One over the page, so "there is more" is a fact rather than an inference
        // from a full page, the same trick the read-model scan uses.
        let mut events = introspect::page(runtime.store(), &query, direction, cursor, limit + 1)?;
        let next_cursor = (events.len() > limit).then(|| {
            events.truncate(limit);
            events.last().map(|(position, _)| *position)
        });
        let renderer = introspect::Renderer::new(runtime.events_map(), runtime.keystore(), decrypt);
        let rendered = events
            .iter()
            .map(|(position, event)| renderer.event(*position, event))
            .collect::<anyhow::Result<Vec<_>>>()?;
        renderer.audit("GET /admin/events");
        Ok(json!({
            "events": rendered,
            "next_cursor": next_cursor.flatten(),
            "log_head": runtime.log_head(),
        }))
    })
    .await
}

/// `GET /admin/events/{position}`: one event in full.
async fn admin_event(
    State(runtime): State<Shared>,
    Path(position): Path<u64>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let decrypt = match parse_flag(&params, "decrypt", true) {
        Ok(decrypt) => decrypt,
        Err(response) => return *response,
    };
    let found = tokio::task::spawn_blocking({
        let runtime = Arc::clone(&runtime);
        move || {
            let Some(event) = introspect::read_at(runtime.store(), position)? else {
                return Ok(None);
            };
            let renderer =
                introspect::Renderer::new(runtime.events_map(), runtime.keystore(), decrypt);
            let rendered = renderer.event(position, &event)?;
            renderer.audit(&format!("GET /admin/events/{position}"));
            anyhow::Ok(Some(rendered))
        }
    })
    .await;
    match found {
        Ok(Ok(Some(event))) => json_response(200, event),
        Ok(Ok(None)) => not_found(&format!("no event at position {position}")),
        Ok(Err(err)) => read_failed(err),
        Err(err) => task_panicked(err),
    }
}

/// `GET /admin/traces/{correlation_id}`: every event of one correlated flow.
///
/// An indexed tag probe, not a scan: every event carries its flow's correlation tag
/// (`crate::dispatch::correlation_tag`). Events appended before that tag existed carry
/// no correlation tag and so cannot appear here, which is a gap in history rather than
/// in the query.
async fn admin_trace(
    State(runtime): State<Shared>,
    Path(correlation_id): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let limit = match parse_limit(&params) {
        Ok(limit) => limit,
        Err(response) => return *response,
    };
    let decrypt = match parse_flag(&params, "decrypt", true) {
        Ok(decrypt) => decrypt,
        Err(response) => return *response,
    };
    let cursor = match parse_u64(&params, "cursor") {
        Ok(cursor) => cursor,
        Err(response) => return *response,
    };
    if Uuid::parse_str(&correlation_id).is_err() {
        return *bad_request("correlation_id must be a uuid");
    }
    let query = match introspect::correlation_query(&correlation_id) {
        Ok(query) => query,
        Err(err) => return *bad_request(&err.to_string()),
    };
    blocking_json(move || {
        let mut events = introspect::page(
            runtime.store(),
            &query,
            introspect::Direction::Forward,
            cursor,
            limit + 1,
        )?;
        let next_cursor = (events.len() > limit).then(|| {
            events.truncate(limit);
            events.last().map(|(position, _)| *position)
        });
        let renderer = introspect::Renderer::new(runtime.events_map(), runtime.keystore(), decrypt);
        let rendered = events
            .iter()
            .map(|(position, event)| renderer.event(*position, event))
            .collect::<anyhow::Result<Vec<_>>>()?;
        renderer.audit(&format!("GET /admin/traces/{correlation_id}"));
        // Which effects ran on the positions in this page. The envelope records that
        // an effect produced an event but not which one; the journal is keyed by
        // `(effect, position)`, so joining it here answers that exactly instead of
        // leaving every client to guess from the subscription lists.
        //
        // The names come from the running project, so an invocation by an effect since
        // renamed or deleted is absent and reads as though nothing ran. Listing every
        // name the journal holds instead would answer for effects this binary knows
        // nothing about, which is worse: a trace is read to understand the code in
        // front of you. The document says which absence is which.
        let positions: Vec<u64> = events.iter().map(|(position, _)| *position).collect();
        let handles = runtime.effect_handles();
        let effects: Vec<&str> = handles.iter().map(|shared| shared.name.as_str()).collect();
        let invocations: Vec<Value> = runtime
            .invocations_at(&effects, &positions)?
            .iter()
            .map(introspect::invocation_at)
            .collect();
        let next_cursor = next_cursor.flatten();
        Ok(json!({
            "correlation_id": correlation_id,
            "events": rendered,
            "invocations": invocations,
            // A causal chain read partially is worse than one read whole, so say
            // outright when the page cut it off rather than letting the count imply it.
            "complete": next_cursor.is_none(),
            "next_cursor": next_cursor,
        }))
    })
    .await
}

/// `GET /admin/effects`: every effect, with what the operational database knows.
///
/// One read of the durable state for the whole listing, rather than one per effect:
/// the operational database is behind the mutex every journaled call contends for, so
/// lock traffic proportional to the module count is worth avoiding.
async fn admin_effects(State(runtime): State<Shared>) -> Response {
    blocking_json(move || {
        let head = runtime.log_head();
        let states = runtime.effect_states()?;
        let effects: Vec<Value> = runtime
            .effect_handles()
            .into_iter()
            .map(|shared| introspect::effect_detail(shared, head, states.get(&shared.name)))
            .collect();
        Ok(json!({ "effects": effects, "log_head": head }))
    })
    .await
}

/// `GET /admin/effects/{name}`.
async fn admin_effect(State(runtime): State<Shared>, Path(name): Path<String>) -> Response {
    if runtime.effect(&name).is_none() {
        return not_found(&format!("no effect `{name}`"));
    }
    blocking_json(move || {
        let head = runtime.log_head();
        let states = runtime.effect_states()?;
        let shared = runtime
            .effect(&name)
            .context("the effect disappeared between lookup and read")?;
        Ok(introspect::effect_detail(
            shared,
            head,
            states.get(&shared.name),
        ))
    })
    .await
}

/// `GET /admin/effects/{name}/invocations`: newest first.
async fn admin_invocations(
    State(runtime): State<Shared>,
    Path(name): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    if runtime.effect(&name).is_none() {
        return not_found(&format!("no effect `{name}`"));
    }
    let limit = match parse_limit(&params) {
        Ok(limit) => limit,
        Err(response) => return *response,
    };
    let cursor = match parse_u64(&params, "cursor") {
        Ok(cursor) => cursor,
        Err(response) => return *response,
    };
    blocking_json(move || {
        let before = cursor.unwrap_or(u64::MAX);
        let mut rows = runtime.invocations(&name, before, limit + 1)?;
        let next_cursor = (rows.len() > limit).then(|| {
            rows.truncate(limit);
            rows.last().map(|row| row.position)
        });
        Ok(json!({
            "effect": name,
            "invocations": rows.iter().map(introspect::invocation).collect::<Vec<_>>(),
            "next_cursor": next_cursor.flatten(),
        }))
    })
    .await
}

/// `GET /admin/effects/{name}/invocations/{position}`: what the invocation did.
///
/// The journal records each call's result but never its arguments, which are only
/// hashed. So this answers "what came back" and "how far did it get", not "what was
/// sent". Storing the arguments would let plaintext that came out of `reveal()` outlive
/// the erasure of the subject it belonged to.
///
/// The call list pages, because the endpoint's whole use is "the first call missing is
/// where it is stuck": a page boundary that looked like the end of the sequence would
/// point an operator at the wrong call.
async fn admin_invocation(
    State(runtime): State<Shared>,
    Path((name, position)): Path<(String, u64)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    if runtime.effect(&name).is_none() {
        return not_found(&format!("no effect `{name}`"));
    }
    let limit = match parse_limit(&params) {
        Ok(limit) => limit,
        Err(response) => return *response,
    };
    let after_seq = match parse_u64(&params, "cursor") {
        Ok(cursor) => cursor,
        Err(response) => return *response,
    };
    let found = tokio::task::spawn_blocking(move || {
        let Some(row) = runtime.invocation(&name, position)? else {
            return anyhow::Ok(None);
        };
        let skip = after_seq.map_or(0, |seq| seq.saturating_add(1));
        let mut calls = runtime.journal_entries(&name, position, skip, limit + 1)?;
        let next_cursor = (calls.len() > limit).then(|| {
            calls.truncate(limit);
            skip + calls.len() as u64 - 1
        });
        Ok(Some(introspect::invocation_detail(
            &row,
            &calls,
            skip,
            next_cursor,
        )))
    })
    .await;
    match found {
        Ok(Ok(Some(body))) => json_response(200, body),
        Ok(Ok(None)) => not_found(&format!("no invocation at position {position}")),
        Ok(Err(err)) => read_failed(err),
        Err(err) => task_panicked(err),
    }
}

/// `GET /admin/projectors`: every projector, without touching its database.
async fn admin_projectors(State(runtime): State<Shared>) -> Response {
    let head = runtime.log_head();
    let projectors: Vec<Value> = runtime
        .projector_handles()
        .into_iter()
        .map(|shared| introspect::projector_detail(shared, head, None, None))
        .collect();
    json_response(200, json!({ "projectors": projectors, "log_head": head }))
}

/// `GET /admin/projectors/{name}`: one projector, with the definition hash its read
/// model was built under and, on request, its row counts.
///
/// `?counts=true` is opt-in because a count is a full table scan per entity, and it
/// requires a `Ready` projector: a model still at a previous definition's shape would
/// error on a table this one's entities no longer name.
async fn admin_projector(
    State(runtime): State<Shared>,
    Path(name): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let Some(shared) = runtime.projector(&name) else {
        return not_found(&format!("no projector `{name}`"));
    };
    let counts = match parse_flag(&params, "counts", false) {
        Ok(counts) => counts,
        Err(response) => return *response,
    };
    if counts && let Some(response) = not_servable(&name, shared.readiness()) {
        return response;
    }
    let shared = Arc::clone(shared);
    let head = runtime.log_head();
    blocking_json(move || {
        let model = read_api::open_with_retry(&shared.db_path)?;
        let snapshot = model.begin()?;
        let definition_hash = model.read_definition()?;
        let counts = counts
            .then(|| {
                shared
                    .entities
                    .iter()
                    .map(|entity| model.count(entity))
                    .collect::<anyhow::Result<Vec<u64>>>()
            })
            .transpose()?;
        drop(snapshot);
        Ok(introspect::projector_detail(
            &shared,
            head,
            definition_hash,
            counts.as_deref(),
        ))
    })
    .await
}

/// `GET /admin/schema`: what this process actually loaded.
///
/// Internal commands appear here, unlike in the generated OpenAPI document. They are
/// not routed, which is why the document omits them, but they exist and an operator
/// tracing an effect's `invoke_command` needs to see them.
async fn admin_schema(State(runtime): State<Shared>) -> Response {
    blocking_json(move || {
        let modules = runtime.module_metadata()?;
        let mut events: Vec<&EventDef> = runtime.events_map().values().collect();
        events.sort_by(|a, b| a.event_type.cmp(&b.event_type));
        let commands: Vec<Value> = runtime
            .command_units()
            .into_iter()
            .map(|unit| {
                // No fallback arm: the command map holds nothing else, and an arm
                // reporting an empty input would render a command as taking no fields
                // rather than surfacing the mismatch.
                let ModuleDef::Command { input, .. } = &unit.def else {
                    anyhow::bail!(
                        "`{}` is in the command map but is not a command",
                        unit.def.name()
                    );
                };
                let input: Vec<Value> = input
                    .fields
                    .iter()
                    .map(|(name, kind)| json!({ "name": name, "kind": kind.describe() }))
                    .collect();
                Ok(json!({
                    "name": unit.def.name(),
                    "internal": unit.internal,
                    "path": unit.rel_path,
                    "source_hash": unit.source_hash,
                    "input": input,
                }))
            })
            .collect::<anyhow::Result<Vec<Value>>>()?;
        let projectors: Vec<Value> = runtime
            .projector_handles()
            .into_iter()
            .map(|shared| {
                json!({
                    "name": shared.name,
                    "sources": shared.sources,
                    "entities": shared.entities.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
                })
            })
            .collect();
        let effects: Vec<Value> = runtime
            .effect_handles()
            .into_iter()
            .map(|shared| {
                json!({
                    "name": shared.name,
                    "sources": shared.sources,
                })
            })
            .collect();
        Ok(json!({
            "events": events.into_iter().map(introspect::event_def).collect::<Vec<_>>(),
            "commands": commands,
            "projectors": projectors,
            "effects": effects,
            "modules": modules.iter().map(introspect::module).collect::<Vec<_>>(),
        }))
    })
    .await
}

/// `GET /admin/system`: the process, its storage and its effective configuration.
async fn admin_system(State(runtime): State<Shared>) -> Response {
    blocking_json(move || {
        let config = runtime.config();
        let keystore = runtime.keystore().is_some();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": runtime.uptime_seconds(),
            "log_head": runtime.log_head(),
            "data_dir": runtime.data_dir().display().to_string(),
            "opdb_schema_version": runtime.opdb_schema_version()?,
            "verify": runtime.verify(),
            "keystore": {
                "configured": keystore,
                // Which masters stored key material is wrapped under. More than one
                // means a rotation has started and not finished.
                "master_key_ids": if keystore { runtime.master_key_ids()? } else { Vec::new() },
            },
            "config": {
                "effects": { "pool_size": config.effects.pool_size },
                "retention": { "effect_journal_days": config.retention.effect_journal_days },
                "projectors": { "auto_rebuild": config.projectors.auto_rebuild },
                "verify": { "enabled": config.verify.enabled },
            },
        }))
    })
    .await
}

/// `GET /admin/subjects`: which subjects still hold key material.
///
/// Never the key material itself. A subject absent from this list has either been
/// erased or never had a value encrypted under it; erasure deletes the row, so the two
/// are the same state on disk and this cannot tell them apart.
async fn admin_subjects(
    State(runtime): State<Shared>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let limit = match parse_limit(&params) {
        Ok(limit) => limit,
        Err(response) => return *response,
    };
    let after_field = single(&params, "after_field").map(str::to_owned);
    let after_value = single(&params, "after_value").map(str::to_owned);
    if after_field.is_some() != after_value.is_some() {
        return *bad_request("after_field and after_value must be given together");
    }
    blocking_json(move || {
        let after = after_field.as_deref().zip(after_value.as_deref());
        let mut rows = runtime.subject_keys_page(after, limit + 1)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = more.then(|| rows.last()).flatten().map(
            |row| json!({ "after_field": row.subject_field, "after_value": row.subject_value }),
        );
        // The counts are an aggregate no limit can bound, so they are taken once for a
        // listing rather than rescanned for every page of one. They cannot change
        // between pages of the same walk anyway.
        let counts = match after {
            None => Some(
                runtime
                    .subject_key_counts()?
                    .into_iter()
                    .map(|(field, count)| json!({ "subject_field": field, "live_keys": count }))
                    .collect::<Vec<_>>(),
            ),
            Some(_) => None,
        };
        Ok(json!({
            "counts": counts,
            "subjects": rows.iter().map(introspect::subject).collect::<Vec<_>>(),
            "next": next,
        }))
    })
    .await
}

/// `GET /admin/subjects/{field}/{value}`: whether one subject still has a key.
async fn admin_subject(
    State(runtime): State<Shared>,
    Path((field, value)): Path<(String, String)>,
) -> Response {
    blocking_json(move || {
        let live = runtime.subject_key_exists(&field, &value)?;
        Ok(json!({
            "subject_field": field,
            "subject_value": value,
            // "absent" rather than "erased": a subject that never had a value
            // encrypted under it looks exactly the same from here.
            "state": if live { "live" } else { "absent" },
        }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_ctrl_c_handler_never_reports_a_shutdown() {
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            on_ctrl_c(Err(io::Error::other("handler not installed"))),
        )
        .await;
        assert!(
            outcome.is_err(),
            "a server whose signal handler failed must keep serving, not drain at once"
        );
    }

    #[tokio::test]
    async fn an_installed_handler_reports_the_signal() {
        let outcome = tokio::time::timeout(Duration::from_millis(50), on_ctrl_c(Ok(()))).await;
        assert!(outcome.is_ok(), "a real ctrl-c must resolve the wait");
    }
}
