//! The effect runtime: durable execution of side effects.
//!
//! One dedicated thread per effect subscribes to its `handle` keys and processes
//! matching events strictly in order, one invocation per event. An invocation
//! runs the effect's straight-line `handle`, whose impure builtins (`http.*`,
//! `invoke_command`, `now`) are journaled: each call records its
//! result in the operational DB, so a crash mid-handler resumes by replaying the
//! journaled calls and running only the unjournaled tail live. `log` is not
//! journaled.
//!
//! The journal is written call-by-call in autocommit, never wrapped in a
//! per-invocation transaction: journaled side effects must survive a crash so
//! replay skips them. Because completed calls persist, retrying a failed
//! invocation replays them and fails at the same point without re-firing.
//!
//! Durability boundaries: `invoke_command` lands the domain fact exactly-once. It
//! passes a deterministic idempotency key, so the target command tags every event
//! it emits with that key and guards the append against the tag. A replay after a
//! crash (or a concurrent duplicate) finds the prior commit by that tag and returns
//! its recovered outcome instead of committing again, exactly as for HTTP commands;
//! dedupe lives in the event log, not in any op-DB reservation. Raw `http.*` is
//! at-least-once (a crash between a successful request and its journal write
//! re-fires on replay).
//!
//! A handler error (a script bug, or a transport error / retryable status the
//! runtime refuses to surface) wedges the invocation: it retries forever with
//! capped backoff, never skipping, surfacing as a distinct failure count and last
//! error in `/status`. The only escape past a genuinely unprocessable event is an
//! explicit operator skip.
//!
//! A *retryable* status is 408, 425, 429 or any 5xx: each names a condition that
//! clears on its own, with the same request. Keeping 429 out of the script is not
//! a convenience. A response that reaches Starlark is journaled, so an effect that
//! raised on one would replay the recorded 429 on every attempt and wedge forever
//! without ever re-sending. A `Retry-After` on such a response raises that
//! attempt's backoff, so a rate limiter's own window is waited out, not hammered.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};
use starlark::environment::Module;
use tephra::{Event, Position, WaitOutcome, WriteHandle};
use ureq::Agent;
use ureq::typestate::{WithBody, WithoutBody};

use crate::config::Config;
use crate::context::{CommandContext, EffectCtx, EffectHost};
use crate::crypto::KeyStore;
use crate::dispatch::{self, EventDefs, arm_selects, lower_dispatch};
use crate::envelope::{self, Envelope};
use crate::loader::EffectUnit;
use crate::opdb::{InvocationState, SWEEP_CHUNK};
use crate::runtime::{self, Runtime};
use crate::starlark_builtins::{
    EventSpec, LoadedModule, ModuleDef, alloc_event, call_handler_with_effect_ctx,
    call_handler_with_query_ctx, initial_state, parse_event_dispatch, parse_event_specs, thaw,
};
use crate::verify::Violation;

/// Per-handler instruction budget. Bounds a runaway script at dispatch time.
const MAX_TICKS: u64 = 10_000_000;
/// How long an idle, caught-up effect waits before polling again.
const IDLE_POLL: Duration = Duration::from_millis(250);
/// The ceiling on the wedge retry backoff, so a stuck effect keeps retrying at a
/// steady cadence rather than backing off unboundedly.
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// The base wedge retry backoff, doubled each attempt up to [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::from_millis(200);
/// The ceiling on a `Retry-After` a server asked for. The header is honored past
/// [`BACKOFF_CAP`], because a rate limiter legitimately names a window longer than
/// any backoff we would pick for ourselves, but not without bound: this stops a
/// stray or hostile value from parking an effect for a day.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(300);
/// How long a graceful shutdown waits for effects to drain before abandoning a
/// stuck one (its invocation stays `running` and replays next start).
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the retention sweeper runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Observable state for one effect, shared with the runtime (for `/status`) and
/// the skip endpoint. Holds no reference to the runtime, so nothing cycles.
pub struct EffectShared {
    pub name: String,
    /// The event types this effect subscribes to, or `None` for `all_events()`. On the
    /// handle for the same reason a projector's is.
    pub sources: Option<Vec<String>>,
    position: AtomicU64,
    shutdown: AtomicBool,
    /// How many times the *current* position has failed in a row while retrying.
    /// A terminal skip never touches this, so a non-zero value means a genuine wedge
    /// and nothing else.
    consecutive_failures: AtomicU64,
    last_error: Mutex<Option<String>>,
    /// Cumulative count of positions abandoned by a terminal (non-retryable) failure,
    /// e.g. a `reveal()` of an erased subject. Distinct from `consecutive_failures`: a
    /// terminal skip advances rather than wedges, so it must not read as a wedge.
    terminal_skips: AtomicU64,
    last_terminal_error: Mutex<Option<String>>,
    /// The position an operator asked to skip, or `0` for none (no event sits at
    /// position 0).
    skip_position: AtomicU64,
    /// When this effect started, so a retry deadline can be held as a monotonic
    /// offset from it.
    started: Instant,
    /// Millis since [`EffectShared::started`] at which the current backoff expires, or
    /// `0` for "not waiting". Monotonic rather than wall clock on both ends: the
    /// server's clock can step, and the reader's clock is a different machine's, so a
    /// deadline published as an instant would render as a negative or hour-long
    /// countdown for a retry that is actually 400ms away. A remaining duration is
    /// immune to both.
    ///
    /// Zero is a safe sentinel: `retry_delay` never returns less than [`BACKOFF_BASE`],
    /// so a real deadline is never at offset zero.
    retry_at_ms: AtomicU64,
    /// Set when a verify-mode check found a broken invariant. The driver stops
    /// rather than retries: a divergence is not a transient failure, and every
    /// later position would be processed on the strength of an assumption that has
    /// just been shown false.
    quarantined: AtomicBool,
}

impl EffectShared {
    /// The last watermark this effect has processed every matching event up to.
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    /// How many times the current (stuck) invocation has failed in a row, `0`
    /// when healthy. A terminal skip never bumps this, so any non-zero value is a
    /// genuine wedge (a position retrying under backoff), not a skipped one.
    pub fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// The last error the current invocation hit, if it is wedged. Cleared on the next
    /// success; a terminal skip records into `last_terminal_error` instead.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many positions this effect has abandoned to a terminal failure since boot.
    /// Non-zero means data an invocation needed is permanently gone (an erased subject),
    /// not that the effect is stuck.
    pub fn terminal_skips(&self) -> u64 {
        self.terminal_skips.load(Ordering::Relaxed)
    }

    /// The error from the most recent terminal skip, if any. Unlike `last_error` it is
    /// not cleared on a later success: it is a durable record of an abandoned position.
    pub fn last_terminal_error(&self) -> Option<String> {
        self.last_terminal_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Ask the driver to skip `position`: an explicit, manual operator action to
    /// advance past a genuinely unprocessable event.
    pub fn request_skip(&self, position: u64) {
        self.skip_position.store(position, Ordering::Relaxed);
    }

    fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn record_failure(&self, message: &str) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(message.to_owned());
    }

