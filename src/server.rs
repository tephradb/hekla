//! The HTTP surface: `POST /commands/{name}`, `GET /status`, `GET /health`, the
//! generated `GET /openapi.json`, and a Scalar reference UI over it at `GET /docs`.
//!
//! Handlers are thin. They pull the correlation id and idempotency key from
//! headers, mint a per-request [`CommandContext`], and run the (synchronous)
//! command cycle on a blocking thread. The [`Runtime`] owns the outcome-to-status
//! mapping, so the server only turns an [`ExecResult`] into a JSON response.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tephra::WriteCoordinator;
use tokio::net::TcpListener;
use tokio::signal;
use uuid::Uuid;

use crate::context::CommandContext;
use crate::effect::EffectRuntime;
use crate::projector::{ProjectorSet, ProjectorShared};
use crate::read_api;
use crate::runtime::{Runtime, error_body};
use crate::starlark_builtins::EntityDef;

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
    let app = router(runtime);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("kiln listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving http")?;
    tracing::info!("draining effects, then projectors, then shutting down the writer");
    effects.shutdown_and_join();
    projectors.shutdown_and_join();
    coordinator.shutdown();
    Ok(())
}

/// The HTTP application over an already-built runtime, for in-process testing
/// (drive it with `tower::ServiceExt::oneshot`).
pub fn app(runtime: Arc<Runtime>) -> Router {
    router(runtime)
}

fn router(shared: Shared) -> Router {
    Router::new()
        .route("/commands/{name}", post(execute))
        .route("/read/{projector}/{entity}/{key}", get(read_one))
        .route("/read/{projector}/{entity}", get(read_scan))
        .route("/projectors/{name}/replay", post(replay))
        .route("/effects/{name}/skip/{position}", post(skip))
        .route("/status", get(status))
        .route("/health", get(health))
        .route("/openapi.json", get(openapi_doc))
        .route("/docs", get(docs))
        .with_state(shared)
}

/// A Scalar API reference over the generated spec. The page loads Scalar from a
/// CDN and points it at `/openapi.json`, so it needs a network connection but no
/// bundled assets.
const SCALAR_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <title>kiln API reference</title>
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
    let mut response = runtime.openapi_json().to_owned().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
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

    // Every cheap param is validated: now honor the read-your-writes wait.
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
/// projector picks it up between batches and callers watch `/status` for lag.
async fn replay(State(runtime): State<Shared>, Path(name): Path<String>) -> Response {
    match runtime.projector(&name) {
        Some(shared) => {
            shared.request_replay();
            json_response(
                202,
                json!({ "status": "replay_scheduled", "projector": name }),
            )
        }
        None => json_response(
            404,
            read_error("not_found", &format!("no projector `{name}`")),
        ),
    }
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

/// Resolve a projector and one of its entities to the projector's shared handle
/// and the entity definition, or a 404 response (boxed, since it is the rare
/// variant of a hot path's result). Handlers reuse the handle for both the
/// read-your-writes wait and the read model's path, avoiding a second lookup.
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
    match read_api::find_entity(&shared.entities, entity) {
        Some(entity_def) => Ok((Arc::clone(shared), entity_def.clone())),
        None => Err(Box::new(json_response(
            404,
            read_error(
                "not_found",
                &format!("no entity `{entity}` in projector `{projector}`"),
            ),
        ))),
    }
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
const READ_WAIT_DEFAULT: Duration = Duration::from_millis(5_000);
/// The ceiling on a client-supplied `timeout_ms`, so one read cannot pin a request
/// for longer than this.
const READ_WAIT_MAX: Duration = Duration::from_millis(30_000);

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
    if signal::ctrl_c().await.is_err() {
        tracing::error!("failed to install the ctrl-c handler");
    }
    tracing::info!("shutdown signal received");
}
