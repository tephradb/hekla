//! The effect runtime: durable execution of side effects.
//!
//! One dedicated thread per effect subscribes to its `source` and processes
//! matching events strictly in order, one invocation per event. An invocation
//! runs the effect's straight-line `handle`, whose impure builtins (`http.*`,
//! `invoke_command`, `read`, `scan`, `now`) are journaled: each call records its
//! result in the operational DB, so a crash mid-handler resumes by replaying the
//! journaled calls and running only the unjournaled tail live. `log` is not
//! journaled.
//!
//! The journal is written call-by-call in autocommit, never wrapped in a
//! per-invocation transaction: journaled side effects must survive a crash so
//! replay skips them. Because completed calls persist, retrying a failed
//! invocation replays them and fails at the same point without re-firing.
//!
//! Durability boundaries: `invoke_command` lands the domain fact exactly-once when
//! the target command is idempotent under replay. Its deterministic idempotency
//! key deduplicates in the common path; across the narrow crash window between the
//! command's append and its finalize, the key is cleared at startup (like every
//! pending key), so the command's own DCB boundary is what dedupes the replay,
//! exactly as for HTTP commands. Raw `http.*` is at-least-once (a crash between a
//! successful request and its journal write re-fires on replay).
//!
//! A handler error (a script bug, or a transport error / 5xx the runtime refuses
//! to surface) wedges the invocation: it retries forever with capped backoff,
//! never skipping, surfacing as a distinct failure count and last error in
//! `/status`. The only escape past a genuinely unprocessable event is an explicit
//! operator skip.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};
use starlark::environment::Module;
use tephra::{Event, Position, WaitOutcome};
use ureq::Agent;
use ureq::typestate::{WithBody, WithoutBody};

use crate::config::Config;
use crate::context::{CommandContext, EffectCtx, EffectHost};
use crate::dispatch;
use crate::envelope::{self, Envelope};
use crate::loader::EffectUnit;
use crate::opdb::{InvocationState, SWEEP_CHUNK};
use crate::runtime::{self, Runtime};
use crate::starlark_builtins::{
    LoadedModule, ModuleDef, alloc_event, call_handler_with_effect_ctx, thaw,
};

/// Per-handler instruction budget. Bounds a runaway script at dispatch time.
const MAX_TICKS: u64 = 10_000_000;
/// How long an idle, caught-up effect waits before polling again.
const IDLE_POLL: Duration = Duration::from_millis(250);
/// The ceiling on the wedge retry backoff, so a stuck effect keeps retrying at a
/// steady cadence rather than backing off unboundedly.
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// The base wedge retry backoff, doubled each attempt up to [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::from_millis(200);
/// How long a graceful shutdown waits for effects to drain before abandoning a
/// stuck one (its invocation stays `running` and replays next start).
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the retention sweeper runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Observable state for one effect, shared with the runtime (for `/status`) and
/// the skip endpoint. Holds no reference to the runtime, so nothing cycles.
pub struct EffectShared {
    pub name: String,
    position: AtomicU64,
    shutdown: AtomicBool,
    consecutive_failures: AtomicU64,
    last_error: Mutex<Option<String>>,
    /// The position an operator asked to skip, or `0` for none (no event sits at
    /// position 0).
    skip_position: AtomicU64,
}

