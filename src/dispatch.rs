//! Command dispatch: the DCB decision cycle for a loaded command.
//!
//! `query` derives the consistency boundary; the events inside it are read and
//! `fold`ed into state (by one function, or per type through a dispatch map, in
//! which case an unmapped type is read but not folded); `handle` decides; and any
//! events it emits are appended guarded by that same boundary, so a concurrent
//! write inside the boundary makes the append fail rather than silently violate an
//! invariant.
//!
//! A DCB conflict is retried in place, per the caller's [`Retry`] policy, by folding
//! what landed since onto the state the previous attempt already built; only an
//! exhausted budget returns [`CommandOutcome::Conflict`]. The events a command emits
//! are wrapped in a host-stamped [`envelope`] at the append seam, and every read
//! unwraps it, so handlers only ever see the payload.

use std::collections::HashMap;
use std::env;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use starlark::ErrorKind;
use starlark::environment::{FrozenModule, Module};
use starlark::values::{OwnedFrozenValue, Value, ValueError};
use tephra::{
    AppendCondition, AppendError, ConflictClause, Event, EventRef, EventType, Matches, Position,
    PositionRange, Query, QueryItem, Tag, Tags, WriteHandle,
};
use uuid::Uuid;

use crate::context::{CommandContext, HandleCtx};
use crate::crypto::KeyStore;
use crate::envelope::{self, Envelope};
use crate::hash;
use crate::starlark_builtins::{
    EmittedEvent, EventDef, EventDispatch, EventSpec, HandleOutcome, LoadedModule, ModuleDef,
    alloc_event, alloc_input, call_handler, call_handler_with_ctx, call_handler_with_query_ctx,
    check_fold_result, dispatch_arm_functions, initial_state, parse_event_dispatch,
    parse_event_specs, parse_handle_result, scalar_to_string, thaw, validate_command_input,
};

/// The module slot a chunk's folded state is exported through, so it survives the
/// freeze that ends the chunk.
const STATE_SLOT: &str = "state";

/// The reserved tag-key prefix for the global uniqueness tag of a `unique` field:
/// `_hekla_uniq_<field>`. Host-stamped, so it lives in the reserved namespace a user
/// field can never occupy.
const UNIQUE_TAG_PREFIX: &str = "_hekla_uniq_";

/// The global uniqueness tag key for `field`.
fn unique_tag_key(field: &str) -> String {
    format!("{UNIQUE_TAG_PREFIX}{field}")
}

/// A reserved tag no event ever carries, added to a query clause whose subject key is
/// absent so the clause matches nothing (rather than minting a key on the read path).
const NOMATCH_TAG: &str = "_hekla_nomatch";

/// The event definitions the dispatch layer needs: type name to its declared field
/// metadata, for encryption and for wrapping subject fields as opaque handles.
pub type EventDefs = HashMap<String, EventDef>;

/// Derived tags as `(key, Some(value))` / `(key, None)` pairs, before lowering to
/// tephra tags.
type TagPairs = Vec<(String, Option<String>)>;

/// Per-handler instruction budget. Bounds a runaway script at dispatch time.
const MAX_TICKS: u64 = 10_000_000;

/// The reserved tag-key prefix hekla stamps onto events for host bookkeeping. The
/// loader rejects this namespace on both sides: an event tag field can't emit one
/// (so a handler can't forge a host tag, or an append condition) and a `query()` /
/// `events()` tag can't name one (so a handler can't fold over other requests' host
/// tags).
pub const RESERVED_TAG_PREFIX: &str = "_hekla_";

/// The reserved tag key carrying a command's per-request idempotency identity. Every
/// event a keyed command emits gets this tag, and the append is guarded against it,
/// so exactly-once is enforced by the log itself rather than by op-DB bookkeeping.
const IDEMPOTENCY_TAG_KEY: &str = "_hekla_idem";

/// The idempotency tag for a `(command, key)` pair: `_hekla_idem:<sha256(command\0key)>`.
/// Hashing binds the tag to the command (so the same key on two commands cannot
/// collide) and yields a fixed-length, fixed-charset value regardless of the client's
/// raw key.
///
/// The tag deliberately excludes the request body: the key alone identifies the
/// request, so reusing a key with a different body replays the first outcome rather
/// than running the new body. This is standard idempotency-key semantics; a client
/// that wants a distinct outcome must use a distinct key.
pub fn idempotency_tag(command: &str, key: &str) -> String {
    let mut material = Vec::with_capacity(command.len() + 1 + key.len());
    material.extend_from_slice(command.as_bytes());
    material.push(0);
    material.extend_from_slice(key.as_bytes());
    format!("{IDEMPOTENCY_TAG_KEY}:{}", hash::sha256_hex(&material))
}

/// A command outcome recovered from the log by its idempotency tag: a prior committed
/// attempt's events and identity, enough to rebuild the exact success response a
/// replay must return without re-running `handle`.
pub struct RecoveredOutcome {
    pub events: Vec<RecoveredEvent>,
    pub positions: PositionRange,
    pub correlation_id: Uuid,
    pub causation_id: Uuid,
}

/// One recovered event: its type and its derived tags (reserved host tags stripped),
/// already rendered as the `"key:value"` strings the response reports.
pub struct RecoveredEvent {
    pub event_type: String,
    pub tags: Vec<String>,
}

/// The outcome of one command attempt.
pub enum CommandOutcome {
    /// `handle` emitted events (possibly none) and they were appended.
    Committed {
        events: Vec<EmittedEvent>,
        /// The assigned positions, or `None` when nothing was emitted.
        positions: Option<PositionRange>,
    },
    /// `handle` rejected the command on state grounds; nothing was written.
    Rejected { code: String, message: String },
    /// The input was malformed (host-side validation or `invalid_input`); nothing
    /// was written.
    InvalidInput { message: String },
    /// The append hit a concurrent write inside the boundary. The caller should
    /// rebuild state and retry.
    Conflict,
    /// This request already committed under its idempotency tag (a crashed or
    /// concurrent duplicate): the outcome was recovered from the log rather than
    /// re-decided, and the caller returns it verbatim.
    AlreadyCommitted(RecoveredOutcome),
    /// The store could not service the append for a transient reason (the write
    /// coordinator is draining). The caller should surface a retryable status.
    Unavailable { message: String },
}

/// Host-side validation of a command's raw input against its declared schema.
/// A malformed body is the equivalent of `invalid_input(...)` and never reaches a
/// handler. The runtime validates once before dispatch, so [`run_command`] can assume
/// an already-validated body on every attempt it makes.
pub fn validate_input(loaded: &LoadedModule, input: &serde_json::Value) -> anyhow::Result<()> {
    let ModuleDef::Command { input: schema, .. } = &loaded.def else {
        anyhow::bail!("validate_input called on a non-command module");
    };
    validate_command_input(schema, input)
}

