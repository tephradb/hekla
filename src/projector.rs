//! The projector runtime: one sequential task per projector.
//!
//! Each projector owns a SQLite read model at `data/projectors/{name}.db` and a
//! dedicated thread. The thread subscribes to the projector's `source` from its
//! persisted checkpoint, and for each batch of events runs `handle`, applies the
//! emitted ops, and advances the checkpoint, all in one transaction, so state and
//! progress can never disagree. `get()` inside a handler reads through the batch's
//! own uncommitted writes because every read and write runs on the one connection.
//!
//! Replay is rebuild-and-swap: a fresh database is built from position 0 and
//! renamed into place, so a crash mid-rebuild leaves the live model untouched.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;
use starlark::environment::{FrozenModule, Module};
use tephra::{Event, Position, WaitOutcome, WriteHandle};

use crate::context::{EntityReader, ProjectorCtx};
use crate::dispatch;
use crate::envelope;
use crate::loader::ProjectorUnit;
use crate::read_model::ReadModel;
use crate::starlark_builtins::{
    EntityDef, LoadedModule, ModuleDef, alloc_event, call_handler_with_projector_ctx,
    parse_entity_ops, thaw,
};

/// Per-handler instruction budget, matching the command dispatch bound.
const MAX_TICKS: u64 = 10_000_000;

/// How long a caught-up projector blocks before re-checking its shutdown flag.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// The shared, observable state of a running projector. The read API reads its
/// `db_path` and `entities`; `/status` reads its `position`; the thread reads its
/// flags. Kept behind an `Arc` so all three can hold it at once.
pub struct ProjectorShared {
    pub name: String,
    pub db_path: PathBuf,
    pub entities: Arc<Vec<EntityDef>>,
    position: AtomicU64,
    shutdown: AtomicBool,
    replay: AtomicBool,
}

impl ProjectorShared {
    /// The last checkpoint position the thread has committed.
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    /// Ask the projector to rebuild-and-swap its read model. Picked up between
    /// batches; returns immediately.
    pub fn request_replay(&self) {
        self.replay.store(true, Ordering::Relaxed);
    }

    fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
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
) -> anyhow::Result<(Vec<Arc<ProjectorShared>>, ProjectorSet)> {
    let mut shared = Vec::with_capacity(projectors.len());
    let mut set = ProjectorSet::default();
    for unit in projectors {
        let (handle, join) = spawn(unit, store.clone(), projectors_dir)?;
        shared.push(Arc::clone(&handle));
        set.push(handle, join);
    }
    Ok((shared, set))
}

fn spawn(
    unit: Arc<ProjectorUnit>,
    store: WriteHandle,
    projectors_dir: &Path,
) -> anyhow::Result<(Arc<ProjectorShared>, JoinHandle<()>)> {
    let ModuleDef::Projector { name, entities, .. } = &unit.loaded.def else {
        anyhow::bail!("spawn called on a non-projector module");
    };
    let name = name.clone();
    let entities = entities.clone();

    let db_path = projectors_dir.join(format!("{name}.db"));
    let model = ReadModel::open(&db_path, &entities)
        .with_context(|| format!("opening read model for projector `{name}`"))?;
    let start = model.read_checkpoint()?;

    let shared = Arc::new(ProjectorShared {
        name: name.clone(),
        db_path,
        entities: Arc::new(entities),
        position: AtomicU64::new(start.get()),
        shutdown: AtomicBool::new(false),
        replay: AtomicBool::new(false),
    });

    let task_shared = Arc::clone(&shared);
    let join = thread::Builder::new()
        .name(format!("projector-{name}"))
        .spawn(move || run(task_shared, unit, store, model))
        .with_context(|| format!("spawning projector `{name}`"))?;
    Ok((shared, join))
}

fn run(
    shared: Arc<ProjectorShared>,
    unit: Arc<ProjectorUnit>,
    store: WriteHandle,
    model: ReadModel,
) {
    if let Err(err) = run_inner(&shared, &unit, &store, model) {
        tracing::error!("projector `{}` stopped: {err:#}", shared.name);
    }
}

fn run_inner(
    shared: &ProjectorShared,
    unit: &ProjectorUnit,
    store: &WriteHandle,
    mut model: ReadModel,
) -> anyhow::Result<()> {
    let ModuleDef::Projector { sources, .. } = &unit.loaded.def else {
        anyhow::bail!("run called on a non-projector module");
    };
    let query = dispatch::to_query(sources)?;
    let by_id = by_id_map(&shared.entities);
    let frozen = &unit.loaded.module;

    let mut sub = store.subscribe(query.clone(), model.read_checkpoint()?);
    loop {
        if shared.replay.swap(false, Ordering::Relaxed) {
            model = rebuild(shared, unit, store, model)?;
            let resume = model.read_checkpoint()?;
            shared.position.store(resume.get(), Ordering::Relaxed);
            sub = store.subscribe(query.clone(), resume);
            continue;
        }

        let batch = sub
            .poll_batch()
            .map_err(|err| anyhow::anyhow!("reading events: {err}"))?;
        if !batch.is_empty() {
            apply_batch(&model, frozen, &by_id, &batch, sub.position())?;
            shared
                .position
                .store(sub.position().get(), Ordering::Relaxed);
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
            shared.position.store(watermark.get(), Ordering::Relaxed);
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

/// Project every event up to the current head into `model`, committing in batches
/// and advancing the checkpoint. Does not tail live events. Used by replay and by
/// tests; the live loop drives batches itself so it can also wait and shut down.
pub fn project_to_head(
    store: &WriteHandle,
    unit: &LoadedModule,
    model: &ReadModel,
) -> anyhow::Result<usize> {
    let ModuleDef::Projector {
        entities, sources, ..
    } = &unit.def
    else {
        anyhow::bail!("project_to_head called on a non-projector module");
    };
    let query = dispatch::to_query(sources)?;
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
        apply_batch(model, &unit.module, &by_id, &batch, sub.position())?;
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
) -> anyhow::Result<()> {
    let tx = model.begin()?;
    Module::with_temp_heap(|module| {
        let handle_fn = frozen
            .get_option("handle")?
            .ok_or_else(|| anyhow::anyhow!("projector has no handle() function"))?;
        for (_position, event) in batch {
            let (_envelope, data) = envelope::decode(event.data())
                .map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
            let value = alloc_event(&module, event.event_type(), &data);
            let reader = BatchReader { model, by_id };
            let ctx = ProjectorCtx { reader: &reader };
            let result = call_handler_with_projector_ctx(
                &module,
                thaw(&handle_fn, &module),
                &[value],
                MAX_TICKS,
                &ctx,
            )
            .map_err(|err| anyhow::anyhow!("handle() failed: {err}"))?;
            for op in parse_entity_ops(result)? {
                let entity = by_id.get(&op.entity_id).ok_or_else(|| {
                    anyhow::anyhow!("op references an entity the projector didn't declare")
                })?;
                model.apply_one(entity, op.kind)?;
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
/// (closing its connection before the swap) and returns the reopened one.
fn rebuild(
    shared: &ProjectorShared,
    unit: &ProjectorUnit,
    store: &WriteHandle,
    model: ReadModel,
) -> anyhow::Result<ReadModel> {
    let db_path = &shared.db_path;
    let rebuild_path = db_path.with_extension("rebuild.db");
    remove_db_files(&rebuild_path)?;

    let fresh = ReadModel::open(&rebuild_path, &shared.entities)?;
    let count = project_to_head(store, &unit.loaded, &fresh)?;
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
    tracing::info!("projector `{}` replayed {count} events", shared.name);
    Ok(reopened)
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
