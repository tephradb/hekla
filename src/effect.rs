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
//! a convenience. A response that reaches a handler is journaled, so an effect that
//! raised on one would replay the recorded 429 on every attempt and wedge forever
//! without ever re-sending. A `Retry-After` on such a response raises that
//! attempt's backoff, so a rate limiter's own window is waited out, not hammered.

use std::cell::RefCell;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use heklang::host::{Calls, Recorded};
use heklang::{Interpreter, Invocation as HekInvocation};
use tephra::{Position, WaitOutcome};

use crate::config::Config;
use crate::context::CommandContext;
use crate::hash::sha256_hex;
use crate::heklang_host::{HeklaHost, Journal, from_tephra, query_of_types};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::invariant::Violation;
use crate::loader::EffectUnit;
use crate::opdb::{InvocationState, SWEEP_CHUNK};
use crate::runtime::{self, Runtime};
use crate::schema::ModuleDef;

/// How long an idle, caught-up effect waits before polling again.
const IDLE_POLL: Duration = Duration::from_millis(250);
/// The ceiling on the wedge retry backoff, so a stuck effect keeps retrying at a
/// steady cadence rather than backing off unboundedly.
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// The base wedge retry backoff, doubled each attempt up to [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::from_millis(200);
/// The ceiling on a `Retry-After` a server asked for. The header is honored past
/// [`BACKOFF_CAP`], because a limiter naming a long window means it, but not without
/// bound: a hostile or broken peer must not be able to park an effect for a day.
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
    /// The event types this effect subscribes to. On the handle for the same reason a
    /// projector's is.
    pub sources: Vec<String>,
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
    /// `restore_quarantine` sets the flag and `last_error` but never
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
    /// any thread that has not drained within `SHUTDOWN_JOIN_TIMEOUT` (a stuck
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
    let ModuleDef::Effect { name, sources } = &unit.def else {
        anyhow::bail!("spawn called on a non-effect module");
    };
    let sources = sources.clone();
    let name = name.clone();
    let resume = runtime.effect_resume_after(&name)?;
    for position in runtime.running_with_hash_mismatch(&name, &unit.digest_hash)? {
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
    supervise(&shared, |subscribed| {
        run_inner(&shared, &unit, &runtime, &http, subscribed)
    });
}

/// Supervise the driver: a transient store or op-DB error must not silently kill the
/// effect. It surfaces as a wedge (so `/status` shows it) and the driver re-subscribes
/// with capped backoff, matching the invocation-level "retry forever" promise. `drive`
/// returns `Ok` only on a clean stop.
///
/// It reports getting back on the log by calling the callback it is handed, rather than
/// by returning, because the two facts that recovery carries land at different times:
/// the wedge clears the moment the driver is reading events again, which can be hours
/// before the run ends, while the retry ladder resets when it does. Taking the driver
/// as a parameter is what lets both be tested without a store behind them.
fn supervise(shared: &EffectShared, mut drive: impl FnMut(&mut dyn FnMut()) -> anyhow::Result<()>) {
    let mut attempt: u32 = 0;
    loop {
        let mut recovered = false;
        let outcome = drive(&mut || {
            // Back on the log, so whatever failure was recorded for the error that sent
            // us round this loop is over. Nothing else clears it: on an idle effect
            // there is no next invocation to, so a single transient store error left
            // `state()` reporting `wedged` until an unrelated event happened to arrive.
            // An invocation wedge is not lost here, since that position replays and
            // records its own failure again.
            shared.clear_failures();
            recovered = true;
        });
        let Err(err) = outcome else { break };
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let (delay, next) = next_backoff(attempt, recovered);
        shared.record_failure(&format!("driver: {err:#}"));
        tracing::error!(
            "effect `{}` driver error (attempt {}): {err:#}",
            shared.name,
            next
        );
        shared.set_retry_deadline(delay);
        let stop = sleep_watching(shared, None, delay);
        shared.clear_retry_deadline();
        if stop {
            break;
        }
        attempt = next;
    }
}