/// How a DCB conflict is retried: how many attempts, and what to do between them.
///
/// The policy belongs to the caller (the runtime picks the budget and the backoff)
/// but the loop belongs here, so the work that does not vary between attempts (the
/// input struct, the boundary, the lowered `fold` plan) is done once per request
/// rather than once per attempt.
pub struct Retry<'a> {
    /// Total attempts including the first. Below 1, nothing runs and the command
    /// reports a conflict.
    pub max_attempts: u32,
    /// Called before each retry with the zero-based number of the attempt that just
    /// conflicted.
    pub backoff: &'a dyn Fn(u32),
}

impl Retry<'_> {
    /// One attempt and no backoff: a conflict is reported rather than retried. What
    /// `hekla test` wants, where the log is seeded and nothing else is writing.
    pub fn once() -> Retry<'static> {
        Retry {
            max_attempts: 1,
            backoff: &|_| {},
        }
    }
}

/// Run the command's decision cycle against the store: read the boundary, fold,
/// handle, append, retrying per `retry` on a DCB conflict. The caller validates input
/// once via [`validate_input`] first, so this cycle assumes a well-formed body. `now`
/// is the request's pinned append time, visible to `handle` through `now()` and
/// stamped into each event's envelope, and it does not move across attempts.
///
/// `query`, `initial` and `handle` are resolved once: the input is invariant and all
/// three are pure, so a retry repeats only the fold and the append. The fold itself is
/// incremental. Each attempt keeps the state it folded and the last position that
/// state covers, and the next one resumes strictly after that position, folding what
/// landed in between onto the state it already has. A conflict on a boundary tens of
/// thousands of events deep therefore costs the handful of events that caused it
/// rather than the whole boundary again.
///
/// The carried state is frozen, so `handle` cannot mutate what the next attempt folds
/// onto; see [`fold_frozen`] for why that is enforced rather than documented.
///
/// When `idem_tag` is set, exactly-once is enforced atomically at the append: every
/// emitted event carries the tag and the append condition's existence clause
/// ([`AppendCondition::fail_if_exists`]) rejects it if the tag exists anywhere. So a
/// duplicate (a crash replay or a concurrent request) fails with
/// [`ConflictClause::Existence`] rather than committing twice, and the runtime then
/// recovers the original outcome via [`find_committed_outcome`]. There is no
/// pre-`handle` read: a fresh request pays nothing, and a duplicate re-runs the pure
/// `handle` but its events never land. The gaps the append can't catch are the
/// decisions that never append at all: a `handle` that rejects, and one that emits
/// nothing. Both are checked against the tag directly (see [`recover_if_committed`]).
#[allow(clippy::too_many_arguments)]
pub fn run_command(
    store: &WriteHandle,
    loaded: &LoadedModule,
    events: &EventDefs,
    keystore: Option<&KeyStore>,
    input: &serde_json::Value,
    ctx: &CommandContext,
    now: &str,
    idem_tag: Option<&str>,
    verify: bool,
    retry: &Retry<'_>,
) -> anyhow::Result<CommandOutcome> {
    let ModuleDef::Command { input: schema, .. } = &loaded.def else {
        anyhow::bail!("run_command called on a non-command module");
    };
    let frozen = &loaded.module;

    Module::with_temp_heap(|module| {
        let input_value = alloc_input(&module, schema, input)?;

        // Consistency boundary from `query` (optional). May return one spec or a
        // list of them, OR'd into the boundary.
        let boundary = match frozen.get_option("query")? {
            Some(func) => {
                let result = call_handler_with_query_ctx(
                    &module,
                    thaw(&func, &module),
                    &[input_value],
                    MAX_TICKS,
                )
                .map_err(|err| anyhow::anyhow!("query() failed: {err}"))?;
                let specs =
                    parse_event_specs(result).map_err(|err| anyhow::anyhow!("query() {err}"))?;
                Some(to_query(&specs, events, keystore)?)
            }
            None => None,
        };

        let plan = FoldPlan::build(frozen, &module, events, keystore)?;

        // `handle` alone sees the pinned clock.
        let handle_fn = frozen
            .get_option("handle")?
            .ok_or_else(|| anyhow::anyhow!("command has no handle() function"))?;
        let handle_value = thaw(&handle_fn, &module);
        let handle_ctx = HandleCtx {
            now: now.to_owned(),
        };

        // What an attempt hands the next one: the state it folded, frozen, and the
        // last position that state covers. `None` on the first attempt, which folds
        // from the start of the boundary onto `initial`.
        let mut folded: Option<(OwnedFrozenValue, Position)> = None;

        for attempt in 0..retry.max_attempts {
            if attempt > 0 {
                (retry.backoff)(attempt - 1);
            }
            let (state, after) = match &boundary {
                Some(query) => {
                    let resume_after = folded.as_ref().map_or(Position::ZERO, |(_, at)| *at);
                    let (frozen_state, after) = fold_frozen(
                        folded.as_ref().map(|(state, _)| state),
                        &FoldInputs {
                            frozen,
                            store,
                            query,
                            plan: &plan,
                            events,
                            resume_after,
                            upto: None,
                            verify,
                        },
                    )?;
                    let state = thaw(&frozen_state, &module);
                    folded = Some((frozen_state, after));
                    (state, after)
                }
                // Resolved only here: a boundaried command's state comes from the fold,
                // which builds its own `initial` in the heap each chunk folds in.
                None => (
                    initial_state(frozen, &module)
                        .map_err(|err| anyhow::anyhow!("initial failed: {err}"))?,
                    Position::ZERO,
                ),
            };

            let decision = call_handler_with_ctx(
                &module,
                handle_value,
                &[input_value, state],
                MAX_TICKS,
                &handle_ctx,
            )
            .map_err(handle_error)?;

            let outcome = attempt_outcome(
                store,
                events,
                keystore,
                ctx,
                now,
                idem_tag,
                boundary.as_ref(),
                after,
                decision,
            )?;
            match outcome {
                // A concurrent write inside the boundary: fold what landed and
                // decide again.
                CommandOutcome::Conflict => continue,
                settled => return Ok(settled),
            }
        }
        Ok(CommandOutcome::Conflict)
    })
}

/// How much a single fold chunk may allocate before it is closed off and its heap
/// released. Overridable through `HEKLA_FOLD_HEAP_BUDGET`, in bytes.
const DEFAULT_FOLD_HEAP_BUDGET: usize = 1 << 20;

/// The floor under `HEKLA_FOLD_HEAP_BUDGET`. A budget small enough to close a chunk
/// every event or two is strictly worse than not chunking: each seam costs a freeze and
/// leaves the previous chunk's state retained (see [`fold_chunks`]), so tuning the knob
/// down to save memory spends more of it.
const MIN_FOLD_HEAP_BUDGET: usize = 64 << 10;

/// How much larger a chunk must be than the state it carries. The chain of retained
/// per-chunk states is the price of chunking, and this is what bounds it: at `N`, the
/// retained chain can never exceed `1/N` of what folding the whole boundary in one heap
/// would have held, whatever the state's size or the boundary's depth.
const FOLD_CHUNK_STATE_RATIO: usize = 8;

