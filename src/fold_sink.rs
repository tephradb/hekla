//! `heklang::host::Columns` over SQLite: where a record's arms land.
//!
//! This is to [`crate::fold_store`] what [`crate::heklang_host::RowWriter`] is to
//! [`crate::read_model`]. heklang's `Tables::apply` decides which columns a record names,
//! at which keys, and what each one accumulates to; everything here is storage.
//!
//! **Nothing in this file knows what an arm means.** That was the point of the seam: the
//! store used to re-fold a whole column over the log for every key a record touched, and
//! the only way to do better was to apply the record to the value already there, which is
//! interpreting heklang. So the interpreter does it and this holds the cells.

use std::collections::{BTreeMap, BTreeSet};
use std::mem;

use heklang::host::Columns;
use heklang::interp::{Error, ErrorKind, key_as_value};
use heklang::ir::Type;
use heklang::value::{Defs, Value};
use heklang::{Key, Program};
use rusqlite::Transaction;
use rusqlite::types::{Value as SqlValue, ValueRef};

use crate::fold_store::{FoldStore, SealedCell, cell_of, stored_text};
use crate::heklang_host::{to_heklang_json, wire_form};
use crate::read_model::from_sql;
use crate::tables::{ColumnDef, TableDef, holding, main_of};

/// One batch's writes into the derived tables.
///
/// Holds the transaction, so every column a batch touches commits with the checkpoint or
/// not at all. `Columns::value` owes the value *including* writes this record has already
/// made, which is what lets two arms of one event accumulate onto each other, so it reads a
/// pending cell before the store.
pub struct ColumnWriter<'a> {
    pub store: &'a FoldStore,
    pub tx: &'a Transaction<'a>,
    pub program: &'a Program,
    pub tables: &'a [TableDef],
    /// Columns written, which is what a log line and a metric report.
    pub written: usize,
    /// Main-table keys this batch has already made sure of, so a list pushed onto ten
    /// thousand times in one batch costs one insert for its key and not ten thousand.
    pub ensured: BTreeSet<(String, Vec<String>)>,
    /// Set while a backfill replays the log, and `None` while following it.
    pub replay: Option<Replay>,
    /// Cells set and not yet written, per table and key, each column as the cell the store
    /// will hold. [`ColumnWriter::flush`] writes each row once; until then
    /// `Columns::value` reads a cell from here before the store, and `Columns::keys` counts
    /// a pending row's key as one the table has.
    pub pending: BTreeMap<(&'a str, Vec<String>), Pending<'a>>,
}

/// How many rows a writer holds before it writes them, which bounds a backfill's memory:
/// it replays the whole log in one writer, and a flush mid-way reads back the same.
const FLUSH_ROWS: usize = 10_000;

/// One row's cells a batch has set, and the table they are for.
pub struct Pending<'a> {
    table: &'a TableDef,
    cells: BTreeMap<&'a str, SqlValue>,
}

/// What a backfill replays the log under.
///
/// The replay runs `Tables::only` over the columns being rebuilt, which also runs their
/// closure: the columns of their tables that mint keys, since a range of theirs reaches
/// only keys something has minted, and the folds they look up, since a lookup reads one as
/// of each record. Only the columns being rebuilt reach the store. The closure's own values
/// are kept in [`Replay::overlay`] for the length of the replay and thrown away after it:
/// they are already right at the checkpoint, and writing a replay into them would fold
/// every record into them a second time.
///
/// "As followed live" here means followed from the start of the log with the program as
/// it is now, not with the deploys it actually went through: a key a fold since removed
/// had minted is not one the replay ranges over.
#[derive(Default)]
pub struct Replay {
    /// The columns being rebuilt, as the holding table and the column name.
    pub only: BTreeSet<(String, String)>,
    /// Every key some arm has written so far in the replay, per main table.
    ///
    /// **A range arm sees these, not the store's rows.** Followed live, a range at position
    /// `p` reaches only the keys minted before `p`; the store at the checkpoint also holds
    /// every key minted after it, and ranging over those would apply a record to a key that
    /// did not exist yet when it was appended.
    pub minted: BTreeMap<String, BTreeSet<Vec<String>>>,
    /// The closure's values as of the record being replayed, per holding table and column,
    /// then key. A lookup reads these, which is why they cannot be the final values the
    /// store holds. Memory grows with the keys the closure writes over the whole log.
    pub overlay: BTreeMap<(String, String), BTreeMap<Vec<String>, Value>>,
}