    fn clear_failures(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// How long until the next retry attempt, or `None` when nothing is waiting.
    ///
    /// Saturates at zero rather than going negative: the deadline can pass between
    /// this load and the driver actually waking.
    pub fn retry_in_ms(&self) -> Option<u64> {
        match self.retry_at_ms.load(Ordering::Relaxed) {
            0 => None,
            due => Some(due.saturating_sub(self.elapsed_ms())),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn set_retry_deadline(&self, delay: Duration) {
        let due = self.elapsed_ms().saturating_add(delay.as_millis() as u64);
        self.retry_at_ms.store(due, Ordering::Relaxed);
    }

    fn clear_retry_deadline(&self) {
        self.retry_at_ms.store(0, Ordering::Relaxed);
    }

    /// This effect's health in one word, against a log head.
    ///
    /// Derived here rather than by each reader so `/status`, the introspection API
    /// and any dashboard cannot disagree about what "stuck" means.
    ///
    /// The order is load-bearing. A quarantine outranks everything because
    /// [`EffectShared::restore_quarantine`] sets the flag and `last_error` but never
    /// touches `consecutive_failures`, so an effect quarantined by an *earlier*
    /// process has a zero failure count and would otherwise read as merely lagging. A
    /// wedge outranks lag because a wedged effect lags precisely because it is
    /// wedged, and reporting the symptom would bury the cause.
    ///
    /// `terminal_skips` is deliberately not a state here: it is cumulative and never
    /// cleared, so a label derived from it would stick for the life of the process
    /// and hide a later wedge.
    pub fn state(&self, head: u64) -> &'static str {
        if self.quarantined() {
            "quarantined"
        } else if self.consecutive_failures() > 0 {
            // Both an invocation retrying under backoff and the driver re-subscribing
            // after a store error land here. Both are stuck and retrying.
            "wedged"
        } else if self.position() < head {
            "lagging"
        } else {
            "healthy"
        }
    }

    /// Whether a verify-mode check stopped this effect. Unlike a wedge, nothing
    /// clears this on its own.
    pub fn quarantined(&self) -> bool {
        self.quarantined.load(Ordering::Relaxed)
    }

    /// Re-apply a quarantine recorded by an earlier process, so `/status` reports it
    /// the same way whether or not the server has restarted since.
    fn restore_quarantine(&self, position: u64, reason: &str) {
        self.quarantined.store(true, Ordering::Relaxed);
        self.position.store(position, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(reason.to_owned());
    }

    /// Stop the effect after a broken invariant, recording what broke.
    fn quarantine(&self, violation: &Violation) {
        tracing::error!("effect `{}` quarantined: {violation}", self.name);
        self.quarantined.store(true, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(violation.to_string());
    }
    /// Record a terminal skip: count it and keep its message, without touching the wedge
    /// counter (the position is abandoned, not stuck). Pair with `clear_failures` so any
    /// wedge state from earlier retries of the same position is reset.
    fn record_terminal_skip(&self, message: &str) {
        self.terminal_skips.fetch_add(1, Ordering::Relaxed);
        *self
            .last_terminal_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(message.to_owned());
    }
}

/// The join handles for the effect threads and the sweeper, kept by the server so
/// it can drain them on shutdown (before the write coordinator, since effects
/// append through commands).
pub struct EffectRuntime {
    shared: Vec<Arc<EffectShared>>,
    joins: Vec<JoinHandle<()>>,
    sweeper: Sweeper,
}

impl EffectRuntime {
    /// Clones of the shared handles, for the runtime to read positions in
    /// `/status`.
    pub fn shared_handles(&self) -> Vec<Arc<EffectShared>> {
        self.shared.iter().map(Arc::clone).collect()
    }

    /// Signal every effect and the sweeper to stop, then join them, abandoning
    /// any thread that has not drained within [`SHUTDOWN_JOIN_TIMEOUT`] (a stuck
    /// invocation stays `running` and replays next start).
    pub fn shutdown_and_join(self) {
        let EffectRuntime {
            shared,
            joins,
            sweeper,
        } = self;
        for handle in &shared {
            handle.stop();
        }
        sweeper.signal_stop();

        let (tx, rx) = mpsc::channel();
        let joiner = thread::Builder::new()
            .name("effect-join".to_owned())
            .spawn(move || {
                for join in joins {
                    if let Err(err) = join.join() {
                        tracing::error!("an effect thread panicked: {err:?}");
                    }
                }
                sweeper.join();
                let _ = tx.send(());
            });
        let joiner = match joiner {
            Ok(joiner) => joiner,
            Err(err) => {
                tracing::error!("spawning the effect joiner failed: {err}");
                return;
            }
        };
        match rx.recv_timeout(SHUTDOWN_JOIN_TIMEOUT) {
            Ok(()) => {
                let _ = joiner.join();
            }
            Err(_) => tracing::warn!(
                "effect drain timed out after {}s; leaving stuck invocation(s) to replay next start",
                SHUTDOWN_JOIN_TIMEOUT.as_secs()
            ),
        }
    }
}

/// The retention sweeper thread and the signal that wakes it to stop.
struct Sweeper {
    stop: Arc<(Mutex<bool>, Condvar)>,
    join: JoinHandle<()>,
}

impl Sweeper {
    fn signal_stop(&self) {
        let (lock, cvar) = &*self.stop;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cvar.notify_all();
    }

    fn join(self) {
        if let Err(err) = self.join.join() {
            tracing::error!("the retention sweeper thread panicked: {err:?}");
        }
    }
}

/// Start one thread per effect plus the retention sweeper. The threads hold
/// `Arc<Runtime>` (for `invoke_command` and the boundary fold); the runtime does not hold the
/// returned [`EffectRuntime`], so nothing cycles.
pub fn start_all(
    effects: Vec<Arc<EffectUnit>>,
    runtime: &Arc<Runtime>,
    http: Arc<dyn HttpClient>,
    config: &Config,
) -> anyhow::Result<EffectRuntime> {
    let mut shared = Vec::with_capacity(effects.len());
    let mut joins = Vec::with_capacity(effects.len());
    for unit in effects {
        let (handle, join) = spawn(unit, Arc::clone(runtime), Arc::clone(&http))?;
        shared.push(handle);
        joins.push(join);
    }
    let sweeper = spawn_sweeper(Arc::clone(runtime), config)?;
    Ok(EffectRuntime {
        shared,
        joins,
        sweeper,
    })
}

fn spawn(
    unit: Arc<EffectUnit>,
    runtime: Arc<Runtime>,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<(Arc<EffectShared>, JoinHandle<()>)> {
    let ModuleDef::Effect { name, sources } = &unit.loaded.def else {
        anyhow::bail!("spawn called on a non-effect module");
    };
    let sources = EventSpec::source_types(sources)
        .map(|types| types.into_iter().map(str::to_owned).collect());
    let name = name.clone();
    let resume = runtime.effect_resume_after(&name)?;
    for position in runtime.running_with_hash_mismatch(&name, &unit.loaded.source_hash)? {
        tracing::warn!(
            "effect `{name}` has an in-flight invocation at position {position} recorded under a \
             different script hash; replaying it against the current code"
        );
    }

    let shared = Arc::new(EffectShared {
        name: name.clone(),
        sources,
        position: AtomicU64::new(resume),
        shutdown: AtomicBool::new(false),
        consecutive_failures: AtomicU64::new(0),
        last_error: Mutex::new(None),
        terminal_skips: AtomicU64::new(0),
        last_terminal_error: Mutex::new(None),
        skip_position: AtomicU64::new(0),
        started: Instant::now(),
        retry_at_ms: AtomicU64::new(0),
        quarantined: AtomicBool::new(false),
    });
    let task_shared = Arc::clone(&shared);
    let join = thread::Builder::new()
        .name(format!("effect-{name}"))
        .spawn(move || run(task_shared, unit, runtime, http))
        .with_context(|| format!("spawning effect `{name}`"))?;
    Ok((shared, join))
}

fn run(
    shared: Arc<EffectShared>,
    unit: Arc<EffectUnit>,
    runtime: Arc<Runtime>,
    http: Arc<dyn HttpClient>,
) {
    // Supervise the driver: a transient store or op-DB error must not silently
    // kill the effect. It surfaces as a wedge (so `/status` shows it) and the
    // driver re-subscribes with capped backoff, matching the invocation-level
    // "retry forever" promise. `run_inner` returns `Ok` only on a clean stop.
    let mut attempt: u32 = 0;
    loop {
        // Whether the driver got as far as re-subscribing. A run that did is a
        // recovery however it ended: the ladder is for a driver that cannot start,
        // and without this a few unrelated blips over a process's life park every
        // later re-subscribe at the 60s cap, now published as a countdown.
        let mut subscribed = false;
        match run_inner(&shared, &unit, &runtime, http.as_ref(), &mut subscribed) {
            Ok(()) => break,
            Err(err) => {
                if shared.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                if subscribed {
                    attempt = 0;
                }
                shared.record_failure(&format!("driver: {err:#}"));
                tracing::error!(
                    "effect `{}` driver error (attempt {}): {err:#}",
                    shared.name,
                    attempt + 1
                );
                let delay = backoff(attempt);
                shared.set_retry_deadline(delay);
                let stop = sleep_watching(&shared, None, delay);
                shared.clear_retry_deadline();
                if stop {
                    break;
                }
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// Whether an invocation finished, or the driver should stop mid-wedge.
enum Progress {
    /// The invocation reached a terminal state (completed or skipped); advance.
    Advanced,
    /// Shutdown fired while the invocation was wedged; leave it `running`.
    Interrupted,
}

fn run_inner(
    shared: &EffectShared,
    unit: &EffectUnit,
    runtime: &Arc<Runtime>,
    http: &dyn HttpClient,
    subscribed: &mut bool,
) -> anyhow::Result<()> {
    let ModuleDef::Effect { name, sources } = &unit.loaded.def else {
        anyhow::bail!("run called on a non-effect module");
    };
    // Sources filter on plaintext fields only (check-time rejects encrypted source
    // constraints), so no key store is needed to lower them.
    let query = dispatch::to_query(sources, runtime.events_map(), None)?;
    // A recorded quarantine outlives the process that found it. Refusing to start is
    // the point: an effect stopped for a broken invariant must not resume because
    // someone restarted the server, which is the ordinary reaction to a stuck effect.
    if let Some((position, reason)) = runtime.effect_quarantine(name)? {
        shared.restore_quarantine(position, &reason);
        tracing::error!(
            "effect `{name}` stays quarantined from position {position} across this restart: {reason}"
        );
        return Ok(());
    }
    let resume = runtime.effect_resume_after(name)?;
    let mut sub = runtime.store().subscribe(query, Position::new(resume));
    // Back on the log, so whatever failure the supervisor recorded for the error that
    // sent us round its loop is over. Nothing else clears it: on an idle effect there
    // is no next invocation to, so a single transient store error left `state()`
    // reporting `wedged` until an unrelated event happened to arrive. An invocation
    // wedge is not lost here, since that position replays below and records its own
    // failure again.
    *subscribed = true;
    shared.clear_failures();
    loop {
        let batch = sub
            .poll_batch()
            .map_err(|err| anyhow::anyhow!("reading events: {err}"))?;
        if !batch.is_empty() {
            for (position, event) in &batch {
                match run_invocation(
                    shared,
                    name,
                    &unit.loaded,
                    runtime,
                    http,
                    position.get(),
                    event,
                )? {
                    Progress::Advanced => {}
                    // Leave the watermark where it is: the running invocation
                    // replays next start.
                    Progress::Interrupted => return Ok(()),
                }
            }
            advance_watermark(shared, runtime, name, sub.position().get())?;
            continue;
        }
        // Caught up: advance past any non-matching tail so a restart does not
        // re-scan the whole log, then idle.
        advance_watermark(shared, runtime, name, sub.position().get())?;
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if let WaitOutcome::Closed = sub.wait_timeout(IDLE_POLL) {
            break;
        }
    }
    Ok(())
}

/// Persist and publish the effect's watermark, but only forward. It is safe to
/// store here because every matching position up to it is now terminal.
fn advance_watermark(
    shared: &EffectShared,
    runtime: &Runtime,
    name: &str,
    watermark: u64,
) -> anyhow::Result<()> {
    if watermark > shared.position.load(Ordering::Relaxed) {
        runtime.set_effect_watermark(name, watermark)?;
        shared.position.store(watermark, Ordering::Relaxed);
    }
    Ok(())
}

fn run_invocation(
    shared: &EffectShared,
    effect: &str,
    loaded: &LoadedModule,
    runtime: &Arc<Runtime>,
    http: &dyn HttpClient,
    position: u64,
    event: &Event,
) -> anyhow::Result<Progress> {
    match runtime.begin_invocation(
        effect,
        position,
        &loaded.source_hash,
        &runtime::now_rfc3339(),
    )? {
        InvocationState::AlreadyTerminal => return Ok(Progress::Advanced),
        InvocationState::Running => {}
    }
    let (env, data) =
        envelope::decode(event.data()).map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
    let event_type = event.event_type().to_owned();

    let mut attempt: u32 = 0;
    loop {
        // Honor an operator skip only once this position is genuinely wedged (it
        // has failed at least once). Checking before the first attempt would let a
        // skip requested for a not-yet-reached position drop a healthy event.
        if attempt > 0 && shared.skip_position.load(Ordering::Relaxed) == position {
            honor_skip(shared, || {
                runtime.complete_invocation(effect, position, &runtime::now_rfc3339())
            })?;
            tracing::warn!(
                "effect `{effect}` skipped wedged position {position} by operator request"
            );
            return Ok(Progress::Advanced);
        }
        match try_invocation(
            effect,
            position,
            &env,
            event,
            &event_type,
            &data,
            loaded,
            runtime,
            http,
        ) {
            Ok(()) => {
                // Complete first, then check. The live run has already performed and
                // journaled its side effects, so this position's work is genuinely
                // done; leaving the row `running` so the check could report on it
                // would make the next boot re-enter the handler in `Live` mode and
                // perform for real the very call the sealed replay refused. The
                // detection would become the double-fire it exists to prevent.
                runtime.complete_invocation(effect, position, &runtime::now_rfc3339())?;
                shared.clear_failures();
                if runtime.verify() {
                    let violations = verify_replay(
                        effect,
                        position,
                        &env,
                        event,
                        &event_type,
                        &data,
                        loaded,
                        runtime,
                    );
                    if let Some(violation) = violations.first() {
                        // Durable, so the restart a wedged effect invites does not
                        // silently clear it. The watermark is deliberately left where
                        // it is: this position is terminal, but nothing past it should
                        // be processed until an operator has looked.
                        runtime.quarantine_effect(effect, position, &violation.to_string())?;
                        shared.quarantine(violation);
                        return Ok(Progress::Interrupted);
                    }
                }
                return Ok(Progress::Advanced);
            }
            // A terminal failure (an erased subject a `reveal()` needed) cannot be
            // recovered by retrying, so complete the invocation and move on rather
            // than wedge forever.
            Err(failure) if failure.terminal => {
                tracing::error!(
                    "effect `{effect}` invocation at position {position} failed terminally: {}",
                    failure.message
                );
                // Record durably before touching the shared counters, as the success
                // arm does. A failing op-DB write here propagates and the position is
                // retried, so mutating first would count the skip twice and clear the
                // wedge state for an invocation that never actually completed.
                runtime.complete_invocation(effect, position, &runtime::now_rfc3339())?;
                shared.record_terminal_skip(&failure.message);
                shared.clear_failures();
                return Ok(Progress::Advanced);
            }
            Err(failure) => {
                shared.record_failure(&failure.message);
                let delay = retry_delay(attempt, failure.retry_after);
                tracing::error!(
                    "effect `{effect}` invocation at position {position} failed (attempt {}), \
                     retrying in {delay:?}: {}",
                    attempt + 1,
                    failure.message
                );
                shared.set_retry_deadline(delay);
                let stop = sleep_watching(shared, Some(position), delay);
                shared.clear_retry_deadline();
                if stop {
                    return Ok(Progress::Interrupted);
                }
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// Run the effect's `handle` once against a fresh journaling host. Journaled
/// calls replay from the journal; the unjournaled tail runs live.
#[allow(clippy::too_many_arguments)]
fn try_invocation(
    effect: &str,
    position: u64,
    env: &Envelope,
    event: &Event,
    event_type: &str,
    data: &Value,
    loaded: &LoadedModule,
    runtime: &Arc<Runtime>,
    http: &dyn HttpClient,
) -> Result<(), InvocationFailure> {
    let host = EffectHostImpl {
        runtime,
        http,
        env: env.clone(),
        effect: effect.to_owned(),
        position,
        disambiguators: RefCell::new(HashMap::new()),
        terminal: Cell::new(false),
        retry_after: Cell::new(None),
        mode: HostMode::Live,
        trace: RefCell::new(Vec::new()),
        sealed_miss: RefCell::new(None),
    };
    let inv = Invocation {
        events: runtime.events_map(),
        store: runtime.store(),
        keystore: runtime.keystore(),
        position,
        event,
        env,
        event_type,
        data,
        verify: runtime.verify(),
    };
    run_handle(loaded, &inv, &host).map_err(|err| InvocationFailure {
        message: format!("{err:#}"),
        terminal: host.terminal.get(),
        retry_after: host.retry_after.get(),
    })
}

/// One event to route through an effect, and everything the boundary needs to fold
/// state for it. Bundled so [`run_handle`] stays under the argument limit.
pub(crate) struct Invocation<'a> {
    pub events: &'a EventDefs,
    pub store: &'a WriteHandle,
    pub keystore: Option<&'a KeyStore>,
    /// The triggering event's position, and so the inclusive upper bound on the
    /// fold. This is what makes the state deterministic: it is a function of the
    /// log prefix and this position, not of how far the log had run by the time
    /// the handler executed.
    pub position: u64,
    pub event: &'a Event,
    pub env: &'a Envelope,
    pub event_type: &'a str,
    pub data: &'a Value,
    /// Whether the boundary fold is checked for determinism on this run.
    pub verify: bool,
}

/// Route one event through an effect's `handle` and run every arm whose clause selects
/// it, in declaration order, each against the state its boundary folds.
///
/// This is the dispatch half only. The durable half (journal, retry, completion) stays
/// in [`try_invocation`], which is why the host arrives as a trait object: anything
/// that can serve the impure builtins can drive a handler, including `hekla test`.
///
/// The fold is deliberately **not** journaled. It is derived from the log prefix and
/// the triggering position, so every attempt and every replay reproduces it exactly;
/// recording it would buy nothing and would freeze a point-in-time answer the way a
/// journaled read once did.
pub(crate) fn run_handle(
    loaded: &LoadedModule,
    inv: &Invocation<'_>,
    host: &dyn EffectHost,
) -> anyhow::Result<()> {
    let events = inv.events;
    Module::with_temp_heap(|module| {
        let ctx = EffectCtx { host };
        let frozen = &loaded.module;
        let handle_owned = frozen
            .get_option("handle")?
            .ok_or_else(|| anyhow::anyhow!("effect has no handle map"))?;
        let handle = parse_event_dispatch(thaw(&handle_owned, &module))
            .map_err(|err| anyhow::anyhow!("`handle` {err}"))?;
        // `None` matches how the subscription is lowered: filtering a
        // subject-encrypted field in a `handle` key is a static error.
        let lowered = lower_dispatch(&handle, events, None)
            .map_err(|err| anyhow::anyhow!("`handle` {err}"))?;
        let selected: Vec<usize> = lowered
            .iter()
            .enumerate()
            .filter(|(_, item)| arm_selects(item.as_ref(), inv.event.as_ref()))
            .map(|(index, _)| index)
            .collect();
        // No arm selects this event, so the effect has decided it needs no side
        // effect. The invocation still completes, so the cursor advances past it.
        // Checked before the boundary, so an unselected event pays for no fold.
        if selected.is_empty() {
            return Ok(());
        }
        let value = alloc_event(
            &module,
            inv.env.event_id,
            &inv.env.timestamp,
            inv.event_type,
            inv.data,
            events.get(inv.event_type),
        );

        // The boundary is scoped by the triggering event, so `query` takes it where a
        // command's takes `input`.
        let boundary = match frozen.get_option("query")? {
            Some(func) => {
                let result =
                    call_handler_with_query_ctx(&module, thaw(&func, &module), &[value], MAX_TICKS)
                        .map_err(|err| anyhow::anyhow!("query() failed: {err}"))?;
                let specs =
                    parse_event_specs(result).map_err(|err| anyhow::anyhow!("query() {err}"))?;
                Some(dispatch::to_query(&specs, events, inv.keystore)?)
            }
            None => None,
        };
        let state = match &boundary {
            Some(query) => {
                let plan = dispatch::FoldPlan::build(frozen, &module, events, inv.keystore)?;
                dispatch::fold_boundary(
                    &module,
                    &dispatch::FoldInputs {
                        frozen,
                        store: inv.store,
                        query,
                        plan: &plan,
                        events,
                        resume_after: Position::ZERO,
                        upto: Some(inv.position),
                        verify: inv.verify,
                    },
                )?
                .0
            }
            // Resolved only here: a boundaried effect's state comes from the fold,
            // which builds its own `initial` in the heap it folds in.
            None => initial_state(frozen, &module)
                .map_err(|err| anyhow::anyhow!("initial failed: {err}"))?,
        };

        // Every selecting arm runs in declaration order, so a replay journals and
        // replays the same call sequence. Each sees the same state: the fold is of
        // the log, not of what an earlier arm did.
        for index in selected {
            let arm = &handle.arms()[index];
            call_handler_with_effect_ctx(&module, arm.func, &[value, state], MAX_TICKS, &ctx)
                .map_err(|err| {
                    dispatch::effect_handle_error(&handle.label("handle", arm.spec.as_ref()), err)
                })?;
        }
        Ok(())
    })
}

/// A failed invocation attempt: its message, whether the failure is terminal (no
/// retry can succeed, e.g. a `reveal()` of an erased subject) rather than a wedge,
/// and how long the peer asked us to wait before the next attempt.
struct InvocationFailure {
    message: String,
    terminal: bool,
    retry_after: Option<Duration>,
}

/// The wedge backoff for `attempt`, doubling from [`BACKOFF_BASE`] up to
/// [`BACKOFF_CAP`].
fn backoff(attempt: u32) -> Duration {
    BACKOFF_BASE
        .saturating_mul(1u32 << attempt.min(10))
        .min(BACKOFF_CAP)
}

/// Whether an HTTP status means "send this same request again", in which case the
/// runtime absorbs it into the wedge rather than handing it to the script.
///
/// 408 (request timeout), 425 (too early) and 429 (too many requests) join every
/// 5xx: each names a condition that clears on its own, with the request unchanged.
/// 429 especially cannot be left to the script, because a response that reaches
/// Starlark is journaled: an effect that raised on one would replay the recorded
/// 429 on every retry and wedge forever without re-sending, leaving an operator
/// skip (which abandons the work) as the only way out.
pub(crate) fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || status >= 500
}

/// The `Retry-After` a retryable response asked for, if it named one in seconds.
///
/// The header's other legal form is an HTTP-date (RFC 9110 10.2.3). Honoring that
/// would mean taking on a date parser and turning the peer's clock into a duration
/// against ours, so a date reads as absent and the wedge backoff applies unchanged.
fn retry_after_hint(headers: &[(String, String)]) -> Option<Duration> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value.trim())?;
    // Rejects a date, a negative, and anything else non-numeric, all as "absent".
    value.parse::<u64>().ok().map(Duration::from_secs)
}

/// How long to wait before the next attempt: never sooner than the wedge backoff,
/// and never sooner than a `Retry-After` asked for.
///
/// Taking the larger of the two rather than the header alone is what matters on a
/// limiter that keeps answering `Retry-After: 1`. Obeying that literally would
/// retry once a second forever, so the backoff still grows underneath it.
fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(after) => backoff(attempt).max(after.min(RETRY_AFTER_CAP)),
        None => backoff(attempt),
    }
}

/// Honor a pending operator skip by completing the wedged invocation. The request
/// is cleared only once the completion is durable, so a failed op-DB write leaves
/// the skip pending for the driver's next pass rather than losing it.
fn honor_skip(
    shared: &EffectShared,
    complete: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    complete()?;
    shared.skip_position.store(0, Ordering::Relaxed);
    shared.clear_failures();
    Ok(())
}

/// Sleep up to `total`, returning `true` if shutdown fired (the caller stops
/// mid-wedge). A pending skip for `skip` also returns early (`false`), so the retry
/// loop re-checks it at the top; the driver supervisor passes `None`, having no
/// per-event skip to honor.
fn sleep_watching(shared: &EffectShared, skip: Option<u64>, total: Duration) -> bool {
    let tick = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < total {
        if shared.shutdown.load(Ordering::Relaxed) {
            return true;
        }
        if skip.is_some_and(|position| shared.skip_position.load(Ordering::Relaxed) == position) {
            return false;
        }
        thread::sleep(tick.min(total - waited));
        waited += tick;
    }
    shared.shutdown.load(Ordering::Relaxed)
}

// --- the journaling host ---------------------------------------------------

/// The [`EffectHost`] implementation: it journals every impure call against the
/// operational DB and performs the real side effect only on a journal miss.
struct EffectHostImpl<'a> {
    runtime: &'a Arc<Runtime>,
    http: &'a dyn HttpClient,
    env: Envelope,
    effect: String,
    position: u64,
    /// Per-call-hash counters, reset for each invocation run, so identical
    /// repeated calls get 0, 1, 2 ... and a replay lines them up.
    disambiguators: RefCell<HashMap<String, u64>>,
    /// Set when a `reveal()` hit an erased subject: the failure is terminal (the
    /// data is gone, no retry recovers it), so the driver completes rather than
    /// wedges the invocation.
    terminal: Cell<bool>,
    /// The `Retry-After` a retryable HTTP status named, for the driver's backoff. A
    /// `Cell` for the same reason as `terminal`: it has to reach the driver past the
    /// starlark boundary, which flattens a host error down to its message.
    retry_after: Cell<Option<Duration>>,
    /// Whether a journal miss may perform the call. [`HostMode::Sealed`] is what
    /// makes the replay check safe to run: it cannot fire the side effect it is
    /// checking for.
    mode: HostMode,
    /// Every journaled call this run reached, in order. The replay check compares
    /// it against what the journal holds, which is how a handler that changed the
    /// *order* of its calls gets caught: the journal is keyed by call content, so
    /// a reordered run still hits every entry and would otherwise look faithful.
    trace: RefCell<Vec<CallKey>>,
    /// The first call a sealed run found no journal entry for. Held separately from
    /// the error it raises so the caller can report which call diverged rather than
    /// parsing a message.
    sealed_miss: RefCell<Option<CallKey>>,
}

/// The identity of one journaled call within an invocation: its content hash and
/// which repeat of that content it is.
pub(crate) type CallKey = (String, u64);

/// Whether a host may perform side effects, or only replay recorded ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostMode {
    /// A journal miss performs the call and records it. The live path.
    Live,
    /// A journal miss is a violation: nothing is performed, and the run stops.
    /// Used by the replay check, so verifying an invocation can never repeat it.
    Sealed,
}

impl EffectHostImpl<'_> {
    /// The journal wrapper enforcing the op-DB lock discipline: look up (short
    /// lock), run the side effect with no lock held, record (short lock). The
    /// side effect closure receives the call's disambiguator (for a deterministic
    /// idempotency key).
    ///
    /// `kind` is recorded alongside the result and is otherwise unrecoverable: it
    /// only exists inside the pre-image of `call_hash`, so without the column a
    /// stored row can say what a call returned but not what it was.
    fn journaled<F>(&self, kind: &str, call_hash: &str, run: F) -> anyhow::Result<Value>
    where
        F: FnOnce(u64) -> anyhow::Result<Value>,
    {
        let disambiguator = self.next_disambiguator(call_hash);
        self.trace
            .borrow_mut()
            .push((call_hash.to_owned(), disambiguator));
        if let Some(recorded) =
            self.runtime
                .journal_get(&self.effect, self.position, call_hash, disambiguator)?
        {
            return serde_json::from_str(&recorded).context("decoding a journaled call result");
        }
        // A sealed run stops here rather than performing anything. The miss *is* the
        // finding: the handler reached a call the recorded run did not make, which
        // on a real retry is the double-fire this check exists to detect.
        if self.mode == HostMode::Sealed {
            *self.sealed_miss.borrow_mut() = Some((call_hash.to_owned(), disambiguator));
            anyhow::bail!("sealed replay reached a call with no journal entry");
        }
        let result = run(disambiguator)?;
        let encoded = serde_json::to_string(&result).context("encoding a call result")?;
        self.runtime.journal_put(
            &self.effect,
            self.position,
            call_hash,
            disambiguator,
            kind,
            &encoded,
            &runtime::now_rfc3339(),
        )?;
        Ok(result)
    }

    fn next_disambiguator(&self, call_hash: &str) -> u64 {
        let mut map = self.disambiguators.borrow_mut();
        let counter = map.entry(call_hash.to_owned()).or_insert(0);
        let value = *counter;
        *counter += 1;
        value
    }
}

/// The content-hash key of a journaled call: SHA-256 over a canonical JSON of the
/// call kind and its arguments (`serde_json::Value` sorts object keys).
fn call_hash(kind: &str, call: &Value) -> String {
    let canonical = json!({ "kind": kind, "call": call });
    crate::hash::sha256_hex(canonical.to_string().as_bytes())
}

/// Collapse an anyhow cause chain into a single flat message.
///
/// A host error leaves this module through a starlark builtin, where it becomes
/// `starlark::ErrorKind::Native` and is rendered with plain `Display`: every cause
/// under the outermost context is dropped long before the wedge records
/// `last_error`. Rendering the chain here keeps the reason attached to the call
/// that names it.
fn flatten_chain(err: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("{err:#}")
}

impl EffectHost for EffectHostImpl<'_> {
    fn http(
        &self,
        method: &str,
        url: &str,
        headers: Vec<(String, String)>,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let headers_sorted: BTreeMap<&str, &str> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let hash = call_hash(
            "http",
            &json!({ "method": method, "url": url, "headers": headers_sorted, "body": body }),
        );
        self.journaled("http", &hash, |_| {
            let body_bytes = match &body {
                Some(value) => Some(serde_json::to_vec(value).context("serialising an http body")?),
                None => None,
            };
            let mut request_headers = headers;
            if body_bytes.is_some()
                && !request_headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case("content-type"))
            {
                request_headers.push(("content-type".to_owned(), "application/json".to_owned()));
            }
            let request = HttpRequest {
                method: method.to_owned(),
                url: url.to_owned(),
                headers: request_headers,
                body: body_bytes,
            };
            let response = self
                .http
                .send(&request)
                .with_context(|| format!("http {method} {url}"))?;
            // A retryable status never reaches the script: surface it as a retryable
            // error so the wedge absorbs it, keeping any `Retry-After` for the
            // driver. Bailing here is before the journal write, which is exactly what
            // lets the next attempt re-send instead of replaying the refusal.
            if is_retryable_status(response.status) {
                self.retry_after.set(retry_after_hint(&response.headers));
                anyhow::bail!("http {method} {url} returned {}", response.status);
            }
            Ok(http_response_to_json(response))
        })
        .map_err(flatten_chain)
    }

    fn invoke_command(&self, name: &str, input: Value) -> anyhow::Result<Value> {
        let hash = call_hash(
            "invoke_command",
            &json!({ "command": name, "input": input }),
        );
        self.journaled("invoke_command", &hash, |disambiguator| {
            let idempotency_key =
                format!("{}:{}:{hash}:{disambiguator}", self.effect, self.position);
            let ctx = CommandContext::from_effect(self.env.correlation_id, self.env.event_id);
            let result =
                self.runtime
                    .execute_from_effect(name, input, &ctx, Some(&idempotency_key))?;
            Ok(json!({ "status": result.status, "body": result.body }))
        })
        .map_err(flatten_chain)
    }

    fn now(&self) -> anyhow::Result<String> {
        let hash = call_hash("now", &json!({}));
        let value = self
            .journaled("now", &hash, |_| Ok(Value::String(runtime::now_rfc3339())))
            .map_err(flatten_chain)?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("journaled now() was not a string"))
    }

    fn log(&self, message: &str) {
        // A sealed run is a verification pass over work that already happened, so
        // repeating its log lines would double every effect's trace output for no
        // information. The journaled calls are what the check reads.
        if self.mode == HostMode::Sealed {
            return;
        }
        tracing::info!("effect `{}` @ {}: {message}", self.effect, self.position);
    }

    fn erase(&self, subject_field: &str, subject_value: &str) -> anyhow::Result<bool> {
        // Auditable like `reveal`, and for a stronger reason: this one is the
        // irreversible half of the pair. Which is exactly why a sealed run stays
        // silent: it performs nothing, and an audit line for an erasure that did not
        // happen is worse than no line at all.
        if self.mode == HostMode::Live {
            tracing::info!(
                "effect `{}` @ {}: erase {subject_field}={subject_value}",
                self.effect,
                self.position
            );
        }
        let hash = call_hash(
            "erase",
            &json!({ "subject_field": subject_field, "subject_value": subject_value }),
        );
        let result = self.journaled("erase", &hash, |_| {
            let keystore = self.runtime.keystore().ok_or_else(|| {
                anyhow::anyhow!("erase() needs a master key, but none is configured")
            })?;
            Ok(json!(keystore.erase(subject_field, subject_value)?))
        })?;
        Ok(result.as_bool().unwrap_or(false))
    }

    fn reveal(
        &self,
        subject_field: &str,
        subject_value: &str,
        field: &str,
        ciphertext: &str,
    ) -> anyhow::Result<String> {
        // Auditable: every crossing of the decrypt boundary is traced, for a live
        // run. A sealed replay re-runs `reveal` (it is not journaled) but performs
        // nothing outward, so tracing it would double the audit trail.
        if self.mode == HostMode::Live {
            tracing::debug!(
                "effect `{}` @ {}: reveal {subject_field}={subject_value} field={field}",
                self.effect,
                self.position
            );
        }
        let keystore = self.runtime.keystore().ok_or_else(|| {
            anyhow::anyhow!("reveal() needs a master key, but none is configured")
        })?;
        match keystore
            .decrypt_subject(subject_field, subject_value, field, ciphertext)
            .map_err(flatten_chain)?
        {
            Some(plaintext) => Ok(plaintext),
            None => {
                // The subject was erased. No retry can recover the data, so mark this
                // invocation terminal rather than let it wedge forever.
                self.terminal.set(true);
                anyhow::bail!(
                    "reveal() cannot decrypt `{field}`: subject `{subject_field}` = `{subject_value}` has been erased"
                )
            }
        }
    }
}

fn http_response_to_json(response: HttpResponse) -> Value {
    let body = serde_json::from_slice::<Value>(&response.body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&response.body).into_owned()));
    // Headers are a multimap: each name maps to a list of values, so repeated
    // headers (for example several `set-cookie`) all survive rather than the last
    // silently winning.
    let mut headers: serde_json::Map<String, Value> = serde_json::Map::new();
    for (name, value) in response.headers {
        match headers
            .entry(name)
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            Value::Array(values) => values.push(Value::String(value)),
            _ => unreachable!("header entries are always arrays"),
        }
    }
    json!({ "status": response.status, "body": body, "headers": headers })
}