/// The ceiling on `HEKLA_FOLD_HEAP_BUDGET`. A budget no fold ever reaches is a fold
/// that never chunks, which is the depth-linear live heap the chunking exists to
/// remove, so the knob cannot switch the mechanism off by being set high.
const MAX_FOLD_HEAP_BUDGET: usize = 64 << 20;

/// The effective per-chunk heap budget, read once.
///
/// Out-of-range values are clamped to the nearest bound and unparseable ones fall back
/// to the default, both with a warning: a budget silently 32x what the operator asked
/// for is how a deployment-config typo stays invisible.
///
/// A memory knob rather than caller policy, so it lives here rather than on
/// [`FoldInputs`]: every fold in the process wants the same ceiling, and the number
/// that matters is the aggregate live heap across concurrent folds.
fn fold_heap_budget() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let Some(raw) = env::var("HEKLA_FOLD_HEAP_BUDGET").ok() else {
            return DEFAULT_FOLD_HEAP_BUDGET;
        };
        let Ok(asked) = raw.trim().parse::<usize>() else {
            tracing::warn!(
                "HEKLA_FOLD_HEAP_BUDGET is not a byte count: {raw:?}; using {DEFAULT_FOLD_HEAP_BUDGET}"
            );
            return DEFAULT_FOLD_HEAP_BUDGET;
        };
        let clamped = asked.clamp(MIN_FOLD_HEAP_BUDGET, MAX_FOLD_HEAP_BUDGET);
        if clamped != asked {
            tracing::warn!("HEKLA_FOLD_HEAP_BUDGET {asked} is out of range; using {clamped}");
        }
        clamped
    })
}

/// How many events this process has folded, and how many chunk seams it has crossed
/// doing it.
///
/// Read amplification is the diagnostic number for a DCB workload and a command API
/// cannot expose it per request, so it is counted per process instead. The seam count
/// is what makes the chunking observable at all: folding in chunks and folding in one
/// pass give the same state by construction, so without it nothing distinguishes a
/// chunked fold from a fold whose budget was never reached.
static EVENTS_FOLDED: AtomicU64 = AtomicU64::new(0);
static CHUNK_SEAMS: AtomicU64 = AtomicU64::new(0);

/// What one fold cost, before it is folded into the process totals. Kept separate so a
/// verify-mode re-fold, which is the check's cost rather than the request's, can be
/// left out of them.
#[derive(Default)]
struct FoldWork {
    events: u64,
    seams: u64,
}

/// Events folded and chunk seams crossed since the process started. Reported by
/// `/status` as `folds`.
pub fn fold_counters() -> (u64, u64) {
    (
        EVENTS_FOLDED.load(Ordering::Relaxed),
        CHUNK_SEAMS.load(Ordering::Relaxed),
    )
}

/// Fold a boundary in bounded-memory chunks and hand back the state frozen, alongside
/// the last position it covers.
///
/// Under [`FoldInputs::verify`] the fold runs twice and the two states are compared. A
/// disagreement is returned as an error rather than a typed violation, because there is
/// no safe way to continue from it: the caller's decision would be built on a state
/// that does not reproduce. The command path surfaces it as a failed request; the
/// effect path wedges the invocation, which is the quarantine.
fn fold_frozen(
    base: Option<&OwnedFrozenValue>,
    inputs: &FoldInputs<'_>,
) -> anyhow::Result<(OwnedFrozenValue, Position)> {
    let (state, after, work) = fold_chunks(base, inputs)?;
    EVENTS_FOLDED.fetch_add(work.events, Ordering::Relaxed);
    CHUNK_SEAMS.fetch_add(work.seams, Ordering::Relaxed);
    if !inputs.verify {
        return Ok((state, after));
    }
    // The second fold is bounded at where the first one ended, which is what makes
    // this a determinism check rather than a race. A command folds with `upto: None`,
    // and each read pins the watermark as of its own call, so an unbounded re-fold
    // would absorb any append that landed in between and report a concurrent write as
    // nondeterminism, turning ordinary DCB contention into a 500.
    let bounded = FoldInputs {
        upto: Some(after.get()),
        ..*inputs
    };
    // The re-fold's work is deliberately not counted: it is the check's cost, not the
    // request's, and counting it would report twice the read amplification a verified
    // deployment actually has.
    let (again, after_again, _) = fold_chunks(base, &bounded)?;
    if let Some((first, second)) = frozen_states_differ(&state, &again)? {
        anyhow::bail!(
            "the same boundary folded to two different states at position {after}: {first} then {second}"
        );
    }
    if after != after_again {
        anyhow::bail!(
            "the same boundary ended at two different positions: {after} then {after_again}"
        );
    }
    Ok((state, after))
}

