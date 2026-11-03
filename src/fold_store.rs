//! The derived read model at rest: one SQLite file, one checkpoint, one catalog row per
//! column.
//!
//! [`crate::tables`] says what the tables are; this keeps them. It is the sibling of
//! [`crate::read_model`] and deliberately not part of `hekla.db`: the operational
//! database is chartered for bookkeeping that is not domain truth, it is backed up and
//! its positions are clamped on restore, and it migrates in place. A derived store is the
//! opposite of all three. It is rebuildable by construction, so a backup leaves it out
//! the way it leaves out `projectors/`, and it is versioned by what each column's fold
//! does rather than migrated.
//!
//! **Two invariants carry the design.**
//!
//! *Every column of every table is at one log position*, which is why there is one
//! `_hekla_checkpoint` for the whole file rather than one per table. A query then reads
//! one position and a row is never half updated. Projectors do not have this: each holds
//! its own checkpoint and `/status` reports them as a list.
//!
//! *Maintenance is per fold.* A column is keyed by its fold's digest entry hash, so
//! changing one fold rebuilds one column rather than a whole database. That is the
//! headline win over a projector and the reason the catalog exists at all.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use heklang::value::Value;
use heklang::{Digest, Json, Kind};
use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};

use crate::heklang_host::{from_heklang_json, peeled, stored_form};
use crate::read_model::quote_ident;
use crate::schema::{FieldKind, FieldMeta};
use crate::tables::{ColumnDef, TableDef};

/// The checkpoint is one row because the whole store is at one position. The catalog is
/// one row per column because a column is what a fold change rebuilds.
const INTERNAL_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _hekla_checkpoint (
    id       INTEGER PRIMARY KEY CHECK (id = 0),
    position INTEGER NOT NULL
);
INSERT OR IGNORE INTO _hekla_checkpoint (id, position) VALUES (0, 0);
CREATE TABLE IF NOT EXISTS _hekla_column (
    \"table\"        TEXT    NOT NULL,
    column_name     TEXT    NOT NULL,
    fold            TEXT    NOT NULL,
    fold_hash       TEXT    NOT NULL,
    signature_hash  TEXT    NOT NULL,
    seed            TEXT,
    built_to        INTEGER NOT NULL,
    PRIMARY KEY (\"table\", column_name)
);";

/// One column, as the store records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub table: String,
    pub column: String,
    pub fold: String,
    /// The fold's digest entry hash: what it *does*. This is the column's identity, and
    /// it is a hash out of [`heklang::Digest`] rather than one hekla computes, because
    /// that is the one source of every hash hekla records.
    pub fold_hash: String,
    /// What of the fold is visible from outside it: its parameters and its return type,
    /// which is to say its table, its key order and its own type.
    pub signature_hash: String,
    /// The fold's seed as the SQL literal the column's `DEFAULT` carries.
    ///
    /// Recorded because **SQLite cannot alter a default**, and the seed does not live in
    /// the signature: it is in the fold's stage, so changing `-> Bool = false` to
    /// `-> Bool = true` moves `hash` and not `signature_hash`. Without this the verdict
    /// would be a refold, the column would stay, and every row created afterwards for a
    /// key no arm fires for would hold the old seed, which is the store and the fold
    /// disagreeing about what the column means.
    pub seed: Option<String>,
    /// The log position this column has been folded to. Zero for a column that exists
    /// and holds nothing but its seed.
    pub built_to: u64,
}

/// What a deploy would do to one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Its fold does the same thing, so its values still stand.
    UpToDate,
    /// Not recorded at all.
    Add,
    /// **Same column, different behaviour.** Its table, key order and type did not move,
    /// so the column stays where it is and its values are refolded.
    Refold,
    /// Its signature moved, so it is a column of another shape under the same name:
    /// dropped and added rather than refolded in place.
    Reshape,
    /// Recorded, and gone from the program.
    Drop,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::UpToDate => "up to date",
            Verdict::Add => "add",
            Verdict::Refold => "refold",
            Verdict::Reshape => "reshape",
            Verdict::Drop => "drop",
        }
    }

    /// Whether the column has to be folded again before it can be served.
    pub fn refills(self) -> bool {
        matches!(self, Verdict::Add | Verdict::Refold | Verdict::Reshape)
    }
}

