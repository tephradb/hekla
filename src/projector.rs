//! The projector runtime: one sequential task per projector.
//!
//! Each projector owns a SQLite read model at `data/projectors/{name}.db` and a
//! dedicated thread. The thread subscribes to the projector's `handle` keys from its
//! persisted checkpoint, and for each batch of events runs `handle`, applies the
//! emitted ops, and advances the checkpoint, all in one transaction, so state and
//! progress can never disagree. `get()` inside a handler reads through the batch's
//! own uncommitted writes because every read and write runs on the one connection.
//!
//! Replay is rebuild-and-swap: a fresh database is built from position 0 and
//! renamed into place, so a crash mid-rebuild leaves the live model untouched.

use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;
use starlark::environment::{FrozenModule, Module};
use tephra::{Event, Position, WaitOutcome, WriteHandle};

use crate::context::{EntityReader, ProjectorCtx};
use crate::dispatch::{self, EventDefs};
use crate::dispatch::{arm_selects, lower_dispatch};
use crate::envelope;
use crate::loader::ProjectorUnit;
use crate::read_model::ReadModel;
use crate::starlark_builtins::{
    EntityDef, EventSpec, LoadedModule, ModuleDef, alloc_event, call_handler_with_projector_ctx,
    parse_entity_ops, parse_event_dispatch, thaw,
};

/// Per-handler instruction budget, matching the command dispatch bound.
const MAX_TICKS: u64 = 10_000_000;

/// How long a caught-up projector blocks before re-checking its shutdown flag.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// Whether a projector's read model on disk currently has the shape the read API
/// would serve it at.
///
/// The read API builds its `SELECT` from [`ProjectorShared::entities`], the *current*
/// definition, while `db_path` holds whatever the last run left. `ReadModel::open`
/// issues `CREATE TABLE IF NOT EXISTS`, so it will not add a column to an existing
/// table; until a rebuild swaps a fresh model in, the two disagree and every query
/// fails on the missing column. Serving a shaped 503 for that window beats leaking a
/// SQLite error as a 500.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The on-disk model matches the current definition.
    Ready,
    /// The definition changed and a rebuild is in flight; this resolves on its own.
    Rebuilding,
    /// The definition changed and auto-rebuild is off, so nothing will resolve it
    /// but an operator-triggered replay.
    Stale,
    /// A rebuild was attempted and failed, leaving the model at the shape it had.
    /// Like [`Readiness::Stale`] it needs an operator, but the cause is an error
    /// rather than a setting, so [`ProjectorShared::last_error`] names it.
    Failed,
}

impl Readiness {
    fn from_u8(raw: u8) -> Readiness {
        match raw {
            1 => Readiness::Rebuilding,
            2 => Readiness::Stale,
            3 => Readiness::Failed,
            _ => Readiness::Ready,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Readiness::Ready => 0,
            Readiness::Rebuilding => 1,
            Readiness::Stale => 2,
            Readiness::Failed => 3,
        }
    }

    /// The wire word for `/status` and the read API's error body.
    pub fn label(self) -> &'static str {
        match self {
            Readiness::Ready => "ready",
            Readiness::Rebuilding => "rebuilding",
            Readiness::Stale => "stale",
            Readiness::Failed => "rebuild_failed",
        }
    }

    /// Whether a batch built from the current entity definitions can be applied to
    /// the model on disk. Only the shape matters, so a lagging or failed projector
    /// whose model is current still applies.
    fn applies_batches(self) -> bool {
        matches!(self, Readiness::Ready | Readiness::Rebuilding)
    }
}

/// The shared, observable state of a running projector. The read API reads its
/// `db_path`, `entities` and `readiness`; `/status` reads its `position`; the thread
/// reads its flags. Kept behind an `Arc` so all three can hold it at once.
pub struct ProjectorShared {
    pub name: String,
    pub db_path: PathBuf,
    pub entities: Arc<Vec<EntityDef>>,
    /// Stored Release and loaded Acquire: a reader that observes a position must also
    /// observe the commit that produced it, which is what read-your-writes rests on.
    /// `readiness` is ordered the same way, for the same reason (a reader that sees
    /// `Ready` must see the rebuilt model behind it). The plain flags below carry no
    /// such payload and stay Relaxed.
    position: AtomicU64,
    shutdown: AtomicBool,
    replay: AtomicBool,
    /// Cleared when the thread exits, so a replay request can say plainly that
    /// nothing is left to pick it up rather than accept it and drop it.
    running: AtomicBool,
    /// Set when an operation failed: the thread exited on an error (a poison event,
    /// a decode failure, a `handle` bug), or a rebuild failed but the thread lives on
    /// to retry. The read API keeps serving the frozen model, so `/status` reports
    /// this to distinguish a wedged projector from one merely lagging.
    failed: AtomicBool,
    /// A [`Readiness`] discriminant. Decided synchronously in [`spawn`], before the
    /// handle is published, so the read API never observes a shape it cannot serve.
    readiness: AtomicU8,
    last_error: Mutex<Option<String>>,
}