/// Fold the boundary a chunk at a time, each chunk in a scratch heap of its own, and
/// return the final state frozen.
///
/// Two properties come out of the freeze between chunks, and the second is why the
/// chunking exists at all:
///
/// - **`handle` cannot mutate what the next attempt folds onto.** A frozen state fails
///   an assignment with `Immutable`, which is already what happens when the boundary
///   is empty and `state` is the frozen `initial`, so this makes the rule uniform
///   rather than inventing one. Without it a `handle` that wrote into `state` would
///   corrupt every later attempt and commit past the boundary it exists to protect,
///   silently and with a 200.
/// - **The events a chunk allocates die with it.** Starlark collects only when
///   executing a statement at the root of a module, which a fold loop never does, so
///   nothing a fold allocates is released until its heap is dropped: every event
///   struct, every string, and every superseded state from `dict(state, ...)` survives
///   to the end. One heap for the whole boundary therefore costs memory linear in its
///   depth, and per-event cost stops being flat once that working set outgrows the
///   cache. Freezing every [`fold_heap_budget`] bytes copies out only what the state
///   reaches and drops the rest.
///
/// It is *not* true that a fold of any depth holds a constant amount live. Thawing the
/// carry adds a reference to the previous chunk's frozen heap, and freezing keeps every
/// referenced heap alive, so the per-chunk states form a chain that is only released
/// when the whole fold ends. What bounds it is [`FOLD_CHUNK_STATE_RATIO`]: a chunk must
/// be at least that many times the size of the state it carries, so the chain can never
/// exceed `1/ratio` of what folding in one heap would have held. A fold whose state is
/// a handful of scalars chunks at the configured budget; one accumulating a large dict
/// chunks less often, which is the right trade, since that is exactly the fold whose
/// per-chunk copy is expensive.
///
/// Sound for the same reason the retry carry is: a left fold over an append-only log,
/// where folding `[0, a]` and then `(a, b]` gives the state folding `[0, b]` would.
/// The read is planned once, before the first chunk, so the whole fold still runs
/// against a single pinned watermark and reports one `after` for the append condition.
fn fold_chunks(
    base: Option<&OwnedFrozenValue>,
    inputs: &FoldInputs<'_>,
) -> anyhow::Result<(OwnedFrozenValue, Position, FoldWork)> {
    // Destructured exhaustively so a new field on `FoldInputs` has to be considered
    // here rather than silently ignored by the fold.
    let FoldInputs {
        frozen,
        store,
        query,
        plan,
        events,
        resume_after,
        upto,
        verify: _,
    } = *inputs;

    // Resolved once rather than per chunk: the owned frozen value is heap-independent,
    // and only the thaw into each chunk's heap has to be repeated.
    let fold_owned = frozen.get_option("fold")?;
    let mut reads = store.read(query, resume_after, None);
    let mut carry = base.cloned();
    // Starts at the resume point, not at zero, so a fold that matches nothing new
    // still reports the position its state already covers.
    let mut after = resume_after;
    let mut budget = fold_heap_budget();
    let mut work = FoldWork::default();
    loop {
        // The chunk body is inline because tephra exports neither `Reads` nor its
        // lending item type, so the iterator cannot be named in a helper's signature.
        let (state, at, folded, more) = Module::with_temp_heap(|scratch| -> anyhow::Result<_> {
            let mut state = match &carry {
                Some(value) => thaw(value, &scratch),
                None => initial_state(frozen, &scratch)
                    .map_err(|err| anyhow::anyhow!("initial failed: {err}"))?,
            };
            // The map is a module-level literal and its clauses were lowered once by
            // the plan, so a chunk lifts only the arm functions into its heap. Going
            // through `parse_event_dispatch` here would clone an `EventSpec` per arm
            // per chunk and discard every one.
            let arms = fold_owned
                .as_ref()
                .map(|owned| dispatch_arm_functions(thaw(owned, &scratch)));
            // The plan indexes the arms, so a plan built from a different module than
            // the one being folded would index out of bounds inside `fold_event`.
            // Nothing in the types pairs them, so it is checked rather than assumed.
            if let Some(arms) = &arms
                && arms.len() != plan.lowered.len()
            {
                anyhow::bail!("the fold plan does not match the module's fold");
            }

            // Measured against what the carried state cost to thaw, so the budget
            // bounds what this chunk adds rather than what it inherited.
            let baseline = scratch.heap().allocated_bytes();
            let mut at = after;
            let mut folded = 0u64;
            let mut more = false;
            let mut selected: Vec<usize> = Vec::new();
            while let Some(item) = reads.next() {
                let seq = item.map_err(|err| anyhow::anyhow!("read failed: {err}"))?;
                // The store reads ascending up to the watermark pinned when the read
                // was planned, and takes no upper bound, so the bound is a break.
                if upto.is_some_and(|limit| seq.position.get() > limit) {
                    break;
                }
                at = seq.position;
                let Some(arms) = &arms else { continue };
                // Matching before decoding: a map over a wide boundary pays nothing
                // for the events no arm selects. The buffer is reused across events so
                // a boundary of any depth costs one allocation.
                selected.clear();
                selected.extend(
                    plan.lowered
                        .iter()
                        .enumerate()
                        .filter(|(_, item)| arm_selects(item.as_ref(), seq.event))
                        .map(|(index, _)| index),
                );
                if selected.is_empty() {
                    continue;
                }
                state = fold_event(&scratch, arms, plan, events, &selected, seq.event, state)?;
                folded += 1;
                // Checked after folding, so a chunk always makes progress and a single
                // event larger than the budget cannot spin.
                if scratch.heap().allocated_bytes() - baseline >= budget {
                    more = true;
                    break;
                }
            }

            scratch.set(STATE_SLOT, state);
            let frozen = scratch
                .freeze()
                .map_err(|err| anyhow::Error::from(err).context("freezing the folded state"))?;
            Ok((frozen.get(STATE_SLOT)?, at, folded, more))
        })?;
        after = at;
        work.events += folded;
        // A chunk that folded nothing produces a state equal to the one it carried, so
        // keeping the old one drops a link from the retained chain for free. This is
        // the boundary that ended exactly on a budget trip.
        if folded > 0 || carry.is_none() {
            // The next chunk must be large enough to dwarf what it will carry, or the
            // chain of retained states costs more than the events ever did.
            budget = budget.max(state.owner().allocated_bytes() * FOLD_CHUNK_STATE_RATIO);
            carry = Some(state);
        }
        if !more {
            break;
        }
        work.seams += 1;
    }
    Ok((
        carry.expect("the chunk loop always runs at least once"),
        after,
        work,
    ))
}

/// Decode one selected event and thread the state through every arm that selected it,
/// in declaration order.
fn fold_event<'v>(
    module: &Module<'v>,
    arms: &[Value<'v>],
    plan: &FoldPlan,
    events: &EventDefs,
    selected: &[usize],
    event: EventRef<'_>,
    mut state: Value<'v>,
) -> anyhow::Result<Value<'v>> {
    let event_type = event.event_type();
    let (envelope, data) =
        envelope::decode(event.data()).map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
    let value = alloc_event(
        module,
        envelope.event_id,
        &envelope.timestamp,
        event_type,
        &data,
        events.get(event_type),
    );
    for &index in selected {
        let what = &plan.labels[index];
        state = call_handler(module, arms[index], &[state, value], MAX_TICKS)
            .map_err(|err| fold_error(what, err))?;
        check_fold_result(state, what)?;
    }
    Ok(state)
}

/// Compare two folded states, rendering both when they disagree.
///
/// They live in separate frozen heaps, so the comparison needs a module to thaw them
/// into. Rendering only on disagreement keeps the happy path free of a `to_json_value`
/// per fold.
fn frozen_states_differ(
    first: &OwnedFrozenValue,
    second: &OwnedFrozenValue,
) -> anyhow::Result<Option<(String, String)>> {
    Module::with_temp_heap(|scratch| {
        let first = thaw(first, &scratch);
        let second = thaw(second, &scratch);
        if values_agree(first, second)? {
            return Ok(None);
        }
        Ok(Some((render_state(first), render_state(second))))
    })
}