// --- the HTTP client seam --------------------------------------------------

/// A raw HTTP request: what the effect host hands the transport.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// A raw HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The transport behind the journaled `http.*` builtins. Returns the response for
/// any HTTP status (including 4xx and 5xx); only a transport-level failure is an
/// `Err`. Split out so tests substitute a deterministic stub.
pub trait HttpClient: Send + Sync {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse>;
}

/// The production transport, a blocking `ureq` agent with connect and overall
/// timeouts so a hung request cannot stall shutdown.
pub struct UreqClient {
    agent: Agent,
}

impl UreqClient {
    pub fn new() -> UreqClient {
        let config = Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)))
            // 4xx/5xx come back as responses to inspect, not transport errors.
            .http_status_as_error(false)
            .build();
        UreqClient {
            agent: Agent::new_with_config(config),
        }
    }

    fn without_body(
        &self,
        mut builder: ureq::RequestBuilder<WithoutBody>,
        request: &HttpRequest,
    ) -> anyhow::Result<HttpResponse> {
        for (key, value) in &request.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let response = builder
            .call()
            .map_err(|err| anyhow::anyhow!("http transport error: {err}"))?;
        read_response(response)
    }

    fn with_body(
        &self,
        mut builder: ureq::RequestBuilder<WithBody>,
        request: &HttpRequest,
    ) -> anyhow::Result<HttpResponse> {
        for (key, value) in &request.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let body = request.body.clone().unwrap_or_default();
        let response = builder
            .send(&body[..])
            .map_err(|err| anyhow::anyhow!("http transport error: {err}"))?;
        read_response(response)
    }
}