impl ProjectorShared {
    /// The last checkpoint position the thread has committed.
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Acquire)
    }

    /// Whether the projector's last operation failed. Read alongside
    /// [`ProjectorShared::running`] and [`ProjectorShared::readiness`], which say
    /// whether it can recover on its own.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Whether the projector's thread is still alive. A stopped projector serves its
    /// frozen model but will never advance or act on a replay again.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Whether the on-disk read model can be served at the current definition.
    pub fn readiness(&self) -> Readiness {
        Readiness::from_u8(self.readiness.load(Ordering::Acquire))
    }

    fn set_readiness(&self, readiness: Readiness) {
        self.readiness.store(readiness.as_u8(), Ordering::Release);
    }

    /// The error that stopped the thread, if it failed.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Ask the projector to rebuild-and-swap its read model. Picked up between
    /// batches; returns immediately.
    pub fn request_replay(&self) {
        self.replay.store(true, Ordering::Relaxed);
    }

    fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn record_failure(&self, message: &str) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(message.to_owned());
        self.failed.store(true, Ordering::Relaxed);
    }

    /// Clear a recorded failure once the projector has recovered from it, so
    /// `/status` reports the current state rather than the worst one it ever saw.
    fn clear_failure(&self) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.failed.store(false, Ordering::Relaxed);
    }
}

/// The join handles for the running projector threads, kept out of the shared
/// `Runtime` so shutdown can join them without interior mutability.
#[derive(Default)]
pub struct ProjectorSet {
    shared: Vec<Arc<ProjectorShared>>,
    joins: Vec<JoinHandle<()>>,
}

impl ProjectorSet {
    fn push(&mut self, shared: Arc<ProjectorShared>, join: JoinHandle<()>) {
        self.shared.push(shared);
        self.joins.push(join);
    }

    /// Signal every projector to stop, then join its thread. Called while the
    /// write coordinator is still live, so each thread can drain to head and
    /// commit its final batch before exiting.
    pub fn shutdown_and_join(self) {
        for shared in &self.shared {
            shared.stop();
        }
        for join in self.joins {
            if let Err(err) = join.join() {
                tracing::error!("a projector thread panicked: {err:?}");
            }
        }
    }
}

/// Open every projector's read model and start its thread. Read models are opened
/// synchronously here, before any thread runs, so the read API never races a
/// missing database file.
pub fn start_all(
    projectors: Vec<Arc<ProjectorUnit>>,
    store: &WriteHandle,
    projectors_dir: &Path,
    events: Arc<EventDefs>,
    auto_rebuild: bool,
) -> anyhow::Result<(Vec<Arc<ProjectorShared>>, ProjectorSet)> {
    let mut shared = Vec::with_capacity(projectors.len());
    let mut set = ProjectorSet::default();
    for unit in projectors {
        let (handle, join) = spawn(
            unit,
            store.clone(),
            projectors_dir,
            events.clone(),
            auto_rebuild,
        )?;
        shared.push(Arc::clone(&handle));
        set.push(handle, join);
    }
    Ok((shared, set))
}

