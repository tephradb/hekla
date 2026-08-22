//! The command runtime: the live process behind `kiln serve`.
//!
//! A [`Runtime`] owns the tephra store handle, the operational DB, and the loaded
//! command modules, and turns an HTTP request into a decision cycle. It layers
//! three concerns on top of the pure dispatch in [`crate::dispatch`]: a pinned
//! `now()` for the request, built-in per-command idempotency, and optimistic-
//! concurrency retry.
//!
//! Idempotency lives in the event log, not here: a keyed command tags its events and
//! guards the append against that tag, and [`Runtime::execute`] reconstructs the
//! original `(status, body)` from those events on a replay. It is synchronous
//! (Starlark and tephra appends are), so the server calls it on a blocking thread.

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
use crate::crypto::{KeyStore, MasterKeys};
use crate::dispatch::{self, CommandOutcome, EventDefs};
use crate::effect::{self, EffectRuntime, EffectShared, HttpClient};
use crate::loader::{CommandUnit, EffectUnit, LoadedProject, ProjectorUnit};
use crate::opdb::{InvocationState, OpDb};
use crate::openapi;
use crate::projector::{self, ProjectorSet, ProjectorShared};
use crate::read_api;
use crate::starlark_builtins::{EmittedEvent, EventDef, InputSchema, ModuleDef};

/// Individual event segments before rolling to a new file. 256 MiB matches
/// tephra's own default sizing.
const SEGMENT_SIZE: usize = 256 * 1024 * 1024;

/// How many times a command re-runs its whole decision cycle on a DCB conflict
/// before the runtime gives up and returns a concurrency conflict.
const MAX_ATTEMPTS: u32 = 5;

/// The final HTTP outcome of a command execution: the status and the response body.
pub struct ExecResult {
    pub status: u16,
    pub body: Value,
}

