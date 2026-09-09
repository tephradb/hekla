//! The effect runtime: durable execution of side effects.
//!
//! One dedicated thread per effect subscribes to its `handle` keys and hands each
//! matching event to the lane its `@key` names; positions in one lane are processed
//! strictly in order, and positions in different lanes need not wait for each other.
//! One invocation per event, except where rule 15's `on latest` declares otherwise:
//! there an arm runs once per key per dispatch batch, at the newest matching position
//! in it, and the invocation records the range it folded. An invocation
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

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError, mpsc};
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
use heklang::ir::Delivery;

use crate::lane::LaneId;
use crate::lanes::{LaneState, Work};
use crate::loader::{self, EffectUnit};
use crate::metrics;
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
/// How long an idle pool worker waits before re-checking for work it was not notified
/// about. Purely a backstop: `offer` and `stop` both notify.
const POOL_IDLE_WAIT: Duration = Duration::from_secs(5);
/// How often the retention sweeper runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// One lane that is not healthy right now: the position it is stuck on, how many times
/// it has failed in a row, and when it next tries.
///
/// Only unhealthy lanes are recorded, so an effect with a million keys costs nothing
/// until something breaks.
#[derive(Debug, Clone)]
struct StuckLane {
    position: u64,
    attempt: u32,
    error: String,
    retry_at_ms: u64,
}

/// The lane a driver-level failure is recorded under.
///
/// A store or op-DB error belongs to no lane, but it has to reach the same `/status`
/// fields a lane failure does. Position `0` makes it always the lowest, so it is always
/// the failure reported, which is right: a driver that cannot read the log at all is a
/// worse problem than any one lane's. It is filtered out of the lane counters and never
/// named as a pinning key, because it is not a key. No event sits at position 0 and `!`
/// is not a key tag, so it cannot collide with a real lane.
static DRIVER_LANE: LazyLock<LaneId> = LazyLock::new(|| LaneId::from("!driver"));

fn driver_lane() -> &'static LaneId {
    &DRIVER_LANE
}

/// Every word [`EffectShared::state`] can return, which `crate::metrics` renders as a
/// state set. Beside that function rather than in the metrics module, because the failure
/// mode of the two disagreeing is silent: every series reads 0 and an `== 1` alert simply
/// stops firing. `a_state_set_covers_every_effect_state` keeps them honest.
pub const EFFECT_STATES: [&str; 5] = ["healthy", "lagging", "wedged", "quarantined", "blocked"];

/// Observable state for one effect, shared with the runtime (for `/status`) and
/// the skip endpoint. Holds no reference to the runtime, so nothing cycles.
///
/// Under rule 15's lanes several positions can be in flight at once, so the fields that
/// used to describe "the invocation" now describe **the pinning lane**: the stuck lane
/// holding the lowest position, which is the one an operator has to clear first and the
/// one holding journal retention down. Everything a reader saw before means the same
/// thing it did, which is why [`EffectShared::state`] did not have to change.
pub struct EffectShared {
    pub name: String,
    /// The event types this effect subscribes to. On the handle for the same reason a
    /// projector's is.
    pub sources: Vec<String>,
    /// This effect's digest hash, on the handle for the same reason `sources` is.
    /// `hekla_module_info` reports it, so two replicas that disagree here are running
    /// different code.
    pub digest_hash: String,
    position: AtomicU64,
    shutdown: AtomicBool,
    /// Cleared when the reader thread exits, so `hekla_effect_up` distinguishes an
    /// effect that is merely idle from one that is not there at all. The projector's
    /// handle carries the same flag for the same reason.
    running: AtomicBool,
    /// How many times the *pinning* lane has failed in a row. Republished from
    /// [`EffectShared::stuck`], and still zero exactly when nothing is wedged, which is
    /// what keeps `state` reading the same as it always did.
    consecutive_failures: AtomicU64,
    last_error: Mutex<Option<String>>,
    /// Cumulative count of positions abandoned by a terminal (non-retryable) failure,
    /// e.g. a `reveal()` of an erased subject. Distinct from `consecutive_failures`: a
    /// terminal skip advances rather than wedges, so it must not read as a wedge.
    terminal_skips: AtomicU64,
    last_terminal_error: Mutex<Option<String>>,
    /// Positions an operator asked to skip. A set rather than one slot, because lanes
    /// mean several positions can be wedged at once and a single slot would drop every
    /// request but the last.
    skips: Mutex<BTreeSet<u64>>,
    /// When this effect started, so a retry deadline can be held as a monotonic
    /// offset from it.
    started: Instant,
    /// Millis since [`EffectShared::started`] at which the pinning lane's backoff
    /// expires, or `0` for "not waiting". Monotonic rather than wall clock on both ends:
    /// the server's clock can step, and the reader's clock is a different machine's, so a
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
    /// The unhealthy lanes, and the driver's own failures under [`driver_lane`].
    stuck: Mutex<BTreeMap<LaneId, StuckLane>>,
    /// How many real lanes are wedged. `consecutive_failures` counts one lane's
    /// attempts and cannot say how many lanes are in that state.
    wedged_lanes: AtomicU64,
    /// Why this effect will not start, when a partition key changed while lanes were
    /// still outstanding. Set once at spawn and never cleared: fixing the code and
    /// restarting is the recovery, which is what makes this different from a quarantine.
    blocked: Mutex<Option<String>>,
    /// The position `on live` arms decline at or below: the log head the first time this
    /// effect ran against this data directory. Rule 15 resolves it once and keeps it, so
    /// source states intent and dev, staging and production each resolve correctly.
    live_boundary: u64,
    /// How many positions the boundary has declined since this process started, so an
    /// operator can see `on live` working rather than guess why nothing fired.
    live_suppressed: AtomicU64,
    /// How many positions an `on latest` arm has folded into another invocation since this
    /// process started. The same job `live_suppressed` does for the other modifier: an
    /// effect whose lag falls without a matching number of invocations is doing exactly
    /// what its author asked for, and this is what says so.
    latest_collapsed: AtomicU64,
    /// The lane whose failure is holding the watermark down, and the position it failed
    /// at, republished from `stuck` alongside `last_error`.
    ///
    /// Derived from the same entry as `consecutive_failures` and `last_error` on purpose:
    /// an operator pairs the key with the position and skips it, so the two must describe
    /// one lane. Publishing the *oldest in flight* instead would name a healthy but slow
    /// lane while a different one was the failure being reported.
    pinning: Mutex<Option<(String, u64)>>,
}

impl EffectShared {
    fn new(
        name: String,
        sources: Vec<String>,
        digest_hash: String,
        resume: u64,
        live_boundary: u64,
    ) -> EffectShared {
        EffectShared {
            name,
            sources,
            digest_hash,
            position: AtomicU64::new(resume),
            shutdown: AtomicBool::new(false),
            running: AtomicBool::new(true),
            consecutive_failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
            terminal_skips: AtomicU64::new(0),
            last_terminal_error: Mutex::new(None),
            skips: Mutex::new(BTreeSet::new()),
            started: Instant::now(),
            retry_at_ms: AtomicU64::new(0),
            quarantined: AtomicBool::new(false),
            live_boundary,
            live_suppressed: AtomicU64::new(0),
            latest_collapsed: AtomicU64::new(0),
            blocked: Mutex::new(None),
            stuck: Mutex::new(BTreeMap::new()),
            wedged_lanes: AtomicU64::new(0),
            pinning: Mutex::new(None),
        }
    }

    /// Whether the reader thread is still alive: false once it has stopped, for any
    /// reason including a panic.
    ///
    /// True from the moment the handle is published rather than from the moment the
    /// thread is scheduled, so a blocked effect reads `1` here for as long as it takes
    /// the OS to run the thread that will clear it. That window is real and is left
    /// alone: `state()` already reports `blocked` throughout it, and starting at `false`
    /// would make every healthy effect read down at boot instead, which is the same
    /// disagreement pointed the other way and far more often.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// The position `on live` arms decline at or below.
    pub fn live_boundary(&self) -> u64 {
        self.live_boundary
    }