impl EffectShared {
    /// The last watermark this effect has processed every matching event up to.
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    /// How many times the current (stuck) invocation has failed in a row, `0`
    /// when healthy. Distinguishes a wedge from ordinary lag.
    pub fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// The last error the current invocation hit, if it is failing.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
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
/// `Arc<Runtime>` (for `invoke_command` and `read`); the runtime does not hold the
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
    let ModuleDef::Effect { name, .. } = &unit.loaded.def else {
        anyhow::bail!("spawn called on a non-effect module");
    };
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
        position: AtomicU64::new(resume),
        shutdown: AtomicBool::new(false),
        consecutive_failures: AtomicU64::new(0),
        last_error: Mutex::new(None),
        skip_position: AtomicU64::new(0),
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
        match run_inner(&shared, &unit, &runtime, http.as_ref()) {
            Ok(()) => break,
            Err(err) => {
                if shared.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                shared.record_failure(&format!("driver: {err:#}"));
                tracing::error!(
                    "effect `{}` driver error (attempt {}): {err:#}",
                    shared.name,
                    attempt + 1
                );
                if sleep_watching_shutdown(&shared, backoff(attempt)) {
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
) -> anyhow::Result<()> {
    let ModuleDef::Effect { name, sources } = &unit.loaded.def else {
        anyhow::bail!("run called on a non-effect module");
    };
    let query = dispatch::to_query(sources)?;
    let resume = runtime.effect_resume_after(name)?;
    let mut sub = runtime.store().subscribe(query, Position::new(resume));
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
            shared.skip_position.store(0, Ordering::Relaxed);
            runtime.complete_invocation(effect, position, &runtime::now_rfc3339())?;
            shared.clear_failures();
            tracing::warn!(
                "effect `{effect}` skipped wedged position {position} by operator request"
            );
            return Ok(Progress::Advanced);
        }
        match try_invocation(
            effect,
            position,
            &env,
            &event_type,
            &data,
            loaded,
            runtime,
            http,
        ) {
            Ok(()) => {
                runtime.complete_invocation(effect, position, &runtime::now_rfc3339())?;
                shared.clear_failures();
                return Ok(Progress::Advanced);
            }
            Err(err) => {
                let message = format!("{err:#}");
                shared.record_failure(&message);
                tracing::error!(
                    "effect `{effect}` invocation at position {position} failed (attempt {}): {message}",
                    attempt + 1
                );
                if wedge_wait(shared, position, backoff(attempt)) {
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
    event_type: &str,
    data: &Value,
    loaded: &LoadedModule,
    runtime: &Arc<Runtime>,
    http: &dyn HttpClient,
) -> anyhow::Result<()> {
    Module::with_temp_heap(|module| {
        let handle_fn = loaded
            .module
            .get_option("handle")?
            .ok_or_else(|| anyhow::anyhow!("effect has no handle() function"))?;
        let value = alloc_event(&module, event_type, data);
        let host = EffectHostImpl {
            runtime,
            http,
            env: env.clone(),
            effect: effect.to_owned(),
            position,
            disambiguators: RefCell::new(HashMap::new()),
        };
        let ctx = EffectCtx { host: &host };
        call_handler_with_effect_ctx(
            &module,
            thaw(&handle_fn, &module),
            &[value],
            MAX_TICKS,
            &ctx,
        )
        .map_err(|err| anyhow::anyhow!("handle() failed: {err}"))?;
        anyhow::Ok(())
    })
}

/// The wedge backoff for `attempt`, doubling from [`BACKOFF_BASE`] up to
/// [`BACKOFF_CAP`].
fn backoff(attempt: u32) -> Duration {
    BACKOFF_BASE
        .saturating_mul(1u32 << attempt.min(10))
        .min(BACKOFF_CAP)
}

/// Sleep up to `total`, returning early. Returns `true` if shutdown fired (the
/// caller stops mid-wedge); a pending skip for `position` also returns early
/// (`false`), so the retry loop re-checks it at the top.
fn wedge_wait(shared: &EffectShared, position: u64, total: Duration) -> bool {
    let tick = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < total {
        if shared.shutdown.load(Ordering::Relaxed) {
            return true;
        }
        if shared.skip_position.load(Ordering::Relaxed) == position {
            return false;
        }
        thread::sleep(tick.min(total - waited));
        waited += tick;
    }
    shared.shutdown.load(Ordering::Relaxed)
}

/// Sleep up to `total`, returning `true` early if shutdown fired. Used by the
/// driver supervisor, which retries on infrastructure errors but has no per-event
/// skip to honor.
fn sleep_watching_shutdown(shared: &EffectShared, total: Duration) -> bool {
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
}

impl EffectHostImpl<'_> {
    /// The journal wrapper enforcing the op-DB lock discipline: look up (short
    /// lock), run the side effect with no lock held, record (short lock). The
    /// side effect closure receives the call's disambiguator (for a deterministic
    /// idempotency key).
    fn journaled<F>(&self, call_hash: &str, run: F) -> anyhow::Result<Value>
    where
        F: FnOnce(u64) -> anyhow::Result<Value>,
    {
        let disambiguator = self.next_disambiguator(call_hash);
        if let Some(recorded) =
            self.runtime
                .journal_get(&self.effect, self.position, call_hash, disambiguator)?
        {
            return serde_json::from_str(&recorded).context("decoding a journaled call result");
        }
        let result = run(disambiguator)?;
        let encoded = serde_json::to_string(&result).context("encoding a call result")?;
        self.runtime.journal_put(
            &self.effect,
            self.position,
            call_hash,
            disambiguator,
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
        let method = method.to_owned();
        let url = url.to_owned();
        self.journaled(&hash, move |_| {
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
                method: method.clone(),
                url: url.clone(),
                headers: request_headers,
                body: body_bytes,
            };
            let response = self
                .http
                .send(&request)
                .with_context(|| format!("http {method} {url}"))?;
            // 5xx never reaches the script: surface it as a retryable error so the
            // wedge absorbs it.
            if response.status >= 500 {
                anyhow::bail!("http {method} {url} returned {}", response.status);
            }
            Ok(http_response_to_json(response))
        })
    }

    fn invoke_command(&self, name: &str, input: Value) -> anyhow::Result<Value> {
        let hash = call_hash(
            "invoke_command",
            &json!({ "command": name, "input": input.clone() }),
        );
        let key_hash = hash.clone();
        let name = name.to_owned();
        self.journaled(&hash, move |disambiguator| {
            let idempotency_key = format!(
                "{}:{}:{}:{}",
                self.effect, self.position, key_hash, disambiguator
            );
            let ctx = CommandContext::from_effect(self.env.correlation_id, self.env.event_id);
            let result =
                self.runtime
                    .execute_from_effect(&name, input, &ctx, Some(&idempotency_key))?;
            Ok(json!({ "status": result.status, "body": result.body }))
        })
    }

    fn read(&self, projector: &str, entity: &str, key: &str) -> anyhow::Result<Value> {
        let hash = call_hash(
            "read",
            &json!({ "projector": projector, "entity": entity, "key": key }),
        );
        let (projector, entity, key) = (projector.to_owned(), entity.to_owned(), key.to_owned());
        self.journaled(&hash, move |_| {
            self.runtime.read_projector(&projector, &entity, &key)
        })
    }

    fn scan(
        &self,
        projector: &str,
        entity: &str,
        filter: Option<(String, String)>,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> anyhow::Result<Value> {
        let filter_json = filter
            .as_ref()
            .map(|(field, value)| json!({ "field": field, "value": value }));
        let hash = call_hash(
            "scan",
            &json!({
                "projector": projector,
                "entity": entity,
                "filter": filter_json,
                "cursor": cursor,
                "limit": limit,
            }),
        );
        let (projector, entity) = (projector.to_owned(), entity.to_owned());
        self.journaled(&hash, move |_| {
            self.runtime
                .scan_projector(&projector, &entity, filter, cursor, limit)
        })
    }

    fn now(&self) -> anyhow::Result<String> {
        let hash = call_hash("now", &json!({}));
        let value = self.journaled(&hash, |_| Ok(Value::String(runtime::now_rfc3339())))?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("journaled now() was not a string"))
    }

    fn log(&self, message: &str) {
        tracing::info!("effect `{}` @ {}: {message}", self.effect, self.position);
    }
}

fn http_response_to_json(response: HttpResponse) -> Value {
    let body = match serde_json::from_slice::<Value>(&response.body) {
        Ok(json) => json,
        Err(_) => Value::String(String::from_utf8_lossy(&response.body).into_owned()),
    };
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

impl UreqClient {
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
    let idempotency_days = config.retention.idempotency_key_days;
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let thread_stop = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("effect-sweeper".to_owned())
        .spawn(move || sweeper_loop(&runtime, effect_days, idempotency_days, &thread_stop))
        .context("spawning the retention sweeper")?;
    Ok(Sweeper { stop, join })
}

fn sweeper_loop(
    runtime: &Runtime,
    effect_days: u32,
    idempotency_days: u32,
    stop: &(Mutex<bool>, Condvar),
) {
    loop {
        if let Err(err) = run_sweep(runtime, effect_days, idempotency_days) {
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

/// Sweep completed effect journals and idempotency keys past their retention
/// window, in bounded chunks so the op-DB lock is never held across a long scan,
/// yielding briefly between chunks so the effect hot path is not starved.
fn run_sweep(runtime: &Runtime, effect_days: u32, idempotency_days: u32) -> anyhow::Result<()> {
    let effect_cutoff = runtime::rfc3339_days_ago(effect_days);
    while runtime.sweep_effect_journal(&effect_cutoff, SWEEP_CHUNK)? == SWEEP_CHUNK {
        thread::sleep(SWEEP_CHUNK_PAUSE);
    }
    let idempotency_cutoff = runtime::rfc3339_days_ago(idempotency_days);
    while runtime.sweep_idempotency(&idempotency_cutoff, SWEEP_CHUNK)? == SWEEP_CHUNK {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(0), BACKOFF_BASE);
        assert_eq!(backoff(1), BACKOFF_BASE * 2);
        assert_eq!(backoff(100), BACKOFF_CAP);
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
}
