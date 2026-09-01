//! The command runtime: the live process behind `hekla serve`.
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
//! (the interpreter and tephra appends are), so the server calls it on a blocking thread.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs};

use anyhow::Context;
use serde_json::{Value, json};
use tephra::{
    PositionRange, SegmentConfig, SegmentSet, WriteCoordinator, WriteHandle, WriterConfig,
};
use time::Duration as TimeDuration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use heklang::Program;

use crate::config::Config;
use crate::context::CommandContext;
use crate::crypto::{KeyStore, MasterKeys};
use crate::dispatch::{self, CommandOutcome};
use crate::effect::{self, EffectRuntime, EffectShared};
use crate::http::HttpClient;
use crate::loader::{CommandUnit, EffectUnit, LoadedProject, ProjectorUnit};
use crate::lock::DataDirLock;
use crate::opdb::{
    EffectState, InvocationAt, InvocationRow, InvocationState, JournalRow, ModuleRow, OpDb,
    SubjectInfo,
};
use crate::openapi;
use crate::projector::{self, ProjectorSet, ProjectorShared};
use crate::schema::{EmittedEvent, EventDef, EventDefs};
use crate::tags;

/// Individual event segments before rolling to a new file. 256 MiB matches
/// tephra's own default sizing.
const SEGMENT_SIZE: usize = 256 * 1024 * 1024;

/// How many times a command re-runs its whole decision cycle on a DCB conflict
/// before the runtime gives up and returns a concurrency conflict.
///
/// In-runtime retry is the right default for an application: a hot boundary is
/// usually a transient loser, and answering 409 for something a re-read would settle
/// pushes work onto every client. It is overridable because a benchmark harness
/// brings its own fixed retry policy, and two nested budgets measure neither.
const DEFAULT_MAX_ATTEMPTS: u32 = 5;

/// The ceiling on `HEKLA_MAX_ATTEMPTS`. Each attempt re-folds and re-decides, so this
/// bounds how much work one request can spend losing races; the wait itself is bounded
/// separately by [`BACKOFF_CAP_MS`].
const MAX_ATTEMPTS_CAP: u32 = 15;

/// The ceiling on one retry's wait. The sleep blocks a request thread and its blocking
/// slot, so an uncapped doubling makes a deep retry budget *worse* than a shallow one:
/// at 10 attempts the last wait alone would be about a second, and the clients that
/// conflicted are all waiting through it.
const BACKOFF_CAP_MS: u64 = 16;

