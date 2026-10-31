//! Filling the derived tables, and following the log to keep them filled.
//!
//! [`crate::tables`] says what the tables are, [`crate::fold_store`] keeps them, and this
//! is what puts values in them. One sequential pass, mirroring `crate::projector`: each record
//! of a batch is applied to the values already there, and the values and the checkpoint
//! commit together.
//!
//! **What an arm means is heklang's `Tables::apply` and nothing else**, both following the
//! log and replaying it into a column a deploy left behind. Working out a column's new value
//! here would be a second implementation of the arm, and the arm's own interpreter is right
//! there.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use heklang::host::{Host, Query};
use heklang::value::Record;
use heklang::{Interpreter, Program, Tables, Unstored};

use crate::fold_sink::{ColumnWriter, Replay};
use crate::fold_store::{FoldStore, Recorded};
use crate::tables::TableDef;

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
        replay: None,
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

/// Bring every column that is behind the checkpoint up to it, with nothing else running.
///
/// A column added or refolded by a deploy records `built_to = 0`, which says it holds its
/// seed and nothing else. This is the pass that answers that, and it blocks rather than
/// interleaving: a projector rebuild does the same, and the alternative is a column
/// chasing a head that keeps moving. Obligation 3 is intact either way, because only the
/// columns that are behind are written, not the whole store.
///
/// **A replay of the log, not a fold per key.** The keys a column holds are the ones its
/// arms named, and a column that is behind may name keys no row has yet: a fold added in
/// a table of its own has no rows at all. Refolding the rows already there filled nothing
/// for it, marked it built, and served it empty. So the behind columns go back to their
/// seed and the log is applied to them again through the same `Tables::apply` that
/// follows it, which is also what keeps a range arm to the keys that existed at its
/// record (see [`Replay::minted`]).
///
/// **The target is the store's own checkpoint and not a head the caller chose.** Every
/// column of every table is at one log position, so a column brought to anywhere else
/// would hold values the one `_hekla_checkpoint` does not admit to.
pub fn backfill<H: Host>(
    store: &FoldStore,
    program: &Program,
    interpreter: &Interpreter<'_, H>,
    tables: &[TableDef],
) -> anyhow::Result<Filled> {
    let head = store.read_checkpoint()?;
    let behind = store.behind()?;
    if behind.is_empty() {
        return Ok(Filled::default());
    }
    let tx = store.begin()?;
    let mut replay = Replay::default();
    let mut folds = Vec::new();
    for row in &behind {
        let Some(table) = tables.iter().find(|table| table.name == row.table) else {
            continue;
        };
        let Some(column) = table.columns.iter().find(|c| c.name == row.column) else {
            continue;
        };
        store.reset_column(&tx, table, column)?;
        replay.only.insert((row.table.clone(), row.column.clone()));
        folds.push(column.fold.clone());
    }

    // Only the behind columns and what heklang says they need beside them: the columns that
    // mint the keys a range of theirs reaches, and the folds they look up. Those run into
    // the sink's overlay and never reach the store, so a column that is already right is
    // neither doubled nor reset.
    let derived = Tables::new(program);
    let rebuild = derived.only(&folds);
    let mut writer = ColumnWriter {
        store,
        tx: &tx,
        program,
        tables,
        written: 0,
        ensured: BTreeSet::new(),
        replay: Some(replay),
    };
    let mut records = 0;
    if head > 0 {
        let query = Query {
            slices: derived.query().slices,
            from: 0,
            upto: Some(head - 1),
        };
        interpreter
            .host()
            .read(&query, &mut |record| {
                records += 1;
                rebuild.apply(record, &mut writer)
            })
            .map_err(|err| anyhow::anyhow!("replaying the log into the derived tables: {err}"))?;
    }
    let filled = Filled {
        columns: writer.written,
        records,
    };
    // Only what was reset and replayed: a behind row naming a column no table has was
    // never folded, so it is not built either.
    let rebuilt = writer
        .replay
        .take()
        .map(|replay| replay.only)
        .unwrap_or_default();

    for (table, column) in &rebuilt {
        store.mark_built(&tx, table, column, head)?;
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
    /// The named folds `layout` keeps no column for, and which parameters no arm matches,
    /// so a by-name read can say why without laying the program out again.
    unstored: Vec<Unstored>,
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

    /// The parameters no arm of `fold` matches, if it is a named fold `layout` keeps no
    /// column for.
    pub fn unstored(&self, fold: &str) -> Option<&[String]> {
        self.unstored
            .iter()
            .find(|unstored| unstored.fold == fold)
            .map(|unstored| unstored.unmatched.as_slice())
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
            FoldShared::idle(&program),
            FoldThread {
                shared: FoldShared::idle(&program),
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
        unstored: heklang::layout(&program).unstored,
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
    // empty here: a fold has no clock, no network and no `secret`, and the sink never reaches
    // `Keys` either, since it hands a sealed value through rather than shredding it.
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
    let filled = backfill(fold_store, program, &interpreter, tables)?;
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
    /// The tables are what the program declares, with no file behind them, so a read of one is
    /// told this node does not serve it rather than that it does not exist.
    pub fn idle(program: &Program) -> Arc<FoldShared> {
        Arc::new(FoldShared {
            tables: TableDef::all(program),
            unstored: heklang::layout(program).unstored,
            head: AtomicU64::new(0),
            behind: AtomicU64::new(0),
            filling_to: AtomicU64::new(0),
            shutdown: AtomicBool::new(true),
            running: AtomicBool::new(false),
            last_error: Mutex::new(None),
            path: None,
        })
    }
}