/// Turn one attempt's `handle` decision into its outcome: reject and invalid-input
/// pass straight through, and an emit is packed, guarded by the boundary and
/// appended.
///
/// Split out of [`run_command`] so the attempt loop there reads as fold, decide,
/// settle rather than burying the append in a nest of match arms.
#[allow(clippy::too_many_arguments)]
fn attempt_outcome(
    store: &WriteHandle,
    events: &EventDefs,
    keystore: Option<&KeyStore>,
    ctx: &CommandContext,
    now: &str,
    idem_tag: Option<&str>,
    boundary: Option<&Query>,
    after: Position,
    decision: Value<'_>,
) -> anyhow::Result<CommandOutcome> {
    match parse_handle_result(decision, events)? {
        HandleOutcome::Reject(rejection) => {
            if let Some(recovered) = recover_if_committed(store, events, boundary, idem_tag)? {
                return Ok(CommandOutcome::AlreadyCommitted(recovered));
            }
            Ok(CommandOutcome::Rejected {
                code: rejection.code,
                message: rejection.message,
            })
        }
        HandleOutcome::InvalidInput(invalid) => Ok(CommandOutcome::InvalidInput {
            message: invalid.message,
        }),
        HandleOutcome::Emit(emitted) => {
            if emitted.is_empty() {
                if let Some(recovered) = recover_if_committed(store, events, boundary, idem_tag)? {
                    return Ok(CommandOutcome::AlreadyCommitted(recovered));
                }
                return Ok(CommandOutcome::Committed {
                    events: emitted,
                    positions: None,
                });
            }
            let packed = emitted
                .iter()
                .map(|event| {
                    build_event(
                        event,
                        events.get(&event.event_type),
                        keystore,
                        ctx,
                        now,
                        idem_tag,
                        Uuid::new_v4(),
                    )
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let condition = build_condition(boundary, after, idem_tag)?;
            match store.append(packed, condition) {
                Ok(positions) => Ok(CommandOutcome::Committed {
                    events: emitted,
                    positions: Some(positions),
                }),
                // The existence clause fired: this request already committed (a
                // crash replay or a concurrent duplicate), caught atomically at the
                // append with no TOCTOU. Recover its original outcome.
                Err(AppendError::Conflict {
                    clause: ConflictClause::Existence,
                    ..
                }) => {
                    let tag = idem_tag.expect("the existence clause is only set when keyed");
                    match find_committed_outcome(store, events, tag)? {
                        Some(recovered) => Ok(CommandOutcome::AlreadyCommitted(recovered)),
                        None => Err(anyhow::anyhow!(
                            "idempotency existence guard fired but no committed outcome was found"
                        )),
                    }
                }
                // A concurrent write inside the boundary: rebuild state on a fresh
                // read and retry.
                Err(AppendError::Conflict {
                    clause: ConflictClause::Boundary,
                    ..
                }) => Ok(CommandOutcome::Conflict),
                // The coordinator is draining: the request never landed, and a
                // retry against a fresh process can succeed, so surface it as a
                // retryable 503 rather than an opaque 500.
                Err(AppendError::Shutdown) => Ok(CommandOutcome::Unavailable {
                    message: "the write coordinator is shutting down; retry".to_owned(),
                }),
                // A handler emitted a batch too large to ever store: an author
                // bug, distinct from the integrity and I/O failures below.
                Err(err @ AppendError::TooLarge { .. }) => Err(anyhow::anyhow!(
                    "command emitted an oversized event batch: {err}"
                )),
                // An event already on the log failed to decode during the
                // condition scan: an integrity failure, not a normal outcome.
                Err(err @ AppendError::Corrupt(_)) => Err(anyhow::anyhow!(
                    "append aborted on a corrupt event in the boundary: {err}"
                )),
                // Empty (guarded above) and AfterBeyondTip (a position hekla
                // never hands out) are host bugs; Log is a durable write failure.
                Err(err) => Err(anyhow::anyhow!("append failed: {err}")),
            }
        }
    }
}

/// What [`fold_boundary`] needs besides the heap and the starting state, bundled so
/// the call stays under the argument limit.
///
/// Everything about a `fold` that does not vary between attempts: each arm's lowered
/// query clause and its error label.
///
/// Lowering is the expensive half. A clause over a subject-scoped field costs a
/// keystore lookup and a deterministic encryption per arm, so doing it per attempt
/// would put crypto on the hot contention path for nothing: the map is a module-level
/// literal and the events and keys it lowers against are fixed for the request.
pub(crate) struct FoldPlan {
    lowered: Vec<Option<QueryItem>>,
    labels: Vec<String>,
}

impl FoldPlan {
    /// Resolve, thaw and lower a module's `fold` once. `module` is only borrowed for
    /// the thaw; nothing in the plan outlives it, so the caller is free to fold in a
    /// different heap.
    pub(crate) fn build(
        frozen: &FrozenModule,
        module: &Module<'_>,
        events: &EventDefs,
        keystore: Option<&KeyStore>,
    ) -> anyhow::Result<FoldPlan> {
        let Some(owned) = frozen.get_option("fold")? else {
            return Ok(FoldPlan {
                lowered: Vec::new(),
                labels: Vec::new(),
            });
        };
        let fold = parse_event_dispatch(thaw(&owned, module))
            .map_err(|err| anyhow::anyhow!("`fold` {err}"))?;
        let lowered = lower_dispatch(&fold, events, keystore)
            .map_err(|err| anyhow::anyhow!("`fold` {err}"))?;
        let labels: Vec<String> = fold
            .arms()
            .iter()
            .map(|arm| fold.label("fold", arm.spec.as_ref()))
            .collect();
        Ok(FoldPlan { lowered, labels })
    }
}

pub(crate) struct FoldInputs<'a> {
    pub frozen: &'a FrozenModule,
    pub store: &'a WriteHandle,
    pub query: &'a Query,
    pub plan: &'a FoldPlan,
    pub events: &'a EventDefs,
    /// Exclusive lower bound on the positions read: the fold resumes strictly after
    /// this position, onto the state it is handed rather than onto `initial`.
    /// [`Position::ZERO`] folds the whole boundary.
    ///
    /// A command's retry sets it to the last position its previous attempt folded, so
    /// a DCB conflict costs the delta instead of the boundary again. That is sound
    /// because the fold is a left fold over an append-only log: folding `[0, a]` and
    /// then `(a, b]` gives the state folding `[0, b]` would.
    pub resume_after: Position,
    /// Inclusive upper bound on the positions folded. `None` folds the whole
    /// boundary, which is what a command wants: its state is the log as of now.
    /// `Some(n)` stops after position `n`, which is what an effect wants: folding
    /// past its own position would make the state depend on how far the log had
    /// run by the time the handler happened to execute.
    pub upto: Option<u64>,
    /// Fold twice and compare, for verify mode. The boundary is a pure function of
    /// the log prefix and `upto`, so two folds that disagree mean the state an
    /// effect derives is not reproducible, which is the assumption the whole
    /// fold-instead-of-store design rests on.
    pub verify: bool,
}

/// Fold a boundary into state in the caller's heap, returning it alongside the last
/// position the query matched.
///
/// The effect path's entry point. Shared with commands through [`fold_frozen`] so the
/// two cannot drift on the parts that are not obvious: arms are matched against the
/// raw event *before* the envelope is decoded, the dispatch map is lowered once rather
/// than per event, and the live heap is bounded rather than linear in the boundary's
/// depth.
///
/// The returned position advances for every event in the boundary, folded or not, so
/// a command's append condition covers everything the query matched rather than only
/// what an arm consumed. An effect ignores it.
pub(crate) fn fold_boundary<'v>(
    module: &Module<'v>,
    inputs: &FoldInputs<'_>,
) -> anyhow::Result<(Value<'v>, Position)> {
    let (state, after) = fold_frozen(None, inputs)?;
    Ok((thaw(&state, module), after))
}