fn spawn(
    unit: Arc<ProjectorUnit>,
    store: WriteHandle,
    projectors_dir: &Path,
    events: Arc<EventDefs>,
    auto_rebuild: bool,
) -> anyhow::Result<(Arc<ProjectorShared>, JoinHandle<()>)> {
    let ModuleDef::Projector {
        name,
        entities,
        sources,
    } = &unit.loaded.def
    else {
        anyhow::bail!("spawn called on a non-projector module");
    };
    let name = name.clone();
    let definition = definition_hash(sources, entities);
    let entities = entities.clone();

    let db_path = projectors_dir.join(format!("{name}.db"));
    let model = ReadModel::open(&db_path, &entities)
        .with_context(|| format!("opening read model for projector `{name}`"))?;
    let start = model.read_checkpoint()?;
    // Comparing the recorded definition is one small read, so it happens here, before
    // the handle is published. Only the replay it may imply is slow, and that stays on
    // the thread: boot does not wait for it, but the read API knows not to serve this
    // projector until it lands.
    let plan = reconcile_plan(&model, &definition, auto_rebuild)?;

    let shared = Arc::new(ProjectorShared {
        name: name.clone(),
        db_path,
        entities: Arc::new(entities),
        position: AtomicU64::new(start.get()),
        shutdown: AtomicBool::new(false),
        replay: AtomicBool::new(false),
        running: AtomicBool::new(true),
        failed: AtomicBool::new(false),
        readiness: AtomicU8::new(plan.readiness().as_u8()),
        last_error: Mutex::new(None),
    });

    let task_shared = Arc::clone(&shared);
    let join = thread::Builder::new()
        .name(format!("projector-{name}"))
        .spawn(move || run(task_shared, unit, store, model, events, definition, plan))
        .with_context(|| format!("spawning projector `{name}`"))?;
    Ok((shared, join))
}

/// Clears [`ProjectorShared::running`] when the projector thread leaves, so a replay
/// is never accepted by a projector that is no longer there to act on it.
struct RunningFlag<'a>(&'a ProjectorShared);

impl Drop for RunningFlag<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Relaxed);
    }
}

/// What reconciling the read model against the current definition requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reconcile {
    /// The recorded definition matches the current one.
    UpToDate,
    /// A fresh model: record the current definition as its baseline and build forward.
    Stamp,
    /// The model was built from a different event set or at a different shape, and
    /// auto-rebuild is on.
    Rebuild,
    /// The same mismatch, but auto-rebuild is off, so only a manual replay resolves it.
    Stale,
}

impl Reconcile {
    fn readiness(self) -> Readiness {
        match self {
            Reconcile::UpToDate | Reconcile::Stamp => Readiness::Ready,
            Reconcile::Rebuild => Readiness::Rebuilding,
            Reconcile::Stale => Readiness::Stale,
        }
    }
}

/// Decide how the read model must be reconciled with `definition`.
///
/// A model with no recorded definition predates that field. If it is empty there is
/// nothing to preserve, so it takes the current definition as its baseline; if it is
/// populated its shape cannot be verified, so it is treated as a mismatch rather than
/// blessed as current.
fn reconcile_plan(
    model: &ReadModel,
    definition: &str,
    auto_rebuild: bool,
) -> anyhow::Result<Reconcile> {
    let mismatch = if auto_rebuild {
        Reconcile::Rebuild
    } else {
        Reconcile::Stale
    };
    Ok(match model.read_definition()? {
        Some(previous) if previous == definition => Reconcile::UpToDate,
        Some(_) => mismatch,
        None if model.read_checkpoint()?.get() == 0 => Reconcile::Stamp,
        None => mismatch,
    })
}

fn run(
    shared: Arc<ProjectorShared>,
    unit: Arc<ProjectorUnit>,
    store: WriteHandle,
    model: ReadModel,
    events: Arc<EventDefs>,
    definition: String,
    plan: Reconcile,
) {
    // Declared first so it drops last: `running` is cleared however the thread leaves,
    // a panic included, and only once the failure below has been recorded.
    let _running = RunningFlag(&shared);
    if let Err(err) = run_inner(&shared, &unit, &store, model, &events, &definition, plan) {
        let message = format!("{err:#}");
        tracing::error!("projector `{}` stopped: {message}", shared.name);
        shared.record_failure(&message);
    }
}