impl Default for UreqClient {
    fn default() -> UreqClient {
        UreqClient::new()
    }
}

impl HttpClient for UreqClient {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse> {
        match request.method.as_str() {
            "GET" => self.without_body(self.agent.get(&request.url), request),
            "DELETE" => self.without_body(self.agent.delete(&request.url), request),
            "POST" => self.with_body(self.agent.post(&request.url), request),
            "PUT" => self.with_body(self.agent.put(&request.url), request),
            "PATCH" => self.with_body(self.agent.patch(&request.url), request),
            other => anyhow::bail!("unsupported http method `{other}`"),
        }
    }
}

fn read_response(mut response: ureq::http::Response<ureq::Body>) -> anyhow::Result<HttpResponse> {
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|err| anyhow::anyhow!("reading http response body: {err}"))?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

// --- the retention sweeper -------------------------------------------------

fn spawn_sweeper(runtime: Arc<Runtime>, config: &Config) -> anyhow::Result<Sweeper> {
    let effect_days = config.retention.effect_journal_days;
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let thread_stop = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("effect-sweeper".to_owned())
        .spawn(move || sweeper_loop(&runtime, effect_days, &thread_stop))
        .context("spawning the retention sweeper")?;
    Ok(Sweeper { stop, join })
}

fn sweeper_loop(runtime: &Runtime, effect_days: u32, stop: &(Mutex<bool>, Condvar)) {
    loop {
        if let Err(err) = run_sweep(runtime, effect_days) {
            tracing::error!("retention sweep failed: {err:#}");
        }
        let (lock, cvar) = stop;
        let mut stopped = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if *stopped {
            break;
        }
        stopped = cvar
            .wait_timeout(stopped, SWEEP_INTERVAL)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        if *stopped {
            break;
        }
    }
}