/// The effective retry budget, read once from `HEKLA_MAX_ATTEMPTS`. Values below 1
/// and unparseable ones fall back to the default rather than disabling the append,
/// and the whole thing is capped, because every attempt is another fold of the
/// boundary and another decision.
fn max_attempts() -> u32 {
    static ATTEMPTS: OnceLock<u32> = OnceLock::new();
    *ATTEMPTS.get_or_init(|| {
        env::var("HEKLA_MAX_ATTEMPTS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
            .filter(|attempts| *attempts >= 1)
            .unwrap_or(DEFAULT_MAX_ATTEMPTS)
            .min(MAX_ATTEMPTS_CAP)
    })
}

/// How long to wait before the retry after `attempt`: full jitter over a capped
/// exponential, uniform in `[0, min(2^attempt ms, BACKOFF_CAP_MS)]`.
///
/// The jitter is the load-bearing half. Requests that conflict are by definition in
/// lockstep, so an undithered backoff sleeps them for the same duration and lines them
/// up to conflict again on the same boundary; spreading them over the window is what
/// lets one through per round. `roll` is the caller's random draw, so the schedule
/// stays a pure function and is testable without sleeping.
fn backoff_delay(attempt: u32, roll: u64) -> Duration {
    // Clamped before the shift, not after: `checked_shl` guards only the shift width,
    // so it returns `Some` of a wrapped product for every attempt from 22 to 63, and
    // `1000 << 61` is exactly 0. Clamping first keeps the exponential exact over the
    // range that can reach the cap and saturating past it.
    let exponential_ms = 1u64 << attempt.min(BACKOFF_CAP_MS.ilog2() + 1);
    Duration::from_micros(roll % (exponential_ms.min(BACKOFF_CAP_MS) * 1_000 + 1))
}

/// A per-thread random draw for [`backoff_delay`]. xorshift64 rather than a real RNG:
/// this decorrelates retries and guards nothing, so the bar is "cheap and not the same
/// on every thread". Seeded once per thread from the OS, since threads that wake
/// together must not draw the same sequence.
fn jitter_roll() -> u64 {
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|state| {
        let mut seed = state.get();
        if seed == 0 {
            let mut bytes = [0u8; 8];
            // A failure here must not fail the request, but it must not collapse the
            // seed either: `bytes` stays zero, so without mixing in this thread's own
            // slot address every thread would draw the identical sequence, which is
            // precisely the lockstep the jitter exists to break.
            let _ = getrandom::fill(&mut bytes);
            // The slot address alone is not enough: thread-local blocks come from one
            // contiguous allocation, so addresses across a pool differ in a few middle
            // bits, and one xorshift round then a reduction modulo the window leaves
            // the first draws correlated. A per-thread counter run through a splitmix
            // finaliser separates them regardless of layout.
            static THREADS: AtomicU64 = AtomicU64::new(0);
            let mut mix = THREADS.fetch_add(1, AtomicOrdering::Relaxed)
                ^ (ptr::from_ref(state) as u64)
                ^ u64::from_ne_bytes(bytes);
            mix ^= mix >> 30;
            mix = mix.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            mix ^= mix >> 27;
            mix = mix.wrapping_mul(0x94d0_49bb_1331_11eb);
            seed = (mix ^ (mix >> 31)) | 1;
        }
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        state.set(seed);
        seed
    })
}

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
    keystore: Option<Arc<KeyStore>>,
    /// The one parsed program every thread reads. `Program` is `Send + Sync`, so this
    /// is shared rather than copied.
    program: Arc<Program>,
    started: Instant,
    projectors: HashMap<String, Arc<ProjectorShared>>,
    /// The effect handles, for `/status` and the skip endpoint. Set once, right
    /// after the effect threads spawn (they need `Arc<Runtime>` first).
    effects: OnceLock<Vec<Arc<EffectShared>>>,
    /// The exclusive claim on the data directory, held for as long as the runtime
    /// is open. tephra does not lock its segment directory, so without this a
    /// second process on one directory would corrupt the log rather than refuse to
    /// start. Never read: it exists for its `Drop`.
    _lock: DataDirLock,
    /// Whether the continuous invariant checks run. Set from `[verify] enabled` in
    /// `hekla.toml`, which `serve --verify` turns on without editing the file.
    verify: bool,
    /// The OpenAPI document serialized once at startup: the public command set is
    /// fixed for the process lifetime, so `/openapi.json` serves this verbatim.
    openapi_json: String,
    /// The effective configuration, retained for introspection. Everything the
    /// runtime itself needs is read out of it at boot (`auto_rebuild` into the
    /// projector threads, the retention window into the sweeper closure), so
    /// without this the values a process is actually running under would be
    /// unreportable.
    config: Config,
    /// The resolved data directory, for the same reason.
    data_dir: PathBuf,
}

impl Runtime {
    /// Open the store and operational DB under `data_dir`, start one thread per
    /// projector and per effect (plus the retention sweeper), and build the runtime
    /// from an already-loaded, error-free project. `http` is the transport the
    /// journaled `http.*` builtins use (a [`UreqClient`](crate::http::UreqClient)
    /// in production, a stub in tests). Returns the runtime, the write coordinator,
    /// the projector set, and the effect runtime; the caller keeps the last three
    /// to drain and join on shutdown.
    pub fn open(
        project: LoadedProject,
        data_dir: &Path,
        http: Arc<dyn HttpClient>,
        master: Option<MasterKeys>,
    ) -> anyhow::Result<(Arc<Runtime>, WriteCoordinator, ProjectorSet, EffectRuntime)> {
        // Taken before anything opens the log. tephra does not lock its segment
        // directory, so a second process here would corrupt it rather than fail.
        fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
        let lock = DataDirLock::acquire(data_dir)?;
        let events_dir = data_dir.join("events");
        fs::create_dir_all(&events_dir)
            .with_context(|| format!("creating {}", events_dir.display()))?;
        let set = SegmentSet::open(&events_dir, SegmentConfig::new(SEGMENT_SIZE))
            .with_context(|| format!("opening event store at {}", events_dir.display()))?;
        let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default())
            .context("starting the write coordinator")?;