fn run_inner(
    shared: &ProjectorShared,
    unit: &ProjectorUnit,
    store: &WriteHandle,
    mut model: ReadModel,
    events: &EventDefs,
    definition: &str,
    plan: Reconcile,
) -> anyhow::Result<()> {
    let ModuleDef::Projector { sources, .. } = &unit.loaded.def else {
        anyhow::bail!("run called on a non-projector module");
    };
    let query = dispatch::to_query(sources, events, None)?;
    let by_id = by_id_map(&shared.entities);
    let frozen = &unit.loaded.module;

    // Act on the plan [`spawn`] decided before publishing the handle. A rebuild has to
    // finish before any batch lands, or a reader could briefly see stale data at the
    // wrong shape. The definition hash lives in the read model and is written by the
    // rebuild's atomic swap, so this is crash-safe: a crash mid-rebuild leaves the old
    // hash, and the next boot rebuilds again.
    match plan {
        Reconcile::UpToDate => {}
        Reconcile::Stamp => model.set_definition(definition)?,
        Reconcile::Rebuild => {
            tracing::info!(
                "projector `{}` definition changed; rebuilding its read model",
                shared.name
            );
            model = rebuild_or_degrade(shared, unit, store, model, events, definition)?;
        }
        // Do not stamp the current definition onto a model we did not rebuild: that
        // would bless possibly-stale data at a possibly-old shape as current and
        // silence this forever. The read API refuses to serve it until a manual
        // replay rebuilds and records it.
        Reconcile::Stale => tracing::warn!(
            "projector `{}` read model does not match its definition and auto-rebuild is off; POST /projectors/{}/replay to rebuild it",
            shared.name,
            shared.name
        ),
    }

    let mut sub = store.subscribe(query.clone(), model.read_checkpoint()?);
    loop {
        if shared.replay.swap(false, Ordering::Relaxed) {
            // A replay is the only way out of `Stale` or `Failed`, and it is harmless
            // otherwise.
            model = rebuild_or_degrade(shared, unit, store, model, events, definition)?;
            sub = store.subscribe(query.clone(), model.read_checkpoint()?);
            continue;
        }

        // A model at a previous definition's shape cannot take a batch built from the
        // current entities: the apply would fail on a missing column and stop the
        // thread. Idle instead, leaving the checkpoint where it is, until a replay
        // rebuilds it.
        if !shared.readiness().applies_batches() {
            if shared.shutdown.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(IDLE_POLL);
            continue;
        }

        let batch = sub
            .poll_batch()
            .map_err(|err| anyhow::anyhow!("reading events: {err}"))?;
        if !batch.is_empty() {
            apply_batch(&model, frozen, &by_id, &batch, sub.position(), events)?;
            shared
                .position
                .store(sub.position().get(), Ordering::Release);
            continue;
        }

        // Caught up: no matching events remain, but the subscription's watermark may
        // have advanced past a non-matching tail. Persist and publish it so a
        // selective projector tracks head (honest /status lag, and a read-your-writes
        // wait resolves) instead of stalling at its last matching event. The
        // checkpoint is a watermark, so resuming past the tail skips nothing.
        let watermark = sub.position();
        if watermark.get() > shared.position() {
            model.advance_checkpoint(watermark)?;
            shared.position.store(watermark.get(), Ordering::Release);
        }

        // Stop only here, so a pending shutdown still drains to head.
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if let WaitOutcome::Closed = sub.wait_timeout(IDLE_POLL) {
            break;
        }
    }
    Ok(())
}

/// Rebuild and swap, keeping the thread alive when it fails.
///
/// A rebuild that stopped the thread left the read API serving a self-resolving
/// `rebuilding` 503 that nothing would ever resolve, and a replay request with no
/// thread to pick it up: the only recovery was a restart. Recording the failure and
/// idling instead keeps the replay endpoint meaningful, so an operator can fix the
/// cause and retry in place.
fn rebuild_or_degrade(
    shared: &ProjectorShared,
    unit: &ProjectorUnit,
    store: &WriteHandle,
    model: ReadModel,
    events: &EventDefs,
    definition: &str,
) -> anyhow::Result<ReadModel> {
    let err = match rebuild(shared, unit, store, model, events, definition) {
        Ok(fresh) => {
            shared.clear_failure();
            shared.set_readiness(Readiness::Ready);
            return Ok(fresh);
        }
        Err(err) => err,
    };
    let message = format!("{err:#}");
    tracing::error!(
        "projector `{}` could not rebuild its read model: {message}",
        shared.name
    );
    shared.record_failure(&message);
    shared.set_readiness(Readiness::Failed);

    // `rebuild` closes the live model before swapping, so reopen whatever survived on
    // disk. The swap is its last step and may well have landed, in which case the
    // model is current after all and the projector goes on serving and tailing.
    let reopened = ReadModel::open(&shared.db_path, &shared.entities).with_context(|| {
        format!(
            "reopening the read model for projector `{}` after a failed rebuild",
            shared.name
        )
    })?;
    if reconcile_plan(&reopened, definition, false)? != Reconcile::Stale {
        shared.set_readiness(Readiness::Ready);
    }
    shared
        .position
        .store(reopened.read_checkpoint()?.get(), Ordering::Release);
    Ok(reopened)
}