/// Compare two folded states structurally.
///
/// Starlark equality rather than a JSON round-trip: `check_fold_result` accepts any
/// non-`None` value, so a fold may legitimately return a set, or a float that is NaN
/// or infinite, none of which survive `to_json_value`. Comparing through JSON would
/// make verify mode reject state the runtime otherwise allows.
///
/// Starlark equality has two gaps, and this errs toward reporting a disagreement that
/// is not one rather than missing one that is. A NaN in the state is not equal to
/// itself, and a value whose type does not implement `equals` (a constructed event) is
/// never equal to itself either, so a deterministic fold producing either is reported
/// as nondeterministic: a failed command, or a wedged effect. That is the survivable
/// direction. Comparing rendered forms instead would close both gaps and open a worse
/// one, because several of this crate's types render lossily: a `CipherHandle` prints
/// as `<encrypted:field>` with the ciphertext dropped, so two states differing in a
/// subject-encrypted value would render identically and verify would call a real
/// divergence reproducible. A checker that can say "fine" when it is not is worse than
/// no checker.
fn values_agree<'v>(first: Value<'v>, second: Value<'v>) -> anyhow::Result<bool> {
    first
        .equals(second)
        .map_err(|err| anyhow::anyhow!("comparing folded states: {err}"))
}

/// Render a folded state for an error message, falling back to its debug form when it
/// is not JSON-representable.
fn render_state(value: Value<'_>) -> String {
    match value.to_json_value() {
        Ok(json) => json.to_string(),
        Err(_) => format!("{value:?}"),
    }
}

/// Prefix a fold failure with the entry that produced it, and spell out the one
/// starlark error whose own message is a single word.
///
/// `initial` is a frozen module global, so mutating `state` fails with a bare
/// `Immutable`. That is exactly the mistake the return-new-state contract exists to
/// prevent, so it is worth naming rather than leaving to the source span.
fn fold_error(what: &str, err: starlark::Error) -> anyhow::Error {
    if mutated_immutable(&err) {
        anyhow::anyhow!(
            "{what} failed: {err}; fold returns the new state (e.g. `return dict(state, taken = True)`) rather than mutating the one it was handed"
        )
    } else {
        anyhow::anyhow!("{what} failed: {err}")
    }
}

/// The same treatment for `handle`, whose own `Immutable` is the folded state.
///
/// Worth naming rather than leaving to the source span, because the reason it is
/// frozen is not local to the handler: a DCB retry folds what landed since onto the
/// state the previous attempt built, so a mutation here would decide the next attempt
/// rather than dying with this one.
fn handle_error(err: starlark::Error) -> anyhow::Error {
    if mutated_immutable(&err) {
        anyhow::anyhow!(
            "handle() failed: {err}; the folded state is read-only, because a retry folds onto it rather than rebuilding it; decide from `state` and return the decision"
        )
    } else {
        anyhow::anyhow!("handle() failed: {err}")
    }
}

/// The same treatment for an effect's `handle`, whose state is also frozen (it comes
/// out of a chunked fold). Shared with `effect.rs` so the two paths cannot drift into
/// reporting the same mistake differently: a bare `Immutable` names a line but not the
/// rule, and an effect that hits it wedges until someone works out why.
pub(crate) fn effect_handle_error(what: &str, err: starlark::Error) -> anyhow::Error {
    if mutated_immutable(&err) {
        anyhow::anyhow!(
            "{what} failed: {err}; the folded state is read-only, because it is derived from the log rather than stored; a write to it would be discarded, so decide from `state` and act through the effect builtins"
        )
    } else {
        anyhow::anyhow!("{what} failed: {err}")
    }
}

/// Whether a starlark error is the rejection of a write to a frozen value.
fn mutated_immutable(err: &starlark::Error) -> bool {
    matches!(
        starlark_cause(err).and_then(|cause| cause.downcast_ref::<ValueError>()),
        Some(ValueError::CannotMutateImmutableValue)
    )
}

/// The `anyhow` error a starlark error carries, whatever kind it is filed under.
/// Every variant wraps one; the kind only records where it came from, and a frozen
/// value's rejection is filed under `Other` rather than `Value`.
fn starlark_cause(err: &starlark::Error) -> Option<&anyhow::Error> {
    match err.kind() {
        ErrorKind::Fail(inner)
        | ErrorKind::StackOverflow(inner)
        | ErrorKind::Value(inner)
        | ErrorKind::Function(inner)
        | ErrorKind::Scope(inner)
        | ErrorKind::Parser(inner)
        | ErrorKind::Freeze(inner)
        | ErrorKind::Internal(inner)
        | ErrorKind::Native(inner)
        | ErrorKind::Other(inner) => Some(inner),
        // Upstream marks the enum non-exhaustive; a kind added later just means no
        // hint, never a wrong one.
        _ => None,
    }
}