/// The live runtime shared across request handlers.
pub struct Runtime {
    commands: HashMap<String, Arc<CommandUnit>>,
    store: WriteHandle,
    opdb: Arc<Mutex<OpDb>>,
    /// Event type to its declared field metadata, for emit encryption and for
    /// wrapping subject fields as opaque handles in a fold.
    events: Arc<EventDefs>,
    /// The subject-key store, present when a master key is configured. Required at
    /// boot when the project uses subject-scoped encryption.
    keystore: Option<KeyStore>,
    started: Instant,
    projectors: HashMap<String, Arc<ProjectorShared>>,
    /// The effect handles, for `/status` and the skip endpoint. Set once, right
    /// after the effect threads spawn (they need `Arc<Runtime>` first).
    effects: OnceLock<Vec<Arc<EffectShared>>>,
    event_count: usize,
    /// The OpenAPI document serialized once at startup: the public command set is
    /// fixed for the process lifetime, so `/openapi.json` serves this verbatim.
    openapi_json: String,
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
        master: Option<MasterKeys>,
    ) -> anyhow::Result<(Arc<Runtime>, WriteCoordinator, ProjectorSet, EffectRuntime)> {
        let events_dir = data_dir.join("events");
        fs::create_dir_all(&events_dir)
            .with_context(|| format!("creating {}", events_dir.display()))?;
        let set = SegmentSet::open(&events_dir, SegmentConfig::new(SEGMENT_SIZE))
            .with_context(|| format!("opening event store at {}", events_dir.display()))?;
        let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default())
            .context("starting the write coordinator")?;

        let opdb = OpDb::open(&data_dir.join("kiln.db"))?;
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
        let auto_rebuild = project.config.projectors.auto_rebuild;
        for unit in &projector_units {
            opdb.upsert_module_metadata(
                unit.loaded.def.name(),
                "projector",
                &unit.loaded.source_hash,
                &now,
            )?;
        }
        // Event field metadata, shared with the projector and effect runtimes so a
        // fold or a `handle` sees subject fields as opaque handles.
        let events = Arc::new(project.events.by_type.clone());
        let uses_subjects = events
            .values()
            .any(|def| def.fields.iter().any(|(_, meta)| meta.subject.is_some()));
        if uses_subjects && master.is_none() {
            anyhow::bail!(
                "this project uses subject-scoped encryption (a field with subject = \"...\"), so KILN_MASTER_KEY must be set"
            );
        }

        // Each projector reconciles its own definition hash (stored in its read model)
        // against the current one at startup, rebuilding if it changed before applying
        // any batch. Recording the hash with the rebuild's atomic swap makes this
        // crash-safe, unlike recording it here at boot.
        let (shared, projector_set) = projector::start_all(
            projector_units,
            &store,
            &projectors_dir,
            events.clone(),
            auto_rebuild,
        )?;
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

        let openapi_json = openapi::build(&public_command_schemas(&commands)).to_string();

        let opdb = Arc::new(Mutex::new(opdb));
        let keystore = master.map(|master| KeyStore::new(opdb.clone(), master));
        // Fail fast if a stored subject key was wrapped under a master that is not
        // configured now (a wrong or rotated-away KILN_MASTER_KEY), rather than
        // surfacing it as a read error after boot.
        if let Some(keystore) = &keystore {
            keystore.verify_masters_present()?;
        }
        let event_count = events.len();

        let runtime = Arc::new(Runtime {
            commands,
            store,
            opdb,
            events: events.clone(),
            keystore,
            started: Instant::now(),
            projectors,
            effects: OnceLock::new(),
            event_count,
            openapi_json,
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

    /// The shared execution path once a command is resolved. Idempotency lives
    /// entirely in the event log: a keyed command tags every event with a per-request
    /// idempotency tag and guards the append against it, so a crashed or concurrent
    /// duplicate fails the condition instead of committing twice, and a replay
    /// reconstructs its original response from those tagged events. There is no
    /// separate response cache; the log is the single source of truth.
    fn run_resolved(
        &self,
        name: &str,
        command: &CommandUnit,
        body: Value,
        ctx: &CommandContext,
        idem_key: Option<&str>,
    ) -> anyhow::Result<ExecResult> {
        let now = now_rfc3339();
        let idem_tag = idem_key.map(|key| dispatch::idempotency_tag(name, key));
        self.run_with_retry(command, &body, ctx, &now, idem_tag.as_deref())
    }

    /// Run the decision cycle, retrying on a DCB conflict so a re-read rebuilds the
    /// decision model, with a small exponential backoff so a hot boundary does not
    /// hammer the single writer. When a keyed request already committed (a crash or a
    /// concurrent duplicate), `run_command` returns `AlreadyCommitted` with the
    /// outcome recovered from the log rather than re-deciding, so a duplicate never
    /// re-runs `handle`.
    fn run_with_retry(
        &self,
        command: &CommandUnit,
        body: &Value,
        ctx: &CommandContext,
        now: &str,
        idem_tag: Option<&str>,
    ) -> anyhow::Result<ExecResult> {
        // Input is invariant across attempts, so validate once before the loop.
        if let Err(err) = dispatch::validate_input(&command.loaded, body) {
            return Ok(ExecResult {
                status: 400,
                body: error_body(ctx, "invalid_input", &format!("{err}")),
            });
        }

        for attempt in 0..MAX_ATTEMPTS {
            match dispatch::run_command(
                &self.store,
                &command.loaded,
                &self.events,
                self.keystore.as_ref(),
                body,
                ctx,
                now,
                idem_tag,
            )? {
                CommandOutcome::Conflict => {
                    if attempt + 1 < MAX_ATTEMPTS {
                        thread::sleep(Duration::from_millis(1u64 << attempt));
                    }
                }
                CommandOutcome::Committed { events, positions } => {
                    return Ok(ExecResult {
                        status: 200,
                        body: success_body(ctx, positions, &events, &self.events),
                    });
                }
                CommandOutcome::AlreadyCommitted(recovered) => {
                    return Ok(ExecResult {
                        status: 200,
                        body: recovered_body(&recovered),
                    });
                }
                CommandOutcome::Rejected { code, message } => {
                    return Ok(ExecResult {
                        status: 422,
                        body: error_body(ctx, &code, &message),
                    });
                }
                CommandOutcome::InvalidInput { message } => {
                    return Ok(ExecResult {
                        status: 400,
                        body: error_body(ctx, "invalid_input", &message),
                    });
                }
                CommandOutcome::Unavailable { message } => {
                    return Ok(ExecResult {
                        status: 503,
                        body: error_body(ctx, "unavailable", &message),
                    });
                }
            }
        }
        Ok(ExecResult {
            status: 409,
            body: conflict_body(ctx),
        })
    }

    /// A JSON snapshot for `GET /status`. Reports the log head, the loaded-module
    /// inventory, each projector's committed position, lag, and health (whether its
    /// thread died on an error, with the message), and each effect's position, lag,
    /// and health (its consecutive-failure count and last error), so a wedge reads
    /// as broken rather than merely lagging.
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
                    "failed": handle.failed(),
                    "last_error": handle.last_error(),
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
        let (row, _position) =
            read_api::get_one(&shared.db_path, entity_def, key, self.keystore.as_ref())?;
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
            self.keystore.as_ref(),
        )?;
        Ok(json!({ "items": page.items, "next_cursor": page.next_cursor }))
    }

    /// The public commands and their input schemas, for OpenAPI generation.
    pub fn public_commands(&self) -> Vec<(&str, &InputSchema)> {
        public_command_schemas(&self.commands)
    }

    /// The OpenAPI document, serialized once at startup.
    pub fn openapi_json(&self) -> &str {
        &self.openapi_json
    }

    /// The declared field metadata for an event type. The effect runtime uses this
    /// to materialise subject fields as opaque handles when a `handle` reads an event.
    pub fn event_def(&self, event_type: &str) -> Option<&EventDef> {
        self.events.get(event_type)
    }

    /// The full event-definition map, for lowering an effect's `source` to a query.
    pub fn events_map(&self) -> &EventDefs {
        &self.events
    }

    /// The subject-key store, if a master key is configured. The read API decrypts
    /// subject columns through it, and an effect's `reveal()` reads plaintext.
    pub fn keystore(&self) -> Option<&KeyStore> {
        self.keystore.as_ref()
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

/// The public (HTTP-routed) commands and their declared input schemas, sorted by
/// name, for OpenAPI generation.
fn public_command_schemas(
    commands: &HashMap<String, Arc<CommandUnit>>,
) -> Vec<(&str, &InputSchema)> {
    let mut out = Vec::new();
    for unit in commands.values() {
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

/// The 409 body for a request whose consistency boundary kept changing until the
/// runtime gave up retrying. One builder so the message never drifts.
fn conflict_body(ctx: &CommandContext) -> Value {
    error_body(
        ctx,
        "concurrency_conflict",
        "the command's consistency boundary kept changing during the request; retry",
    )
}

/// The 200 body for a freshly committed command. Subject-encrypted (and `unique`)
/// tags are omitted: their stored form is ciphertext, and the idempotent-recovery
/// path cannot reconstruct them, so reporting only the plaintext non-subject tags
/// keeps the fresh and recovered responses identical.
fn success_body(
    ctx: &CommandContext,
    positions: Option<PositionRange>,
    events: &[EmittedEvent],
    defs: &EventDefs,
) -> Value {
    let events: Vec<Value> = events
        .iter()
        .map(|event| {
            let def = defs.get(&event.event_type);
            let tags: Vec<(String, Option<String>)> = event
                .tags
                .iter()
                .filter(|(key, _)| !def.is_some_and(|def| def.is_subject(key)))
                .cloned()
                .collect();
            json!({ "type": event.event_type, "tags": tag_strings(&tags) })
        })
        .collect();
    committed_body(
        &ctx.correlation_id.to_string(),
        &ctx.causation_id.to_string(),
        positions,
        events,
    )
}

/// The 200 body for a command whose prior commit was recovered from the log under its
/// idempotency tag. Uses the original request's identity (from the stored envelope),
/// so a log-recovered replay is byte-identical to the op-DB-cached one.
fn recovered_body(recovered: &dispatch::RecoveredOutcome) -> Value {
    let events: Vec<Value> = recovered
        .events
        .iter()
        .map(|event| json!({ "type": event.event_type, "tags": event.tags }))
        .collect();
    committed_body(
        &recovered.correlation_id.to_string(),
        &recovered.causation_id.to_string(),
        Some(recovered.positions),
        events,
    )
}

/// The shared shape of a committed-command response, so the fresh, log-recovered, and
/// op-DB-cached paths all return the same body.
fn committed_body(
    correlation_id: &str,
    causation_id: &str,
    positions: Option<PositionRange>,
    events: Vec<Value>,
) -> Value {
    let positions = match positions {
        Some(range) => json!({ "first": range.first.get(), "last": range.last.get() }),
        None => Value::Null,
    };
    json!({
        "correlation_id": correlation_id,
        "causation_id": causation_id,
        "positions": positions,
        "events": events,
    })
}

/// The standard error envelope, shared with the HTTP layer so the two error paths
/// cannot diverge.
pub(crate) fn error_body(ctx: &CommandContext, code: &str, message: &str) -> Value {
    json!({
        "correlation_id": ctx.correlation_id.to_string(),
        "causation_id": ctx.causation_id.to_string(),
        "error": { "code": code, "message": message },
    })
}

/// Render derived tags as `"key:value"` / `"key"` for the response, sorted so a
/// live outcome matches one recovered from the log (whose tag set is stored sorted).
fn tag_strings(tags: &[(String, Option<String>)]) -> Vec<String> {
    let mut out: Vec<String> = tags
        .iter()
        .map(|(key, value)| match value {
            Some(value) => format!("{key}:{value}"),
            None => key.clone(),
        })
        .collect();
    out.sort();
    out
}

/// Resolve the data directory: the flag if given, else `<project>/data`.
pub fn resolve_data_dir(project_dir: &Path, data_dir: Option<&Path>) -> PathBuf {
    match data_dir {
        Some(dir) => dir.to_path_buf(),
        None => project_dir.join("data"),
    }
}