/// Project every event up to the current head into `model`, committing in batches
/// and advancing the checkpoint. Does not tail live events. Used by replay and by
/// tests; the live loop drives batches itself so it can also wait and shut down.
pub fn project_to_head(
    store: &WriteHandle,
    unit: &LoadedModule,
    model: &ReadModel,
    events: &EventDefs,
) -> anyhow::Result<usize> {
    let ModuleDef::Projector {
        entities, sources, ..
    } = &unit.def
    else {
        anyhow::bail!("project_to_head called on a non-projector module");
    };
    let query = dispatch::to_query(sources, events, None)?;
    let by_id = by_id_map(entities);
    let mut sub = store.subscribe(query, model.read_checkpoint()?);
    let mut seen = 0usize;
    loop {
        let batch = sub
            .poll_batch()
            .map_err(|err| anyhow::anyhow!("reading events: {err}"))?;
        if batch.is_empty() {
            break;
        }
        seen += batch.len();
        apply_batch(model, &unit.module, &by_id, &batch, sub.position(), events)?;
    }
    Ok(seen)
}

/// Apply one batch of events and advance the checkpoint, in one transaction. Every
/// read and write runs on `model`'s single connection, so a `get()` in a later
/// event sees a `put()` from an earlier one in the same batch.
fn apply_batch(
    model: &ReadModel,
    frozen: &FrozenModule,
    by_id: &HashMap<u64, EntityDef>,
    batch: &[(Position, Event)],
    checkpoint: Position,
    events: &EventDefs,
) -> anyhow::Result<()> {
    let tx = model.begin()?;
    Module::with_temp_heap(|module| {
        let handle_owned = frozen
            .get_option("handle")?
            .ok_or_else(|| anyhow::anyhow!("projector has no handle() function"))?;
        let handle = parse_event_dispatch(thaw(&handle_owned, &module))
            .map_err(|err| anyhow::anyhow!("`handle` {err}"))?;
        // `None` matches how a projector lowers its subscription: filtering a
        // subject-encrypted field in a `handle` key is a static error, so no key is needed.
        let lowered = lower_dispatch(&handle, events, None)
            .map_err(|err| anyhow::anyhow!("`handle` {err}"))?;
        for (_position, event) in batch {
            // Matching before decoding: an event no arm selects costs nothing, and the
            // checkpoint still advances past it.
            let selected: Vec<usize> = lowered
                .iter()
                .enumerate()
                .filter(|(_, item)| arm_selects(item.as_ref(), event.as_ref()))
                .map(|(index, _)| index)
                .collect();
            if selected.is_empty() {
                continue;
            }
            let event_type = event.event_type();
            let (_envelope, data) = envelope::decode(event.data())
                .map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
            let value = alloc_event(&module, event_type, &data, events.get(event_type));
            // Every selecting arm runs in declaration order, and `get()` reads through
            // the batch's own uncommitted writes, so a later arm sees an earlier one's
            // ops.
            for index in selected {
                let arm = &handle.arms()[index];
                let reader = BatchReader { model, by_id };
                let ctx = ProjectorCtx { reader: &reader };
                let result =
                    call_handler_with_projector_ctx(&module, arm.func, &[value], MAX_TICKS, &ctx)
                        .map_err(|err| {
                        anyhow::anyhow!(
                            "{} failed: {err}",
                            handle.label("handle", arm.spec.as_ref())
                        )
                    })?;
                for op in parse_entity_ops(result)? {
                    let entity = by_id.get(&op.entity_id).ok_or_else(|| {
                        anyhow::anyhow!("op references an entity the projector didn't declare")
                    })?;
                    model
                        .apply_one(entity, op.kind)
                        .with_context(|| format!("applying an op to entity `{}`", entity.name))?;
                }
            }
        }
        anyhow::Ok(())
    })?;
    model.write_checkpoint(checkpoint, &tx)?;
    tx.commit().context("committing a projector batch")?;
    Ok(())
}

