//! The HTTP surface: `POST /commands/{name}`, `GET /status`, `GET /health`, the
//! generated `GET /openapi.json`, and a Scalar reference UI over it at `GET /docs`.
//!
//! Handlers are thin. They pull the correlation id and idempotency key from
//! headers, mint a per-request [`CommandContext`], and run the (synchronous)
//! command cycle on a blocking thread. The [`Runtime`] owns the outcome-to-status
//! mapping, so the server only turns an [`ExecResult`] into a JSON response.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
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
use crate::projector::ProjectorSet;
use crate::read_api;
use crate::runtime::Runtime;
use crate::{openapi, starlark_builtins::EntityDef};

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
    let idem_key = header_string(&headers, "idempotency-key");

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

async fn openapi_doc(State(runtime): State<Shared>) -> Json<Value> {
    Json(openapi::build(&runtime.public_commands()))
}

async fn docs() -> Html<&'static str> {
    Html(SCALAR_HTML)
}

/// `GET /read/{projector}/{entity}/{key}`: one row by key, with the projector's
/// log position. 404 for an unknown projector, entity, or missing row.
async fn read_one(
    State(runtime): State<Shared>,
    Path((projector, entity, key)): Path<(String, String, String)>,
) -> Response {
    let (db_path, entity_def) = match resolve_entity(&runtime, &projector, &entity) {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let task = tokio::task::spawn_blocking(move || read_api::get_one(&db_path, &entity_def, &key));
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
/// a 400, never a table scan.
async fn read_scan(
    State(runtime): State<Shared>,
    Path((projector, entity)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (db_path, entity_def) = match resolve_entity(&runtime, &projector, &entity) {
        Ok(resolved) => resolved,
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
    if let Some((field, _)) = &filter
        && !read_api::is_filterable(&entity_def, field)
    {
        return json_response(
            400,
            read_error(
                "unindexed_filter",
                &format!("filter field `{field}` is not indexed; declare an index on it"),
            ),
        );
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

    let task = tokio::task::spawn_blocking(move || {
        let filter = filter
            .as_ref()
            .map(|(field, value)| (field.as_str(), value.as_str()));
        read_api::scan(&db_path, &entity_def, filter, after_key.as_deref(), limit)
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

/// Resolve a projector and one of its entities to the read model's path and the
/// entity definition, or a 404 response (boxed, since it is the rare variant of a
/// hot path's result).
fn resolve_entity(
    runtime: &Runtime,
    projector: &str,
    entity: &str,
) -> Result<(PathBuf, EntityDef), Box<Response>> {
    let Some(shared) = runtime.projector(projector) else {
        return Err(Box::new(json_response(
            404,
            read_error("not_found", &format!("no projector `{projector}`")),
        )));
    };
    match read_api::find_entity(&shared.entities, entity) {
        Some(entity_def) => Ok((shared.db_path.clone(), entity_def.clone())),
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

fn error_body(ctx: &CommandContext, code: &str, message: &str) -> Value {
    json!({
        "correlation_id": ctx.correlation_id.to_string(),
        "causation_id": ctx.causation_id.to_string(),
        "error": { "code": code, "message": message },
    })
}

async fn shutdown_signal() {
    if signal::ctrl_c().await.is_err() {
        tracing::error!("failed to install the ctrl-c handler");
    }
    tracing::info!("shutdown signal received");
}