/// Lower a set of clauses to a tephra `Query`: OR across items, AND within an item's
/// tags. `all_events()` subsumes everything, so it short-circuits to `Query::All`.
pub(crate) fn to_query(
    specs: &[EventSpec],
    events: &EventDefs,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Query> {
    let mut items = Vec::with_capacity(specs.len());
    for spec in specs {
        match to_query_item(spec, events, keystore)? {
            Some(item) => items.push(item),
            None => return Ok(Query::all()),
        }
    }
    Ok(Query::items(items))
}

/// Lower one clause to a tephra `QueryItem`, or `None` for `all_events()`, which has
/// no item form.
///
/// This is also the match predicate for per-clause dispatch: the item is handed to
/// `tephra::Matches`, the same predicate the store evaluates, so an arm's filter and
/// the subscription's filter cannot drift apart.
pub(crate) fn to_query_item(
    spec: &EventSpec,
    events: &EventDefs,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Option<QueryItem>> {
    let EventSpec::Filter {
        event_type,
        constraints,
        ..
    } = spec
    else {
        return Ok(None);
    };
    let ty = EventType::new(event_type.as_str())
        .map_err(|err| anyhow::anyhow!("invalid event type `{event_type}`: {err}"))?;
    // Fail closed: the constructor came from a declared event, so its def
    // must be in the registry, and every constrained field must exist and
    // be indexed. This backstops the static check, whose input-branch blind
    // spot could otherwise let an undeclared, non-indexed, or reserved-name
    // constraint through as a tag that silently matches nothing (or injects
    // into the host namespace).
    let def = events
        .get(event_type)
        .ok_or_else(|| anyhow::anyhow!("query references unknown event type `{event_type}`"))?;
    let mut tags = Vec::with_capacity(constraints.len());
    let mut unmatchable = false;
    for (field, value) in constraints {
        let meta = def.field(field).ok_or_else(|| {
            anyhow::anyhow!("query filters `{event_type}` on undeclared field `{field}`")
        })?;
        if !meta.indexed {
            anyhow::bail!("query filters `{event_type}` on `{field}`, which is not indexed");
        }
        match &meta.subject {
            Some(subject_field) => {
                let ks = keystore.ok_or_else(|| {
                    anyhow::anyhow!(
                        "filtering encrypted field `{field}` needs a master key (set HEKLA_MASTER_KEY)"
                    )
                })?;
                let subject_value = constraints
                    .iter()
                    .find(|(f, _)| f == subject_field)
                    .map(|(_, v)| v);
                let resolved = match subject_value {
                    // Scoped: encrypt with an existing per-subject key only,
                    // so a query never mints or resurrects one. An absent key
                    // means no matchable events, so the clause matches nothing.
                    Some(subject_value) => ks
                        .encrypt_subject_existing(subject_field, subject_value, field, value)?
                        .map(|ct| (field.clone(), ct)),
                    // Global (uniqueness): use the global key, creating it if
                    // this is the first-ever use. The global key is a
                    // never-erased singleton, so creating it on a query is
                    // safe (no resurrection), and a deterministic tag is what
                    // makes concurrent first-writers of the same value conflict
                    // at the DCB boundary instead of both committing.
                    None if meta.unique => {
                        Some((unique_tag_key(field), ks.encrypt_global(field, value)?))
                    }
                    None => anyhow::bail!(
                        "cannot filter subject-encrypted field `{field}`: also constrain its subject `{subject_field}` (scoped), or the field is not `unique` for a global match"
                    ),
                };
                match resolved {
                    Some((tag_key, ciphertext)) => tags.push((tag_key, Some(ciphertext))),
                    None => unmatchable = true,
                }
            }
            None => tags.push((field.clone(), Some(value.clone()))),
        }
    }
    if unmatchable {
        tags.push((NOMATCH_TAG.to_owned(), None));
    }
    Ok(Some(QueryItem::new(vec![ty], to_tags(&tags, &[])?)))
}

/// Lower each arm's clause to the store's own match predicate, in declaration order.
/// `None` at a position means that arm selects every event: either the
/// `all_events()` key, which selects everything.
///
/// Lowering is where a bad constraint is caught (undeclared, non-indexed, or a subject
/// field with no derivable key), so a map whose clause could never be honoured fails
/// before any event is read rather than at the first one that arrives.
pub(crate) fn lower_dispatch(
    dispatch: &EventDispatch<'_>,
    events: &EventDefs,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Vec<Option<QueryItem>>> {
    dispatch
        .arms()
        .iter()
        .map(|arm| match &arm.spec {
            Some(spec) => to_query_item(spec, events, keystore),
            None => Ok(None),
        })
        .collect()
}

/// Whether a lowered arm selects `event`, via `tephra::Matches`: the same predicate
/// the store evaluates, so an arm's filter cannot drift from the subscription's.
pub(crate) fn arm_selects(item: Option<&QueryItem>, event: EventRef<'_>) -> bool {
    match item {
        Some(item) => item.matches(event),
        None => true,
    }
}
/// Pack an emitted event for the store: its payload wrapped in a host-stamped
/// envelope, with the derived tags kept separate as tephra tags so the DCB index
/// still matches on them. When `idem_tag` is set it is added as an extra host tag,
/// so the append condition and a later recovery read can find this request's events.
/// This is the only place enveloping happens.
///
/// `event_id` is the caller's, rather than minted here, because a handler can now
/// read it back as `event.id` and derive ids from it: a live append wants a fresh
/// `Uuid::new_v4()`, and `hekla test` wants a fixed one so a derived id is the same
/// on every run.
pub fn build_event(
    event: &EmittedEvent,
    event_def: Option<&EventDef>,
    keystore: Option<&KeyStore>,
    ctx: &CommandContext,
    now: &str,
    idem_tag: Option<&str>,
    event_id: Uuid,
) -> anyhow::Result<Event> {
    let ty = EventType::new(event.event_type.as_str())
        .map_err(|err| anyhow::anyhow!("invalid event type `{}`: {err}", event.event_type))?;
    let (data, derived) = lower_event(event, event_def, keystore)?;
    let extra = idem_tag.as_slice();
    let tags = to_tags(&derived, extra)?;
    let envelope = Envelope {
        event_id,
        timestamp: now.to_owned(),
        correlation_id: ctx.correlation_id,
        causation_id: ctx.causation_id,
        triggering_event_id: ctx.triggering_event_id,
    };
    let payload = envelope::encode(&envelope, &data)?;
    Event::new(&ty, &tags, &payload)
        .map_err(|err| anyhow::anyhow!("encoding event `{}`: {err}", event.event_type))
}

/// Lower an emitted event to its stored form: encrypt every subject-scoped field
/// (in the payload and in its tag), add the global-key tag for a `unique` field, and
/// derive the plaintext tags of the remaining indexed fields. Returns the payload to
/// envelope and the tag pairs.
///
/// Fails closed on an event type the registry does not know: passing one through
/// would store a `subject` field as plaintext in both the payload and its tag,
/// unerasable and silent. Events from a handler are already checked against the
/// registry by identity when they are collected, so this is the last line of defence,
/// covering the host-built events a test harness seeds a log with.
fn lower_event(
    event: &EmittedEvent,
    event_def: Option<&EventDef>,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<(serde_json::Value, TagPairs)> {
    let def = event_def.ok_or_else(|| {
        anyhow::anyhow!(
            "event type `{}` is not declared in events/; define it there and load() it, so its schema (and any `subject` encryption) is applied",
            event.event_type
        )
    })?;
    let Some(obj) = event.data.as_object() else {
        return Ok((event.data.clone(), event.tags.clone()));
    };
    if !def.fields.iter().any(|(_, meta)| meta.subject.is_some()) {
        // No subjects: the constructor's plaintext tags are already correct.
        return Ok((event.data.clone(), event.tags.clone()));
    }
    let mut payload = obj.clone();
    let mut tags = Vec::with_capacity(def.fields.len());
    for (name, meta) in &def.fields {
        let Some(value) = obj.get(name) else { continue };
        if value.is_null() {
            continue;
        }
        match &meta.subject {
            Some(subject_field) => {
                let plaintext = scalar_to_string(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "event `{}`: subject field `{name}` must be a scalar",
                        event.event_type
                    )
                })?;
                let subject_value = obj.get(subject_field).and_then(scalar_to_string).ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "event `{}`: subject id `{subject_field}` for `{name}` is missing or not scalar",
                            event.event_type
                        )
                    },
                )?;
                let ks = keystore.ok_or_else(|| {
                    anyhow::anyhow!(
                        "event `{}` has subject-encrypted field `{name}` but no master key is configured (set HEKLA_MASTER_KEY)",
                        event.event_type
                    )
                })?;
                let ciphertext =
                    ks.encrypt_subject(subject_field, &subject_value, name, &plaintext)?;
                payload.insert(name.clone(), serde_json::Value::String(ciphertext.clone()));
                if meta.indexed {
                    tags.push((name.clone(), Some(ciphertext)));
                }
                if meta.unique {
                    let global = ks.encrypt_global(name, &plaintext)?;
                    tags.push((unique_tag_key(name), Some(global)));
                }
            }
            None if meta.indexed => {
                let text = scalar_to_string(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "event `{}`: indexed field `{name}` must be a scalar",
                        event.event_type
                    )
                })?;
                tags.push((name.clone(), Some(text)));
            }
            None => {}
        }
    }
    Ok((serde_json::Value::Object(payload), tags))
}