/// Rebuild the read model from position 0 into a sibling file, then swap it in by
/// rename so state and position move together atomically. Consumes the live model
/// (closing its connection before the swap), publishes the rebuilt checkpoint as the
/// projector's position, and returns the reopened model.
fn rebuild(
    shared: &ProjectorShared,
    unit: &ProjectorUnit,
    store: &WriteHandle,
    model: ReadModel,
    events: &EventDefs,
    definition: &str,
) -> anyhow::Result<ReadModel> {
    let db_path = &shared.db_path;
    let rebuild_path = db_path.with_extension("rebuild.db");
    remove_db_files(&rebuild_path)?;

    let fresh = ReadModel::open(&rebuild_path, &shared.entities)?;
    let count = project_to_head(store, &unit.loaded, &fresh, events)?;
    // Stamp the definition the fresh model was built under, so it swaps in atomically
    // with the data (and a crash before the swap leaves the old model and its old
    // definition intact).
    fresh.set_definition(definition)?;
    // Fold the WAL back into the main file and drop to rollback mode, so the file
    // is self-contained: a reader that opens it mid-swap ignores any stale `-wal`.
    fresh.seal()?;
    drop(fresh);
    drop(model);

    fs::rename(&rebuild_path, db_path)
        .with_context(|| format!("swapping in the rebuilt read model for `{}`", shared.name))?;
    // The old inode's `-wal`/`-shm`, named after `db_path`, are now orphaned.
    remove_sidecars(db_path)?;

    let reopened = ReadModel::open(db_path, &shared.entities)?;
    shared
        .position
        .store(reopened.read_checkpoint()?.get(), Ordering::Release);
    tracing::info!("projector `{}` replayed {count} events", shared.name);
    Ok(reopened)
}

/// A stable hash of a projector's *definition* (its source set and entity schema),
/// not its handler logic, so a restart can tell when the event set it was built from
/// has changed and rebuild it. The entity's process-unique `id` and the handler body
/// are deliberately excluded: an id changes every run, and a handler fix is the
/// author's call to replay, not an automatic one.
fn definition_hash(sources: &[EventSpec], entities: &[EntityDef]) -> String {
    let mut canonical = String::new();
    for spec in sources {
        match spec {
            EventSpec::All => canonical.push_str("all;"),
            EventSpec::Filter {
                event_type,
                constraints,
                ..
            } => {
                canonical.push_str(event_type);
                canonical.push('(');
                for (field, value) in constraints {
                    canonical.push_str(field);
                    canonical.push('=');
                    canonical.push_str(value);
                    canonical.push(',');
                }
                canonical.push_str(");");
            }
        }
    }
    canonical.push('|');
    for entity in entities {
        canonical.push_str(&entity.name);
        canonical.push(':');
        canonical.push_str(&entity.key);
        canonical.push('{');
        for (name, meta) in &entity.fields {
            canonical.push_str(name);
            canonical.push('=');
            let _ = write!(canonical, "{:?}", meta.kind);
            if let Some(subject) = &meta.subject {
                canonical.push('@');
                canonical.push_str(subject);
            }
            canonical.push(',');
        }
        canonical.push('}');
        for index in &entity.indexes {
            canonical.push_str(&index.name);
            canonical.push('(');
            canonical.push_str(&index.columns.join(","));
            canonical.push(')');
        }
        canonical.push(';');
    }
    crate::hash::sha256_hex(canonical.as_bytes())
}

/// Resolve `put`/`patch`/`delete`/`get` entity references (which carry only an id)
/// back to their resolved definitions.
fn by_id_map(entities: &[EntityDef]) -> HashMap<u64, EntityDef> {
    entities
        .iter()
        .map(|entity| (entity.id, entity.clone()))
        .collect()
}

/// The read model's view for one batch: `get()` resolves an entity id to its
/// definition and reads the current row through the batch's open transaction.
struct BatchReader<'a> {
    model: &'a ReadModel,
    by_id: &'a HashMap<u64, EntityDef>,
}

impl EntityReader for BatchReader<'_> {
    fn get(&self, entity_id: u64, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let entity = self.by_id.get(&entity_id).ok_or_else(|| {
            anyhow::anyhow!("get() references an entity the projector didn't declare")
        })?;
        self.model.get(entity, key)
    }
}

fn remove_db_files(db_path: &Path) -> anyhow::Result<()> {
    remove_if_exists(db_path)?;
    remove_sidecars(db_path)
}

fn remove_sidecars(db_path: &Path) -> anyhow::Result<()> {
    remove_if_exists(&sidecar(db_path, "-wal"))?;
    remove_if_exists(&sidecar(db_path, "-shm"))
}

