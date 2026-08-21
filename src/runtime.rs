//! The command runtime: the live process behind `kiln serve`.
//!
//! A [`Runtime`] owns the tephra store handle, the operational DB, and the loaded
//! command modules, and turns an HTTP request into a decision cycle. It layers
//! three concerns on top of the pure dispatch in [`crate::dispatch`]: a pinned
//! `now()` for the request, built-in per-command idempotency, and optimistic-
//! concurrency retry.
//!
//! [`Runtime::execute`] produces the final `(status, body)` so idempotency can
//! store the exact response a replay must return. It is synchronous (Starlark and
//! tephra appends are), so the server calls it on a blocking thread.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};
use tephra::{
    PositionRange, SegmentConfig, SegmentSet, WriteCoordinator, WriteHandle, WriterConfig,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::context::CommandContext;
use crate::dispatch::{self, CommandOutcome};
use crate::loader::{CommandUnit, LoadedProject, ProjectorUnit};
use crate::opdb::{OpDb, Reserve};
use crate::projector::{self, ProjectorSet, ProjectorShared};
use crate::starlark_builtins::{EmittedEvent, InputSchema, ModuleDef};

/// Individual event segments before rolling to a new file. 256 MiB matches
/// tephra's own default sizing.
const SEGMENT_SIZE: usize = 256 * 1024 * 1024;

/// How many times a command re-runs its whole decision cycle on a DCB conflict
/// before the runtime gives up and returns a concurrency conflict.
const MAX_ATTEMPTS: u32 = 5;

/// The final HTTP outcome of a command execution: the status and the response
/// body. The body is what idempotency stores, so a replay returns it verbatim.
pub struct ExecResult {
    pub status: u16,
    pub body: Value,
}

/// The live runtime shared across request handlers.
pub struct Runtime {
    commands: HashMap<String, Arc<CommandUnit>>,
    store: WriteHandle,
    opdb: Arc<Mutex<OpDb>>,
    started: Instant,
    projectors: HashMap<String, Arc<ProjectorShared>>,
    effect_names: Vec<String>,
    event_count: usize,
}

impl Runtime {
    /// Open the store and operational DB under `data_dir`, start one thread per
    /// projector, and build the runtime from an already-loaded, error-free
    /// project. Returns the runtime, the write coordinator, and the projector set;
    /// the caller keeps the last two to drain and join on shutdown.
    pub fn open(
        project: LoadedProject,
        data_dir: &Path,
    ) -> anyhow::Result<(Runtime, WriteCoordinator, ProjectorSet)> {
        let events_dir = data_dir.join("events");
        fs::create_dir_all(&events_dir)
            .with_context(|| format!("creating {}", events_dir.display()))?;
        let set = SegmentSet::open(&events_dir, SegmentConfig::new(SEGMENT_SIZE))
            .with_context(|| format!("opening event store at {}", events_dir.display()))?;
        let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default())
            .context("starting the write coordinator")?;

        let opdb = OpDb::open(&data_dir.join("kiln.db"))?;
        let cleared = opdb.clear_pending()?;
        if cleared > 0 {
            tracing::warn!("cleared {cleared} stale idempotency reservation(s) from a prior run");
        }

        let mut commands = HashMap::new();
        for unit in project.commands {
            let name = unit.loaded.def.name().to_owned();
            commands.insert(name, Arc::new(unit));
        }

        let projectors_dir = data_dir.join("projectors");
        fs::create_dir_all(&projectors_dir)
            .with_context(|| format!("creating {}", projectors_dir.display()))?;
        let units: Vec<Arc<ProjectorUnit>> = project.projectors.into_iter().map(Arc::new).collect();
        let (shared, projector_set) = projector::start_all(units, &store, &projectors_dir)?;
        let projectors: HashMap<String, Arc<ProjectorShared>> = shared
            .into_iter()
            .map(|handle| (handle.name.clone(), handle))
            .collect();

        let mut effect_names: Vec<String> = project
            .effects
            .iter()
            .map(|unit| unit.loaded.def.name().to_owned())
            .collect();
        effect_names.sort();

        let runtime = Runtime {
            commands,
            store,
            opdb: Arc::new(Mutex::new(opdb)),
            started: Instant::now(),
            projectors,
            effect_names,
            event_count: project.events.by_type.len(),
        };
        Ok((runtime, coordinator, projector_set))
    }

    /// Execute a command by name. Resolves public commands only; applies
    /// idempotency when `idem_key` is set; retries on DCB conflict; and returns
    /// the status and body to send (and, for idempotent requests, to store).
    pub fn execute(
        &self,
        name: &str,
        body: Value,
        ctx: &CommandContext,
        idem_key: Option<&str>,
    ) -> anyhow::Result<ExecResult> {
        let Some(command) = self.commands.get(name).filter(|unit| !unit.internal) else {
            return Ok(ExecResult {
                status: 404,
                body: error_body(ctx, "not_found", &format!("no public command `{name}`")),
            });
        };
        let now = now_rfc3339();

        // Idempotency: reserve the key, replaying a completed outcome or refusing a
        // still-running duplicate. The DB lock is held only for the reservation.
        if let Some(key) = idem_key {
            match self.lock_opdb().reserve(name, key, &now)? {
                Reserve::Done { status, outcome } => {
                    let body = serde_json::from_str(&outcome).unwrap_or(Value::Null);
                    return Ok(ExecResult { status, body });
                }
                Reserve::Pending => {
                    return Ok(ExecResult {
                        status: 409,
                        body: error_body(
                            ctx,
                            "in_progress",
                            "a request with this idempotency key is still being processed",
                        ),
                    });
                }
                Reserve::Acquired => {}
            }
        }

        match self.run_with_retry(command, &body, ctx, &now) {
            Ok(result) => {
                if let Some(key) = idem_key {
                    let outcome = result.body.to_string();
                    self.lock_opdb()
                        .finalize(name, key, result.status, &outcome, &now)?;
                }
                Ok(result)
            }
            Err(err) => {
                // An internal error caches nothing; free the reservation so a retry
                // can proceed.
                if let Some(key) = idem_key {
                    let _ = self.lock_opdb().release(name, key);
                }
                Err(err)
            }
        }
    }

    /// Run the decision cycle, retrying the whole cycle on a DCB conflict so a
    /// re-read rebuilds the decision model. A small exponential backoff keeps a
    /// hot boundary from hammering the single writer.
    fn run_with_retry(
        &self,
        command: &CommandUnit,
        body: &Value,
        ctx: &CommandContext,
        now: &str,
    ) -> anyhow::Result<ExecResult> {
        for attempt in 0..MAX_ATTEMPTS {
            match dispatch::run_command(&self.store, &command.loaded, body, ctx, now)? {
                CommandOutcome::Conflict => {
                    if attempt + 1 < MAX_ATTEMPTS {
                        thread::sleep(Duration::from_millis(1u64 << attempt));
                    }
                }
                outcome => return Ok(outcome_to_result(outcome, ctx)),
            }
        }
        Ok(ExecResult {
            status: 409,
            body: error_body(
                ctx,
                "concurrency_conflict",
                "the command's consistency boundary kept changing during the request; retry",
            ),
        })
    }

    /// A JSON snapshot for `GET /status`. Reports the log head, the loaded-module
    /// inventory, and each projector's committed position and lag (head minus
    /// position). Effect lag lands with the effect runtime in a later phase.
    pub fn status(&self) -> Value {
        let (public, internal): (Vec<&str>, Vec<&str>) = self
            .commands
            .values()
            .map(|unit| (unit.loaded.def.name(), unit.internal))
            .fold((Vec::new(), Vec::new()), |mut acc, (name, internal)| {
                if internal {
                    acc.1.push(name);
                } else {
                    acc.0.push(name);
                }
                acc
            });
        let mut public = public;
        let mut internal = internal;
        public.sort();
        internal.sort();

        let head = self.store.head().get();
        let mut handles: Vec<&Arc<ProjectorShared>> = self.projectors.values().collect();
        handles.sort_by(|a, b| a.name.cmp(&b.name));
        let projectors: Vec<Value> = handles
            .iter()
            .map(|handle| {
                let position = handle.position();
                json!({
                    "name": handle.name,
                    "position": position,
                    "lag": head.saturating_sub(position),
                })
            })
            .collect();

        json!({
            "log_head": head,
            "uptime_seconds": self.started.elapsed().as_secs(),
            "commands": { "public": public, "internal": internal },
            "projectors": projectors,
            "effects": self.effect_names,
            "events": self.event_count,
        })
    }

    /// The running projector by name, for the read API and the replay endpoint.
    pub fn projector(&self, name: &str) -> Option<&Arc<ProjectorShared>> {
        self.projectors.get(name)
    }

    /// The public commands and their input schemas, for OpenAPI generation.
    pub fn public_commands(&self) -> Vec<(&str, &InputSchema)> {
        let mut out = Vec::new();
        for unit in self.commands.values() {
            if unit.internal {
                continue;
            }
            if let ModuleDef::Command { name, input } = &unit.loaded.def {
                out.push((name.as_str(), input));
            }
        }
        out.sort_by_key(|(name, _)| *name);
        out
    }

    fn lock_opdb(&self) -> MutexGuard<'_, OpDb> {
        self.opdb.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The current instant as an RFC 3339 string, the request's pinned `now()`.
fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Map a command outcome to its HTTP status and response body.
fn outcome_to_result(outcome: CommandOutcome, ctx: &CommandContext) -> ExecResult {
    match outcome {
        CommandOutcome::Committed { events, positions } => ExecResult {
            status: 200,
            body: success_body(ctx, positions, &events),
        },
        CommandOutcome::Rejected { code, message } => ExecResult {
            status: 422,
            body: error_body(ctx, &code, &message),
        },
        CommandOutcome::InvalidInput { message } => ExecResult {
            status: 400,
            body: error_body(ctx, "invalid_input", &message),
        },
        // Handled by the retry loop; reaching here means it exhausted retries.
        CommandOutcome::Conflict => ExecResult {
            status: 409,
            body: error_body(
                ctx,
                "concurrency_conflict",
                "the boundary kept changing; retry",
            ),
        },
    }
}

fn success_body(
    ctx: &CommandContext,
    positions: Option<PositionRange>,
    events: &[EmittedEvent],
) -> Value {
    let positions = match positions {
        Some(range) => json!({ "first": range.first.get(), "last": range.last.get() }),
        None => Value::Null,
    };
    let events: Vec<Value> = events
        .iter()
        .map(|event| json!({ "type": event.event_type, "tags": tag_strings(&event.tags) }))
        .collect();
    json!({
        "correlation_id": ctx.correlation_id.to_string(),
        "causation_id": ctx.causation_id.to_string(),
        "positions": positions,
        "events": events,
    })
}

fn error_body(ctx: &CommandContext, code: &str, message: &str) -> Value {
    json!({
        "correlation_id": ctx.correlation_id.to_string(),
        "causation_id": ctx.causation_id.to_string(),
        "error": { "code": code, "message": message },
    })
}

/// Render derived tags as `"key:value"` / `"key"` for the response.
fn tag_strings(tags: &[(String, Option<String>)]) -> Vec<String> {
    tags.iter()
        .map(|(key, value)| match value {
            Some(value) => format!("{key}:{value}"),
            None => key.clone(),
        })
        .collect()
}

/// Resolve the data directory: the flag if given, else `<project>/data`.
pub fn resolve_data_dir(project_dir: &Path, data_dir: Option<&Path>) -> PathBuf {
    match data_dir {
        Some(dir) => dir.to_path_buf(),
        None => project_dir.join("data"),
    }
}