/// How long to wait after a driver error, and what the attempt counter becomes.
///
/// A run that got back on the log starts the ladder over however it ended: the ladder
/// is for a driver that cannot start at all. Carrying the count across a recovery meant
/// a few unrelated blips over a process's life parked every later re-subscribe at the
/// 60s cap, which `retry_in_ms` now publishes as a minute-long countdown for what was
/// a single transient error.
fn next_backoff(attempt: u32, recovered: bool) -> (Duration, u32) {
    let attempt = if recovered { 0 } else { attempt };
    (backoff(attempt), attempt.saturating_add(1))
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
    http: &Arc<dyn HttpClient>,
    subscribed: &mut dyn FnMut(),
) -> anyhow::Result<()> {
    let ModuleDef::Effect { name, sources } = &unit.def else {
        anyhow::bail!("run called on a non-effect module");
    };
    // Sources filter on plaintext fields only (check-time rejects encrypted source
    // constraints), so no key store is needed to lower them.
    let query = query_of_types(sources).map_err(|err| anyhow::anyhow!("{err}"))?;
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
    // Only a writer-backed store advances a watermark; see `Store::subscribe`. An effect
    // driver on a read-only log would park forever rather than idle.
    let mut sub = runtime
        .store()
        .subscribe("an effect", query, Position::new(resume))?;
    // Tell the supervisor the driver is back on the log; it owns what that means.
    subscribed();
    loop {
        let batch = sub
            .poll_batch()
            .map_err(|err| anyhow::anyhow!("reading events: {err}"))?;
        if !batch.is_empty() {
            for (position, _event) in &batch {
                match run_invocation(
                    shared,
                    name,
                    &unit.digest_hash,
                    runtime,
                    http,
                    position.get(),
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
    source_hash: &str,
    runtime: &Arc<Runtime>,
    http: &Arc<dyn HttpClient>,
    position: u64,
) -> anyhow::Result<Progress> {
    match runtime.begin_invocation(effect, position, source_hash, &runtime::now_rfc3339())? {
        InvocationState::AlreadyTerminal => return Ok(Progress::Advanced),
        InvocationState::Running => {}
    }

    let mut attempt: u32 = 0;
    loop {
        // Honor an operator skip only once this position is genuinely wedged (it
        // has failed at least once). Checking before the first attempt would let a
        // skip requested for a not-yet-reached position drop a healthy event.
        if attempt > 0 && shared.skip_position.load(Ordering::Relaxed) == position {
            honor_skip(shared, || {
                runtime.skip_invocation(effect, position, &runtime::now_rfc3339())
            })?;
            tracing::warn!(
                "effect `{effect}` skipped wedged position {position} by operator request"
            );
            return Ok(Progress::Advanced);
        }
        match try_invocation(effect, position, runtime, http) {
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
                    // `Live`: this process just watched the invocation complete, and an
                    // operator skip returned above rather than reaching here, so an empty
                    // journal here is a run that genuinely called nothing.
                    let outcome = replay(effect, position, runtime, Asked::Live);
                    if let Some(violation) = outcome.violation(effect, position) {
                        // Durable, so the restart a wedged effect invites does not
                        // silently clear it. The watermark is deliberately left where
                        // it is: this position is terminal, but nothing past it should
                        // be processed until an operator has looked.
                        runtime.quarantine_effect(effect, position, &violation.to_string())?;
                        shared.quarantine(&violation);
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

/// One invocation, against heklang's own effect machinery.
///
/// The durable half stays here (the journal, the retry, the completion) and the
/// decidable half is the language's: `deliver` runs the arm the event selects, folds
/// its boundary to this position inclusive, and journals every impure call. hekla no
/// longer retries a single request, because `docs/effects.md` rule 5 puts that loop
/// inside the language so only a decidable result reaches the handler.
fn try_invocation(
    effect: &str,
    position: u64,
    runtime: &Arc<Runtime>,
    http: &Arc<dyn HttpClient>,
) -> Result<(), InvocationFailure> {
    let now = runtime::now_rfc3339();
    let call = Arc::new(Mutex::new(None));
    let host = HeklaHost {
        program: Arc::clone(runtime.program_shared()),
        events: Arc::clone(runtime.events_shared()),
        store: runtime.store().clone(),
        keystore: runtime.keystore_shared().cloned(),
        // An effect's appends go through `invoke`, and they belong to the flow that
        // triggered it: the correlation carries across command, event, effect and
        // command, which is what makes a trace one chain rather than two.
        ctx: trigger_context(runtime, position)?,
        now: now.clone(),
        idem_tag: None,
        call: Some(Arc::clone(&call)),
        appended: None,
        emitted: Vec::new(),
        unavailable: None,
        duplicated: false,
        retry_after: None,
        last_transport: None,
        minted: None,
        sealed: false,
        http: Some(Arc::clone(http)),
    };
    let mut journal = Journal {
        opdb: runtime.opdb(),
        effect,
        position,
        now: &now,
        call,
    };
    let mut interpreter = Interpreter::with_host(runtime.program(), host);
    // heklang counts from zero and tephra from one, so the trigger is one lower there.
    // The journal key stays the tephra position: it is a row in hekla.db.
    let outcome = interpreter.deliver(effect, from_tephra(Position::new(position)), &mut journal);
    for line in interpreter.lines() {
        tracing::info!("effect `{effect}` @ {position}: {line}");
    }
    // What the last retryable response asked for, if anything. Read off the host
    // because the language never sees the header: rule 5 absorbed the response that
    // carried it.
    let retry_after = interpreter.host().retry_after;
    let transport = interpreter.host().last_transport.clone();
    match outcome {
        // Rule 4: done and ignored both advance, and so do the two terminal answers.
        // Only a wedge does not, and a wedge is the error case below.
        Ok(HekInvocation::Done | HekInvocation::Ignored) => Ok(()),
        Ok(HekInvocation::Failed(message)) => Err(InvocationFailure {
            message,
            terminal: true,
            retry_after,
        }),
        Ok(HekInvocation::Skipped(message)) => Err(InvocationFailure {
            message,
            terminal: true,
            retry_after,
        }),
        // The language says which call did not answer and this says why: rule 5
        // absorbed the attempts that carried the reason, so nothing else still has it.
        Err(err) => Err(InvocationFailure {
            message: match transport {
                Some(reason) => format!("{err} ({reason})"),
                None => format!("{err}"),
            },
            terminal: false,
            retry_after,
        }),
    }
}

/// A failed invocation attempt: its message, and whether the failure is terminal (no
/// retry can succeed) rather than a wedge.
///
/// The flow one triggering event belongs to.
///
/// Read off the log rather than carried down from the batch, so a replay after a
/// restart lands in the same flow as the original run: the correlation and the
/// causation are properties of the event, not of the process that noticed it.
fn trigger_context(
    runtime: &Arc<Runtime>,
    position: u64,
) -> Result<CommandContext, InvocationFailure> {
    let wedge = |why: String| InvocationFailure {
        message: why,
        terminal: false,
        retry_after: None,
    };
    let at = Position::new(position);
    let mut reads =
        runtime
            .store()
            .read(&tephra::Query::all(), Position::new(position - 1), Some(1));
    let seq = reads
        .next()
        .ok_or_else(|| wedge(format!("no event at position {position}")))?
        .map_err(|err| wedge(format!("reading the triggering event: {err}")))?;
    if seq.position != at {
        return Err(wedge(format!("no event at position {position}")));
    }
    let (envelope, _) = crate::envelope::decode(seq.event.data())
        .map_err(|err| wedge(format!("decoding the triggering event: {err}")))?;
    Ok(CommandContext::from_effect(
        envelope.correlation_id,
        envelope.event_id,
    ))
}

/// Rule 5 puts the per-request retry inside the language, so a retryable status is
/// absorbed before an arm sees it and the only backoff left is the one between whole
/// invocations. `retry_after` is what a limiter asked for on the last such status, so
/// that wait is the one the server named rather than this driver's own ladder.
struct InvocationFailure {
    message: String,
    terminal: bool,
    retry_after: Option<Duration>,
}

/// How long to wait before the next attempt: never sooner than the wedge backoff,
/// and never sooner than a `Retry-After` asked for.
///
/// Taking the larger of the two rather than the header alone is what matters on a
/// limiter that keeps answering `Retry-After: 1`. Obeying that literally would retry
/// once a second forever, so the backoff still grows underneath it.
fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(after) => backoff(attempt).max(after.min(RETRY_AFTER_CAP)),
        None => backoff(attempt),
    }
}

/// The wedge backoff for `attempt`, doubling from [`BACKOFF_BASE`] up to
/// [`BACKOFF_CAP`].
fn backoff(attempt: u32) -> Duration {
    BACKOFF_BASE
        .saturating_mul(1u32 << attempt.min(10))
        .min(BACKOFF_CAP)
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

/// What re-running one recorded invocation came to.
///
/// The distinctions here are the whole point of the type. `verify` needs to know whether
/// an invocation reproduced; `plan` needs that *and* whether an invocation it could not
/// replay was counted as reproducing, because a coverage number that quietly includes
/// what it never covered is worse than no coverage number at all.
#[derive(Debug, Clone)]
pub enum Replayed {
    /// It made the same calls, in the same order, and performed none of them again.
    Matched,
    /// It reached a call the journal has no entry for. On a real retry that call would
    /// have been performed a second time; on a candidate deploy it is a call the
    /// recorded run never made.
    NewCall { call: String, asked: Asked },
    /// It hit every journal entry but not in the recorded order, or made fewer calls.
    Different { journal: String, replay: String },
    /// No arm selects the triggering event any more, so this code would not run at all
    /// for it. Reachable only when the arms themselves changed, which is why `verify`
    /// never sees it and `plan` does.
    NoLongerHandled,
    /// A `reveal` of a subject whose key has been erased. Nothing can be concluded: the
    /// plaintext the handler branches on is gone, by design, and journaling it to make
    /// this replayable would defeat the erasure. Not a divergence, and not a match.
    SubjectErased { reason: String },
    /// The recorded invocation journaled no call at all, and the replay reached one.
    ///
    /// Only for rows written before schema v7, which had nowhere to record an operator
    /// skip: a run that took a branch calling nothing and a run skipped before its first
    /// call both landed as `terminal` with an empty journal, and nothing on the row told
    /// them apart. Reporting a call the second one *did* make as a call it did not would
    /// fail a healthy directory, so this is uncovered rather than divergent. Rows written
    /// since answer the question outright and produce
    /// [`OperatorSkipped`](Replayed::OperatorSkipped) instead, whatever their journal
    /// holds. [`Asked::Live`] never produces this either: it watched the run complete, so
    /// it gets [`NewCall`](Replayed::NewCall). When the replay also calls nothing the two
    /// agree and it is [`Matched`](Replayed::Matched), which is a real check.
    NoJournal { call: String },
    /// The invocation's row went away between being listed and being read.
    ///
    /// Retention deletes the row and cascades the journal, so an invocation it reclaimed
    /// is invisible rather than skipped. That is normally true before the replay starts,
    /// but `--replay` runs against a directory whose server is still sweeping, so the row
    /// can go while this is looking at it. Nothing is left to compare against, and the
    /// empty journal it leaves behind must not be read as a run that called nothing.
    Reclaimed,
    /// The handler reached `fail(...)`: rule 4's terminal outcome, which advances the
    /// cursor rather than wedging.
    ///
    /// Only [`Asked::Candidate`] produces this. When the program going in is the one that
    /// wrote the journal, a terminal failure is what that program does with this event
    /// and the calls it made on the way are still the thing under test, so the comparison
    /// runs and this never appears. When the program is a candidate, the record cannot
    /// say whether the deployed one failed here too, and "this deploy would fail on 40 of
    /// the last 100 events" is worth saying either way.
    TerminallyFailed { detail: String },
    /// An operator skipped this invocation, so nothing ran it to a conclusion.
    ///
    /// Read from the row rather than inferred from the journal, which is why this says
    /// what [`NoJournal`](Replayed::NoJournal) can only guess at. A skip completes a
    /// wedged position on an operator's say-so: the handler stopped wherever it stopped,
    /// and whatever the journal holds is a prefix of a run that never finished. Comparing
    /// a replay against that prefix would report a healthy directory as divergent, which
    /// is what it did for four rounds of this, so it is uncovered whatever shape the
    /// journal has and whatever the replay does.
    OperatorSkipped,
    /// The record could not be read, so nothing was compared.
    ///
    /// A database error is not evidence about the handler. `--replay` runs against a
    /// directory whose server is live, so a busy op-DB is ordinary, and concluding
    /// anything on the strength of a failed read would turn contention into a finding.
    /// The same policy `reclaimed` applies to its own read, applied to the journal's.
    Unreadable { detail: String },
    /// It errored part-way through, so whatever it would have done, it did not do what
    /// the journal records.
    ///
    /// Not conditional on the journal having anything in it: an error that never reached
    /// a call is the candidate failing on this event, which the record neither explains
    /// nor excuses.
    Failed { detail: String },
}

/// Why an outcome could not be compared against the record at all.
///
/// The set [`Replayed::is_covered`] excludes, named rather than re-derived: every caller
/// that reports *why* an invocation went uncounted reads this, so a new reason is a
/// compile error at each of them instead of a wildcard that quietly miscounts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uncovered {
    /// The plaintext the handler branches on has been shredded.
    SubjectErased,
    /// The record is empty, and cannot say whether that is because the run called
    /// nothing or because an operator skipped it.
    NoJournal,
    /// Retention took the record while the replay was running.
    Reclaimed,
    /// An operator skipped the invocation, so no run of it ever reached an end.
    OperatorSkipped,
    /// The record could not be read at all.
    Unreadable,
}

/// Who is asking, which settles two readings the outcome alone cannot.
///
/// An empty journal is ambiguous to a caller reading a row back, and unambiguous to the
/// driver that watched the run complete. A terminal `fail` is what the recorded program
/// does when the program being replayed *is* the recorded one, and news when it is not.
/// Both are facts about the caller's situation rather than about the invocation, which is
/// why they arrive as a parameter instead of being guessed at from the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// The live check inside the effect driver: this process watched the invocation
    /// complete moments ago, an operator skip returned before reaching here, and the
    /// program is the one that recorded the journal.
    Live,
    /// [`crate::verify`]'s sweep: the same program, over a record some earlier process
    /// wrote.
    Sweep,
    /// [`crate::plan`]: a record written by a program that is not the one going in.
    Candidate,
}

impl Replayed {
    /// Why the replay could not reach an answer about this invocation, if it could not.
    ///
    /// A failure is not one of these: erroring part-way through is a way of not
    /// reproducing, and one worth reporting. What counts here is an invocation the replay
    /// could not put a question to at all, because the key it needed is gone or because
    /// the record it would be compared against cannot say what happened.
    pub fn uncovered(&self) -> Option<Uncovered> {
        match self {
            Replayed::SubjectErased { .. } => Some(Uncovered::SubjectErased),
            Replayed::NoJournal { .. } => Some(Uncovered::NoJournal),
            Replayed::Reclaimed => Some(Uncovered::Reclaimed),
            Replayed::OperatorSkipped => Some(Uncovered::OperatorSkipped),
            Replayed::Unreadable { .. } => Some(Uncovered::Unreadable),
            _ => None,
        }
    }

    /// Whether the replay reached an answer about this invocation, either way.
    pub fn is_covered(&self) -> bool {
        self.uncovered().is_none()
    }

    /// A stable one-word name for this outcome, for a machine reading `--json`.
    pub fn label(&self) -> &'static str {
        match self {
            Replayed::Matched => "matched",
            Replayed::NewCall { .. } => "new_call",
            Replayed::Different { .. } => "different_calls",
            Replayed::NoLongerHandled => "no_longer_handled",
            Replayed::SubjectErased { .. } => "subject_erased",
            Replayed::NoJournal { .. } => "no_journal",
            Replayed::Reclaimed => "reclaimed",
            Replayed::OperatorSkipped => "operator_skipped",
            Replayed::Unreadable { .. } => "unreadable",
            Replayed::TerminallyFailed { .. } => "terminally_failed",
            Replayed::Failed { .. } => "failed",
        }
    }

    /// Whether the replay and the record agree.
    pub fn reproduces(&self) -> bool {
        matches!(self, Replayed::Matched)
    }

    /// This outcome as a check reads it: a violation, or nothing.
    ///
    /// An invocation nothing could be concluded about is not a violation. It is also not
    /// a pass, which is what a caller's coverage counts are for. Everything the caller's
    /// situation decides was decided in [`replay`], so this reads the outcome alone.
    pub(crate) fn violation(&self, effect: &str, position: u64) -> Option<Violation> {
        if self.reproduces() || !self.is_covered() {
            return None;
        }
        Some(Violation::ReplayDivergence {
            effect: effect.to_owned(),
            position,
            detail: self.to_string(),
        })
    }
}

impl fmt::Display for Replayed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Replayed::Matched => write!(f, "it made the calls the journal records"),
            // The same observation reads two ways, and printing only the retry one told a
            // `plan` reader that a retry would double-fire when the finding is that the
            // candidate makes a call the recorded run never did.
            Replayed::NewCall { call, asked } => match asked {
                Asked::Candidate => {
                    write!(f, "it reached a call the recorded run never made ({call})")
                }
                Asked::Live | Asked::Sweep => write!(
                    f,
                    "it reached a call with no journal entry ({call}); a real retry would \
                     have performed it a second time"
                ),
            },
            Replayed::Different { journal, replay } => write!(
                f,
                "it made a different sequence of calls than the journal records \
                 (journal {journal}, replay {replay})"
            ),
            Replayed::NoLongerHandled => write!(
                f,
                "no arm selects the event that triggered it, so it would not run at all"
            ),
            Replayed::SubjectErased { reason } => write!(f, "it cannot be replayed: {reason}"),
            Replayed::NoJournal { call } => write!(
                f,
                "it reached a call ({call}) against a journal with no entries, and an \
                 operator skip and a run that called nothing both journal nothing, so \
                 what it would now call cannot be compared"
            ),
            Replayed::Reclaimed => write!(
                f,
                "retention reclaimed its record while the replay was running, so there \
                 is nothing left to compare against"
            ),
            Replayed::TerminallyFailed { detail } => write!(
                f,
                "it would end in a terminal `fail`, advancing past the event rather than \
                 handling it: {detail}"
            ),
            Replayed::OperatorSkipped => write!(
                f,
                "an operator skipped it, so nothing ran it to an end and its journal is \
                 whatever the wedged run had reached"
            ),
            Replayed::Unreadable { detail } => write!(
                f,
                "its record could not be read, so nothing was compared: {detail}"
            ),
            Replayed::Failed { detail } => write!(f, "it failed part-way through: {detail}"),
        }
    }
}

/// Re-run a recorded invocation against a sealed host and say how it went.
///
/// Safe against a live system by construction: the sealed host performs nothing, so
/// the worst outcome is a report. Sealing the transport alone is not enough, and that
/// is the trap this walked into once. heklang performs a journal miss for real, so an
/// unjournaled `invoke` would run its command and append (with no idempotency clause,
/// since a replay holds no key), and an unjournaled `erase` would destroy the subject
/// key. Both reach the store through [`HeklaHost`], which is why `sealed` is set here
/// as well as `SealedHttp`. `reveal` is re-run because it is not journaled, but it
/// decrypts and returns without reaching anything outward.
///
/// Two shapes of divergence are caught, and the second is the one nothing else
/// detects: a call the journal has no entry for, and a journal entry the handler no
/// longer makes. Because the journal is keyed by call content rather than by
/// sequence, a handler that merely *reorders* its calls still hits every entry, so
/// comparing the visited set against the recorded set is what surfaces it.
///
/// **The program comes from `runtime`, and from nowhere else.** Handing it the code that
/// recorded the journal asks "did this reproduce itself", which is
/// [`verify`](crate::verify); handing it code that has not been deployed yet asks "would
/// this still do what happened", which is [`plan`](crate::plan), whose
/// [`Runtime::open_following`] wraps the candidate project for exactly that. One machine,
/// two questions, and the only difference is which runtime goes in.
///
/// It used to be a parameter beside the runtime, which let the two disagree: the
/// interpreter ran one program while [`HeklaHost`] decoded the log through another, so a
/// candidate's handler would fold events typed by the deployed schema and report the
/// mismatch as a behaviour change. Taking both from one place makes that unrepresentable.
pub fn replay(effect: &str, position: u64, runtime: &Arc<Runtime>, asked: Asked) -> Replayed {
    // First, and before the handler is run at all, because an operator skip is a fact
    // about the record rather than about this replay. A skipped position is one nothing
    // ran to an end: the handler stopped where it wedged, so its journal is the prefix of
    // an unfinished run and every comparison below would be against half of one. Inferred
    // from journal shape this was wrong in three separate ways; read from the row it is
    // one question. `Live` never reaches a skipped invocation, since `honor_skip` returns
    // before the check runs, so it does not pay for the read. A read that fails is taken
    // as "not skipped", which lands on the inference below rather than turning a busy
    // op-DB into a coverage gap.
    if asked != Asked::Live
        && runtime
            .invocation_skipped(effect, position)
            .unwrap_or(false)
    {
        return Replayed::OperatorSkipped;
    }
    let now = runtime::now_rfc3339();
    let host = HeklaHost {
        program: Arc::clone(runtime.program_shared()),
        events: Arc::clone(runtime.events_shared()),
        store: runtime.store().clone(),
        keystore: runtime.keystore_shared().cloned(),
        ctx: CommandContext::new(uuid::Uuid::new_v4()),
        now: now.clone(),
        idem_tag: None,
        // A sealed replay reaches no unjournaled call, so nothing appends and there is
        // no tag to key.
        call: None,
        appended: None,
        emitted: Vec::new(),
        unavailable: None,
        duplicated: false,
        retry_after: None,
        last_transport: None,
        minted: None,
        // The other half of the seal. `SealedHttp` stops a send, and this stops the
        // append an unjournaled `invoke` would make and the shred an unjournaled
        // `erase` would perform, both of which reach the store through the host.
        sealed: true,
        http: Some(Arc::new(SealedHttp) as Arc<dyn HttpClient>),
    };
    let mut journal = SealedJournal {
        inner: Journal {
            opdb: runtime.opdb(),
            effect,
            position,
            now: &now,
            call: Arc::new(Mutex::new(None)),
        },
        visited: RefCell::new(Vec::new()),
        missed: RefCell::new(None),
    };
    let mut interpreter = Interpreter::with_host(runtime.program(), host);
    // heklang counts from zero and tephra from one, so the trigger is one lower there.
    // The journal key stays the tephra position: it is a row in hekla.db.
    let outcome = interpreter.deliver(effect, from_tephra(Position::new(position)), &mut journal);
    let visited = journal.visited.borrow().clone();
    let missed = journal.missed.borrow().clone();

    // Before the journal is even read, because neither turns on what the record holds and
    // reading it can fail. Ordering them after a database error would turn a busy op-DB
    // into a violation for an invocation that is unanswerable by design.
    //
    // `Invocation::Skipped` has exactly one producer in heklang, `ErrorKind::Erased`, so
    // this is the erased-subject case and nothing else. For a live retry it is the
    // documented cost of the `erase last` rule; for a replay it is the one thing the
    // journal deliberately cannot answer for.
    if let Ok(HekInvocation::Skipped(reason)) = &outcome {
        return Replayed::SubjectErased {
            reason: reason.clone(),
        };
    }
    // And an arm that no longer selects the event makes no calls at all, which every
    // comparison below would report as a quieter fact than it is. A recorded invocation
    // exists only because the deployed code *did* select this event (an ignored position
    // journals nothing and gets no row), so an ignored replay is unambiguous news.
    if matches!(outcome, Ok(HekInvocation::Ignored)) {
        return Replayed::NoLongerHandled;
    }

    let recorded: Vec<CallKey> = match runtime.journal_keys(effect, position) {
        Ok(recorded) => recorded,
        // Uncovered rather than failed. The handler did nothing wrong; the database was
        // busy, which against a live directory is ordinary. Reporting it as a divergence
        // put "N recorded invocation(s) would diverge" in front of an operator for
        // invocations nothing ever compared. Same policy as `reclaimed`, whose read this
        // sits beside.
        Err(err) => {
            return Replayed::Unreadable {
                detail: format!("{err:#}"),
            };
        }
    };

    // An empty journal can be hit (both called nothing), unanswerable (only the replay
    // called something, and an operator skip journals nothing either), or beside the
    // point (the replay failed before it got anywhere near a call, which the record
    // neither explains nor excuses). Nothing can be in `visited`: with no entries to
    // hit, every lookup misses.
    if recorded.is_empty() {
        return match (&outcome, missed) {
            (_, Some(call)) if asked == Asked::Live => Replayed::NewCall { call, asked },
            (_, Some(call)) => Replayed::NoJournal { call },
            (Err(err), None) => Replayed::Failed {
                detail: format!("{err}"),
            },
            // Nothing was compared, so before calling that agreement, make sure there was
            // still something to compare against. A live server's retention sweeper takes
            // the row and the journal together, and it can take them between the listing
            // that produced this position and the read above.
            (Ok(_), None) => match reclaimed(effect, position, runtime, asked) {
                Some(gone) => gone,
                None => terminal_failure(&outcome, asked).unwrap_or(Replayed::Matched),
            },
        };
    }

    if let Err(err) = outcome {
        return match missed {
            Some(call) => Replayed::NewCall { call, asked },
            None => Replayed::Failed {
                detail: format!("{err}"),
            },
        };
    }
    if let Some(call) = missed {
        return Replayed::NewCall { call, asked };
    }
    // Ordered comparison. A subset test would be blind to exactly the case the
    // content-keyed journal cannot see on its own: the pairs are unique within an
    // invocation and a sealed run can never visit a key the journal lacks (that path
    // returned above), so equal-as-sets is guaranteed and only the sequence is news.
    if visited != recorded {
        return Replayed::Different {
            journal: render_keys(&recorded),
            replay: render_keys(&visited),
        };
    }
    // Last, because a `fail` after every recorded call is still a handler that made
    // exactly the recorded calls, and for the two callers replaying the program that
    // wrote them that is the whole question. Only a candidate is doing something the
    // record cannot vouch for.
    terminal_failure(&outcome, asked).unwrap_or(Replayed::Matched)
}

/// `Ok(Invocation::Failed)` as `asked` reads it, and `None` when it is not news.
///
/// heklang's rule 4 makes `fail(...)` an *outcome* rather than an error: the position is
/// recorded failed and the cursor advances, so the row on disk is the same `terminal` row
/// a success leaves and nothing says which it was. Replaying the program that wrote it
/// therefore learns nothing by noticing (it fails where it failed), while a candidate that
/// would newly `fail` on recorded events is a finding worth the whole command.
fn terminal_failure(
    outcome: &Result<HekInvocation, heklang::Error>,
    asked: Asked,
) -> Option<Replayed> {
    match outcome {
        Ok(HekInvocation::Failed(detail)) if asked == Asked::Candidate => {
            Some(Replayed::TerminallyFailed {
                detail: detail.clone(),
            })
        }
        _ => None,
    }
}

/// [`Replayed::Reclaimed`] when the invocation's row is gone, and `None` while it is
/// there.
///
/// Only asked when the journal read came back empty, which is the one answer retention
/// and a callless run produce identically. Only [`Asked::Candidate`] can reach it, since
/// it alone runs against a directory a server still holds: [`Asked::Live`] is that server
/// and the sweeper works off a cutoff days in the past, so the row this process just
/// wrote cannot be in range, and [`Asked::Sweep`] runs under the exclusive data-directory
/// lock, so no sweeper exists to race. The other two would pay a lock and a query per
/// empty journal to be told what they already know. A read that fails is treated as
/// "still there", because refusing to conclude on the strength of a database error would
/// turn a busy directory into a coverage gap.
fn reclaimed(
    effect: &str,
    position: u64,
    runtime: &Arc<Runtime>,
    asked: Asked,
) -> Option<Replayed> {
    if asked != Asked::Candidate {
        return None;
    }
    match runtime.invocation(effect, position) {
        Ok(None) => Some(Replayed::Reclaimed),
        Ok(Some(_)) | Err(_) => None,
    }
}

/// A journal that reads but never writes, and remembers what it was asked for.
///
/// A miss is the thing the check exists to find: the replay reached a call the first
/// run never journaled, which on a real retry would have performed it a second time.
struct SealedJournal<'a> {
    inner: Journal<'a>,
    /// Every call the replay looked up, in the order it looked them up. `Calls` reads
    /// through `&self`, so the record of what was asked for has to be behind a cell.
    visited: RefCell<Vec<CallKey>>,
    /// The first call the journal could not answer.
    missed: RefCell<Option<String>>,
}