/// The append guard for one attempt: the DCB boundary check (fail if a matching event
/// landed after the fold read) plus, when keyed, tephra's independent existence check
/// (fail if this request's idempotency tag exists anywhere, at an implicit `after = 0`).
/// The two clauses have separate positions, so a single append atomically asserts both
/// the moving decision boundary and the whole-log uniqueness of the request; a
/// duplicate that committed anywhere is caught even when the boundary's `after` has
/// advanced past it. A boundaryless keyed command is the pure-existence case.
fn build_condition(
    boundary: Option<&Query>,
    after: Position,
    idem_tag: Option<&str>,
) -> anyhow::Result<Option<AppendCondition>> {
    let existence = match idem_tag {
        Some(tag) => Some(Query::item(idem_item(tag)?)),
        None => None,
    };
    // Cloned rather than moved: the boundary is derived once per request and every
    // attempt guards against the same one.
    match (boundary.cloned(), existence) {
        (None, None) => Ok(None),
        (None, Some(exists)) => Ok(Some(AppendCondition::exists_only(exists))),
        (Some(query), None) => Ok(Some(AppendCondition::new(query).after(after))),
        (Some(query), Some(exists)) => Ok(Some(
            AppendCondition::new(query)
                .after(after)
                .fail_if_exists(exists),
        )),
    }
}

/// A query item matching any event carrying the idempotency tag.
fn idem_item(tag: &str) -> anyhow::Result<QueryItem> {
    let tag = Tag::new(tag.to_owned())
        .map_err(|err| anyhow::anyhow!("invalid idempotency tag `{tag}`: {err}"))?;
    let tags =
        Tags::new([tag]).map_err(|err| anyhow::anyhow!("invalid idempotency tag set: {err}"))?;
    Ok(QueryItem::with_tags(tags))
}

/// A keyed request's own prior commit, checked when this attempt is about to return
/// without appending anything. Both such decisions (a `handle` that rejects, and one
/// that emits nothing) can be spurious under a crashed or concurrent same-key
/// duplicate: this attempt folded the duplicate's just-committed events and concluded
/// the work was already done. Neither appends, so the append's existence clause can't
/// catch it, and only a boundaried command folds state at all, which is why the tag
/// re-read is confined to that case.
fn recover_if_committed(
    store: &WriteHandle,
    event_defs: &EventDefs,
    boundary: Option<&Query>,
    idem_tag: Option<&str>,
) -> anyhow::Result<Option<RecoveredOutcome>> {
    match (boundary, idem_tag) {
        (Some(_), Some(tag)) => find_committed_outcome(store, event_defs, tag),
        _ => Ok(None),
    }
}

/// Look for a prior committed attempt of this request in the log, by its idempotency
/// tag, so a replay returns the original outcome without re-running `handle`. Returns
/// `None` when the request has not committed yet. The read is an indexed existence
/// check on a unique, high-cardinality tag at `after = 0`: a term-dictionary probe per
/// segment, no posting-list decode (the single position inlines into the FST value).
fn find_committed_outcome(
    store: &WriteHandle,
    event_defs: &EventDefs,
    idem_tag: &str,
) -> anyhow::Result<Option<RecoveredOutcome>> {
    let query = Query::item(idem_item(idem_tag)?);
    let mut reads = store.read(&query, Position::ZERO, None);
    let mut events = Vec::new();
    let mut range: Option<(Position, Position)> = None;
    let mut ids: Option<(Uuid, Uuid)> = None;
    while let Some(item) = reads.next() {
        let seq = item.map_err(|err| anyhow::anyhow!("reading idempotent replay: {err}"))?;
        range = Some(match range {
            Some((first, _)) => (first, seq.position),
            None => (seq.position, seq.position),
        });
        let (envelope, _data) = envelope::decode(seq.event.data())
            .map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
        match ids {
            None => ids = Some((envelope.correlation_id, envelope.causation_id)),
            // Every event of one command execution shares its causation id. A second
            // distinct one means two logical requests matched this tag (a double
            // commit, or an astronomically unlikely hash collision): surface it rather
            // than splice both commits into one bogus recovered outcome.
            Some((_, causation)) if envelope.causation_id != causation => {
                anyhow::bail!("idempotency tag matches events from more than one command execution")
            }
            Some(_) => {}
        }
        // Report the same tags a fresh commit does: plaintext, non-subject indexed
        // fields only. Subject fields are stored as ciphertext (and the recovery path
        // deliberately cannot decrypt), and the reserved host tags (`_hekla_idem`, the
        // `_hekla_uniq_` global tags) are internal, so both are dropped.
        let def = event_defs.get(seq.event.event_type());
        let mut tags: Vec<String> = seq
            .event
            .tags()
            .filter(|tag| !tag.starts_with(RESERVED_TAG_PREFIX))
            .filter(|tag| {
                let key = tag.split(':').next().unwrap_or(tag);
                !def.is_some_and(|def| def.is_subject(key))
            })
            .map(str::to_owned)
            .collect();
        // Stored tag sets are sorted; sorting the response tags too keeps recovery
        // byte-identical to the live outcome, which also sorts (see `tag_strings`).
        tags.sort();
        events.push(RecoveredEvent {
            event_type: seq.event.event_type().to_owned(),
            tags,
        });
    }
    let Some((first, last)) = range else {
        return Ok(None);
    };
    let (correlation_id, causation_id) = ids.expect("a matched event carries an envelope");
    Ok(Some(RecoveredOutcome {
        events,
        positions: PositionRange { first, last },
        correlation_id,
        causation_id,
    }))
}

/// `(key, Some(value))` → `"key:value"`, `(key, None)` → `"key"`, plus any `extra`
/// raw tag strings appended verbatim (the host's reserved tags). Query and event go
/// through the same pair mapping, so a keyed tag matches only a keyed tag.
fn to_tags(pairs: &[(String, Option<String>)], extra: &[&str]) -> anyhow::Result<Tags> {
    let mut tags = pairs
        .iter()
        .map(|(key, value)| {
            let raw = match value {
                Some(value) => format!("{key}:{value}"),
                None => key.clone(),
            };
            Tag::new(raw).map_err(|err| anyhow::anyhow!("invalid tag `{key}`: {err}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for raw in extra {
        tags.push(
            Tag::new((*raw).to_owned())
                .map_err(|err| anyhow::anyhow!("invalid tag `{raw}`: {err}"))?,
        );
    }
    Tags::new(tags).map_err(|err| anyhow::anyhow!("invalid tag set: {err}"))
}