/// A brief pause between sweep chunks, so a large backlog does not monopolise the
/// shared op-DB lock that every effect and command also needs.
const SWEEP_CHUNK_PAUSE: Duration = Duration::from_millis(10);

/// Sweep completed effect journals past their retention window, in bounded chunks
/// so the op-DB lock is never held across a long scan, yielding briefly between
/// chunks so the effect hot path is not starved.
fn run_sweep(runtime: &Runtime, effect_days: u32) -> anyhow::Result<()> {
    let effect_cutoff = runtime::rfc3339_days_ago(effect_days);
    while runtime.sweep_effect_journal(&effect_cutoff, SWEEP_CHUNK)? == SWEEP_CHUNK {
        thread::sleep(SWEEP_CHUNK_PAUSE);
    }
    Ok(())
}

// --- test support ----------------------------------------------------------

/// A deterministic [`HttpClient`] for tests: it records every request and returns
/// whatever its handler produces (given the zero-based call index), so a test can
/// assert on calls, program a status sequence, or simulate a transport failure.
pub struct StubHttpClient {
    calls: Mutex<Vec<HttpRequest>>,
    #[allow(clippy::type_complexity)]
    handler: Box<dyn Fn(usize, &HttpRequest) -> anyhow::Result<HttpResponse> + Send + Sync>,
}

