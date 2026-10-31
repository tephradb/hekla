//! Filling the derived tables, and following the log to keep them filled.
//!
//! [`crate::tables`] says what the tables are, [`crate::fold_store`] keeps them, and this
//! is what puts values in them. One sequential pass, mirroring `crate::projector`: a batch
//! of records names the columns it could have changed, each of those is folded at the
//! batch's own head, and the values and the checkpoint commit together.
//!
//! **The read is `Interpreter::fold_stored` and nothing else.** Working out a column's new
//! value from the record in hand would be a second implementation of what an arm means,
//! and the arm's own interpreter is right here. The cost is that a column is folded over
//! the log rather than updated in place, which is why this is the correct-and-slow shape
//! the per-record sink is measured against rather than the end state.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use heklang::value::{Record, Value};
use heklang::{Interpreter, Program, Tables, host::Host};

use crate::fold_sink::ColumnWriter;
use crate::fold_store::{FoldStore, Recorded};
use crate::tables::{ColumnDef, TableDef, main_of};

/// What one pass did, for a log line and a test to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filled {
    /// Cell writes, which is one per arm the batch's records named per key it named it
    /// for.
    ///
    /// Not distinct columns: a record updates the value already there, so two events of
    /// one batch touching one column are two writes. That is the whole difference between
    /// this and re-folding a column over the log once per key.
    pub columns: usize,
    /// Records the batch carried, whether or not any arm matched one.
    pub records: usize,
}

/// Apply one batch of records, then move the checkpoint, in one transaction.
///
/// `head` is how long the log was, which is the unit the store records. **The records are
/// applied in position order and each one to the value already there**, which is what
/// makes a batch cost one pass over itself rather than one fold over the log per key it
/// touches. `Tables::apply` owns everything about which columns a record names, at which
/// keys, and what each accumulates to; this owns the transaction and the checkpoint.
pub fn apply_batch(
    store: &FoldStore,
    program: &Program,
    tables: &[TableDef],
    records: &[Record],
    head: u64,
) -> anyhow::Result<Filled> {
    // Read before anything moves: a column already at this position is one nothing has
    // left behind, and that is what decides whether it may advance below.
    let was = store.read_checkpoint()?;
    let recorded: BTreeMap<(String, String), Recorded> = store
        .recorded()?
        .into_iter()
        .map(|row| ((row.table.clone(), row.column.clone()), row))
        .collect();

    let tx = store.begin()?;
    let derived = Tables::new(program);
    let mut writer = ColumnWriter {
        store,
        tx: &tx,
        program,
        tables,
        written: 0,
        ensured: BTreeSet::new(),
    };
    for record in records {
        derived
            .apply(record, &mut writer)
            .with_context(|| format!("applying the record at {}", record.position))?;
    }
    let filled = Filled {
        columns: writer.written,
        records: records.len(),
    };

    // **Only a column that was already caught up may advance.** A column a deploy added or
    // refolded carries `built_to = 0` precisely so `behind` finds it, and a column holds
    // the fold's value for *every* key, so writing the handful of keys this batch touched
    // does not make a behind column correct. Advancing it here would make `behind` empty,
    // leave the backfill nothing to do, and strand the column on its seed for every key
    // that existed before the deploy.
    for table in tables {
        for column in &table.columns {
            let key = (table.name.clone(), column.name.clone());
            if recorded.get(&key).is_some_and(|row| row.built_to >= was) {
                store.mark_built(&tx, &table.name, &column.name, head)?;
            }
        }
    }
    store.write_checkpoint(head, &tx)?;
    tx.commit()?;
    Ok(filled)
}

