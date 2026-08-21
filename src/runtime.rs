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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};
use tephra::{
    PositionRange, SegmentConfig, SegmentSet, WriteCoordinator, WriteHandle, WriterConfig,
};
use time::Duration as TimeDuration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::config::Config;
use crate::context::CommandContext;
use crate::dispatch::{self, CommandOutcome};
use crate::effect::{self, EffectRuntime, EffectShared, HttpClient};
use crate::loader::{CommandUnit, EffectUnit, LoadedProject, ProjectorUnit};
use crate::opdb::{InvocationState, OpDb, Reserve};
use crate::projector::{self, ProjectorSet, ProjectorShared};
use crate::read_api;
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
    /// The effect handles, for `/status` and the skip endpoint. Set once, right
    /// after the effect threads spawn (they need `Arc<Runtime>` first).
    effects: OnceLock<Vec<Arc<EffectShared>>>,
    event_count: usize,
}

impl Runtime {
    /// Open the store and operational DB under `data_dir`, start one thread per
    /// projector and per effect (plus the retention sweeper), and build the runtime
    /// from an already-loaded, error-free project. `http` is the transport the
    /// journaled `http.*` builtins use (a [`UreqClient`](crate::effect::UreqClient)
    /// in production, a stub in tests). Returns the runtime, the write coordinator,
    /// the projector set, and the effect runtime; the caller keeps the last three
    /// to drain and join on shutdown.
    pub fn open(
        project: LoadedProject,
        data_dir: &Path,
        http: Arc<dyn HttpClient>,
    ) -> anyhow::Result<(Arc<Runtime>, WriteCoordinator, ProjectorSet, EffectRuntime)> {
        let events_dir = data_dir.join("events");
        fs::create_dir_all(&events_dir)
            .with_context(|| format!("creating {}", events_dir.display()))?;
        let set = SegmentSet::open(&events_dir, SegmentConfig::new(SEGMENT_SIZE))
            .with_context(|| format!("opening event store at {}", events_dir.display()))?;
        let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default())
            .context("starting the write coordinator")?;

        let opdb = OpDb::open(&data_dir.join("kiln.db"))?;
        // Clear stale reservations before effects start, so a replay of an
        // `invoke_command` never sees a `pending` row left by a crashed run.
        let cleared = opdb.clear_pending()?;
        if cleared > 0 {
            tracing::warn!("cleared {cleared} stale idempotency reservation(s) from a prior run");
        }
        let now = now_rfc3339();

        let mut commands = HashMap::new();
        for unit in project.commands {
            opdb.upsert_module_metadata(
                unit.loaded.def.name(),
                "command",
                &unit.loaded.source_hash,
                &now,
            )?;
            let name = unit.loaded.def.name().to_owned();
            commands.insert(name, Arc::new(unit));
        }

        let projectors_dir = data_dir.join("projectors");
        fs::create_dir_all(&projectors_dir)
            .with_context(|| format!("creating {}", projectors_dir.display()))?;
        let projector_units: Vec<Arc<ProjectorUnit>> =
            project.projectors.into_iter().map(Arc::new).collect();
        for unit in &projector_units {
            opdb.upsert_module_metadata(
                unit.loaded.def.name(),
                "projector",
                &unit.loaded.source_hash,
                &now,
            )?;
        }
        let (shared, projector_set) =
            projector::start_all(projector_units, &store, &projectors_dir)?;
        let projectors: HashMap<String, Arc<ProjectorShared>> = shared
            .into_iter()
            .map(|handle| (handle.name.clone(), handle))
            .collect();

        let effect_units: Vec<Arc<EffectUnit>> =
            project.effects.into_iter().map(Arc::new).collect();
        for unit in &effect_units {
            opdb.upsert_module_metadata(
                unit.loaded.def.name(),
                "effect",
                &unit.loaded.source_hash,
                &now,
            )?;
        }
        let config: Config = project.config;

        let runtime = Arc::new(Runtime {
            commands,
            store,
            opdb: Arc::new(Mutex::new(opdb)),
            started: Instant::now(),
            projectors,
            effects: OnceLock::new(),
            event_count: project.events.by_type.len(),
        });

        // Effects need `Arc<Runtime>` (for `invoke_command` and `read`), so they
        // spawn after the runtime is built; the runtime learns their handles for
        // `/status` here, once.
        let effect_runtime = effect::start_all(effect_units, &runtime, http, &config)?;
        let _ = runtime.effects.set(effect_runtime.shared_handles());