impl StubHttpClient {
    pub fn new<F>(handler: F) -> StubHttpClient
    where
        F: Fn(usize, &HttpRequest) -> anyhow::Result<HttpResponse> + Send + Sync + 'static,
    {
        StubHttpClient {
            calls: Mutex::new(Vec::new()),
            handler: Box::new(handler),
        }
    }

    /// Always returns `200` with an empty JSON body.
    pub fn ok() -> StubHttpClient {
        StubHttpClient::status(200)
    }

    /// Always returns `status` with an empty JSON body. A 4xx lets a test drive an
    /// effect that inspects the status and returns without further side effects.
    pub fn status(status: u16) -> StubHttpClient {
        StubHttpClient::new(move |_, _| {
            Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            })
        })
    }

    /// The requests seen so far, in order.
    pub fn calls(&self) -> Vec<HttpRequest> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many requests have been sent.
    pub fn call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl HttpClient for StubHttpClient {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse> {
        let index = {
            let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
            calls.push(request.clone());
            calls.len() - 1
        };
        (self.handler)(index, request)
    }
}

// --- the replay check ------------------------------------------------------

/// An [`HttpClient`] that cannot send. A sealed host never reaches its transport,
/// because a journal miss stops the run before the call is performed; this makes
/// that unreachability explicit rather than relying on a stub that would quietly
/// succeed if the invariant ever broke.
struct SealedHttp;