/// Fold one column for one key, at the batch's head.
///
/// The arguments are named the way the **fold** declares them, which is usually not the
/// way the table spells the key part: `ColumnDef::args` is that mapping, derived once from
/// the same `part_names` the layout used.
fn fold_one<H: Host>(
    interpreter: &Interpreter<'_, H>,
    table: &TableDef,
    column: &ColumnDef,
    key: &[String],
    head: u64,
) -> anyhow::Result<Value> {
    let typed: Vec<(String, Value)> = column
        .args
        .iter()
        .zip(key)
        .zip(&table.key)
        .map(|((name, text), (_, meta))| {
            crate::tables::key_value(&meta.kind, text).map(|value| (name.clone(), value))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    interpreter
        .fold_stored(&column.fold, typed, head)
        .with_context(|| format!("folding `{}`", column.fold))
}

/// Bring every column that is behind the checkpoint up to it, with nothing else running.
///
/// A column added or refolded by a deploy records `built_to = 0`, which says it holds its
/// seed and nothing else. This is the pass that answers that, and it blocks rather than
/// interleaving: a projector rebuild does the same, and the alternative is a column
/// chasing a head that keeps moving. Obligation 3 is intact either way, because only the
/// columns that are behind are folded, not the whole store.
///
/// **The target is the store's own checkpoint and not a head the caller chose.** Every
/// column of every table is at one log position, so a column brought to anywhere else
/// would hold values the one `_hekla_checkpoint` does not admit to.
pub fn backfill<H: Host>(
    store: &FoldStore,
    interpreter: &Interpreter<'_, H>,
    tables: &[TableDef],
) -> anyhow::Result<Filled> {
    let head = store.read_checkpoint()?;
    let behind = store.behind()?;
    if behind.is_empty() {
        return Ok(Filled::default());
    }
    let tx = store.begin()?;
    let mut filled = Filled::default();
    for row in &behind {
        let Some(table) = tables.iter().find(|table| table.name == row.table) else {
            continue;
        };
        let Some(column) = table.columns.iter().find(|c| c.name == row.column) else {
            continue;
        };
        // Every key the key signature holds, read off its **main** table: a side table
        // carries only the keys its own columns have been written for, so ranging over one
        // would leave a collection column added to a populated deployment with no key to
        // fold for at all. A column being backfilled cannot mint a key either, because a
        // key exists only because some event matched it.
        let over = main_of(tables, &table.source).unwrap_or(table);
        let every = vec![None; over.key.len()];
        for key in store.keys_pinned(&tx, over, &every)? {
            let value = fold_one(interpreter, table, column, &key, head)?;
            store.write(&tx, table, &key, column, &value)?;
            filled.columns += 1;
        }
        store.mark_built(&tx, &row.table, &row.column, head)?;
    }
    tx.commit()?;
    Ok(filled)
}

// ---------------------------------------------------------------------------
// Following the log
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use heklang::Digest;
use tephra::Position;

use crate::context::CommandContext;
use crate::crypto::KeyStore;
use crate::heklang_host::{HeklaHost, Stamp};
use crate::schema::EventDefs;
use crate::store::Store;

/// How long a caught-up store blocks before re-checking its shutdown flag.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// The derived store's file inside the data directory.
pub const FILE_NAME: &str = "tables.db";

/// What the HTTP surface and `/status` can see of the store without touching SQLite.
///
/// One of these for the whole store rather than one per table, which is the same reason
/// there is one checkpoint: every column is at one position, so there is one thing to say.
pub struct FoldShared {
    /// The log length every caught-up column has been folded to.
    head: AtomicU64,
    /// Columns that are not at it, which is what a read has to refuse to serve.
    behind: AtomicU64,
    /// The head a store that started empty has to reach before it serves. A store folded
    /// from nothing over a log that already has events holds none of them yet, and every
    /// read would answer short: a 404 for a key that exists, a page with rows missing.
    /// Zero once it has, and for a store that started where it left off.
    filling_to: AtomicU64,
    shutdown: AtomicBool,
    running: AtomicBool,
    last_error: Mutex<Option<String>>,
    /// What a reader may ask for, and where it lives. Empty and `None` for a store nothing
    /// keeps, which is a project with no storable fold or a read-only follower.
    tables: Vec<TableDef>,
    path: Option<PathBuf>,
}

impl FoldShared {
    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// How many columns hold their seed rather than their value. Zero is servable.
    pub fn behind(&self) -> u64 {
        self.behind.load(Ordering::Acquire)
    }

    /// Whether every column is folded far enough to serve: none behind, and not still
    /// filling from an empty start.
    pub fn servable(&self) -> bool {
        self.behind() == 0 && self.head() >= self.filling_to.load(Ordering::Acquire)
    }

    pub fn tables(&self) -> &[TableDef] {
        &self.tables
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn fail(&self, why: &anyhow::Error) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(format!("{why:#}"));
    }
}

/// The store's thread, held so a shutdown can drain it.
pub struct FoldThread {
    shared: Arc<FoldShared>,
    join: Option<JoinHandle<()>>,
}

impl FoldThread {
    /// Signal the thread to stop, then join it.
    ///
    /// Called while the write coordinator is still live, as a projector's is, so the
    /// thread drains to head and commits its final batch before exiting.
    pub fn shutdown_and_join(mut self) {
        self.shared.stop();
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            tracing::error!("the derived-table thread panicked");
        }
    }
}

/// Open the derived store, bring its schema to what the program says, and follow the log.
///
/// The schema reconcile happens here rather than at the first batch because a column a
/// deploy added has to exist before anything can fold it, and the backfill that follows is
/// what turns its seed into its value.
pub fn start(
    store: &Store,
    data_dir: &Path,
    program: Arc<Program>,
    events: Arc<EventDefs>,
    keystore: Option<Arc<KeyStore>>,
    digest: &Digest,
) -> anyhow::Result<(Arc<FoldShared>, FoldThread)> {
    let tables = TableDef::all(&program);
    // **No tables, no store and no thread.** A project whose folds are all unstorable, or
    // that declares none, has nothing to keep: creating the file anyway would put a
    // database in every data directory that nothing ever reads, and leave a thread writing
    // a checkpoint for it.
    if tables.is_empty() {
        return Ok((
            FoldShared::idle(Vec::new()),
            FoldThread {
                shared: FoldShared::idle(Vec::new()),
                join: None,
            },
        ));
    }
    let path = data_dir.join(FILE_NAME);
    let fold_store = FoldStore::open(&path, &tables)?;
    let changes = fold_store.apply(digest, &tables)?;
    for change in changes.iter().filter(|change| change.verdict.refills()) {
        tracing::info!(
            table = %change.table,
            column = %change.column,
            fold = %change.fold,
            verdict = change.verdict.label(),
            "derived column needs folding"
        );
    }

    let shared = Arc::new(FoldShared {
        head: AtomicU64::new(fold_store.read_checkpoint()?),
        behind: AtomicU64::new(fold_store.behind()?.len() as u64),
        filling_to: AtomicU64::new(if fold_store.read_checkpoint()? == 0 {
            store.head().get()
        } else {
            0
        }),
        shutdown: AtomicBool::new(false),
        running: AtomicBool::new(true),
        last_error: Mutex::new(None),
        tables: tables.clone(),
        path: Some(path.clone()),
    });

    let store = store.clone();
    let handle = Arc::clone(&shared);
    let join = thread::Builder::new()
        .name("derived-tables".to_owned())
        .spawn(move || {
            let outcome = follow(
                &handle,
                &store,
                &fold_store,
                &program,
                &events,
                keystore,
                &tables,
            );
            handle.running.store(false, Ordering::Relaxed);
            if let Err(why) = outcome {
                tracing::error!("the derived tables stopped: {why:#}");
                handle.fail(&why);
            }
        })
        .map_err(|err| anyhow::anyhow!("spawning the derived-table thread: {err}"))?;

    Ok((
        Arc::clone(&shared),
        FoldThread {
            shared,
            join: Some(join),
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn follow(
    shared: &FoldShared,
    store: &Store,
    fold_store: &FoldStore,
    program: &Arc<Program>,
    events: &Arc<EventDefs>,
    keystore: Option<Arc<KeyStore>>,
    tables: &[TableDef],
) -> anyhow::Result<()> {
    let derived = Tables::new(program.as_ref());
    // The host reads and never appends, which is why every field an append accumulates is
    // empty here: a fold has no clock, no network and no `secret`, and `fold_stored` never
    // reaches `Keys` either, since it is the one read that does not shred.
    let host = HeklaHost {
        program: Arc::clone(program),
        events: Arc::clone(events),
        store: store.clone(),
        keystore,
        ctx: CommandContext::new(uuid::Uuid::nil()),
        stamp: Stamp::Pinned,
        idem_tag: None,
        call: None,
        appended: None,
        emitted: Vec::new(),
        unavailable: None,
        duplicated: false,
        retry_after: None,
        last_transport: None,
        sealed: false,
        http: None,
        secrets: None,
    };
    // **The subscription comes from `Tables::query`**, lowered through the same `query_of`
    // a command's append condition uses. Spelling the event types here instead is how the
    // first version of this thread followed the log, folded none of it, and reported
    // itself caught up.
    let query = host
        .query_of(&derived.query().slices)
        .map_err(|err| anyhow::anyhow!("building the derived-table subscription: {err}"))?;
    let interpreter = Interpreter::with_host(program.as_ref(), host);

    // A column a deploy left behind holds its seed, so this is what has to happen before
    // the store serves anything. It blocks, as a projector's rebuild does.
    let filled = backfill(fold_store, &interpreter, tables)?;
    if filled.columns > 0 {
        tracing::info!(columns = filled.columns, "backfilled derived columns");
    }
    shared
        .behind
        .store(fold_store.behind()?.len() as u64, Ordering::Release);

    // A heklang head and a tephra position are the same number: heklang counts positions
    // from zero and tephra from one, so a log of N events is head N and position N.
    let mut sub = store.subscribe(
        "the derived tables",
        query,
        Position::new(fold_store.read_checkpoint()?),
    )?;
    loop {
        let batch = sub
            .poll_batch()
            .map_err(|err| anyhow::anyhow!("reading events: {err}"))?;
        let head = sub.position().get();
        if !batch.is_empty() {
            let records: Vec<Record> = batch
                .iter()
                .map(|(position, event)| {
                    crate::heklang_host::record_of(program, *position, event.as_ref())
                        .map_err(|err| anyhow::anyhow!("decoding a record: {err}"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let filled = apply_batch(fold_store, program, tables, &records, head)?;
            crate::metrics::fold_batch(filled.columns);
            shared.head.store(head, Ordering::Release);
            continue;
        }

        // Nothing matched, so every caught-up column is still correct at the watermark and
        // only the position has to move. Driven through the same pass rather than a
        // checkpoint write of its own, so the rule about which columns may advance is
        // stated once.
        if head > shared.head() {
            apply_batch(fold_store, program, tables, &[], head)?;
            shared.head.store(head, Ordering::Release);
        }
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let _ = sub.wait_timeout(IDLE_POLL);
    }
    Ok(())
}

impl FoldShared {
    /// A store nothing is following, for a read-only follower.
    ///
    /// `runtime::follow` opens a data directory without taking its lock and without
    /// starting a writer's threads, so there is no derived store to report on. Behind by
    /// nothing rather than behind by zero columns: a follower serves no derived read at
    /// all, and saying "caught up" would be a claim about a store it never opened.
    ///
    /// `tables` is what the program declares, with no file behind it, so a read of one is
    /// told this node does not serve it rather than that it does not exist.
    pub fn idle(tables: Vec<TableDef>) -> Arc<FoldShared> {
        Arc::new(FoldShared {
            head: AtomicU64::new(0),
            behind: AtomicU64::new(0),
            filling_to: AtomicU64::new(0),
            shutdown: AtomicBool::new(true),
            running: AtomicBool::new(false),
            last_error: Mutex::new(None),
            tables,
            path: None,
        })
    }
}