impl<'a> ColumnWriter<'a> {
    /// Write every pending row, one statement each, and empty the buffer. Owed before the
    /// transaction commits: a cell still pending is a write the store never saw.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        for ((_, key), row) in mem::take(&mut self.pending) {
            self.store.write_row(self.tx, row.table, &key, &row.cells)?;
        }
        Ok(())
    }

    /// The table that holds `column` of the layout table `source`, and the column itself.
    ///
    /// `Tables::apply` names a layout table, and a collection-typed column of it lives in
    /// the side table, so neither name alone answers where a cell is.
    fn cell(&self, source: &str, column: &str) -> Result<(&'a TableDef, &'a ColumnDef), Error> {
        holding(self.tables, source, column).ok_or_else(|| {
            host_error(format!(
                "no derived column `{column}` of `{source}`, which the layout implies"
            ))
        })
    }

    fn defs(&self) -> Defs<'_> {
        Defs::of(self.program)
    }
}

impl<'a> Columns for ColumnWriter<'a> {
    fn value(&self, table: &str, key: &[Key], column: &str) -> Result<Option<Value>, Error> {
        let (def, held) = self.cell(table, column)?;
        let texts = key_texts(def, key)?;
        if let Some(replay) = &self.replay {
            let at = (def.name.clone(), held.name.clone());
            if !replay.only.contains(&at) {
                return Ok(replay
                    .overlay
                    .get(&at)
                    .and_then(|cells| cells.get(&texts))
                    .cloned());
            }
        }
        let at = (def.name.as_str(), texts);
        let pending = self
            .pending
            .get(&at)
            .and_then(|row| row.cells.get(held.name.as_str()));
        // A pending cell goes through the decoding a stored one does, so what an arm reads
        // back is the same whether or not this batch has flushed yet.
        let found = match pending {
            Some(cell) => Some(
                from_sql(&held.meta, ValueRef::from(cell))
                    .map_err(|why| host_error(format!("{why:#}")))?,
            ),
            None => self
                .store
                .cell(self.tx, def, &at.1, &held.name)
                .map_err(|why| host_error(format!("{why:#}")))?,
        };
        let Some(stored) = found else {
            return Ok(None);
        };
        // A sealed cell goes back to the arm as the `Value::Sealed` it was written from,
        // unopened, which is what the sink handed the store: an arm never sees
        // sealed content as plaintext, so there is nothing to decrypt for.
        if held.meta.sealed_under.is_some() {
            // `NULL` is a cell no arm of this fold has written, since an erased subject
            // still stores its seal, so the seed is the answer.
            let Some(text) = stored.as_str() else {
                return Ok(None);
            };
            let sealed = SealedCell::parse(text)
                .map_err(|why| host_error(format!("{why:#}")))?
                .value();
            return Ok(Some(match &held.ty {
                Type::Opt(inner) => Value::Opt {
                    inner: inner.as_ref().clone(),
                    value: Some(Box::new(sealed)),
                },
                _ => sealed,
            }));
        }
        // The same route a projector's row read takes: the stored form back to the wire
        // form, then to the declared type. `column_form` and `wire_form` differ only on a
        // top-level `Timestamp`, and that is exactly the column this would get wrong.
        let json = to_heklang_json(&wire_form(&held.meta.kind, stored));
        Value::from_json(&json, &held.ty, self.defs())
            .map(Some)
            .map_err(|why| Error::new(ErrorKind::Mismatch(why)))
    }

    fn set(&mut self, table: &str, key: Vec<Key>, column: &str, value: Value) -> Result<(), Error> {
        let (def, held) = self.cell(table, column)?;
        let texts = key_texts(def, &key)?;
        if let Some(replay) = &mut self.replay {
            replay
                .minted
                .entry(def.source.clone())
                .or_default()
                .insert(texts.clone());
            let at = (def.name.clone(), held.name.clone());
            if !replay.only.contains(&at) {
                replay.overlay.entry(at).or_default().insert(texts, value);
                return Ok(());
            }
        }
        // **A key exists in its main table whichever of the two a write lands in.** Every
        // reader of "the keys this signature has" reads the main table (a range arm, a
        // backfill, a scan), so a key whose only writes so far are collections would
        // otherwise be one none of them could see.
        if def.name != def.source && self.ensured.insert((def.source.clone(), texts.clone())) {
            let main = main_of(self.tables, &def.source)
                .ok_or_else(|| host_error(format!("no derived table `{}`", def.source)))?;
            self.store
                .ensure_row(self.tx, main, &texts)
                .map_err(|why| host_error(format!("{why:#}")))?;
        }
        let cell =
            cell_of(&def.name, held, &value).map_err(|why| host_error(format!("{why:#}")))?;
        self.pending
            .entry((def.name.as_str(), texts))
            .or_insert_with(|| Pending {
                table: def,
                cells: BTreeMap::new(),
            })
            .cells
            .insert(held.name.as_str(), cell);
        self.written += 1;
        if self.pending.len() >= FLUSH_ROWS {
            self.flush().map_err(|why| host_error(format!("{why:#}")))?;
        }
        Ok(())
    }