/// One column that would not survive the deploy unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub table: String,
    pub column: String,
    pub fold: String,
    pub verdict: Verdict,
}

/// Every column the program implies, with the hashes that give it its identity.
///
/// `built_to` is zero throughout: this is what the program says, not what the store
/// holds.
pub fn catalog(digest: &Digest, tables: &[TableDef]) -> anyhow::Result<Vec<Recorded>> {
    let hashes: BTreeMap<&str, (String, String)> = digest
        .entries()
        .iter()
        .filter(|entry| entry.kind == Kind::Fold)
        .map(|entry| {
            (
                entry.name.as_str(),
                (
                    entry.hash.to_string(),
                    entry
                        .signature_hash
                        .map(|hash| hash.to_string())
                        .unwrap_or_default(),
                ),
            )
        })
        .collect();
    let mut out = Vec::new();
    for table in tables {
        for column in &table.columns {
            // Every named fold earns a digest entry, so a column without one cannot happen
            // for a program that checked. Loud rather than defaulted: two empty hashes
            // compare equal, so a defaulted column would read as up to date on every
            // deploy and never be rebuilt, which is the one failure nothing downstream
            // could notice.
            let (fold_hash, signature_hash) =
                hashes.get(column.fold.as_str()).cloned().with_context(|| {
                    format!(
                        "fold `{}` has no digest entry, so column `{}`.`{}` has no identity",
                        column.fold, table.name, column.name
                    )
                })?;
            out.push(Recorded {
                table: table.name.clone(),
                column: column.name.clone(),
                fold: column.fold.clone(),
                fold_hash,
                signature_hash,
                seed: column.default.clone(),
                built_to: 0,
            });
        }
    }
    Ok(out)
}

/// What a deploy would change, column by column.
///
/// Pure, so `hekla plan` can forecast a deploy without a second implementation of the
/// rule, exactly as `projector::reconcile_from` is reused by `plan::forecast`. Keyed on
/// `(table, column)`: a column that moves to another table is a drop and an add, which is
/// what it is.
pub fn reconcile(recorded: &[Recorded], current: &[Recorded]) -> Vec<Change> {
    let was: BTreeMap<(&str, &str), &Recorded> = recorded
        .iter()
        .map(|row| ((row.table.as_str(), row.column.as_str()), row))
        .collect();
    let is: BTreeMap<(&str, &str), &Recorded> = current
        .iter()
        .map(|row| ((row.table.as_str(), row.column.as_str()), row))
        .collect();

    let mut out = Vec::new();
    for (key, row) in &is {
        let verdict = match was.get(key) {
            None => Verdict::Add,
            Some(before) if before.fold_hash == row.fold_hash => Verdict::UpToDate,
            // A moved seed cannot be refolded in place: it is the column's `DEFAULT`, and
            // SQLite has no way to alter one, so a row created later for a key no arm
            // fires for would still take the old seed.
            Some(before) if before.seed != row.seed => Verdict::Reshape,
            Some(before) if before.signature_hash == row.signature_hash => Verdict::Refold,
            Some(_) => Verdict::Reshape,
        };
        out.push(Change {
            table: row.table.clone(),
            column: row.column.clone(),
            fold: row.fold.clone(),
            verdict,
        });
    }
    for (key, row) in &was {
        if !is.contains_key(key) {
            out.push(Change {
                table: row.table.clone(),
                column: row.column.clone(),
                fold: row.fold.clone(),
                verdict: Verdict::Drop,
            });
        }
    }
    out.sort_by(|one, two| (&one.table, &one.column).cmp(&(&two.table, &two.column)));
    out
}

/// The derived read model: one SQLite file holding every table a program's folds imply.
pub struct FoldStore {
    conn: Connection,
}