fn sidecar(db_path: &Path, suffix: &str) -> PathBuf {
    let mut raw = db_path.as_os_str().to_owned();
    raw.push(suffix);
    PathBuf::from(raw)
}

fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use tephra::Position;

    use super::*;
    use crate::starlark_builtins::{FieldKind, FieldMeta};

    fn entity() -> EntityDef {
        EntityDef {
            id: 1,
            name: "rows".to_owned(),
            key: "id".to_owned(),
            fields: vec![("id".to_owned(), FieldMeta::plain(FieldKind::Uuid))],
            indexes: vec![],
        }
    }

    fn open_temp() -> (ReadModel, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![entity()];
        let model = ReadModel::open(&dir.path().join("p.db"), &entities).unwrap();
        (model, dir)
    }

    /// Move the checkpoint off zero, so the model counts as populated.
    fn populate(model: &ReadModel) {
        model.advance_checkpoint(Position::new(7)).unwrap();
    }

    #[test]
    fn a_matching_definition_is_up_to_date() {
        let (model, _dir) = open_temp();
        model.set_definition("abc").unwrap();
        assert_eq!(
            reconcile_plan(&model, "abc", true).unwrap(),
            Reconcile::UpToDate
        );
    }

    #[test]
    fn a_changed_definition_rebuilds_when_auto_rebuild_is_on() {
        let (model, _dir) = open_temp();
        model.set_definition("old").unwrap();
        assert_eq!(
            reconcile_plan(&model, "new", true).unwrap(),
            Reconcile::Rebuild
        );
    }

    #[test]
    fn a_changed_definition_is_stale_when_auto_rebuild_is_off() {
        let (model, _dir) = open_temp();
        model.set_definition("old").unwrap();
        assert_eq!(
            reconcile_plan(&model, "new", false).unwrap(),
            Reconcile::Stale
        );
    }

    #[test]
    fn a_fresh_model_takes_the_current_definition_as_its_baseline() {
        let (model, _dir) = open_temp();
        assert_eq!(
            reconcile_plan(&model, "new", true).unwrap(),
            Reconcile::Stamp
        );
        assert_eq!(
            reconcile_plan(&model, "new", false).unwrap(),
            Reconcile::Stamp
        );
    }

    #[test]
    fn a_populated_model_with_no_recorded_definition_is_never_blessed() {
        let (model, _dir) = open_temp();
        populate(&model);
        assert_eq!(
            reconcile_plan(&model, "new", true).unwrap(),
            Reconcile::Rebuild
        );
        assert_eq!(
            reconcile_plan(&model, "new", false).unwrap(),
            Reconcile::Stale
        );
    }

    #[test]
    fn readiness_follows_the_plan() {
        assert_eq!(Reconcile::UpToDate.readiness(), Readiness::Ready);
        assert_eq!(Reconcile::Stamp.readiness(), Readiness::Ready);
        assert_eq!(Reconcile::Rebuild.readiness(), Readiness::Rebuilding);
        assert_eq!(Reconcile::Stale.readiness(), Readiness::Stale);
    }

    #[test]
    fn readiness_round_trips_through_its_atomic_form() {
        for readiness in [
            Readiness::Ready,
            Readiness::Rebuilding,
            Readiness::Stale,
            Readiness::Failed,
        ] {
            assert_eq!(Readiness::from_u8(readiness.as_u8()), readiness);
        }
    }

    /// A model the read API refuses to serve is one a batch cannot be applied to
    /// either: both turn on whether the shape on disk is the current one. `Rebuilding`
    /// is the exception, since the rebuild itself is what applies those batches.
    #[test]
    fn only_a_current_shape_takes_batches() {
        assert!(Readiness::Ready.applies_batches());
        assert!(Readiness::Rebuilding.applies_batches());
        assert!(!Readiness::Stale.applies_batches());
        assert!(!Readiness::Failed.applies_batches());
    }

    /// Reconciling never yields `Failed`: it is decided by a rebuild that ran, not by
    /// comparing definitions, so nothing here may produce it.
    #[test]
    fn a_reconcile_plan_never_starts_out_failed() {
        for plan in [
            Reconcile::UpToDate,
            Reconcile::Stamp,
            Reconcile::Rebuild,
            Reconcile::Stale,
        ] {
            assert_ne!(plan.readiness(), Readiness::Failed);
        }
    }
}