impl HttpClient for SealedHttp {
    fn send(&self, _request: &HttpRequest) -> anyhow::Result<HttpResponse> {
        anyhow::bail!("a sealed replay tried to send an HTTP request")
    }
}

/// Re-run a recorded invocation against a sealed host and report every way it fails
/// to reproduce itself.
///
/// Safe against a live system by construction: the sealed host performs nothing, so
/// the worst outcome is a report. `reveal` is re-run because it is not journaled,
/// but it decrypts and returns without reaching anything outward.
///
/// Two shapes of divergence are caught, and the second is the one nothing else
/// detects: a call the journal has no entry for, and a journal entry the handler no
/// longer makes. Because the journal is keyed by call content rather than by
/// sequence, a handler that merely *reorders* its calls still hits every entry, so
/// comparing the visited set against the recorded set is what surfaces it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_replay(
    effect: &str,
    position: u64,
    env: &Envelope,
    event: &Event,
    event_type: &str,
    data: &Value,
    loaded: &LoadedModule,
    runtime: &Arc<Runtime>,
) -> Vec<Violation> {
    let sealed_http = SealedHttp;
    let host = EffectHostImpl {
        runtime,
        http: &sealed_http,
        env: env.clone(),
        effect: effect.to_owned(),
        position,
        disambiguators: RefCell::new(HashMap::new()),
        terminal: Cell::new(false),
        retry_after: Cell::new(None),
        mode: HostMode::Sealed,
        trace: RefCell::new(Vec::new()),
        sealed_miss: RefCell::new(None),
    };
    let inv = Invocation {
        events: runtime.events_map(),
        store: runtime.store(),
        keystore: runtime.keystore(),
        position,
        event,
        env,
        event_type,
        data,
        // The replay check is already a second run; folding twice inside it would
        // re-check determinism the live run already checked.
        verify: false,
    };

    let divergence = |detail: String| Violation::ReplayDivergence {
        effect: effect.to_owned(),
        position,
        detail,
    };

    let outcome = run_handle(loaded, &inv, &host);
    let visited = host.trace.borrow().clone();

    if let Err(err) = outcome {
        // A terminal failure is the documented cost of the `erase last` rule, not a
        // divergence: the replay re-runs the unjournaled `reveal` against a key the
        // invocation itself deleted, exactly as the live retry path already does.
        // Reporting it would quarantine every effect written the recommended way.
        if host.terminal.get() {
            tracing::debug!(
                "effect `{effect}` at {position} is not replayable by design (it erased a \
                 subject it revealed); skipping the replay check"
            );
            return Vec::new();
        }
        let miss = host.sealed_miss.borrow().clone();
        return vec![match miss {
            Some((hash, disambiguator)) => divergence(format!(
                "it reached a call with no journal entry (call {hash}, repeat {disambiguator}); \
                 a real retry would have performed it a second time"
            )),
            // No miss recorded and not terminal, so the handler itself failed on a
            // path the first run got through. That is a genuine surprise.
            None => divergence(format!("it failed part-way through: {err:#}")),
        }];
    }

    let recorded = match runtime.journal_keys(effect, position) {
        Ok(recorded) => recorded,
        Err(err) => {
            return vec![divergence(format!(
                "its journal could not be read: {err:#}"
            ))];
        }
    };

    // Ordered comparison. A subset test would be blind to exactly the case the
    // content-keyed journal cannot see on its own: `(call_hash, disambiguator)` pairs
    // are unique within an invocation, and a sealed run can never visit a key the
    // journal lacks (that path returned above), so equal-as-sets is guaranteed and
    // only the sequence carries new information.
    if visited != recorded {
        return vec![divergence(format!(
            "it made a different sequence of calls than the journal records \
             (journal {}, replay {})",
            render_keys(&recorded),
            render_keys(&visited)
        ))];
    }
    Vec::new()
}