impl FoldStore {
    /// Open (or create) the store at `path` and create any table that is not there yet.
    ///
    /// `CREATE TABLE IF NOT EXISTS` will not add a column to a table that already exists,
    /// which is the whole reason the catalog and [`FoldStore::apply`] exist: a table's
    /// shape moves one column at a time.
    ///
    /// Indexes are not made here but in [`FoldStore::apply`], after it has added the columns
    /// they name: an index on a fold a deploy just added names a column the table does not
    /// have until then.
    pub fn open(path: &Path, tables: &[TableDef]) -> anyhow::Result<FoldStore> {
        let conn = Connection::open(path).context("opening the derived-table database")?;
        // WAL lets a read run beside the single writer, as a read model's does.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_row| Ok(()))
            .context("enabling WAL")?;
        reformat(&conn)?;
        conn.execute_batch(INTERNAL_DDL)
            .context("creating the store's internal tables")?;
        let tx = conn.unchecked_transaction()?;
        for table in tables {
            tx.execute_batch(&table.create_table_sql())
                .with_context(|| format!("creating derived table `{}`", table.name))?;
        }
        tx.commit()?;
        Ok(FoldStore { conn })
    }

    pub fn begin(&self) -> anyhow::Result<Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    /// How long the log was when every column was last folded.
    ///
    /// **A head and not a position**, which is what heklang bounds a read with and
    /// the only number the seam hands out: `Log::head` answers it, a head of zero is a log
    /// with nothing in it, and keeping it in that unit means no call site subtracts one.
    pub fn read_checkpoint(&self) -> anyhow::Result<u64> {
        let position: i64 = self
            .conn
            .query_row(
                "SELECT position FROM _hekla_checkpoint WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .context("reading the derived-table checkpoint")?;
        Ok(position as u64)
    }

    pub fn write_checkpoint(&self, head: u64, tx: &Transaction) -> anyhow::Result<()> {
        tx.execute(
            "UPDATE _hekla_checkpoint SET position = ?1 WHERE id = 0",
            [head as i64],
        )
        .context("writing the derived-table checkpoint")?;
        Ok(())
    }

    /// Every column the store records, in catalog order.
    pub fn recorded(&self) -> anyhow::Result<Vec<Recorded>> {
        let mut stmt = self.conn.prepare(
            "SELECT \"table\", column_name, fold, fold_hash, signature_hash, seed, built_to \
             FROM _hekla_column ORDER BY \"table\", column_name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Recorded {
                table: row.get(0)?,
                column: row.get(1)?,
                fold: row.get(2)?,
                fold_hash: row.get(3)?,
                signature_hash: row.get(4)?,
                seed: row.get(5)?,
                built_to: row.get::<_, i64>(6)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Bring the schema and the catalog to what `program` says, in one transaction.
    ///
    /// **A column is added, dropped or marked for refolding; nothing is folded here.** A
    /// refilled column records `built_to = 0`, which is what says it holds its seed and
    /// nothing else until whatever follows the log catches it up.
    pub fn apply(&self, digest: &Digest, tables: &[TableDef]) -> anyhow::Result<Vec<Change>> {
        let current = catalog(digest, tables)?;
        let changes = reconcile(&self.recorded()?, &current);
        let wanted: BTreeMap<(&str, &str), &Recorded> = current
            .iter()
            .map(|row| ((row.table.as_str(), row.column.as_str()), row))
            .collect();
        let defs: BTreeMap<&str, &TableDef> = tables
            .iter()
            .map(|table| (table.name.as_str(), table))
            .collect();

        let tx = self.begin()?;
        // SQLite will not drop a column an index names, so those indexes go first; the
        // sync below puts back whichever are still wanted.
        for change in &changes {
            if matches!(change.verdict, Verdict::Drop | Verdict::Reshape) {
                for (name, columns) in indexes_on(&tx, &change.table)? {
                    if columns.contains(&change.column) {
                        drop_index(&tx, &name)?;
                    }
                }
            }
        }
        for change in &changes {
            let key = (change.table.as_str(), change.column.as_str());
            match change.verdict {
                Verdict::UpToDate => {}
                Verdict::Drop => {
                    drop_column(&tx, &change.table, &change.column)?;
                    tx.execute(
                        "DELETE FROM _hekla_column WHERE \"table\" = ?1 AND column_name = ?2",
                        (&change.table, &change.column),
                    )?;
                }
                // A reshape is a column of another shape under the same name, so the old
                // one goes before the new one arrives.
                Verdict::Add | Verdict::Refold | Verdict::Reshape => {
                    if change.verdict == Verdict::Reshape {
                        drop_column(&tx, &change.table, &change.column)?;
                    }
                    let table = defs
                        .get(change.table.as_str())
                        .with_context(|| format!("no derived table `{}`", change.table))?;
                    let column = table
                        .columns
                        .iter()
                        .find(|column| column.name == change.column)
                        .with_context(|| format!("no column `{}`", change.column))?;
                    add_column(&tx, table, column)?;
                    let row = wanted[&key];
                    tx.execute(
                        "INSERT INTO _hekla_column \
                         (\"table\", column_name, fold, fold_hash, signature_hash, seed, built_to) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0) \
                         ON CONFLICT(\"table\", column_name) DO UPDATE SET \
                         fold = ?3, fold_hash = ?4, signature_hash = ?5, seed = ?6, \
                         built_to = 0",
                        (
                            &row.table,
                            &row.column,
                            &row.fold,
                            &row.fold_hash,
                            &row.signature_hash,
                            &row.seed,
                        ),
                    )?;
                }
            }
        }
        sync_indexes(&tx, tables)?;
        tx.commit()?;
        Ok(changes)
    }
}

/// How cells are stored, as `PRAGMA user_version`. Moved whenever a stored cell would read
/// back differently, which the catalog cannot see: it keys a column by what its fold does,
/// and a change to how hekla stores the result moves no fold's hash.
///
/// 1: a sealed cell is a [`SealedCell`] rather than bare ciphertext.
/// 2: a key a collection column was written for is in its main table too.
/// 3: a `Timestamp` is stored at one fixed width, so its text order is its time order.
/// 4: a table is named by a hash of its key signature, not a name `layout` derived, so a
/// store of 3 holds every table under a name nothing would drop.
const FORMAT: i64 = 4;

/// Empty a store written in another format, so it refolds from the log.
///
/// **Rebuilt rather than migrated**, because it can be: a derived store holds nothing the
/// log does not, which is why a backup leaves it out. Dropping the catalog with the tables
/// is what makes every column read as `Add`, and the checkpoint coming back at zero is
/// what makes the follow start from the beginning.
fn reformat(conn: &Connection) -> anyhow::Result<()> {
    let at: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if at == FORMAT {
        return Ok(());
    }
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if !tables.is_empty() {
        tracing::info!(
            from = at,
            to = FORMAT,
            "the derived tables are stored in another format and will be refolded"
        );
    }
    let tx = conn.unchecked_transaction()?;
    for table in &tables {
        tx.execute_batch(&format!("DROP TABLE {}", quote_ident(table)))?;
    }
    tx.execute_batch(&format!("PRAGMA user_version = {FORMAT}"))?;
    tx.commit()
        .context("emptying a derived store of another format")
}

/// `ALTER TABLE ... ADD COLUMN`, but only when SQLite does not already have it.
///
/// Idempotent against the real schema rather than against the catalog, because
/// [`FoldStore::open`] creates a brand new table with every column already on it: a
/// column of a new table is an `Add` with nothing to alter.
fn add_column(tx: &Transaction, table: &TableDef, column: &ColumnDef) -> anyhow::Result<()> {
    if has_column(tx, &table.name, &column.name)? {
        return Ok(());
    }
    // **A populated table needs a default for a `NOT NULL` column, and the right default
    // is the fold's seed.** A column holds the fold's value for every key, and for a key
    // no arm of it has fired for that value is the seed, so the existing rows are not
    // being filled with a placeholder: they are being given the answer.
    let default = match &column.default {
        Some(literal) => format!(" DEFAULT {literal}"),
        None => String::new(),
    };
    let null = if nullable(&column.meta) {
        ""
    } else {
        " NOT NULL"
    };
    tx.execute_batch(&format!(
        "ALTER TABLE {} ADD COLUMN {} {}{null}{default}",
        quote_ident(&table.name),
        quote_ident(&column.name),
        column.meta.sql_type(),
    ))
    .with_context(|| format!("adding column `{}` to `{}`", column.name, table.name))
}

fn drop_column(tx: &Transaction, table: &str, column: &str) -> anyhow::Result<()> {
    if !has_column(tx, table, column)? {
        return Ok(());
    }
    tx.execute_batch(&format!(
        "ALTER TABLE {} DROP COLUMN {}",
        quote_ident(table),
        quote_ident(column)
    ))
    .with_context(|| format!("dropping column `{column}` from `{table}`"))
}

fn has_column(tx: &Transaction, table: &str, column: &str) -> anyhow::Result<bool> {
    let mut stmt = tx.prepare(&format!("PRAGMA table_info({})", quote_ident(table)))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// See [`crate::tables`]: a sealed column is nullable whatever its type says, because an
/// erasure writes `NULL` into it.
fn nullable(meta: &FieldMeta) -> bool {
    meta.is_nullable() || meta.sealed_under.is_some()
}

/// A sealed cell at rest: the log's own ciphertext, and what it takes to open it.
///
/// **Not re-sealed under the column's name the way a projector's `put` does**, for two
/// reasons. A projector re-seals under its row's subject column, and a fold's key need
/// not have one: `OrderEmail` is keyed on an order and sealed under a customer, so the
/// id has to ride in the cell or nothing could ever open it. And keeping the log's bytes
/// means a write never touches a key, so a rebuild after an erasure cannot re-mint the
/// key it destroyed, and the erasure makes the cell unreadable with no pass over the
/// store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedCell {
    pub subject: String,
    pub id: String,
    /// The event field the content was sealed under, which the key store binds into the
    /// ciphertext, so it is the name a read decrypts under rather than the column's.
    pub field: String,
    pub sealed: String,
}

impl SealedCell {
    /// The stored text of a value bound for a sealed column: `Some(None)` for an absent
    /// optional, which is `NULL`, and `None` for anything that is not a seal.
    pub(crate) fn text(value: &Value) -> Option<Option<String>> {
        // Borrowed, so a write in the hot path serializes straight off the value.
        #[derive(Serialize)]
        struct Borrowed<'a> {
            subject: &'a str,
            id: &'a str,
            field: &'a str,
            sealed: &'a str,
        }
        match peeled(value) {
            Value::Opt { value: None, .. } => Some(None),
            Value::Sealed {
                field,
                subject,
                id,
                content,
            } => Some(Some(
                serde_json::to_string(&Borrowed {
                    subject,
                    id,
                    field,
                    sealed: content,
                })
                .expect("four strings serialize"),
            )),
            _ => None,
        }
    }

    pub fn parse(text: &str) -> anyhow::Result<SealedCell> {
        serde_json::from_str(text).context("a sealed derived cell that is not one")
    }

    /// The cell as the value the sink handed the store, so an arm reading it back
    /// sees exactly what it wrote.
    pub fn value(self) -> Value {
        Value::Sealed {
            field: self.field,
            subject: self.subject,
            id: self.id,
            content: self.sealed.into(),
        }
    }
}

/// One value as the column stores it, or `None` for a value the column holds as `NULL`.
pub(crate) fn stored_text(kind: &FieldKind, value: &Value) -> Option<String> {
    match value {
        Value::Opt { value: held, .. } => held.as_ref().and_then(|held| stored_text(kind, held)),
        other => match stored_form(kind, from_heklang_json(&Json::from_value(other))) {
            serde_json::Value::Null => None,
            serde_json::Value::Bool(flag) => Some((if flag { "1" } else { "0" }).to_owned()),
            serde_json::Value::String(text) => Some(text),
            rest => Some(rest.to_string()),
        },
    }
}

impl FoldStore {
    /// Make sure `table` has a row for `key`, holding every column's seed if it is new.
    pub fn ensure_row(
        &self,
        tx: &Transaction,
        table: &TableDef,
        key: &[String],
    ) -> anyhow::Result<()> {
        let (names, binds) = key_binds(table, key)?;
        let holders: Vec<String> = (1..=names.len()).map(|at| format!("?{at}")).collect();
        tx.execute(
            &format!(
                "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT DO NOTHING",
                quote_ident(&table.name),
                names.join(", "),
                holders.join(", ")
            ),
            rusqlite::params_from_iter(binds),
        )
        .with_context(|| format!("recording a key of `{}`", table.name))?;
        Ok(())
    }

    /// Write one column of one row, creating the row if no event had matched its key yet.
    ///
    /// Per column rather than per row, because that is the unit a fold is: two folds of
    /// one key are two columns one event may touch separately, and rewriting the row would
    /// put back whatever the other column held when this batch started. A row created here
    /// gets its other columns' seeds from their `DEFAULT`, which is what folding each of
    /// them for this key would return.
    pub fn write(
        &self,
        tx: &Transaction,
        table: &TableDef,
        key: &[String],
        column: &ColumnDef,
        value: &Value,
    ) -> anyhow::Result<()> {
        let (mut names, mut binds) = key_binds(table, key)?;
        names.push(quote_ident(&column.name));
        binds.push(cell_of(&table.name, column, value)?);
        let holders: Vec<String> = (1..=names.len()).map(|at| format!("?{at}")).collect();
        let conflict: Vec<String> = if table.key.is_empty() {
            vec![quote_ident("_one")]
        } else {
            table
                .key
                .iter()
                .map(|(name, _)| quote_ident(name))
                .collect()
        };
        tx.execute(
            &format!(
                "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT({}) DO UPDATE SET {} = ?{}",
                quote_ident(&table.name),
                names.join(", "),
                holders.join(", "),
                conflict.join(", "),
                quote_ident(&column.name),
                names.len()
            ),
            rusqlite::params_from_iter(binds),
        )
        .with_context(|| format!("writing `{}`.`{}`", table.name, column.name))?;
        Ok(())
    }

    /// Put a column back to its fold's seed for every key, which is where a replay of the
    /// log starts it from. A refolded column still holds its old fold's values, and a
    /// replay applies each record to the value already there.
    pub fn reset_column(
        &self,
        tx: &Transaction,
        table: &TableDef,
        column: &ColumnDef,
    ) -> anyhow::Result<()> {
        tx.execute(
            &format!(
                "UPDATE {} SET {} = {}",
                quote_ident(&table.name),
                quote_ident(&column.name),
                column.default.as_deref().unwrap_or("NULL")
            ),
            [],
        )
        .with_context(|| format!("resetting `{}`.`{}`", table.name, column.name))?;
        Ok(())
    }

    /// Record that a column has been folded to `position`.
    pub fn mark_built(
        &self,
        tx: &Transaction,
        table: &str,
        column: &str,
        head: u64,
    ) -> anyhow::Result<()> {
        tx.execute(
            "UPDATE _hekla_column SET built_to = ?3 WHERE \"table\" = ?1 AND column_name = ?2",
            (table, column, head as i64),
        )?;
        Ok(())
    }

    /// Every column that has not caught up with the store's checkpoint.
    ///
    /// A column below it holds its seed and nothing else, so it is not servable until a
    /// backfill brings it up. This is what a readiness gate reads.
    pub fn behind(&self) -> anyhow::Result<Vec<Recorded>> {
        let at = self.read_checkpoint()?;
        Ok(self
            .recorded()?
            .into_iter()
            .filter(|row| row.built_to < at)
            .collect())
    }
}

/// One SQLite value as the text `coerce_value` reads back.
fn render(value: rusqlite::types::Value) -> String {
    match value {
        rusqlite::types::Value::Null => String::new(),
        rusqlite::types::Value::Integer(n) => n.to_string(),
        rusqlite::types::Value::Real(n) => n.to_string(),
        rusqlite::types::Value::Text(text) => text,
        rusqlite::types::Value::Blob(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
    }
}

impl FoldStore {
    /// One cell as it stands inside `tx`, including writes this batch has already made.
    ///
    /// Read through the transaction rather than the connection so that two arms of one
    /// event accumulate onto each other: the second has to see what the first wrote.
    pub fn cell(
        &self,
        tx: &Transaction,
        table: &TableDef,
        key: &[String],
        column: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let mut wheres = Vec::new();
        let mut binds: Vec<rusqlite::types::Value> = Vec::new();
        for ((name, meta), text) in table.key.iter().zip(key) {
            wheres.push(format!("{} = ?{}", quote_ident(name), binds.len() + 1));
            binds.push(crate::read_model::coerce_value(&meta.kind, text)?);
        }
        if table.key.is_empty() {
            wheres.push(format!("{} = 0", quote_ident("_one")));
        }
        let sql = format!(
            "SELECT {} FROM {} WHERE {}",
            quote_ident(column),
            quote_ident(&table.name),
            wheres.join(" AND ")
        );
        let mut stmt = tx.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(binds))?;
        let Some(row) = rows.next()? else {
            // No row for this key yet, which is not the same as a column holding NULL: the
            // arm is about to mint it, and `Columns::value` answering `None` is what says
            // "use the seed".
            return Ok(None);
        };
        // Through `from_sql`, not off the raw cell: a `Bool` is an INTEGER and a `Money` is
        // its decimal string, so the typed conversion is what a projector's row read also
        // goes through. Reading the cell directly gave a number where the fold wanted a
        // bool.
        Ok(Some(crate::read_model::from_sql(
            &meta_of(table, column)?.clone(),
            row.get_ref(0)?,
        )?))
    }

    /// Every key of `table` carrying the parts `parts` names, inside `tx`.
    ///
    /// `parts` is one entry per key part in key order: `Some` where a value is pinned,
    /// `None` where it is the range. Read through the transaction, so a key an earlier
    /// arm of this record minted is in the range.
    pub fn keys_pinned(
        &self,
        tx: &Transaction,
        table: &TableDef,
        parts: &[Option<String>],
    ) -> anyhow::Result<Vec<Vec<String>>> {
        let mut wheres = Vec::new();
        let mut binds: Vec<rusqlite::types::Value> = Vec::new();
        for ((name, meta), value) in table.key.iter().zip(parts) {
            let Some(text) = value else { continue };
            wheres.push(format!("{} = ?{}", quote_ident(name), binds.len() + 1));
            binds.push(crate::read_model::coerce_value(&meta.kind, text)?);
        }
        let columns: Vec<String> = table
            .key
            .iter()
            .map(|(name, _)| quote_ident(name))
            .collect();
        if columns.is_empty() {
            // The single-row table: one key, which is the empty one, and it exists.
            return Ok(vec![Vec::new()]);
        }
        let sql = format!(
            "SELECT {} FROM {}{}",
            columns.join(", "),
            quote_ident(&table.name),
            if wheres.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", wheres.join(" AND "))
            }
        );
        let mut stmt = tx.prepare(&sql)?;
        let width = table.key.len();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), move |row| {
            (0..width)
                .map(|at| row.get::<_, rusqlite::types::Value>(at).map(render))
                .collect::<Result<Vec<String>, _>>()
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// A key's columns and values as an insert binds them: each part typed to its column, or
/// the guard column alone for the one-row table.
fn key_binds(
    table: &TableDef,
    key: &[String],
) -> anyhow::Result<(Vec<String>, Vec<rusqlite::types::Value>)> {
    let mut names = Vec::new();
    let mut binds = Vec::new();
    for ((name, meta), text) in table.key.iter().zip(key) {
        names.push(quote_ident(name));
        binds.push(crate::read_model::coerce_value(&meta.kind, text)?);
    }
    if table.key.is_empty() {
        names.push(quote_ident("_one"));
        binds.push(rusqlite::types::Value::Integer(0));
    }
    Ok((names, binds))
}

/// One column's metadata, by name, for a typed read of its cell.
fn meta_of<'a>(table: &'a TableDef, column: &str) -> anyhow::Result<&'a FieldMeta> {
    table
        .key
        .iter()
        .find(|(name, _)| name == column)
        .map(|(_, meta)| meta)
        .or_else(|| {
            table
                .columns
                .iter()
                .find(|held| held.name == column)
                .map(|held| &held.meta)
        })
        .with_context(|| format!("no column `{column}` of `{}`", table.name))
}

/// Make each derived table carry exactly the indexes its definition asks for.
///
/// An index's name spells its columns, so one whose declaration moved is a new name: the
/// old goes and the new is built. One already there is left alone, which is what keeps an
/// ordinary boot from rebuilding every index of a large store.
fn sync_indexes(tx: &Transaction, tables: &[TableDef]) -> anyhow::Result<()> {
    for table in tables {
        let wanted: Vec<(String, String)> = table
            .indexes
            .iter()
            .map(|index| format!("{}_{}", table.name, index.name))
            .zip(table.create_index_sql())
            .collect();
        for (name, _) in indexes_on(tx, &table.name)? {
            if owned(&table.name, &name) && !wanted.iter().any(|(want, _)| *want == name) {
                drop_index(tx, &name)?;
            }
        }
        for (_, sql) in &wanted {
            tx.execute_batch(sql)
                .with_context(|| format!("indexing derived table `{}`", table.name))?;
        }
    }
    Ok(())
}

/// Whether hekla named this index: a ranged arm's `by_..` or an `@index`'s `sorts__..`. One
/// an operator made by hand on a derived table is theirs, and a sync leaves it alone.
fn owned(table: &str, index: &str) -> bool {
    index
        .strip_prefix(table)
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(|rest| rest.starts_with("by_") || rest.starts_with("sorts__"))
}

/// The indexes hekla made on `table`, with the columns each one names. The one SQLite
/// makes for a primary key is not among them.
fn indexes_on(tx: &Transaction, table: &str) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    let names: Vec<String> = tx
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 \
             AND sql IS NOT NULL",
        )?
        .query_map([table], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for name in names {
        // An expression column has no name, so it is `NULL` here, and it names no column
        // a drop could be blocked by.
        let columns: Vec<String> = tx
            .prepare(&format!("PRAGMA index_info({})", quote_ident(&name)))?
            .query_map([], |row| row.get::<_, Option<String>>(2))?
            .filter_map(Result::transpose)
            .collect::<Result<_, _>>()?;
        out.push((name, columns));
    }
    Ok(out)
}

fn drop_index(tx: &Transaction, name: &str) -> anyhow::Result<()> {
    tx.execute_batch(&format!("DROP INDEX IF EXISTS {}", quote_ident(name)))
        .with_context(|| format!("dropping index `{name}`"))
}

/// A fold's value as the SQL value its column holds, which is what a write binds and what a
/// history renders through the same read a stored cell takes.
pub(crate) fn cell_of(
    table: &str,
    column: &ColumnDef,
    value: &Value,
) -> anyhow::Result<rusqlite::types::Value> {
    Ok(if column.meta.sealed_under.is_some() {
        match SealedCell::text(value) {
            Some(Some(text)) => rusqlite::types::Value::Text(text),
            Some(None) => rusqlite::types::Value::Null,
            // Plaintext in a sealed column would be personal data stored in the clear,
            // and heklang's parse-time rule is what keeps it from happening, so this is
            // the backstop rather than a case.
            None => anyhow::bail!(
                "`{table}`.`{}` is sealed and was handed plaintext to store",
                column.name
            ),
        }
    } else {
        match stored_text(&column.meta.kind, value) {
            Some(text) => crate::read_model::coerce_value(&column.meta.kind, &text)?,
            None => rusqlite::types::Value::Null,
        }
    })
}