impl Calls for SealedJournal<'_> {
    fn recorded(&self, call: &str, ordinal: u32) -> Result<Option<Recorded>, heklang::Error> {
        let found = self.inner.recorded(call, ordinal)?;
        match &found {
            Some(_) => self
                .visited
                .borrow_mut()
                .push((sha256_hex(call.as_bytes()), u64::from(ordinal))),
            // The miss is caught here rather than at the write, because heklang asks
            // this immediately before it performs a call: a `None` *is* the replay
            // reaching something the first run never journaled, whatever the sealed
            // host then refuses to do about it.
            None => {
                let mut missed = self.missed.borrow_mut();
                if missed.is_none() {
                    *missed = Some(format!("{call} #{ordinal}"));
                }
            }
        }
        Ok(found)
    }

    fn record(
        &mut self,
        _call: &str,
        _ordinal: u32,
        _recorded: Recorded,
    ) -> Result<(), heklang::Error> {
        // A sealed replay writes nothing: the miss was already reported above, and the
        // journal belongs to the run that made the calls.
        Ok(())
    }
}

/// One journaled call, as the operational database keys it: the hash of heklang's
/// readable key, plus the ordinal that separates repeats of an identical call.
type CallKey = (String, u64);

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
            sources: Vec::new(),
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

    /// The regression this guards is invisible on a busy effect and permanent on a
    /// quiet one: nothing but an invocation used to clear a driver-level failure, so a
    /// single transient store error left an idle effect reporting `wedged` until an
    /// unrelated event happened to arrive. That state drives the console's red count,
    /// the "needs attention" panel and `/status`, so it reads as a live incident.
    #[test]
    fn a_driver_back_on_the_log_clears_the_wedge_the_supervisor_recorded() {
        let shared = test_shared();
        shared.record_failure("driver: reading events: op-db is locked");
        assert_eq!(shared.state(5), "wedged");

        // Re-subscribes, then stops cleanly. No backoff is slept here: this is the
        // recovery path rather than the retry one.
        supervise(&shared, |subscribed| {
            subscribed();
            Ok(())
        });

        assert_eq!(shared.consecutive_failures(), 0);
        assert_eq!(shared.last_error(), None);
        assert_eq!(
            shared.state(5),
            "lagging",
            "an effect reading the log again is behind, not wedged"
        );
    }

    /// A quarantine is not a wedge and nothing clears it on its own, so a driver that
    /// re-subscribes must not launder one away.
    #[test]
    fn getting_back_on_the_log_does_not_clear_a_quarantine() {
        let shared = test_shared();
        shared.restore_quarantine(4, "fold diverged from the read model");

        supervise(&shared, |subscribed| {
            subscribed();
            Ok(())
        });

        assert!(shared.quarantined());
        assert_eq!(shared.state(5), "quarantined");
    }

    /// The ladder is for a driver that cannot start at all. Carrying the count across a
    /// recovery parked every later re-subscribe at the cap, which `retry_in_ms` then
    /// publishes as a minute-long countdown for a one-off blip.
    #[test]
    fn the_retry_ladder_escalates_but_starts_over_after_a_recovery() {
        assert_eq!(next_backoff(0, false), (BACKOFF_BASE, 1));
        assert_eq!(next_backoff(1, false), (BACKOFF_BASE * 2, 2));
        // Deep enough to be saturated, which is the state a long-lived process reaches.
        assert_eq!(next_backoff(9, false), (BACKOFF_CAP, 10));
        assert_eq!(
            next_backoff(9, true),
            (BACKOFF_BASE, 1),
            "a run that got back on the log starts the ladder over"
        );
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(0), BACKOFF_BASE);
        assert_eq!(backoff(1), BACKOFF_BASE * 2);
        assert_eq!(backoff(100), BACKOFF_CAP);
    }

    // The tests that stood here covered the per-request retry, the `Retry-After`
    // hint, the journal's call hashing and the response-to-JSON shaping. Rule 5 moved
    // every one of those into heklang, which has its own suite for them, so keeping
    // copies here would assert on machinery hekla no longer owns.

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

    /// A terminal `fail` is a reproduction to two callers and news to the third.
    ///
    /// Rule 4 makes `fail(...)` an outcome rather than an error, and the `terminal` row it
    /// leaves is the one a success leaves. So replaying the program that wrote the row
    /// learns nothing by noticing (it fails where it failed), while a candidate that would
    /// newly fail on recorded events is the finding the command exists for.
    #[test]
    fn who_is_asking_decides_a_terminal_fail() {
        let failed = Ok(HekInvocation::Failed("rate limited".to_owned()));
        assert!(matches!(
            terminal_failure(&failed, Asked::Candidate),
            Some(Replayed::TerminallyFailed { .. })
        ));
        for asked in [Asked::Live, Asked::Sweep] {
            assert!(
                terminal_failure(&failed, asked).is_none(),
                "the program that wrote the journal failing where it failed is the record"
            );
        }
        assert!(terminal_failure(&Ok(HekInvocation::Done), Asked::Candidate).is_none());
    }

    /// A violation is an outcome that was answerable and did not reproduce. Everything
    /// the caller's situation decides is decided in `replay`, so this reads the outcome
    /// alone, and the three uncovered ones are neither a pass nor a fault.
    #[test]
    fn only_a_covered_outcome_that_did_not_reproduce_is_a_violation() {
        assert!(Replayed::Matched.violation("Notify", 7).is_none());

        let new_call = Replayed::NewCall {
            call: "http.post #0".to_owned(),
            asked: Asked::Sweep,
        };
        let violation = new_call
            .violation("Notify", 7)
            .expect("a call the journal has no entry for is the whole point");
        let Violation::ReplayDivergence { detail, .. } = &violation else {
            panic!("expected a replay divergence, got {violation:?}");
        };
        assert!(
            detail.contains("http.post #0") && detail.contains("second time"),
            "the detail names the call and what a retry would do: {detail}"
        );
        for outcome in [
            Replayed::Failed {
                detail: "boom".to_owned(),
            },
            Replayed::TerminallyFailed {
                detail: "rate limited".to_owned(),
            },
        ] {
            assert!(outcome.violation("Notify", 7).is_some(), "{outcome}");
        }

        for outcome in [
            Replayed::SubjectErased {
                reason: "gone".to_owned(),
            },
            Replayed::NoJournal {
                call: "http.post #0".to_owned(),
            },
            Replayed::Reclaimed,
        ] {
            assert!(outcome.uncovered().is_some(), "{outcome}");
            assert!(
                outcome.violation("Notify", 7).is_none(),
                "nothing could be concluded, which is not a fault: {outcome}"
            );
        }
    }
}
