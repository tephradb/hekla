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
use heklang::{Interpreter, Layout, Program, host::Host};

use crate::fold_store::{FoldStore, Recorded};
use crate::tables::{ColumnDef, Pins, TableDef, holding, main_of, pinned};

/// What one pass did, for a log line and a test to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filled {
    /// Columns written, counting one per (key, column) pair.
    pub columns: usize,
    /// Records the batch carried, whether or not any arm matched one.
    pub records: usize,
}

/// One column of one row to fold: where it lives, which key, and which fold.
type Target = (String, Vec<String>, String);

/// Apply one batch of records, then move the checkpoint, in one transaction.
///
/// `head` is how long the log was, which is the unit `Interpreter::fold_stored` takes and
/// the only number the seam hands out. Every column is folded at that one head, because
/// *every column of every table is at one log position* is the property a query reads:
/// `docs/host.md` is explicit that `head` and `read` are not atomic together, so a bound
/// taken per column would leave a row half updated.
pub fn apply_batch<H: Host>(
    store: &FoldStore,
    interpreter: &Interpreter<'_, H>,
    program: &Program,
    layout: &Layout,
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
    let mut filled = Filled {
        columns: 0,
        records: records.len(),
    };

    // **Fully pinned arms first, and ranges second.** An arm that leaves a key part open
    // is a range update over every key that carries the parts it did pin, and a key this
    // same batch has just minted is one of them. Reading the range before the pinned
    // writes would miss it, and nothing afterwards would correct it: the column would be
    // marked built at this head holding the seed its `DEFAULT` gave it. Both halves run
    // inside the transaction and on one connection, so the range sees the writes above it.
    let mut pins: Vec<Pins> = Vec::new();
    for record in records {
        pins.extend(pinned(program, layout, record));
    }

    let mut done: BTreeSet<Target> = BTreeSet::new();
    for complete in [true, false] {
        for pin in pins.iter().filter(|pin| pin.complete() == complete) {
            let Some((table, column)) = holding(tables, &pin.table, &pin.column) else {
                continue;
            };
            // A side table holds only the keys its column has been written for, so an
            // open range is read against the main table of the key signature.
            let over = main_of(tables, &pin.table).unwrap_or(table);
            for key in store.keys_matching(over, pin)? {
                let target = (table.name.clone(), key, column.name.clone());
                if !done.insert(target.clone()) {
                    continue;
                }
                let value = fold_one(interpreter, table, column, &target.1, head)?;
                store.write(&tx, table, &target.1, column, &value)?;
                filled.columns += 1;
            }
        }
    }

    // **Only a column that was already caught up may advance.** A column a deploy added or
    // refolded carries `built_to = 0` precisely so `behind` finds it, and a column holds
    // the fold's value for *every* key, so writing the handful of keys this batch touched
    // does not make a behind column correct. Advancing it here would make `behind` empty,
    // leave the backfill nothing to do, and strand the column on its seed for every key
    // that existed before the deploy.
    for table in tables {
        for column in &table.columns {
            let key = (table.name.clone(), column.name.clone());
            let caught_up = recorded.get(&key).is_some_and(|row| row.built_to >= was);
            if caught_up {
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
        for key in store.keys_matching(over, &every_key(over, row))? {
            let value = fold_one(interpreter, table, column, &key, head)?;
            store.write(&tx, table, &key, column, &value)?;
            filled.columns += 1;
        }
        store.mark_built(&tx, &row.table, &row.column, head)?;
    }
    tx.commit()?;
    Ok(filled)
}

/// Every part open, which reads as every key the table holds.
fn every_key(table: &TableDef, row: &Recorded) -> Pins {
    Pins {
        table: table.source.clone(),
        column: row.column.clone(),
        fold: row.fold.clone(),
        parts: vec![None; table.key.len()],
    }
}