        let opdb = OpDb::open(&data_dir.join("hekla.db"))?;
        let now = now_rfc3339();

        // Generated before the project is taken apart below: `Surface` borrows the whole
        // of it, and `hekla openapi` goes through the same two calls, so the served
        // document and the dumped one cannot disagree.
        let openapi_json = openapi::build(&openapi::Surface::from_project(&project)).to_string();

        let mut commands = HashMap::new();
        for unit in project.commands {
            opdb.upsert_module_metadata(unit.def.name(), "command", &unit.source_hash, &now)?;
            let name = unit.def.name().to_owned();
            commands.insert(name, Arc::new(unit));
        }

        let projectors_dir = data_dir.join("projectors");
        fs::create_dir_all(&projectors_dir)
            .with_context(|| format!("creating {}", projectors_dir.display()))?;
        let projector_units: Vec<Arc<ProjectorUnit>> =
            project.projectors.into_iter().map(Arc::new).collect();
        let auto_rebuild = project.config.projectors.auto_rebuild;
        let verify = project.config.verify.enabled;
        let config = project.config.clone();
        for unit in &projector_units {
            opdb.upsert_module_metadata(unit.def.name(), "projector", &unit.source_hash, &now)?;
        }
        // Event field metadata, shared with the projector and effect runtimes so a
        // fold or a `handle` sees subject fields as opaque handles.
        let events = Arc::new(project.events.clone());
        let uses_subjects = events
            .values()
            .any(|def| def.fields.iter().any(|(_, meta)| meta.subject.is_some()));
        if uses_subjects && master.is_none() {
            anyhow::bail!(
                "this project uses subject-scoped encryption (a field with subject = \"...\"), so HEKLA_MASTER_KEY must be set"
            );
        }

        let effect_units: Vec<Arc<EffectUnit>> =
            project.effects.into_iter().map(Arc::new).collect();
        for unit in &effect_units {
            opdb.upsert_module_metadata(unit.def.name(), "effect", &unit.source_hash, &now)?;
        }

        let opdb = Arc::new(Mutex::new(opdb));
        let keystore = master.map(|master| KeyStore::new(opdb.clone(), master));
        // Fail fast if a stored subject key was wrapped under a master that is not
        // configured now (a wrong or rotated-away HEKLA_MASTER_KEY), rather than
        // surfacing it as a read error after boot.
        if let Some(keystore) = &keystore {
            keystore.verify_masters_present()?;
        }
        let keystore = keystore.map(Arc::new);
        let program = Arc::new(project.program);

        // Each projector reconciles its own definition hash (stored in its read model)
        // against the current one at startup, rebuilding if it changed before applying
        // any batch. Recording the hash with the rebuild's atomic swap makes this
        // crash-safe, unlike recording it here at boot.
        let (shared, projector_set) = projector::start_all(
            projector_units,
            &store,
            &projectors_dir,
            Arc::clone(&program),
            keystore.clone(),
            auto_rebuild,
        )?;
        let projectors: HashMap<String, Arc<ProjectorShared>> = shared
            .into_iter()
            .map(|handle| (handle.name.clone(), handle))
            .collect();

        let runtime = Arc::new(Runtime {
            commands,
            store,
            opdb,
            events: events.clone(),
            program,
            keystore,
            started: Instant::now(),
            projectors,
            effects: OnceLock::new(),
            openapi_json,
            _lock: lock,
            verify,
            config,
            data_dir: data_dir.to_path_buf(),
        });

        // Effects need `Arc<Runtime>` (for `invoke_command` and the boundary fold), so they
        // spawn after the runtime is built; the runtime learns their handles for
        // `/status` here, once.
        let effect_runtime = effect::start_all(effect_units, &runtime, http, &project.config)?;
        let _ = runtime.effects.set(effect_runtime.shared_handles());