        Ok((runtime, coordinator, projector_set, effect_runtime))
    }

    /// Execute a command by name over the public surface. Resolves public commands
    /// only (internal commands are 404); applies idempotency when `idem_key` is set;
    /// retries on DCB conflict; and returns the status and body to send (and, for
    /// idempotent requests, to store).
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
        self.run_resolved(name, &Arc::clone(command), body, ctx, idem_key)
    }

    /// Execute a command invoked by an effect. Unlike [`execute`](Runtime::execute)
    /// this resolves **public or internal** commands, so an effect can complete
    /// work through an internal command that is off the HTTP surface. The
    /// idempotency key an effect passes is deterministic, so a replay returns the
    /// original outcome and the command lands exactly once.
    pub fn execute_from_effect(
        &self,
        name: &str,
        body: Value,
        ctx: &CommandContext,
        idem_key: Option<&str>,
    ) -> anyhow::Result<ExecResult> {
        let Some(command) = self.commands.get(name) else {
            return Ok(ExecResult {
                status: 404,
                body: error_body(ctx, "not_found", &format!("no command `{name}`")),
            });
        };
        self.run_resolved(name, &Arc::clone(command), body, ctx, idem_key)
    }

    /// The shared execution path once a command is resolved: idempotency reserve,
    /// the retrying decision cycle, then finalize or release.
    fn run_resolved(
        &self,
        name: &str,
        command: &CommandUnit,
        body: Value,
        ctx: &CommandContext,
        idem_key: Option<&str>,
    ) -> anyhow::Result<ExecResult> {
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
    /// inventory, each projector's committed position and lag, and each effect's
    /// position, lag, and health (its consecutive-failure count and last error,
    /// so a wedge reads as broken rather than merely lagging).
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

        let mut effect_handles: Vec<&Arc<EffectShared>> = self
            .effects
            .get()
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        effect_handles.sort_by(|a, b| a.name.cmp(&b.name));
        let effects: Vec<Value> = effect_handles
            .iter()
            .map(|handle| {
                let position = handle.position();
                json!({
                    "name": handle.name,
                    "position": position,
                    "lag": head.saturating_sub(position),
                    "consecutive_failures": handle.consecutive_failures(),
                    "last_error": handle.last_error(),
                })
            })
            .collect();

        json!({
            "log_head": head,
            "uptime_seconds": self.started.elapsed().as_secs(),
            "commands": { "public": public, "internal": internal },
            "projectors": projectors,
            "effects": effects,
            "events": self.event_count,
        })
    }

    /// The running projector by name, for the read API and the replay endpoint.
    pub fn projector(&self, name: &str) -> Option<&Arc<ProjectorShared>> {
        self.projectors.get(name)
    }

    /// The running effect by name, for the skip endpoint.
    pub fn effect(&self, name: &str) -> Option<&Arc<EffectShared>> {
        self.effects
            .get()
            .and_then(|handles| handles.iter().find(|handle| handle.name == name))
    }

    /// The store handle, for the effect drivers' subscriptions.
    pub(crate) fn store(&self) -> &WriteHandle {
        &self.store
    }

    // --- effect-runtime plumbing: each op is a short op-DB critical section, so
    // the mutex is never held across a builtin body or a side effect (which would
    // re-enter it through `execute_from_effect`). ---

    pub(crate) fn effect_resume_after(&self, effect: &str) -> anyhow::Result<u64> {
        self.lock_opdb().effect_resume_after(effect)
    }

    pub(crate) fn set_effect_watermark(&self, effect: &str, watermark: u64) -> anyhow::Result<()> {
        self.lock_opdb().set_effect_watermark(effect, watermark)
    }

    pub(crate) fn begin_invocation(
        &self,
        effect: &str,
        position: u64,
        script_hash: &str,
        now: &str,
    ) -> anyhow::Result<InvocationState> {
        self.lock_opdb()
            .begin_invocation(effect, position, script_hash, now)
    }

    pub(crate) fn complete_invocation(
        &self,
        effect: &str,
        position: u64,
        now: &str,
    ) -> anyhow::Result<()> {
        self.lock_opdb().complete_invocation(effect, position, now)
    }

    pub(crate) fn journal_get(
        &self,
        effect: &str,
        position: u64,
        call_hash: &str,
        disambiguator: u64,
    ) -> anyhow::Result<Option<String>> {
        self.lock_opdb()
            .journal_get(effect, position, call_hash, disambiguator)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn journal_put(
        &self,
        effect: &str,
        position: u64,
        call_hash: &str,
        disambiguator: u64,
        result: &str,
        now: &str,
    ) -> anyhow::Result<()> {
        self.lock_opdb()
            .journal_put(effect, position, call_hash, disambiguator, result, now)
    }

    pub(crate) fn running_with_hash_mismatch(
        &self,
        effect: &str,
        current_hash: &str,
    ) -> anyhow::Result<Vec<u64>> {
        self.lock_opdb()
            .running_with_hash_mismatch(effect, current_hash)
    }

    pub(crate) fn sweep_effect_journal(&self, cutoff: &str, limit: usize) -> anyhow::Result<usize> {
        self.lock_opdb().sweep_effect_journal(cutoff, limit)
    }

    pub(crate) fn sweep_idempotency(&self, cutoff: &str, limit: usize) -> anyhow::Result<usize> {
        self.lock_opdb().sweep_idempotency(cutoff, limit)
    }

    /// Read one row from a projector's read model, for the effect `read()` builtin.
    /// Returns `null` when the row is absent.
    pub(crate) fn read_projector(
        &self,
        projector: &str,
        entity: &str,
        key: &str,
    ) -> anyhow::Result<Value> {
        let shared = self
            .projectors
            .get(projector)
            .ok_or_else(|| anyhow::anyhow!("read(): no projector `{projector}`"))?;
        let entity_def = read_api::find_entity(&shared.entities, entity).ok_or_else(|| {
            anyhow::anyhow!("read(): no entity `{entity}` in projector `{projector}`")
        })?;
        let (row, _position) = read_api::get_one(&shared.db_path, entity_def, key)?;
        Ok(row.unwrap_or(Value::Null))
    }

    /// Scan a projector's read model, for the effect `scan()` builtin. Returns
    /// `{items, next_cursor}`; an unindexed filter is an error.
    pub(crate) fn scan_projector(
        &self,
        projector: &str,
        entity: &str,
        filter: Option<(String, String)>,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> anyhow::Result<Value> {
        let shared = self
            .projectors
            .get(projector)
            .ok_or_else(|| anyhow::anyhow!("scan(): no projector `{projector}`"))?;
        let entity_def = read_api::find_entity(&shared.entities, entity).ok_or_else(|| {
            anyhow::anyhow!("scan(): no entity `{entity}` in projector `{projector}`")
        })?;
        if let Some((field, _)) = &filter
            && !read_api::is_filterable(entity_def, field)
        {
            anyhow::bail!("scan(): filter field `{field}` is not indexed on entity `{entity}`");
        }
        let after_key = match &cursor {
            Some(raw) => Some(read_api::decode_cursor(raw)?),
            None => None,
        };
        let limit = limit
            .unwrap_or(read_api::DEFAULT_LIMIT)
            .clamp(1, read_api::MAX_LIMIT);
        let filter_ref = filter
            .as_ref()
            .map(|(field, value)| (field.as_str(), value.as_str()));
        let page = read_api::scan(
            &shared.db_path,
            entity_def,
            filter_ref,
            after_key.as_deref(),
            limit,
        )?;
        Ok(json!({ "items": page.items, "next_cursor": page.next_cursor }))
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

/// The current instant as an RFC 3339 string, the request's pinned `now()` and an
/// effect's journaled `now()`.
pub(crate) fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// An RFC 3339 timestamp `days` before now, the retention sweeper's cutoff. Both
/// this and the stored timestamps are UTC RFC 3339, which sorts lexicographically,
/// so the sweeper compares them as strings. An absurd `days` that would run off
/// the representable date range falls back to the epoch start, so the cutoff
/// matches nothing and the sweep becomes a no-op rather than panicking.
pub(crate) fn rfc3339_days_ago(days: u32) -> String {
    OffsetDateTime::now_utc()
        .checked_sub(TimeDuration::days(i64::from(days)))
        .and_then(|cutoff| cutoff.format(&Rfc3339).ok())
        .unwrap_or_else(|| "0000-01-01T00:00:00Z".to_owned())
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