/// Render a call sequence compactly: each hash is truncated, since the full digest
/// adds length without helping anyone reading the message.
fn render_keys(keys: &[CallKey]) -> String {
    let rendered: Vec<String> = keys
        .iter()
        .map(|(hash, disambiguator)| format!("{}#{disambiguator}", &hash[..8.min(hash.len())]))
        .collect();
    format!("[{}]", rendered.join(", "))
}
#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn test_shared() -> EffectShared {
        EffectShared {
            name: "test".to_owned(),
            sources: None,
            position: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            consecutive_failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
            terminal_skips: AtomicU64::new(0),
            last_terminal_error: Mutex::new(None),
            skip_position: AtomicU64::new(0),
            started: Instant::now(),
            retry_at_ms: AtomicU64::new(0),
            quarantined: AtomicBool::new(false),
        }
    }

    #[test]
    fn a_failed_completion_leaves_the_skip_request_pending() {
        let shared = test_shared();
        shared.request_skip(7);
        shared.record_failure("boom");

        let err = honor_skip(&shared, || anyhow::bail!("op-db is locked")).unwrap_err();
        assert!(err.to_string().contains("op-db is locked"));
        assert_eq!(
            shared.skip_position.load(Ordering::Relaxed),
            7,
            "a skip must survive a failed completion so the driver honors it on the next pass"
        );
        assert_eq!(
            shared.consecutive_failures(),
            1,
            "the position is still wedged"
        );

        honor_skip(&shared, || Ok(())).unwrap();
        assert_eq!(shared.skip_position.load(Ordering::Relaxed), 0);
        assert_eq!(shared.consecutive_failures(), 0);
        assert_eq!(shared.last_error(), None);
    }

    /// Both the skip and the shutdown paths return before the full backoff elapses,
    /// so each case is timed. Asserting only the returned bool would pass even with
    /// the early return deleted: the call would simply take the whole 30 seconds and
    /// still report `false`.
    #[test]
    fn sleep_watching_returns_early_only_for_the_wedged_position() {
        let long = Duration::from_secs(30);
        let shared = test_shared();
        shared.request_skip(7);

        let started = Instant::now();
        assert!(
            !sleep_watching(&shared, Some(7), long),
            "a skip is not a shutdown"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a skip for this position must cut the backoff short, waited {:?}",
            started.elapsed()
        );

        shared.stop();
        let started = Instant::now();
        assert!(
            sleep_watching(&shared, None, long),
            "a shutdown is reported as such"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown must cut the backoff short, waited {:?}",
            started.elapsed()
        );
    }

    /// The skip only applies to the position it names: a skip for a different
    /// position must let the backoff run its course.
    #[test]
    fn sleep_watching_ignores_a_skip_for_another_position() {
        let shared = test_shared();
        shared.request_skip(7);
        let started = Instant::now();
        assert!(!sleep_watching(
            &shared,
            Some(9),
            Duration::from_millis(300)
        ));
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "waited only {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(0), BACKOFF_BASE);
        assert_eq!(backoff(1), BACKOFF_BASE * 2);
        assert_eq!(backoff(100), BACKOFF_CAP);
    }

    /// The split between what the runtime absorbs and what the script decides on.
    /// Every status here is one an effect author could plausibly branch on, so the
    /// list is spelled out rather than left to the `matches!`.
    #[test]
    fn retryable_statuses_are_the_ones_that_clear_on_their_own() {
        for status in [408, 425, 429, 500, 502, 503, 504, 599] {
            assert!(is_retryable_status(status), "{status} must be absorbed");
        }
        for status in [200, 201, 204, 301, 400, 401, 403, 404, 409, 410, 418, 422] {
            assert!(
                !is_retryable_status(status),
                "{status} is a real result the handler decides on"
            );
        }
    }

    #[test]
    fn retry_after_is_read_only_in_its_delta_seconds_form() {
        let header =
            |value: &str| retry_after_hint(&[("Retry-After".to_owned(), value.to_owned())]);
        assert_eq!(header("30"), Some(Duration::from_secs(30)));
        assert_eq!(header("  30 "), Some(Duration::from_secs(30)));
        assert_eq!(header("0"), Some(Duration::ZERO));
        // The header's other legal form. Unparsed on purpose, so it reads as absent
        // and the wedge backoff stands rather than the effect stalling on a guess.
        assert_eq!(header("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(header("-5"), None);
        assert_eq!(header("soon"), None);
        assert_eq!(header(""), None);
        // A transport need not normalise the name, and the stub client does not.
        assert_eq!(
            retry_after_hint(&[("retry-after".to_owned(), "7".to_owned())]),
            Some(Duration::from_secs(7))
        );
        assert_eq!(retry_after_hint(&[]), None);
    }

    #[test]
    fn retry_after_raises_the_backoff_and_never_lowers_it() {
        assert_eq!(retry_delay(0, None), backoff(0), "nothing was asked for");
        // A window longer than our own backoff is honored, past `BACKOFF_CAP`.
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(120))),
            Duration::from_secs(120)
        );
        // A limiter answering `Retry-After: 1` forever must not pin the effect to one
        // attempt a second: the backoff keeps growing underneath the header.
        assert_eq!(retry_delay(100, Some(Duration::from_secs(1))), BACKOFF_CAP);
        // And a stray or hostile value cannot park the effect for a day.
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(86_400))),
            RETRY_AFTER_CAP
        );
    }

    #[test]
    fn call_hash_is_stable_and_argument_sensitive() {
        let a = call_hash("http", &json!({ "url": "x", "n": 1 }));
        let b = call_hash("http", &json!({ "n": 1, "url": "x" }));
        let c = call_hash("http", &json!({ "url": "y", "n": 1 }));
        assert_eq!(a, b, "key order must not change the hash");
        assert_ne!(a, c, "different arguments must hash differently");
    }

    #[test]
    fn http_response_json_parses_body_or_falls_back_to_text() {
        let json_body = http_response_to_json(HttpResponse {
            status: 200,
            headers: vec![("x".to_owned(), "y".to_owned())],
            body: br#"{"ok":true}"#.to_vec(),
        });
        assert_eq!(json_body["status"], 200);
        assert_eq!(json_body["body"]["ok"], true);

        let text_body = http_response_to_json(HttpResponse {
            status: 500,
            headers: Vec::new(),
            body: b"not json".to_vec(),
        });
        assert_eq!(text_body["body"], "not json");
    }

    #[test]
    fn http_response_json_keeps_every_repeated_header() {
        let value = http_response_to_json(HttpResponse {
            status: 200,
            headers: vec![
                ("set-cookie".to_owned(), "a=1".to_owned()),
                ("set-cookie".to_owned(), "b=2".to_owned()),
                ("content-type".to_owned(), "text/plain".to_owned()),
            ],
            body: Vec::new(),
        });
        assert_eq!(value["headers"]["set-cookie"][0], "a=1");
        assert_eq!(value["headers"]["set-cookie"][1], "b=2");
        assert_eq!(value["headers"]["content-type"][0], "text/plain");
    }

    #[test]
    fn stub_records_calls_and_returns_programmed_responses() {
        let stub = StubHttpClient::new(|index, _| {
            Ok(HttpResponse {
                status: if index == 0 { 503 } else { 200 },
                headers: Vec::new(),
                body: b"{}".to_vec(),
            })
        });
        let request = HttpRequest {
            method: "POST".to_owned(),
            url: "http://x".to_owned(),
            headers: Vec::new(),
            body: None,
        };
        assert_eq!(stub.send(&request).unwrap().status, 503);
        assert_eq!(stub.send(&request).unwrap().status, 200);
        assert_eq!(stub.call_count(), 2);
    }

    #[test]
    fn flatten_chain_folds_every_cause_into_the_display() {
        let err =
            anyhow::anyhow!("connection refused").context("http POST https://example.test/welcome");
        assert_eq!(
            format!("{err}"),
            "http POST https://example.test/welcome",
            "plain Display drops the cause, which is what the starlark boundary sees"
        );

        let flat = flatten_chain(err);
        assert_eq!(
            format!("{flat}"),
            "http POST https://example.test/welcome: connection refused"
        );
        // No cause left to render twice once the chain is folded in.
        assert_eq!(format!("{flat:#}"), format!("{flat}"));
    }
}