        Ok((runtime, coordinator, projector_set, effect_runtime))
    }

    /// Open the store and operational DB without starting a single thread.
    ///
    /// `hekla verify` audits recorded state, so it must not advance it: the full
    /// [`Runtime::open`] starts projectors applying batches and effects performing
    /// side effects, which for an audit is exactly the wrong thing. Nothing here
    /// spawns, and the returned runtime has no projector or effect handles, so a
    /// `/status` built from it would be empty.
    ///
    /// It still needs the log open for writes, because tephra exposes no read-only
    /// handle: `ReadHandle` is reachable only through a `WriteCoordinator`. That is
    /// why the caller holds the data-directory lock, and why verifying a live
    /// directory is refused rather than merely discouraged.
    pub fn open_quiescent(
        project: &LoadedProject,
        data_dir: &Path,
        master: Option<MasterKeys>,
    ) -> anyhow::Result<(Arc<Runtime>, WriteCoordinator)> {
        let lock = DataDirLock::acquire(data_dir)?;
        let events_dir = data_dir.join("events");
        let set = SegmentSet::open(&events_dir, SegmentConfig::new(SEGMENT_SIZE))
            .with_context(|| format!("opening event store at {}", events_dir.display()))?;
        let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default())
            .context("starting the write coordinator")?;

        let events = Arc::new(project.events.clone());
        // The same guard `open` applies. Without it a sweep of a subject-using project
        // with no master key runs every check against a keystore-less runtime, where
        // `reveal` fails before the host can mark the failure terminal, so the replay
        // check reports a divergence for every invocation. A healthy directory would
        // exit non-zero naming corruption that is not there.
        let uses_subjects = events
            .values()
            .any(|def| def.fields.iter().any(|(_, meta)| meta.subject.is_some()));
        if uses_subjects && master.is_none() {
            anyhow::bail!(
                "this project uses subject-scoped encryption (a field with subject = \"...\"), so HEKLA_MASTER_KEY must be set to verify it"
            );
        }
        let opdb = Arc::new(Mutex::new(OpDb::open(&data_dir.join("hekla.db"))?));
        let keystore = master
            .map(|master| KeyStore::new(opdb.clone(), master))
            .map(Arc::new);
        if let Some(keystore) = &keystore {
            keystore.verify_masters_present()?;
        }

        let runtime = Arc::new(Runtime {
            commands: HashMap::new(),
            store,
            opdb,
            events: events.clone(),
            program: Arc::new(project.program.clone()),
            keystore,
            started: Instant::now(),
            projectors: HashMap::new(),
            effects: OnceLock::new(),
            openapi_json: String::new(),
            config: project.config.clone(),
            data_dir: data_dir.to_path_buf(),
            _lock: lock,
            // The sweep calls the checks directly. Leaving this off keeps a replay it
            // runs from scheduling a second replay of itself.
            verify: false,
        });
        Ok((runtime, coordinator))
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
        self.run_resolved(name, command, body, ctx, idem_key)
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
        self.run_resolved(name, command, body, ctx, idem_key)
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
        let idem_tag = idem_key.map(|key| tags::idempotency_tag(name, key));
        self.run_with_retry(command, &body, ctx, &now, idem_tag.as_deref())
    }

    /// Run the decision cycle, retrying on a DCB conflict so the decision model is
    /// rebuilt against the new tail, with a capped jittered backoff so a hot
    /// boundary does not hammer the single writer. The attempts themselves happen
    /// inside heklang, which folds only what landed since the last one; this decides
    /// the budget and the wait. When a keyed request already committed (a crash or a
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
        if let Err(err) = dispatch::validate_input(&self.program, command.def.name(), body) {
            return Ok(ExecResult {
                status: 400,
                body: error_body(ctx, "invalid_input", &format!("{err}")),
            });
        }

        let retry = dispatch::Retry {
            max_attempts: max_attempts(),
            backoff: &|attempt| thread::sleep(backoff_delay(attempt, jitter_roll())),
        };
        match dispatch::run_command(
            &self.store,
            self.program_shared(),
            self.events_shared(),
            command.def.name(),
            self.keystore_shared(),
            body,
            ctx,
            now,
            idem_tag,
            &retry,
        )? {
            CommandOutcome::Conflict => Ok(ExecResult {
                status: 409,
                body: conflict_body(ctx),
            }),
            CommandOutcome::Committed { events, positions } => Ok(ExecResult {
                status: 200,
                body: success_body(ctx, positions, &events, &self.events),
            }),
            CommandOutcome::AlreadyCommitted(recovered) => Ok(ExecResult {
                status: 200,
                body: recovered_body(&recovered),
            }),
            CommandOutcome::Rejected { code, message } => Ok(ExecResult {
                status: 422,
                body: error_body(ctx, &code, &message),
            }),
            CommandOutcome::InvalidInput { message } => Ok(ExecResult {
                status: 400,
                body: error_body(ctx, "invalid_input", &message),
            }),
            CommandOutcome::Unavailable { message } => Ok(ExecResult {
                status: 503,
                body: error_body(ctx, "unavailable", &message),
            }),
        }
    }

    /// A JSON snapshot for `GET /status`. Reports the log head, the loaded-module
    /// inventory, each projector's committed position, lag, and health (whether its
    /// thread died on an error, with the message), and each effect's position, lag,
    /// and health, so a wedge reads as broken rather than merely lagging. An effect
    /// reports `consecutive_failures`/`last_error` for a genuine wedge (a retrying
    /// position) and, separately, `terminal_skips`/`last_terminal_error` for positions
    /// abandoned to unrecoverable failures (an erased subject a `reveal()` needed).
    pub fn status(&self) -> Value {
        let mut public: Vec<&str> = Vec::new();
        let mut internal: Vec<&str> = Vec::new();
        for unit in self.commands.values() {
            if unit.internal {
                internal.push(unit.def.name());
            } else {
                public.push(unit.def.name());
            }
        }
        public.sort();
        internal.sort();

        let head = self.log_head();
        let projectors: Vec<Value> = self
            .projector_handles()
            .iter()
            .map(|handle| {
                let position = handle.position();
                json!({
                    "name": handle.name,
                    "position": position,
                    "lag": head.saturating_sub(position),
                    "readiness": handle.readiness().label(),
                    "running": handle.running(),
                    "failed": handle.failed(),
                    // Cumulative since boot. A rebuild leaves no other trace: it happens
                    // into a sibling file and swaps in by rename, so without these an
                    // operator cannot tell a projector that has replayed twice from one
                    // that has never replayed at all.
                    "replays_completed": handle.replays_completed(),
                    "replays_failed": handle.replays_failed(),
                    "last_error": handle.last_error(),
                })
            })
            .collect();

        let effects: Vec<Value> = self
            .effect_handles()
            .iter()
            .map(|handle| {
                let position = handle.position();
                json!({
                    "name": handle.name,
                    // The same one-word summary `/admin/effects` reports, from the
                    // same function, so the two can never drift.
                    "state": handle.state(head),
                    "position": position,
                    "lag": head.saturating_sub(position),
                    "consecutive_failures": handle.consecutive_failures(),
                    "last_error": handle.last_error(),
                    "quarantined": handle.quarantined(),
                    "terminal_skips": handle.terminal_skips(),
                    "last_terminal_error": handle.last_terminal_error(),
                })
            })
            .collect();

        json!({
            "log_head": head,
            "uptime_seconds": self.uptime_seconds(),
            "verify": self.verify,
            "commands": { "public": public, "internal": internal },
            "projectors": projectors,
            "effects": effects,
            "events": self.events.len(),
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

    /// Record a durable verify-mode quarantine for an effect.
    pub(crate) fn quarantine_effect(
        &self,
        effect: &str,
        position: u64,
        reason: &str,
    ) -> anyhow::Result<()> {
        self.lock_opdb().quarantine_effect(effect, position, reason)
    }

    /// The recorded quarantine for an effect, if any.
    pub(crate) fn effect_quarantine(&self, effect: &str) -> anyhow::Result<Option<(u64, String)>> {
        self.lock_opdb().effect_quarantine(effect)
    }

    /// The journaled calls recorded for one invocation, for the replay check.
    pub(crate) fn journal_keys(
        &self,
        effect: &str,
        position: u64,
    ) -> anyhow::Result<Vec<(String, u64)>> {
        self.lock_opdb().journal_keys(effect, position)
    }

    /// The log head, which is also the total event count: positions are dense and
    /// 1-based.
    pub fn log_head(&self) -> u64 {
        self.store.head().get()
    }

    /// Seconds since this runtime opened.
    pub fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    // --- introspection ------------------------------------------------------
    //
    // Each of these takes and releases the op-DB lock exactly once, the same
    // discipline the effect hot path follows. The bounds are the caller's, so a
    // browsing request can never hold the lock across an unbounded scan.

    /// The effective configuration this process is running under.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The resolved data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Every projector handle, in name order.
    pub fn projector_handles(&self) -> Vec<&Arc<ProjectorShared>> {
        let mut handles: Vec<&Arc<ProjectorShared>> = self.projectors.values().collect();
        handles.sort_by(|a, b| a.name.cmp(&b.name));
        handles
    }

    /// Every effect handle, in name order. Empty before the effect threads spawn.
    pub fn effect_handles(&self) -> Vec<&Arc<EffectShared>> {
        let mut handles: Vec<&Arc<EffectShared>> = self
            .effects
            .get()
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        handles.sort_by(|a, b| a.name.cmp(&b.name));
        handles
    }

    /// The command units, public and internal alike. Unlike the OpenAPI document,
    /// introspection reports internal commands: they are not routed, but they exist
    /// and an operator debugging an effect's `invoke_command` needs to see them.
    pub fn command_units(&self) -> Vec<&Arc<CommandUnit>> {
        let mut units: Vec<&Arc<CommandUnit>> = self.commands.values().collect();
        units.sort_by_key(|unit| unit.def.name());
        units
    }

    pub(crate) fn opdb_schema_version(&self) -> anyhow::Result<i64> {
        self.lock_opdb().schema_version()
    }

    pub(crate) fn invocations(
        &self,
        effect: &str,
        before: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<InvocationRow>> {
        self.lock_opdb().invocations(effect, before, limit)
    }

    pub(crate) fn invocation(
        &self,
        effect: &str,
        position: u64,
    ) -> anyhow::Result<Option<InvocationRow>> {
        self.lock_opdb().invocation(effect, position)
    }

    pub(crate) fn invocations_at(
        &self,
        effects: &[&str],
        positions: &[u64],
    ) -> anyhow::Result<Vec<InvocationAt>> {
        self.lock_opdb().invocations_at(effects, positions)
    }

    pub(crate) fn journal_entries(
        &self,
        effect: &str,
        position: u64,
        offset: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<JournalRow>> {
        self.lock_opdb()
            .journal_entries(effect, position, offset, limit)
    }

    pub(crate) fn effect_states(&self) -> anyhow::Result<HashMap<String, EffectState>> {
        self.lock_opdb().effect_states()
    }

    pub(crate) fn module_metadata(&self) -> anyhow::Result<Vec<ModuleRow>> {
        self.lock_opdb().module_metadata()
    }

    pub(crate) fn subject_key_counts(&self) -> anyhow::Result<Vec<(String, u64)>> {
        self.lock_opdb().subject_key_counts()
    }

    pub(crate) fn subject_keys_page(
        &self,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<SubjectInfo>> {
        self.lock_opdb().subject_keys_page(after, limit)
    }

    pub(crate) fn subject_key_exists(&self, field: &str, value: &str) -> anyhow::Result<bool> {
        self.lock_opdb().subject_key_exists(field, value)
    }

    pub(crate) fn master_key_ids(&self) -> anyhow::Result<Vec<String>> {
        self.lock_opdb().distinct_master_key_ids()
    }

    /// Every terminal invocation recorded for an effect, with its script hash.
    pub(crate) fn terminal_invocations(&self, effect: &str) -> anyhow::Result<Vec<(u64, String)>> {
        self.lock_opdb().terminal_invocations(effect)
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

    /// The OpenAPI document, serialized once at startup.
    /// The one parsed program, for anything that runs a declaration.
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// The same, as a handle a host can keep. `Program` is `Send + Sync`, so a world
    /// shares this rather than copying it.
    pub fn program_shared(&self) -> &Arc<Program> {
        &self.program
    }

    pub fn events_shared(&self) -> &Arc<EventDefs> {
        &self.events
    }

    pub fn keystore_shared(&self) -> Option<&Arc<KeyStore>> {
        self.keystore.as_ref()
    }

    /// The operational database, which is where an invocation's journal lives.
    pub fn opdb(&self) -> &Arc<Mutex<OpDb>> {
        &self.opdb
    }

    pub fn openapi_json(&self) -> &str {
        &self.openapi_json
    }

    /// The declared field metadata for an event type. The effect runtime uses this
    /// to materialise subject fields as opaque handles when a `handle` reads an event.
    pub fn event_def(&self, event_type: &str) -> Option<&EventDef> {
        self.events.get(event_type)
    }

    /// The full event-definition map, for lowering an effect's subscription to a query.
    pub fn events_map(&self) -> &EventDefs {
        &self.events
    }

    /// Whether the continuous invariant checks are on for this process.
    pub fn verify(&self) -> bool {
        self.verify
    }

    /// The subject-key store, if a master key is configured. The read API decrypts
    /// subject columns through it, and an effect's `reveal()` reads plaintext.
    pub fn keystore(&self) -> Option<&KeyStore> {
        self.keystore.as_deref()
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
/// so a log-recovered replay is byte-identical to the original response.
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

/// The shared shape of a committed-command response, so the fresh and log-recovered
/// paths return the same body.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The property that matters is the ceiling, not the distribution: whatever the
    /// draw, a single wait can never grow past the cap, which is what made
    /// `HEKLA_MAX_ATTEMPTS=10` worse than 5 before it existed.
    #[test]
    fn backoff_is_capped_however_large_the_attempt() {
        let cap = Duration::from_millis(BACKOFF_CAP_MS);
        // Every attempt, not just the reachable ones. The window from 22 upwards is
        // where a `checked_shl` wraps to a *small* product instead of saturating, so
        // the wait silently collapses toward zero rather than holding at the cap.
        for attempt in 0..64 {
            for roll in [0, 1, u64::MAX / 2, u64::MAX] {
                let delay = backoff_delay(attempt, roll);
                assert!(delay <= cap, "attempt {attempt} roll {roll}: {delay:?}");
            }
        }
        assert!(backoff_delay(u32::MAX, u64::MAX) <= cap);
    }

    /// The cap must be a ceiling the schedule actually reaches, not one it collapses
    /// past: at a high attempt the largest draw should wait the full cap.
    #[test]
    fn backoff_saturates_at_the_cap_rather_than_wrapping() {
        let cap = Duration::from_millis(BACKOFF_CAP_MS);
        for attempt in [8, 15, 22, 40, 63] {
            assert_eq!(
                backoff_delay(attempt, BACKOFF_CAP_MS * 1_000),
                cap,
                "attempt {attempt}"
            );
        }
    }

    /// Early attempts stay under the exponential, so the cap does not turn the first
    /// retry into a 16 ms wait.
    #[test]
    fn backoff_follows_the_exponential_below_the_cap() {
        for attempt in 0..4 {
            let ceiling = Duration::from_millis(1 << attempt);
            for roll in [0, 7, u64::MAX] {
                assert!(
                    backoff_delay(attempt, roll) <= ceiling,
                    "attempt {attempt} roll {roll}"
                );
            }
        }
        assert_eq!(backoff_delay(0, 0), Duration::ZERO);
    }

    /// Full jitter means the draw actually moves the wait. Without this, two clients
    /// that conflict sleep identically and collide again.
    #[test]
    fn backoff_spreads_across_the_window() {
        let waits: Vec<Duration> = (0..64).map(|roll| backoff_delay(8, roll * 977)).collect();
        let distinct = waits.iter().collect::<HashSet<_>>();
        assert!(distinct.len() > 8, "{waits:?}");
    }

    #[test]
    fn jitter_rolls_differ_within_a_thread() {
        let rolls = [jitter_roll(), jitter_roll(), jitter_roll()];
        assert!(rolls[0] != rolls[1] || rolls[1] != rolls[2], "{rolls:?}");
    }
}