    fn keys(&self, table: &str, pinned: &[(usize, Key)]) -> Result<Vec<Vec<Key>>, Error> {
        // Read off the **main** table of the key signature: a side table carries only the
        // keys its own columns have been written for, so ranging over one would miss a key
        // the scalars already have.
        let over = main_of(self.tables, table)
            .ok_or_else(|| host_error(format!("no derived table for `{table}`")))?;
        let mut parts: Vec<Option<String>> = vec![None; over.key.len()];
        for (at, key) in pinned {
            let meta = over
                .key
                .get(*at)
                .map(|(_, meta)| meta)
                .ok_or_else(|| host_error(format!("`{table}` has no key part {at}")))?;
            parts[*at] = stored_text(&meta.kind, &key_as_value(key));
        }
        let rows: Vec<Vec<String>> = match &self.replay {
            Some(replay) => replay
                .minted
                .get(&over.name)
                .map(|minted| minted_matching(minted, &parts))
                .unwrap_or_default(),
            None => {
                let mut rows: BTreeSet<Vec<String>> = self
                    .store
                    .keys_pinned(self.tx, over, &parts)
                    .map_err(|why| host_error(format!("{why:#}")))?
                    .into_iter()
                    .collect();
                // A key this batch minted and has not written yet is still one the table
                // has. Both sides spell a key through `stored_text`, so one key is one text.
                let minted: BTreeSet<Vec<String>> = self
                    .pending
                    .keys()
                    .filter(|(held, _)| *held == over.name)
                    .map(|(_, key)| key.clone())
                    .collect();
                rows.extend(minted_matching(&minted, &parts));
                rows.into_iter().collect()
            }
        };
        rows.iter()
            .map(|row| {
                row.iter()
                    .zip(&over.key)
                    .map(|(text, (name, meta))| {
                        let value = crate::tables::key_value(&meta.kind, text)
                            .map_err(|why| host_error(format!("{why:#}")))?;
                        Key::from_value(&value).ok_or_else(|| {
                            host_error(format!("`{name}` of `{table}` is not a key value"))
                        })
                    })
                    .collect()
            })
            .collect()
    }
}

/// The minted keys carrying every pinned part.
///
/// A range arm usually pins a leading run of the key, and the set is ordered by key, so
/// that run bounds a seek; only a range pinning a later part alone scans the table's keys.
fn minted_matching(minted: &BTreeSet<Vec<String>>, parts: &[Option<String>]) -> Vec<Vec<String>> {
    let prefix: Vec<String> = parts.iter().map_while(Clone::clone).collect();
    let matches = |row: &&Vec<String>| {
        parts
            .iter()
            .zip(row.iter())
            .all(|(part, text)| part.as_ref().is_none_or(|part| part == text))
    };
    if prefix.is_empty() {
        return minted.iter().filter(matches).cloned().collect();
    }
    minted
        .range(prefix.clone()..)
        .take_while(|row| row.starts_with(&prefix))
        .filter(matches)
        .cloned()
        .collect()
}

/// One key as the texts the store binds, in the table's key order.
fn key_texts(table: &TableDef, key: &[Key]) -> Result<Vec<String>, Error> {
    if key.len() != table.key.len() {
        return Err(host_error(format!(
            "`{}` is keyed on {} column(s) and this key has {}",
            table.name,
            table.key.len(),
            key.len()
        )));
    }
    Ok(table
        .key
        .iter()
        .zip(key)
        .map(|((_, meta), part)| stored_text(&meta.kind, &key_as_value(part)).unwrap_or_default())
        .collect())
}

fn host_error(message: String) -> Error {
    Error::new(ErrorKind::Host(message))
}

/// `anyhow` into heklang's host error, for a caller outside this file.
pub fn host_failure(why: &anyhow::Error) -> Error {
    host_error(format!("{why:#}"))
}