    /// How many positions `on live` arms have declined since this process started.
    pub fn live_suppressed(&self) -> u64 {
        self.live_suppressed.load(Ordering::Relaxed)
    }

    /// How many positions `on latest` arms have folded into another invocation since this
    /// process started.
    pub fn latest_collapsed(&self) -> u64 {
        self.latest_collapsed.load(Ordering::Relaxed)
    }

    /// The last watermark this effect has processed every matching event up to.
    ///
    /// Under lanes this is a **low-water mark**: every position at or below it is
    /// terminal, which is not the same as the newest position finished. One wedged lane
    /// holds it at that lane's position however far the others have run ahead, and that
    /// is the honest answer: it is what a restart resumes from.
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    /// How many times the pinning lane has failed in a row. Zero exactly when nothing is
    /// wedged.
    pub fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// The failure worth reporting. A block outranks a wedge: nothing is retrying, so the
    /// republished lane error would otherwise leave `last_error` empty for an effect that
    /// is stopped and needs a human.
    pub fn last_error(&self) -> Option<String> {
        self.blocked().or_else(|| {
            self.last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        })
    }

    /// Why this effect will not start, or `None` when it will.
    pub fn blocked(&self) -> Option<String> {
        self.blocked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn block(&self, reason: String) {
        tracing::error!("{reason}");
        *self.blocked.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    }

    /// How many lanes are wedged. The driver's own failures are not a lane and are not
    /// counted here.
    pub fn wedged_lanes(&self) -> u64 {
        self.wedged_lanes.load(Ordering::Relaxed)
    }

    /// The lane holding the low-water mark down, and the position it is stuck at.
    ///
    /// This is the whole of what makes a wedge actionable: without it an operator sees
    /// lag in the thousands with no way to find the one bad shop, and the skip endpoint
    /// takes a position they would have no way to name.
    pub fn pinning(&self) -> Option<(String, u64)> {
        self.pinning
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn terminal_skips(&self) -> u64 {
        self.terminal_skips.load(Ordering::Relaxed)
    }

    pub fn last_terminal_error(&self) -> Option<String> {
        self.last_terminal_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Ask the driver to skip `position`: an explicit, manual operator action to
    /// advance past a genuinely unprocessable event.
    pub fn request_skip(&self, position: u64) {
        self.skips
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(position);
    }

    fn skip_requested(&self, position: u64) -> bool {
        self.skips
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&position)
    }

    /// Forget skip requests the mark has passed, so a skip asked for a position that was
    /// never reached self-heals instead of waiting for a position that already ran.
    fn forget_skips(&self, through: u64) {
        self.skips
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|position| *position > through);
    }

    fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Record a driver-level failure, which belongs to no lane. See [`driver_lane`].
    fn record_failure(&self, message: &str, delay: Duration) {
        let attempt = self.lane_attempt(driver_lane()).saturating_add(1);
        self.record_stuck(driver_lane(), 0, attempt, message, delay);
    }

    /// Clear the driver's own failure. A lane's is cleared by [`EffectShared::clear_lane`]
    /// when that lane completes a position.
    fn clear_failures(&self) {
        self.clear_lane(driver_lane());
    }

    /// How many times in a row this lane has failed on its current position.
    fn lane_attempt(&self, lane: &LaneId) -> u32 {
        self.stuck
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(lane)
            .map_or(0, |stuck| stuck.attempt)
    }

    /// Record that `lane` failed on `position` and will try again after `delay`, and
    /// report the attempt count that failure was.
    fn record_lane_failure(
        &self,
        lane: &LaneId,
        position: u64,
        message: &str,
        delay: Duration,
    ) -> u32 {
        let attempt = self.lane_attempt(lane).saturating_add(1);
        self.record_stuck(lane, position, attempt, message, delay);
        attempt
    }

    fn record_stuck(
        &self,
        lane: &LaneId,
        position: u64,
        attempt: u32,
        message: &str,
        delay: Duration,
    ) {
        // Always a real deadline. Publishing zero would read back through `retry_in_ms` as
        // "nothing is waiting" for a lane that is in fact parked.
        let retry_at_ms = self.elapsed_ms().saturating_add(delay.as_millis() as u64);
        let mut stuck = self.stuck.lock().unwrap_or_else(PoisonError::into_inner);
        stuck.insert(
            lane.clone(),
            StuckLane {
                position,
                attempt,
                error: message.to_owned(),
                retry_at_ms,
            },
        );
        self.republish(&stuck);
    }

    /// This lane is healthy again.
    fn clear_lane(&self, lane: &LaneId) {
        let mut stuck = self.stuck.lock().unwrap_or_else(PoisonError::into_inner);
        if stuck.remove(lane).is_some() {
            self.republish(&stuck);
        }
    }

    /// Re-derive the single-valued fields from the lanes that are unhealthy.
    ///
    /// The pinning lane is the one holding the *lowest* position, because that is the one
    /// holding the prefix (and therefore journal retention) down. Every reader stays
    /// lock-free: they load atomics that this writes, rather than walking the map.
    fn republish(&self, stuck: &BTreeMap<LaneId, StuckLane>) {
        let driver = driver_lane();
        let lanes = stuck.keys().filter(|lane| *lane != driver).count();
        self.wedged_lanes.store(lanes as u64, Ordering::Relaxed);
        let worst = stuck.iter().min_by_key(|(_, stuck)| stuck.position);
        *self.pinning.lock().unwrap_or_else(PoisonError::into_inner) = worst
            .filter(|(lane, _)| *lane != driver)
            .map(|(lane, stuck)| (lane.as_str().to_owned(), stuck.position));
        match worst.map(|(_, stuck)| stuck) {
            Some(worst) => {
                self.consecutive_failures
                    .store(u64::from(worst.attempt), Ordering::Relaxed);
                *self
                    .last_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(worst.error.clone());
                self.retry_at_ms.store(worst.retry_at_ms, Ordering::Relaxed);
            }
            None => {
                self.consecutive_failures.store(0, Ordering::Relaxed);
                *self
                    .last_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                self.retry_at_ms.store(0, Ordering::Relaxed);
            }
        }
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
    ///
    /// Every word it can return is in [`EFFECT_STATES`], which `crate::metrics` renders
    /// as a state set; the two are together here because a state set whose list has
    /// fallen behind reads zero everywhere rather than failing.
    pub fn state(&self, head: u64) -> &'static str {
        if self.quarantined() {
            "quarantined"
        } else if self.blocked().is_some() {
            // Stopped and waiting for a person, which is neither a wedge (nothing is
            // retrying) nor lag (nothing is moving). It outranks a wedge for the same
            // reason a quarantine does: reporting the symptom would bury the cause.
            "blocked"
        } else if self.consecutive_failures() > 0 {
            // A wedged lane retrying under backoff and the driver re-subscribing after a
            // store error both land here. Both are stuck and retrying, and under lanes
            // "wedged" means at least one lane is: the healthy ones keep running, which
            // is the whole point, but the effect as a whole is not healthy.
            "wedged"
        } else if self.position() < head {
            "lagging"
        } else {
            "healthy"
        }
    }

    /// Whether this effect may still be handed work.
    ///
    /// Every condition here is one the dispatcher breaks on, and the offer side asks the
    /// same question through [`runnable`]. Keeping it in one place is what stops the two
    /// from drifting: a lane offered to a worker that immediately declines it is released
    /// and offered again, forever.
    pub(crate) fn accepts_work(&self) -> bool {
        !self.shutdown.load(Ordering::Relaxed) && !self.quarantined() && self.blocked().is_none()
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
    /// counter (the position is abandoned, not stuck). Pair with `clear_lane` so any
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
    pool: Arc<LanePool>,
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
    ///
    /// The readers stop before the pool does, and that order is load-bearing: a reader
    /// waits for its lanes to finish before publishing its final mark, so stopping the
    /// workers first would strand in-flight positions and leave the mark short of work
    /// that had actually completed.
    pub fn shutdown_and_join(self) {
        let EffectRuntime {
            shared,
            joins,
            pool,
            sweeper,
        } = self;
        for handle in &shared {
            handle.stop();
        }
        sweeper.signal_stop();

        let (tx, rx) = mpsc::channel();
        let pool_for_join = Arc::clone(&pool);
        let joiner = thread::Builder::new()
            .name("effect-join".to_owned())
            .spawn(move || {
                for join in joins {
                    if let Err(err) = join.join() {
                        tracing::error!("an effect thread panicked: {err:?}");
                    }
                }
                pool_for_join.shutdown();
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
            Err(_) => {
                // The readers did not drain in time. Release the workers anyway, so a
                // process on its way out is not held open by a pool waiting for work
                // nobody will offer it.
                pool.stop();
                tracing::warn!(
                    "effect drain timed out after {}s; leaving stuck invocation(s) to replay next start",
                    SHUTDOWN_JOIN_TIMEOUT.as_secs()
                );
            }
        }
    }
}

/// Everything a worker needs to run one lane of one effect.
///
/// Cloned as an `Arc` into the pool's queue rather than passed by reference, because a
/// lane outlives the batch that admitted it and the worker running it is not the thread
/// that read it off the log.
struct EffectCtx {
    shared: Arc<EffectShared>,
    unit: Arc<EffectUnit>,
    runtime: Arc<Runtime>,
    http: Arc<dyn HttpClient>,
    lanes: Lanes,
    /// Set when this context is finished with, so workers holding lanes from it stop
    /// rather than running on against a dispatcher that has gone away.
    cancelled: AtomicBool,
}

impl EffectCtx {
    fn name(&self) -> &str {
        &self.shared.name
    }

    /// Whether work may still be run for this context. See [`runnable`].
    fn accepts_work(&self) -> bool {
        !self.cancelled.load(Ordering::Relaxed) && self.shared.accepts_work()
    }
}

/// One effect's [`LaneState`] behind a lock, with the condvar the reader waits on.
struct Lanes {
    state: Mutex<LaneState>,
}

impl Lanes {
    /// `carried` says whether an earlier process left `effect_lane` rows behind. It seeds
    /// the sweep flag, because those rows are exactly the ones this process has to clean up
    /// and it may never write one of its own to notice them by.
    fn new(state: LaneState) -> Lanes {
        Lanes {
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LaneState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The shared pool the lanes of every effect run on.
///
/// One pool for the process, not one per effect. The contended resource is the single
/// `Arc<Mutex<OpDb>>` every journaled call goes through, so the bound that matters is a
/// process-wide one; a per-effect pool of sixteen with a dozen effects would put two
/// hundred threads on one mutex. It also lets an effect with one hot lane and an effect
/// with a thousand share capacity rather than being allotted it equally.
///
/// **`pool_size = 1` is not "lanes off".** A single worker still gives every lane mutual
/// exclusion and still lets a wedged lane step aside for a healthy one, because what
/// fixes the stall is the deferral in [`run_lane`], not the parallelism. Nobody has to
/// change a config file to get the fix.
struct LanePool {
    ready: Mutex<VecDeque<(Arc<EffectCtx>, LaneId)>>,
    wake: Condvar,
    stopping: AtomicBool,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl LanePool {
    fn start(size: u32) -> anyhow::Result<Arc<LanePool>> {
        let pool = Arc::new(LanePool {
            ready: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            stopping: AtomicBool::new(false),
            workers: Mutex::new(Vec::new()),
        });
        let mut workers = Vec::with_capacity(size as usize);
        for index in 0..size {
            let worker = Arc::clone(&pool);
            workers.push(
                thread::Builder::new()
                    .name(format!("effect-pool-{index}"))
                    .spawn(move || {
                        while let Some((ctx, lane)) = worker.next() {
                            // A backstop. A panic inside the invocation is caught by
                            // `attempt_caught` and becomes an ordinary lane failure, with
                            // a backoff and a `stuck` entry; this catches one anywhere
                            // else, so that an unwinding worker cannot be lost from a pool
                            // every effect in the process shares.
                            let ran = panic::catch_unwind(AssertUnwindSafe(|| {
                                run_lane(&ctx, &lane, &worker);
                            }));
                            if ran.is_err() {
                                tracing::error!(
                                    "effect `{}` panicked on lane `{lane}`; the lane is \
                                     released and its position retried",
                                    ctx.name()
                                );
                                // Released *and offered back*. `release` re-arms the lane
                                // in the state map, and without a matching offer it would
                                // sit `Queued` with nothing to run it: `admit` refuses to
                                // re-arm a lane that already has a place, so its positions
                                // would pin the mark for the life of the process while
                                // `/status` reported the effect merely lagging.
                                let rearmed = ctx.lanes.lock().release(&lane);
                                if rearmed && runnable(&ctx, &worker) {
                                    worker.offer(&ctx, [lane.clone()]);
                                }
                            }
                        }
                    })
                    .inspect_err(|_| {
                        // The workers already spawned hold an `Arc` on the pool and would
                        // spin in `next()` for the life of the process, keeping it alive
                        // after a boot that failed.
                        pool.stop();
                    })
                    .with_context(|| format!("spawning effect pool worker {index}"))?,
            );
        }
        *pool.workers.lock().unwrap_or_else(PoisonError::into_inner) = workers;
        Ok(pool)
    }

    fn offer(&self, ctx: &Arc<EffectCtx>, lanes: impl IntoIterator<Item = LaneId>) {
        let mut lanes = lanes.into_iter().peekable();
        // Every effect's reader passes through here on every idle tick, and most passes
        // have nothing to offer. Taking the one lock the whole pool shares to push nothing
        // is contention for its own sake.
        if lanes.peek().is_none() {
            return;
        }
        let mut ready = self.ready.lock().unwrap_or_else(PoisonError::into_inner);
        let before = ready.len();
        for lane in lanes {
            ready.push_back((Arc::clone(ctx), lane));
        }
        let added = ready.len() - before;
        drop(ready);
        for _ in 0..added {
            self.wake.notify_one();
        }
    }

    fn next(&self) -> Option<(Arc<EffectCtx>, LaneId)> {
        let mut ready = self.ready.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.stopping.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(claim) = ready.pop_front() {
                return Some(claim);
            }
            // Every `offer` and `stop` notifies, so this wakes on work rather than on the
            // clock. The timeout is insurance against a lost wakeup deadlocking the whole
            // pool, not a poll: at `pool_size` workers a quarter-second tick would be
            // thousands of acquisitions a second of the one lock they all share.
            let (next, _) = self
                .wake
                .wait_timeout(ready, POOL_IDLE_WAIT)
                .unwrap_or_else(PoisonError::into_inner);
            ready = next;
        }
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.wake.notify_all();
    }

    fn shutdown(&self) {
        self.stop();
        let workers = mem::take(&mut *self.workers.lock().unwrap_or_else(PoisonError::into_inner));
        for worker in workers {
            if let Err(err) = worker.join() {
                tracing::error!("an effect pool worker panicked: {err:?}");
            }
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

/// Start one reader thread per effect, the shared lane pool, and the retention sweeper.
/// The threads hold `Arc<Runtime>` (for `invoke_command` and the boundary fold); the
/// runtime does not hold the returned [`EffectRuntime`], so nothing cycles.
pub fn start_all(
    effects: Vec<Arc<EffectUnit>>,
    runtime: &Arc<Runtime>,
    http: Arc<dyn HttpClient>,
    config: &Config,
) -> anyhow::Result<EffectRuntime> {
    // No effects, no pool. A project of commands and projectors used to run no effect
    // threads at all, and should still.
    let size = if effects.is_empty() {
        0
    } else {
        config.effects.pool_size
    };
    let pool = LanePool::start(size)?;
    // Every `?` past this point would otherwise leave the workers spinning for the life of
    // the process: they hold their own `Arc` on the pool, nothing sets `stopping`, and
    // `shutdown_and_join` is unreachable because `EffectRuntime` was never built.
    let started = start_effects(effects, runtime, http, config, &pool);
    let (shared, joins, sweeper) = match started {
        Ok(started) => started,
        Err(err) => {
            pool.shutdown();
            return Err(err);
        }
    };
    Ok(EffectRuntime {
        shared,
        joins,
        pool,
        sweeper,
    })
}

#[allow(clippy::type_complexity)]
fn start_effects(
    effects: Vec<Arc<EffectUnit>>,
    runtime: &Arc<Runtime>,
    http: Arc<dyn HttpClient>,
    config: &Config,
    pool: &Arc<LanePool>,
) -> anyhow::Result<(Vec<Arc<EffectShared>>, Vec<JoinHandle<()>>, Sweeper)> {
    let mut shared = Vec::with_capacity(effects.len());
    let mut joins = Vec::with_capacity(effects.len());
    for unit in effects {
        let (handle, join) = spawn(
            unit,
            Arc::clone(runtime),
            Arc::clone(&http),
            Arc::clone(pool),
        )?;
        shared.push(handle);
        joins.push(join);
    }
    let sweeper = spawn_sweeper(Arc::clone(runtime), config)?;
    Ok((shared, joins, sweeper))
}

fn spawn(
    unit: Arc<EffectUnit>,
    runtime: Arc<Runtime>,
    http: Arc<dyn HttpClient>,
    pool: Arc<LanePool>,
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

    // Rule 15: `on live` is not a position constant in source, it is one the runtime
    // resolves once, at first activation, per data directory. Resolved here rather than in
    // the reader so it is fixed before any position can be dispatched, and for every
    // effect whether or not it has a `live` arm today, and that is what makes the number mean
    // "when this effect first ran here" rather than "when someone added a live arm".
    let activation = runtime.activate_effect(
        &name,
        runtime.log_head(),
        &unit.lane_scheme,
        &runtime::now_rfc3339(),
    )?;

    // Rule 15: changing an arm's `@key` repartitions the lanes, so the per-lane rows above
    // the mark are keyed under a scheme the new key never produces.
    //
    // **This is an operator signal, not a correctness gate, and it must not be turned into
    // one.** Reprocessing under a new key is safe in both directions: `begin_invocation`
    // reports `AlreadyTerminal` for anything the old scheme finished, and those rows are
    // never swept above the mark, so nothing re-fires. Coarse-to-fine, two positions that
    // were serialised now run concurrently, which is what the new key asks for;
    // fine-to-coarse they serialise. What stopping buys is that the change is *noticed*,
    // at the moment it happens, by the person who made it. `/status` is where that lands,
    // and a line in a boot log is not. Do not delete this as dead, and do not build
    // anything load-bearing on top of it.
    //
    // Compared on the key alone rather than on the digest's `signature_hash`, which also
    // moves for a delivery modifier and for an added event path. Neither of those
    // repartitions anything, and gating on the broader hash would stop an effect over an
    // edit that changed no lane.
    // Rendered back with the `@` an author writes, since the message names a declaration
    // in their source rather than a tephra event type.
    let repartitioned: Vec<String> =
        loader::repartitioned(&activation.lane_scheme, &unit.lane_scheme)
            .into_iter()
            .map(|ty| format!("@{ty}"))
            .collect();
    let outstanding = if repartitioned.is_empty() {
        0
    } else {
        runtime.effect_lanes_outstanding(&name)?
    };
    let blocking = !repartitioned.is_empty() && outstanding > 0;
    if !blocking && activation.lane_scheme != unit.lane_scheme {
        // Recorded whenever it moves, not only when it repartitions. An arm added for a
        // *new* event type repartitions nothing and would otherwise never be written down,
        // so a later `@key` change on that arm would compare against a scheme that never
        // mentioned it and be missed by both this guard and `hekla plan`.
        runtime.set_effect_lane_scheme(&name, &unit.lane_scheme)?;
        if !repartitioned.is_empty() {
            tracing::info!(
                "effect `{name}` changed the lane for {} and had drained, so the new key is \
                 in effect",
                repartitioned.join(", ")
            );
        }
    }

    let shared = Arc::new(EffectShared::new(
        name.clone(),
        sources,
        unit.digest_hash.clone(),
        resume,
        activation.live_boundary,
    ));
    let task_shared = Arc::clone(&shared);
    if blocking {
        // The two remedies are not equivalent, and the message says which is which.
        // Draining under the old key costs nothing. `hekla rewind` also deletes the
        // recorded invocations above the target, which is what makes those positions run
        // again, and perform their side effects again with them.
        shared.block(format!(
            "effect `{name}` will not start: the partition key for {} changed while \
             {outstanding} lane(s) are still outstanding above position {resume}. \
             Deploy the previous key and let it drain, which costs nothing; or, as an \
             escape hatch, `hekla \
             rewind {name} {resume}`, which discards those lane rows *and* the recorded \
             invocations above {resume}, so those positions run again and perform their \
             side effects again",
            repartitioned.join(", "),
        ));
    }

    let join = thread::Builder::new()
        .name(format!("effect-{name}"))
        .spawn(move || run(task_shared, unit, runtime, http, pool))
        .with_context(|| format!("spawning effect `{name}`"))?;
    Ok((shared, join))
}

fn run(
    shared: Arc<EffectShared>,
    unit: Arc<EffectUnit>,
    runtime: Arc<Runtime>,
    http: Arc<dyn HttpClient>,
    pool: Arc<LanePool>,
) {
    // Set before the thread started, and never cleared: the recovery is to fix the code
    // and restart, so there is nothing here to supervise.
    // A guard rather than a call at each exit, for the reason the projector's is one: a
    // panic anywhere under `supervise` unwinds past every explicit clear, and an effect
    // whose thread has died reporting `hekla_effect_up 1` forever is the one reading an
    // operator must be able to trust.
    let _running = RunningFlag(&shared);
    if shared.blocked().is_some() {
        return;
    }
    // Held across `supervise`'s retries, so a re-subscribe reuses one dispatcher rather
    // than racing a second one against the workers still holding the first's lanes.
    let mut ctx: Option<Arc<EffectCtx>> = None;
    supervise(&shared, |subscribed| {
        run_inner(&shared, &unit, &runtime, &http, &pool, &mut ctx, subscribed)
    });
    // The thread is done; nothing may keep working lanes it will no longer publish for.
    if let Some(ctx) = ctx {
        ctx.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Clears [`EffectShared::running`] however the thread leaves, panic included.
struct RunningFlag<'a>(&'a EffectShared);

impl Drop for RunningFlag<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Relaxed);
    }
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
        shared.record_failure(&format!("driver: {err:#}"), delay);
        // The driver is about to re-subscribe, which is umari's `restarts_total`. An
        // effect that is down rather than merely wedged shows as this climbing while
        // `hekla_effect_lag` does not fall.
        metrics::effect_restart(&shared.name);
        tracing::error!(
            "effect `{}` driver error (attempt {}): {err:#}",
            shared.name,
            next
        );

        let stop = sleep_watching(shared, delay);

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

/// The most positions one effect will hold in flight before it stops reading.
///
/// Matched to tephra's own batch cap so a whole batch always fits and the reader can
/// never block part-way through admitting one. Without a bound, an effect slower than
/// its arrival rate would grow its lane queues without limit while catching up, which
/// is the unbounded pending queue the sequential design deliberately did not have.
const MAX_INFLIGHT: usize = 1024;

/// The most recorded positions above its mark a dispatcher will read before it stops
/// collapsing.
///
/// Rows above the mark are never swept, so a wedged lane makes their number a function of
/// how long it has been wedged. Reading them all would put an unbounded allocation on the
/// restart path, which is the one an operator reaches for while the effect is wedged.
const RECORDED_MAX: usize = 1 << 16;

/// What one attempt at one position came to.
enum Step {
    /// The position reached a terminal state (completed, skipped, or already done);
    /// retire it and take the next in this lane.
    Done,
    /// It failed and will be retried. The lane parks for this long, and **the position
    /// stays at the head of its queue**: what a failure releases is the worker, not the
    /// work.
    Defer(Duration),
    /// Stop working this lane entirely: shutting down, or quarantined.
    Stop,
}

/// The reader: one thread per effect, owning the subscription and deciding which lane
/// each position belongs to.
///
/// It does no invocations itself. tephra's `Subscription` owns a forward-only cursor and
/// is not meant to be cloned or resumed from a stale one, so exactly one thread per
/// effect may read the log; everything else this loop does is bookkeeping that has to
/// happen in the same place as the read.
fn run_inner(
    shared: &Arc<EffectShared>,
    unit: &Arc<EffectUnit>,
    runtime: &Arc<Runtime>,
    http: &Arc<dyn HttpClient>,
    pool: &Arc<LanePool>,
    resumed_ctx: &mut Option<Arc<EffectCtx>>,
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
    // **Built once per effect thread, and reused when the driver re-subscribes.** A
    // transient store or op-DB error sends `supervise` round again; allocating a second
    // context there would leave the pool holding lanes from the first one, and the two
    // dispatchers would hand the same position to two workers. `begin_invocation` cannot
    // stop that, because reporting `Running` for an in-flight row is the crash-replay
    // contract. The lanes, the in-flight set and the stuck map all describe the effect
    // rather than one subscription, so keeping them across a re-subscribe is also the more
    // honest reading.
    let ctx = match resumed_ctx {
        Some(ctx) => {
            // The new subscription resumes from the persisted mark, which is at or below
            // anything still in flight, so it re-delivers work this context already has.
            // `LaneState::admit` drops those.
            ctx.lanes.lock().set_scanned(resume);
            Arc::clone(ctx)
        }
        None => {
            // Per-lane progress an earlier process recorded above the mark. Purely a
            // shortcut: without it every position above the mark would be dispatched and
            // told `AlreadyTerminal`, which is correct and slower. Read only here, because
            // a re-subscribe reuses the context and would throw the result away.
            let rows: HashMap<LaneId, u64> = runtime
                .effect_lanes(name)?
                .into_iter()
                .map(|(lane, position)| (LaneId::from(lane.as_str()), position))
                .collect();
            // Not a shortcut, unlike the rows above: rule 15 may fold a position into a
            // later invocation, and one an earlier process already began or finished must
            // not be folded *away* like that. It still takes over the group it lands in.
            // Read here for the same reason, since a re-subscribe keeps the context and
            // every row written since is for a position this dispatcher's own lane
            // high-water already answers for.
            //
            // **Only for an effect that can fold**, which is what bounds it. Invocation
            // rows above the mark are never swept, so a lane wedged for a month leaves a
            // month of them, and an effect with no `on latest` arm would pay for reading
            // every one to answer a question it never asks. `hek test`'s resolver
            // short-circuits on the same condition.
            let begun = if unit
                .arms
                .values()
                .any(|arm| arm.delivery == Delivery::Latest)
            {
                runtime.invocations_above(name, resume, RECORDED_MAX + 1)?
            } else {
                Vec::new()
            };
            // Past the bound the answer is incomplete, and an incomplete one is worse than
            // none: it would report a position as safe to fold when it is exactly the one
            // that must not be. Folding nothing is the behaviour that shipped before rule
            // 15, and the mark advancing past the wedge is what makes the next start read a
            // short list again.
            let blind = begun.len() > RECORDED_MAX;
            if blind {
                tracing::warn!(
                    "effect `{name}` has more than {RECORDED_MAX} recorded invocations above \
                     position {resume}, so this process will not collapse its `on latest` \
                     batches; clear the lane holding its watermark down"
                );
            }
            let fresh = Arc::new(EffectCtx {
                shared: Arc::clone(shared),
                unit: Arc::clone(unit),
                runtime: Arc::clone(runtime),
                http: Arc::clone(http),
                lanes: Lanes::new(LaneState::resuming(
                    resume,
                    rows,
                    begun.into_iter().collect(),
                    blind,
                )),
                cancelled: AtomicBool::new(false),
            });
            *resumed_ctx = Some(Arc::clone(&fresh));
            fresh
        }
    };

    // Only a writer-backed store advances a watermark; see `Store::subscribe`. An effect
    // driver on a read-only log would park forever rather than idle.
    let mut sub = runtime
        .store()
        .subscribe("an effect", query, Position::new(resume))?;
    // Tell the supervisor the driver is back on the log; it owns what that means.
    subscribed();

    loop {
        // A lane whose backoff has expired, and a lane an operator has asked to skip
        // past, both become runnable here rather than on a timer of their own.
        let woken = {
            let mut state = ctx.lanes.lock();
            let mut woken = state.promote(Instant::now());
            woken.extend(state.promote_matching(|position| shared.skip_requested(position)));
            woken
        };
        pool.offer(&ctx, woken);

        let draining = shared.shutdown.load(Ordering::Relaxed);
        let mut fetched = false;
        // Stop reading while the in-flight set is full, so a fast log cannot outrun the
        // pool into unbounded memory. The reader picks up again on its next pass rather
        // than being woken: a completion is at most one idle poll away from being noticed,
        // which for a resume point and a sweep bound is nothing.
        let backpressured = !draining && ctx.lanes.lock().inflight() >= MAX_INFLIGHT;
        if !draining && !backpressured {
            let batch = sub
                .poll_batch()
                .map_err(|err| anyhow::anyhow!("reading events: {err}"))?;
            fetched = !batch.is_empty();
            let armed = admit(&ctx, &batch, sub.position().get())?;
            pool.offer(&ctx, armed);
        }
        publish_mark(&ctx, name)?;

        // A quarantine stops the whole effect, not one lane: a replay divergence is
        // evidence about the program, and letting the other lanes run on would be
        // processing them on the strength of an assumption just shown false.
        if shared.quarantined() {
            return Ok(());
        }
        // Only lanes a worker owns. A lane merely queued is abandoned rather than
        // finished, and its positions stay `running` in the op-DB and replay at the next
        // start, so waiting for the queue to empty would wait for work nobody will run.
        if draining && ctx.lanes.lock().running() == 0 {
            break;
        }
        if fetched {
            continue;
        }
        let idle = idle_for(&ctx);
        // The subscription wait is for the *log* to move past the cursor, so it only
        // blocks when this loop is waiting on the log. When the poll was skipped the
        // cursor has not moved and the watermark is already past it, so the wait returns
        // instantly: at the in-flight cap that is a reader spinning at full CPU against
        // the lane lock and the shared pool lock for the length of a catch-up, which is
        // exactly when every other effect can least afford the contention.
        if draining || backpressured {
            thread::sleep(idle);
        } else if let WaitOutcome::Closed = sub.wait_timeout(idle) {
            break;
        }
    }
    // A worker that stopped between positions may have retired one since the last pass
    // published. Cheap here, and it saves re-running those at the next start.
    publish_mark(&ctx, name)?;
    Ok(())
}

/// How long the reader may sleep: until the earliest parked lane is due, capped at the
/// idle poll so a completion is never more than that late in moving the mark.
///
/// Floored well above zero, because a deadline that has just passed would otherwise spin
/// the reader against the lane lock until a worker picked the lane up.
fn idle_for(ctx: &EffectCtx) -> Duration {
    const FLOOR: Duration = Duration::from_millis(10);
    let next = ctx.lanes.lock().next_deadline();
    match next {
        Some(due) => due
            .saturating_duration_since(Instant::now())
            .clamp(FLOOR, IDLE_POLL),
        None => IDLE_POLL,
    }
}

/// Assign every position in a batch to its lane and queue it, returning the lanes that
/// became runnable.
///
/// The lane keys are computed **outside** the state lock: `record_of` decodes an event,
/// and holding the lock across a thousand of those would stall every worker trying to
/// retire a position.
fn admit(
    ctx: &Arc<EffectCtx>,
    batch: &[(Position, tephra::Event)],
    scanned: u64,
) -> anyhow::Result<Vec<LaneId>> {
    let program = ctx.runtime.program();
    let declared = program.effect(ctx.name());
    let mut resolved = Vec::with_capacity(batch.len());
    let mut suppressed = 0u64;
    let mut collapsed = 0u64;
    for (position, event) in batch {
        // The subscription selects on event type, so this normally matches. An event
        // type the effect no longer answers is simply scanned past, exactly as a
        // non-matching one always was.
        let Some(arm) = ctx.unit.arms.get(event.event_type()) else {
            continue;
        };
        // Rule 15: an `on live` arm declines history outright. Declining is not
        // completing: the position gets no `effect_invocation` row, never enters the
        // in-flight set, and the mark passes straight over it. A terminal row with an
        // empty journal would read back through `replay` as `Matched`, a claim that a
        // position nothing ran is covered.
        if arm.delivery == Delivery::Live && position.get() <= ctx.shared.live_boundary() {
            suppressed += 1;
            continue;
        }
        let lane = match declared {
            Some(declared) => lane_of(program, declared, arm.index, *position, event.as_ref()),
            None => LaneId::unreadable(),
        };
        let collapsible = collapsible(arm.delivery, &lane);
        resolved.push((position.get(), lane, arm.index, collapsible));
    }

    if suppressed > 0 {
        ctx.shared
            .live_suppressed
            .fetch_add(suppressed, Ordering::Relaxed);
    }

    let mut armed = Vec::new();
    let mut state = ctx.lanes.lock();
    for (position, lane, arm, collapsible) in resolved {
        let admitted = state.admit(lane.clone(), position, arm, collapsible);
        if admitted.armed {
            armed.push(lane);
        }
        collapsed += u64::from(admitted.folded);
    }
    // In the same critical section as the admissions, so the mark can never be computed
    // from a cursor that has run ahead of the positions it let in.
    state.set_scanned(scanned);
    drop(state);

    if collapsed > 0 {
        ctx.shared
            .latest_collapsed
            .fetch_add(collapsed, Ordering::Relaxed);
    }
    Ok(armed)
}

/// Whether rule 15 may fold this position into another invocation.
///
/// An `on latest` arm, in a lane whose key was actually read. **Never the unreadable
/// lane**, which is not one key but every record whose key could not be read at all, so
/// folding those together would drop work for a key nobody identified. `hek test`'s
/// dispatcher stops collapsing the whole log at the first such record; here only that lane
/// is excluded, because lanes are exactly what makes the other keys independent of it, and
/// it wedges on its own while they run.
fn collapsible(delivery: Delivery, lane: &LaneId) -> bool {
    delivery == Delivery::Latest && *lane != LaneId::unreadable()
}

/// The lane one event belongs to.
///
/// A key that cannot be read is **not** an error here. The position is admitted to
/// [`LaneId::unreadable`] so it wedges where it is, and `deliver` (which computes the
/// same key through the same `partition_key`) reports it in heklang's own words. Failing
/// here instead would either skip the position silently or take down the whole reader for
/// one bad record.
pub(crate) fn lane_of(
    program: &heklang::Program,
    declared: &heklang::ir::Effect,
    arm: usize,
    position: Position,
    event: tephra::EventRef<'_>,
) -> LaneId {
    let Ok(record) = crate::heklang_host::record_of(program, position, event) else {
        return LaneId::unreadable();
    };
    let Some(arm) = declared.arms.get(arm) else {
        return LaneId::unreadable();
    };
    match heklang::partition_key(arm, &record.event) {
        Ok(keys) => crate::lane::encode(&keys),
        Err(_) => LaneId::unreadable(),
    }
}

/// Persist and publish the effect's low-water mark, and say which lane is holding it.
///
/// Forward only, and durable before in-memory, exactly as the sequential watermark was.
/// What changed is only how the number is arrived at: it is derived from the in-flight
/// set rather than being the last position processed.
fn publish_mark(ctx: &Arc<EffectCtx>, name: &str) -> anyhow::Result<()> {
    let mark = ctx.lanes.lock().low_water();
    if mark <= ctx.shared.position() {
        return Ok(());
    }
    ctx.runtime.set_effect_watermark(name, mark)?;
    ctx.shared.position.store(mark, Ordering::Relaxed);
    // The mark moved, which is the only thing that counts as progress for an effect: a
    // wedged lane holds it down no matter how far the healthy ones have run ahead.
    metrics::effect_progress(name);
    ctx.shared.forget_skips(mark);
    // Only when a row would actually be deleted. A healthy effect records none, so this
    // keeps the sweep off the per-event path rather than issuing a delete that matches
    // nothing on every advance. Asked and forgotten in two short critical sections, so the
    // lane lock is never held across the op-DB write.
    //
    // Forgetting the lanes the mark has passed is unconditional, because it is what bounds
    // the in-memory admission guard and a healthy effect never takes the sweep branch at
    // all.
    let sweep = {
        let mut state = ctx.lanes.lock();
        state.forget(mark);
        state.rows_to_sweep(mark)
    };
    if sweep {
        ctx.runtime.sweep_effect_lanes(name, mark)?;
        ctx.lanes.lock().prune_rows(mark);
    }
    Ok(())
}

/// Run one lane until it empties, defers, or the process stops.
///
/// The retry ladder lives on the lane rather than inside this call, which is the whole
/// difference from the sequential driver: a failure records itself, parks the lane and
/// **returns the worker**. `pool_size` wedged lanes therefore cost `pool_size` entries in
/// a map rather than every thread in the pool.
fn run_lane(ctx: &Arc<EffectCtx>, lane: &LaneId, pool: &LanePool) {
    // Ownership is taken here rather than when the pool handed the lane over, so a stale
    // queue entry is declined instead of running a lane a worker already owns.
    if !ctx.lanes.lock().claim(lane) {
        return;
    }
    let mut parked = false;
    loop {
        // A quarantine stops the whole effect, not the lane that found it: every other
        // lane would otherwise keep working on the strength of an assumption a replay has
        // just shown false, which is what the sequential driver's `Interrupted` prevented.
        // Asked through the same predicate the re-offer below uses, so the two cannot
        // disagree about whether this lane should be running.
        if !runnable(ctx, pool) {
            break;
        }
        let Some(work) = ctx.lanes.lock().head(lane) else {
            break;
        };
        match attempt_caught(ctx, lane, work) {
            Step::Done => {
                let owed = ctx.lanes.lock().complete(lane, work.position);
                if owed {
                    record_lane_row(ctx, lane, work.position);
                }
            }
            Step::Defer(delay) => {
                ctx.lanes.lock().defer(lane, Instant::now() + delay);
                parked = true;
                break;
            }
            Step::Stop => break,
        }
    }
    // A parked lane keeps its place; anything else gives it back, and takes it straight
    // to the pool again if work arrived while this worker held it.
    let rearmed = !parked && ctx.lanes.lock().release(lane);
    // Only when there is a dispatcher left to run it for. A lane released after a
    // quarantine or a shutdown keeps its queued work, which replays at the next start;
    // re-offering it would hand it to a worker that breaks on the same condition at the
    // top of this function and releases it again, spinning the pool at full CPU.
    if rearmed && runnable(ctx, pool) {
        pool.offer(ctx, [lane.clone()]);
    }
}

/// Whether this effect should still be handed lanes.
///
/// **The same predicate `run_lane` breaks on**, deliberately one expression rather than two
/// lists that have to be kept in step. When they drifted apart, a lane released after a
/// quarantine was offered to a worker that broke on the condition the offer had not
/// checked, released it, and was offered it again: the pool spun at full CPU for as long
/// as the process lived.
fn runnable(ctx: &EffectCtx, pool: &LanePool) -> bool {
    !pool.stopping() && ctx.accepts_work()
}

/// Record how far a lane has got, for a resume to skip.
///
/// Written after the completion is durable, never before: a crash between the two replays
/// the position into `begin_invocation`'s `AlreadyTerminal`, which costs nothing, while
/// the reverse order would let a row claim a position that never finished. A failure here
/// is logged and dropped for the same reason: the row is an optimisation, and losing one
/// costs a re-dispatch rather than a correctness hole.
fn record_lane_row(ctx: &Arc<EffectCtx>, lane: &LaneId, position: u64) {
    match ctx
        .runtime
        .record_effect_lane(ctx.name(), lane.as_str(), position)
    {
        Ok(()) => ctx.lanes.lock().record_row(lane, position),
        Err(err) => tracing::warn!(
            "effect `{}` could not record lane `{lane}` at position {position}; it will be \
             re-dispatched after a restart: {err:#}",
            ctx.name()
        ),
    }
}

/// [`attempt`], with an unwinding handler turned into an ordinary lane failure.
///
/// A panic is caught here rather than around the whole lane because everything a failure
/// owes has to happen either way: a backoff, so a deterministic panic does not spin the
/// op-DB; a `stuck` entry, so `/status` reports the effect as wedged and names the lane;
/// and an attempt count, because the operator skip is gated on the position having failed
/// at least once. Catching it further out gave none of those, which left the one escape
/// from an unprocessable event unreachable for the very failure the catch was added for.
fn attempt_caught(ctx: &Arc<EffectCtx>, lane: &LaneId, work: Work) -> Step {
    match panic::catch_unwind(AssertUnwindSafe(|| attempt(ctx, lane, work))) {
        Ok(step) => step,
        Err(payload) => {
            let tried = ctx.shared.lane_attempt(lane);
            defer(
                ctx,
                lane,
                work.position,
                tried,
                &panic_message(&payload),
                None,
            )
        }
    }
}

/// What a caught panic said, as far as the payload carries it.
fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    let said = payload
        .downcast_ref::<&str>()
        .map(|said| (*said).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    match said {
        Some(said) => format!("the handler panicked: {said}"),
        None => "the handler panicked".to_owned(),
    }
}
/// One attempt at one position.
///
/// Everything durable about an invocation is unchanged from the sequential driver: the
/// reservation, the journal, the completion, the verify replay. What moved out is the
/// retry loop, which is now the lane's.
///
/// Rule 15's collapse adds one thing and only one: every statement that makes this
/// invocation terminal also records the range it folded. They are one write on purpose.
/// The moment a terminal step returns, the whole group leaves the in-flight set and the
/// mark moves past it, so a completion that landed without the range would leave those
/// positions covered by an invocation that does not admit to covering them.
fn attempt(ctx: &Arc<EffectCtx>, lane: &LaneId, work: Work) -> Step {
    let effect = ctx.name();
    let position = work.position;
    let folded = work.collapsed_from;
    let tried = ctx.shared.lane_attempt(lane);
    match ctx.runtime.begin_invocation(
        effect,
        position,
        &ctx.unit.digest_hash,
        &runtime::now_rfc3339(),
    ) {
        Ok(InvocationState::AlreadyTerminal) => {
            // As terminal as any other completion, so it clears the lane the same way. A
            // completion whose op-DB write succeeded but whose response errored lands
            // here on the retry, and leaving the entry would report a healthy effect as
            // wedged forever and leave `tried > 0` for the lane's next position, which is
            // what the operator-skip guard below relies on being zero.
            //
            // No collapse range is written here, and none is owed. Reaching this with a
            // group means an earlier process completed this position and then lost both
            // ways of saying so, its lane row and a mark advance; it resumed from the same
            // mark with the same lane rows, so it built the same group and its row already
            // carries the same range this one would write.
            ctx.shared.clear_lane(lane);
            return Step::Done;
        }
        Ok(InvocationState::Running) => {}
        // An op-DB error is the same promise as a handler failure: retry forever with
        // backoff. It used to travel up to `supervise` and re-subscribe, which a worker
        // cannot do; parking the lane is the same behaviour without the reader's help.
        Err(err) => return defer(ctx, lane, position, tried, &format!("{err:#}"), None),
    }

    // Honor an operator skip only once this position is genuinely wedged (it has failed
    // at least once). Checking before the first attempt would let a skip requested for a
    // not-yet-reached position drop a healthy event.
    if tried > 0 && ctx.shared.skip_requested(position) {
        match honor_skip(&ctx.shared, lane, position, || {
            ctx.runtime
                .skip_invocation(effect, position, &runtime::now_rfc3339(), folded)
        }) {
            Ok(()) => {
                tracing::warn!(
                    "effect `{effect}` skipped wedged position {position} in lane `{lane}` by \
                     operator request"
                );
                metrics::effect_invocation(effect, "skipped");
                return Step::Done;
            }
            Err(err) => return defer(ctx, lane, position, tried, &format!("{err:#}"), None),
        }
    }

    match try_invocation(effect, position, &ctx.runtime, &ctx.http) {
        Ok(()) => {
            // Complete first, then check. The live run has already performed and
            // journaled its side effects, so this position's work is genuinely
            // done; leaving the row `running` so the check could report on it
            // would make the next boot re-enter the handler in `Live` mode and
            // perform for real the very call the sealed replay refused. The
            // detection would become the double-fire it exists to prevent.
            if let Err(err) =
                ctx.runtime
                    .complete_invocation(effect, position, &runtime::now_rfc3339(), folded)
            {
                return defer(ctx, lane, position, tried, &format!("{err:#}"), None);
            }
            ctx.shared.clear_lane(lane);
            // Before the verify check, not after it. The invocation is complete by every
            // durable measure at this point: the row is written and the lane is clear. A
            // quarantine below stops the effect and returns, so counting afterwards would
            // leave the one invocation an incident turned on counted under no outcome at
            // all, which is exactly the invocation someone goes looking for.
            metrics::effect_invocation(effect, "completed");
            if ctx.runtime.verify() {
                // `Live`: this process just watched the invocation complete, and an
                // operator skip returned above rather than reaching here, so an empty
                // journal here is a run that genuinely called nothing.
                let outcome = replay(effect, position, &ctx.runtime, Asked::Live);
                if let Some(violation) = outcome.violation(effect, position) {
                    // Durable, so the restart a wedged effect invites does not silently
                    // clear it. The mark is deliberately left where it is: this position
                    // is terminal, but nothing further should be dispatched until an
                    // operator has looked.
                    if let Err(err) =
                        ctx.runtime
                            .quarantine_effect(effect, position, &violation.to_string())
                    {
                        tracing::error!("effect `{effect}` could not record a quarantine: {err:#}");
                    }
                    ctx.shared.quarantine(&violation);
                    return Step::Stop;
                }
            }
            Step::Done
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
            // arm does. A failing op-DB write here parks the lane and the position is
            // retried, so mutating first would count the skip twice and clear the
            // wedge state for an invocation that never actually completed.
            if let Err(err) =
                ctx.runtime
                    .complete_invocation(effect, position, &runtime::now_rfc3339(), folded)
            {
                return defer(ctx, lane, position, tried, &format!("{err:#}"), None);
            }
            ctx.shared.record_terminal_skip(&failure.message);
            ctx.shared.clear_lane(lane);
            metrics::effect_invocation(effect, "terminal");
            Step::Done
        }
        Err(failure) => defer(
            ctx,
            lane,
            position,
            tried,
            &failure.message,
            failure.retry_after,
        ),
    }
}

/// Record a failure against its lane and say how long that lane parks for.
fn defer(
    ctx: &Arc<EffectCtx>,
    lane: &LaneId,
    position: u64,
    tried: u32,
    message: &str,
    retry_after: Option<Duration>,
) -> Step {
    let delay = retry_delay(tried, retry_after);
    let attempt = ctx
        .shared
        .record_lane_failure(lane, position, message, delay);
    metrics::effect_invocation(ctx.name(), "failed");
    tracing::error!(
        "effect `{}` invocation at position {position} in lane `{lane}` failed (attempt \
         {attempt}), retrying in {delay:?}: {message}",
        ctx.name()
    );
    Step::Defer(delay)
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
        secrets: Some(Arc::clone(runtime.secrets_shared())),
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
    lane: &LaneId,
    position: u64,
    complete: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    complete()?;
    shared
        .skips
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&position);
    shared.clear_lane(lane);
    Ok(())
}

/// Sleep up to `total`, returning `true` if shutdown fired.
///
/// Only the driver supervisor waits like this now. An invocation's backoff is the lane's
/// (see [`run_lane`]), and a skip wakes a parked lane through the reader's own promote
/// pass rather than by cutting a sleep short, which is what lets a wedged lane wait
/// without holding a worker.
fn sleep_watching(shared: &EffectShared, total: Duration) -> bool {
    let tick = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < total {
        if shared.shutdown.load(Ordering::Relaxed) {
            return true;
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
        // Unjournaled, so it re-runs here the way `reveal` does. Answering nothing
        // would wedge every replay of a secret-reading effect on `MissingSecret` and
        // report a divergence that is not there, which is the fault this check exists
        // to find rather than to cause.
        secrets: Some(Arc::clone(runtime.secrets_shared())),
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
        EffectShared::new("test".to_owned(), Vec::new(), String::new(), 0, 0)
    }

    /// `EFFECT_STATES` is what `crate::metrics` renders as a state set, so a word missing
    /// from it makes every `hekla_effect_state` series read zero rather than making
    /// anything fail. Each branch of `state` is exercised here against the list, and the
    /// length assertion stops a word being added to one and not the other.
    #[test]
    fn a_state_set_covers_every_effect_state() {
        let healthy = test_shared();
        assert_eq!(healthy.state(0), "healthy");
        assert_eq!(healthy.state(9), "lagging");

        let wedged = test_shared();
        wedged.record_failure("boom", Duration::from_millis(1));
        assert_eq!(wedged.state(0), "wedged");

        let quarantined = test_shared();
        quarantined.quarantine(&Violation::ReplayDivergence {
            effect: "test".to_owned(),
            position: 1,
            detail: "diverged".to_owned(),
        });
        assert_eq!(quarantined.state(0), "quarantined");

        let blocked = test_shared();
        blocked.block("the key moved".to_owned());
        assert_eq!(blocked.state(0), "blocked");

        for state in [
            healthy.state(0),
            healthy.state(9),
            wedged.state(0),
            quarantined.state(0),
            blocked.state(0),
        ] {
            assert!(
                EFFECT_STATES.contains(&state),
                "`{state}` is a word `state` returns and is not in EFFECT_STATES"
            );
        }
        assert_eq!(
            EFFECT_STATES.len(),
            5,
            "a word `state` can return belongs in `EFFECT_STATES` as well"
        );
    }

    fn test_lane() -> LaneId {
        LaneId::from("i:1")
    }

    /// The unreadable lane is the one place a key was not read, so it holds records for
    /// keys nobody identified. Folding those together would drop work rather than repeat
    /// it, which is the one way collapse could break the rule it serves. Pinned here
    /// because a program that reaches it cannot be written: the checker refuses a key type
    /// that could fail to read, so only a host handing over an event its own declaration
    /// disagrees with gets there.
    #[test]
    fn the_unreadable_lane_never_collapses() {
        assert!(collapsible(Delivery::Latest, &test_lane()));
        assert!(!collapsible(Delivery::Latest, &LaneId::unreadable()));
        assert!(!collapsible(Delivery::Every, &test_lane()));
        assert!(!collapsible(Delivery::Live, &test_lane()));
    }

    #[test]
    fn a_failed_completion_leaves_the_skip_request_pending() {
        let shared = test_shared();
        let lane = test_lane();
        shared.request_skip(7);
        shared.record_lane_failure(&lane, 7, "boom", Duration::from_millis(200));

        let err = honor_skip(&shared, &lane, 7, || anyhow::bail!("op-db is locked")).unwrap_err();
        assert!(err.to_string().contains("op-db is locked"));
        assert!(
            shared.skip_requested(7),
            "a skip must survive a failed completion so the driver honors it on the next pass"
        );
        assert_eq!(
            shared.consecutive_failures(),
            1,
            "the position is still wedged"
        );

        honor_skip(&shared, &lane, 7, || Ok(())).unwrap();
        assert!(!shared.skip_requested(7));
        assert_eq!(shared.consecutive_failures(), 0);
        assert_eq!(shared.last_error(), None);
    }

    /// Lanes mean several positions can be wedged at once, which one slot could not say:
    /// the second request used to overwrite the first and the operator's first skip was
    /// silently lost.
    #[test]
    fn two_wedged_positions_can_both_be_skipped() {
        let shared = test_shared();
        shared.request_skip(7);
        shared.request_skip(9);
        assert!(shared.skip_requested(7) && shared.skip_requested(9));

        honor_skip(&shared, &test_lane(), 7, || Ok(())).unwrap();
        assert!(!shared.skip_requested(7));
        assert!(shared.skip_requested(9), "the other request is untouched");
    }

    /// A skip for a position the effect never reached would otherwise sit in the set
    /// forever, waiting on work that has already gone past.
    #[test]
    fn a_skip_the_mark_passed_is_forgotten() {
        let shared = test_shared();
        shared.request_skip(3);
        shared.request_skip(11);
        shared.forget_skips(5);
        assert!(!shared.skip_requested(3));
        assert!(shared.skip_requested(11));
    }

    /// The pinning lane is the one holding the *lowest* position, because that is what
    /// holds the prefix (and therefore journal retention) down. It is not the newest
    /// failure, and it is not the worst-looking one.
    #[test]
    fn the_reported_failure_is_the_lane_holding_the_prefix() {
        let shared = test_shared();
        let (old, new) = (LaneId::from("i:1"), LaneId::from("i:2"));
        shared.record_lane_failure(&new, 900, "newer", Duration::from_millis(200));
        shared.record_lane_failure(&old, 500, "older", Duration::from_millis(200));

        assert_eq!(shared.last_error().as_deref(), Some("older"));
        assert_eq!(shared.wedged_lanes(), 2);

        shared.clear_lane(&old);
        assert_eq!(
            shared.last_error().as_deref(),
            Some("newer"),
            "clearing the pinning lane promotes the next one"
        );
        assert_eq!(shared.wedged_lanes(), 1);

        shared.clear_lane(&new);
        assert_eq!(shared.consecutive_failures(), 0);
        assert_eq!(shared.state(5), "lagging", "no lane is wedged any more");
    }

    /// A driver error belongs to no lane, so it must not be counted as one, but it must
    /// still be the failure reported, because a driver that cannot read the log at all is
    /// a worse problem than any one lane's.
    #[test]
    fn a_driver_failure_is_reported_without_being_counted_as_a_lane() {
        let shared = test_shared();
        shared.record_lane_failure(&test_lane(), 4, "lane", Duration::from_millis(200));
        shared.record_failure("driver: reading events", Duration::from_millis(200));

        assert_eq!(shared.wedged_lanes(), 1, "the driver is not a lane");
        assert_eq!(
            shared.last_error().as_deref(),
            Some("driver: reading events")
        );
        assert_eq!(shared.pinning(), None, "and it is not a partition key");
    }

    /// The regression this guards spun a pool worker at full CPU. `run_lane` learned to
    /// break on a quarantine while the re-offer beside it still only checked shutdown, so
    /// a lane released after a quarantine was handed straight back to a worker that
    /// declined it, released it, and was handed it again, starving every other effect in
    /// the process. A live divergence cannot be provoked from a test (the language is pure
    /// and every impure call is journaled, so a replay always agrees with its first run),
    /// which is why the predicate itself is what is pinned here.
    #[test]
    fn an_effect_that_cannot_run_accepts_no_work() {
        let shared = test_shared();
        assert!(shared.accepts_work(), "a healthy effect takes lanes");

        shared.record_lane_failure(&test_lane(), 4, "boom", BACKOFF_BASE);
        assert!(
            shared.accepts_work(),
            "a wedge is not a reason to stop: the other lanes keep running, which is the \
             whole point of partitioning"
        );

        shared.quarantine(&Violation::ReplayDivergence {
            effect: "test".to_owned(),
            position: 1,
            detail: "a call the journal does not have".to_owned(),
        });
        assert!(
            !shared.accepts_work(),
            "a quarantine stops the whole effect"
        );
    }

    #[test]
    fn a_blocked_or_stopping_effect_accepts_no_work() {
        let blocked = test_shared();
        blocked.block("its key changed".to_owned());
        assert!(!blocked.accepts_work());

        let stopping = test_shared();
        stopping.stop();
        assert!(!stopping.accepts_work());
    }

    /// Both the shutdown paths return before the full backoff elapses, so the case is
    /// timed. Asserting only the returned bool would pass even with the early return
    /// deleted: the call would simply take the whole 30 seconds and still report `true`.
    #[test]
    fn sleep_watching_returns_early_for_a_shutdown() {
        let shared = test_shared();
        shared.stop();
        let started = Instant::now();
        assert!(
            sleep_watching(&shared, Duration::from_secs(30)),
            "a shutdown is reported as such"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown must cut the backoff short, waited {:?}",
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
        shared.record_failure("driver: reading events: op-db is locked", BACKOFF_BASE);
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
