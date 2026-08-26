//! SQLite-backed read model for projectors.
//!
//! Each declared `entity(...)` becomes a real SQLite table (created from the
//! `EntityDef`'s generated DDL), and the projector's `put`/`patch`/`delete` ops
//! map onto `INSERT OR REPLACE` / `UPDATE` / `DELETE`. Values are bound as typed
//! columns per the entity's declared `FieldKind`s.
//!
//! Alongside the entity tables sits a single-row `_hekla_checkpoint`, so a batch
//! of ops and the position it advances to commit in one transaction: state and
//! progress can never disagree. The read API opens the same file read-only.

use std::path::Path;
use std::str;

use anyhow::Context;
use rusqlite::config::DbConfig;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, Row, Transaction, params_from_iter};
use tephra::Position;

use crate::starlark_builtins::{EntityDef, EntityOpKind, FieldKind, FieldMeta};

/// The projector's internal tables: the checkpoint, co-located with the read-model
/// tables so it commits in the same transaction as the state it describes, and the
/// definition hash the model was built under. `completed_above` records positions
/// processed above `position`; it is always empty under the sequential model,
/// reserved so parallel lanes need no schema migration.
const INTERNAL_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _hekla_checkpoint (
    id              INTEGER PRIMARY KEY CHECK (id = 0),
    position        INTEGER NOT NULL,
    completed_above TEXT    NOT NULL DEFAULT '[]'
);
INSERT OR IGNORE INTO _hekla_checkpoint (id, position) VALUES (0, 0);
CREATE TABLE IF NOT EXISTS _hekla_definition (
    id              INTEGER PRIMARY KEY CHECK (id = 0),
    definition_hash TEXT
);
INSERT OR IGNORE INTO _hekla_definition (id, definition_hash) VALUES (0, NULL);";

/// A projector's materialised read model: one SQLite table per entity.
pub struct ReadModel {
    conn: Connection,
}

impl ReadModel {
    /// Open (or create) the database at `path` and create each entity's table
    /// and indexes, plus the internal tables. Pass a filesystem path; the tables use
    /// `IF NOT EXISTS`, so reopening an existing read model is fine.
    pub fn open(path: &Path, entities: &[EntityDef]) -> anyhow::Result<ReadModel> {
        let conn = Connection::open(path).context("opening read-model database")?;
        reject_double_quoted_strings(&conn)?;
        // WAL lets the read API read concurrently with this single writer.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_row| Ok(()))
            .context("enabling WAL")?;
        conn.execute_batch(INTERNAL_DDL)
            .context("creating the projector's internal tables")?;
        for entity in entities {
            conn.execute_batch(&entity.create_table_sql())
                .with_context(|| format!("creating table `{}`", entity.name))?;
            for stmt in entity.create_index_sql() {
                conn.execute_batch(&stmt)
                    .with_context(|| format!("indexing table `{}`", entity.name))?;
            }
        }
        Ok(ReadModel { conn })
    }

    /// Open an existing read model read-only, for the read API. WAL lets these
    /// connections read concurrently with the projector's writer; `query_only`
    /// is a guard on top of the read-only open flag.
    pub fn open_readonly(path: &Path) -> anyhow::Result<ReadModel> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI;
        let conn = Connection::open_with_flags(path, flags)
            .context("opening read-model database read-only")?;
        reject_double_quoted_strings(&conn)?;
        conn.pragma_update(None, "query_only", "ON")
            .context("enabling query_only")?;
        Ok(ReadModel { conn })
    }

    /// Begin a transaction on the read model. Reads and writes on this connection
    /// then see each other's uncommitted effects, which is what lets a projector's
    /// `get()` observe a `put()` from an earlier event in the same batch.
    pub fn begin(&self) -> anyhow::Result<Transaction<'_>> {
        self.conn
            .unchecked_transaction()
            .context("beginning a read-model transaction")
    }

    /// The projector's resume position: everything at or below it is applied.
    pub fn read_checkpoint(&self) -> anyhow::Result<Position> {
        let position: i64 = self
            .conn
            .query_row(
                "SELECT position FROM _hekla_checkpoint WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .context("reading the projector checkpoint")?;
        Ok(Position::new(position as u64))
    }

    /// Advance the checkpoint on `tx`. Called inside the same transaction as the
    /// batch's ops, so state and position move together or not at all.
    pub fn write_checkpoint(&self, position: Position, tx: &Transaction) -> anyhow::Result<()> {
        tx.execute(
            "UPDATE _hekla_checkpoint SET position = ?1, completed_above = '[]' WHERE id = 0",
            [position.get() as i64],
        )
        .context("writing the projector checkpoint")?;
        Ok(())
    }

    /// Persist `position` as the checkpoint on its own, with no ops. Used when a
    /// selective projector's watermark advances past a non-matching tail: there is
    /// nothing to apply, but the resume point (and reported position) should still
    /// track head rather than stall at the last matching event.
    pub fn advance_checkpoint(&self, position: Position) -> anyhow::Result<()> {
        let tx = self.begin()?;
        self.write_checkpoint(position, &tx)?;
        tx.commit().context("advancing the projector checkpoint")?;
        Ok(())
    }

    /// The definition hash (source set + entity schema) the read model was last built
    /// under, or `None` for a fresh model. Co-located with the data so it moves with a
    /// rebuild's atomic swap and survives a crash together with the rows it describes.
    pub fn read_definition(&self) -> anyhow::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT definition_hash FROM _hekla_definition WHERE id = 0",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .context("reading the projector definition hash")
    }

    /// Record the definition hash this read model is built under. Committed on its own,
    /// so during a rebuild it is set on the fresh model before the swap.
    pub fn set_definition(&self, definition_hash: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE _hekla_definition SET definition_hash = ?1 WHERE id = 0",
                [definition_hash],
            )
            .context("recording the projector definition hash")?;
        Ok(())
    }

    /// Seal the database for a replay swap: fold the WAL back into the main file
    /// and drop to rollback journal mode, removing the `-wal`/`-shm` sidecars. The
    /// file becomes self-contained, so after the rename a reader that opens it
    /// (before the writer reopens in WAL mode) ignores any stale `-wal`.
    pub fn seal(&self) -> anyhow::Result<()> {
        self.conn
            .query_row("PRAGMA journal_mode = DELETE", [], |_row| Ok(()))
            .context("sealing the read model for a swap")?;
        Ok(())
    }

    /// Apply one entity op on `&self.conn`: `put` → `INSERT OR REPLACE`, `patch`
    /// → `UPDATE` (a no-op when zero rows match), `delete` → `DELETE`. Autocommits
    /// per op unless a transaction is open on this connection, in which case the op
    /// participates in it. A batch opens a [`begin`] first, so its ops commit with
    /// the checkpoint.
    ///
    /// [`begin`]: ReadModel::begin
    pub fn apply_one(&self, entity: &EntityDef, op: EntityOpKind) -> anyhow::Result<()> {
        match op {
            EntityOpKind::Put(row_json) => {
                let row: serde_json::Value = serde_json::from_str(&row_json)?;
                let obj = row.as_object().context("put row is not a JSON object")?;
                let placeholders = vec!["?"; entity.fields.len()].join(", ");
                let sql = format!(
                    "INSERT OR REPLACE INTO {} ({}) VALUES ({placeholders})",
                    quote_ident(&entity.name),
                    column_list(entity),
                );
                let mut values = Vec::with_capacity(entity.fields.len());
                for (name, meta) in &entity.fields {
                    let value = obj.get(name).unwrap_or(&serde_json::Value::Null);
                    values.push(to_sql(meta, value).with_context(|| format!("column `{name}`"))?);
                }
                self.conn.execute(&sql, params_from_iter(values))?;
            }
            EntityOpKind::Patch { key, changes } => {
                let changes: serde_json::Value = serde_json::from_str(&changes)?;
                let obj = changes
                    .as_object()
                    .context("patch changes are not a JSON object")?;
                let mut assignments = Vec::new();
                let mut values = Vec::new();
                for (name, meta) in &entity.fields {
                    if let Some(value) = obj.get(name) {
                        assignments.push(format!("{} = ?", quote_ident(name)));
                        values
                            .push(to_sql(meta, value).with_context(|| format!("column `{name}`"))?);
                    }
                }
                if assignments.is_empty() {
                    return Ok(());
                }
                values.push(bind_or_text(key_kind(entity), &key));
                let sql = format!(
                    "UPDATE {} SET {} WHERE {} = ?",
                    quote_ident(&entity.name),
                    assignments.join(", "),
                    quote_ident(&entity.key)
                );
                self.conn.execute(&sql, params_from_iter(values))?;
            }
            EntityOpKind::Delete(key) => {
                let sql = format!(
                    "DELETE FROM {} WHERE {} = ?",
                    quote_ident(&entity.name),
                    quote_ident(&entity.key)
                );
                self.conn.execute(
                    &sql,
                    params_from_iter([bind_or_text(key_kind(entity), &key)]),
                )?;
            }
        }
        Ok(())
    }

    /// Read every row of an entity back as a JSON object (NULL columns omitted),
    /// ordered by key. For inspection and display.
    pub fn rows(&self, entity: &EntityDef) -> anyhow::Result<Vec<serde_json::Value>> {
        self.scan(entity, None, None, i64::MAX as usize)
    }

    /// Read one row by key, as a JSON object (NULL columns omitted), or `None`.
    pub fn get(&self, entity: &EntityDef, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let columns = column_list(entity);
        let sql = format!(
            "SELECT {columns} FROM {} WHERE {} = ?",
            quote_ident(&entity.name),
            quote_ident(&entity.key)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter([bind_or_text(key_kind(entity), key)]))?;
        rows.next()?.map(|row| row_to_json(entity, row)).transpose()
    }

    /// Scan an entity ordered by key, optionally filtered by one column and
    /// resumed after a key (cursor pagination). `filter`'s column must be a
    /// declared field; the caller enforces that it is indexed. Values bind as
    /// typed parameters, never interpolated.
    pub fn scan(
        &self,
        entity: &EntityDef,
        filter: Option<(&str, &str)>,
        after_key: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let columns = column_list(entity);
        let mut clauses = Vec::new();
        let mut binds: Vec<SqlValue> = Vec::new();
        if let Some((column, value)) = filter {
            clauses.push(format!("{} = ?", quote_ident(column)));
            let kind = entity
                .fields
                .iter()
                .find(|(name, _)| name == column)
                .map(|(_, meta)| &meta.kind);
            binds.push(match kind {
                Some(kind) => bind_or_text(kind, value),
                None => text(value),
            });
        }
        if let Some(after) = after_key {
            clauses.push(format!("{} > ?", quote_ident(&entity.key)));
            binds.push(bind_or_text(key_kind(entity), after));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let key = quote_ident(&entity.key);
        let sql = format!(
            "SELECT {columns} FROM {}{where_clause} ORDER BY {key} LIMIT ?",
            quote_ident(&entity.name)
        );
        binds.push(SqlValue::Integer(limit as i64));
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(binds))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row_to_json(entity, row)?);
        }
        Ok(out)
    }
}

/// The entity's columns as a comma-separated `SELECT` list, in declared order so
/// a row's positional columns line up with `entity.fields`.
fn column_list(entity: &EntityDef) -> String {
    entity
        .fields
        .iter()
        .map(|(name, _)| quote_ident(name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Wrap a generated table, column or index name in SQLite's double-quote syntax,
/// doubling any embedded quote. Every identifier this crate interpolates into SQL
/// goes through this: a field named after a SQLite keyword (`group`, `order`,
/// `select`, ...) is a valid column name that unquoted is a syntax error, and a
/// table whose CREATE fails takes the whole runtime down at boot.
///
/// SQLite resolves `"group"` and `group` to the same name, so quoting changes only
/// how a statement parses, never the stored schema: a read model written before
/// this still opens, reads and writes with no rebuild.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Turn off SQLite's double-quoted-string misfeature on `conn`, in both DML and DDL.
///
/// This is the other half of [`quote_ident`] and is not optional. By default SQLite
/// accepts a double-quoted token that resolves to no identifier as a *string
/// literal*, so `SELECT "email" FROM rows` against a table lacking that column
/// returns the text `email` for every row instead of failing. That would turn a read
/// model whose shape predates a field addition from a loud error into silently wrong
/// data, and would hide exactly the mismatch [`crate::projector::Readiness`] exists
/// to detect. With DQS off, an unknown quoted identifier is `no such column` again.
fn reject_double_quoted_strings(conn: &Connection) -> anyhow::Result<()> {
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)
        .context("disabling double-quoted string literals in DML")?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)
        .context("disabling double-quoted string literals in DDL")?;
    Ok(())
}

/// Reconstruct one selected row as a JSON object, NULL columns omitted, typed per
/// the entity's declared kinds. Columns must be selected in `entity.fields` order.
fn row_to_json(entity: &EntityDef, row: &Row) -> anyhow::Result<serde_json::Value> {
    let mut obj = serde_json::Map::new();
    for (i, (name, meta)) in entity.fields.iter().enumerate() {
        let value = from_sql(meta, row.get_ref(i)?)?;
        if !value.is_null() {
            obj.insert(name.clone(), value);
        }
    }
    Ok(serde_json::Value::Object(obj))
}

pub(crate) fn key_kind(entity: &EntityDef) -> &FieldKind {
    entity
        .fields
        .iter()
        .find(|(name, _)| name == &entity.key)
        .map(|(_, meta)| &meta.kind)
        .expect("the key is a declared field")
}

/// Bind a JSON value as a typed SQLite value per the field's declared kind. A
/// subject-scoped column stores its opaque ciphertext string verbatim (as `TEXT`),
/// regardless of the underlying kind; the read API decrypts and re-types it on read.
fn to_sql(meta: &FieldMeta, value: &serde_json::Value) -> anyhow::Result<SqlValue> {
    if value.is_null() {
        return Ok(SqlValue::Null);
    }
    if meta.subject.is_some() {
        return Ok(SqlValue::Text(
            value
                .as_str()
                .context("a subject-scoped column stores ciphertext text")?
                .to_owned(),
        ));
    }
    Ok(match meta.kind.base() {
        FieldKind::Bool => {
            let n = value
                .as_bool()
                .map(|b| b as i64)
                .or_else(|| value.as_i64())
                .context("expected a boolean")?;
            SqlValue::Integer(n)
        }
        FieldKind::I64 | FieldKind::U64 => {
            SqlValue::Integer(value.as_i64().context("expected an integer")?)
        }
        // Money is a decimal string on the wire, stored verbatim as text.
        FieldKind::Text { .. }
        | FieldKind::Uuid
        | FieldKind::Timestamp
        | FieldKind::OneOf(_)
        | FieldKind::Money => {
            SqlValue::Text(value.as_str().context("expected a string")?.to_owned())
        }
        FieldKind::Json => SqlValue::Text(value.to_string()),
        FieldKind::Optional(_) => unreachable!("base() strips Optional"),
    })
}

/// Coerce a string-form key or filter value to the column's SQLite type. Keys (from
/// an op, a path segment, or a cursor) and filter values both arrive as strings;
/// this binds them per the declared kind. Returns an error when the string cannot be
/// the column's type (`abc` for an integer, `maybe` for a bool), so the read API can
/// answer a bad filter with a 400 rather than a scan that silently matches nothing.
pub(crate) fn coerce_value(kind: &FieldKind, raw: &str) -> anyhow::Result<SqlValue> {
    Ok(match kind.base() {
        // Bool columns are stored as INTEGER, so a `true`/`false` filter must bind
        // as 1/0, not as text (which would match no integer row).
        FieldKind::Bool => match raw {
            "true" | "1" => SqlValue::Integer(1),
            "false" | "0" => SqlValue::Integer(0),
            _ => anyhow::bail!("expected a boolean (`true` or `false`)"),
        },
        FieldKind::I64 | FieldKind::U64 => {
            SqlValue::Integer(raw.parse::<i64>().context("expected an integer")?)
        }
        // Money and the text-shaped kinds bind as their string form.
        _ => SqlValue::Text(raw.to_owned()),
    })
}

/// Bind a key or filter string as its column's type, falling back to text when it
/// does not parse. Internal ops supply well-typed keys; the external filter path
/// validates the value first (via [`coerce_value`]) so a mismatch 400s rather than
/// reaching here.
fn bind_or_text(kind: &FieldKind, raw: &str) -> SqlValue {
    coerce_value(kind, raw).unwrap_or_else(|_| text(raw))
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

/// Reconstruct a JSON value from a stored column per the field's declared kind. A
/// subject-scoped column returns its stored ciphertext string as-is; the read API
/// layer decrypts it (and re-types it to the underlying kind) on the way out.
fn from_sql(meta: &FieldMeta, value: ValueRef) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value as J;
    if meta.subject.is_some() {
        return Ok(match value {
            ValueRef::Null => J::Null,
            ValueRef::Text(bytes) => J::String(
                str::from_utf8(bytes)
                    .context("non-UTF-8 ciphertext column")?
                    .to_owned(),
            ),
            _ => J::Null,
        });
    }
    Ok(match value {
        ValueRef::Null => J::Null,
        ValueRef::Integer(i) => match meta.kind.base() {
            FieldKind::Bool => J::Bool(i != 0),
            _ => J::Number(i.into()),
        },
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(J::Number)
            .unwrap_or(J::Null),
        ValueRef::Text(bytes) => {
            let text = str::from_utf8(bytes).context("non-UTF-8 text column")?;
            match meta.kind.base() {
                // Text that does not parse comes back as the raw string rather than
                // null, so a column that changed kind between deploys reads as bad
                // data instead of silently vanishing from the row.
                FieldKind::Json => {
                    serde_json::from_str(text).unwrap_or_else(|_| J::String(text.to_owned()))
                }
                _ => J::String(text.to_owned()),
            }
        }
        ValueRef::Blob(_) => J::Null,
    })
}

#[cfg(test)]
mod tests {
    use std::slice;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::starlark_builtins::IndexDef;

    fn users_entity() -> EntityDef {
        EntityDef {
            id: 1,
            name: "users".to_owned(),
            key: "user_id".to_owned(),
            fields: vec![
                ("user_id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                (
                    "email".to_owned(),
                    FieldMeta::plain(FieldKind::Text { max_length: None }),
                ),
            ],
            indexes: vec![],
        }
    }

    fn open_temp(entities: &[EntityDef]) -> (ReadModel, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let model = ReadModel::open(&dir.path().join("m.db"), entities).unwrap();
        (model, dir)
    }

    fn put(model: &ReadModel, entity: &EntityDef, id: &str, email: &str) {
        model
            .apply_one(
                entity,
                EntityOpKind::Put(json!({ "user_id": id, "email": email }).to_string()),
            )
            .unwrap();
    }

    #[test]
    fn checkpoint_round_trips() {
        let (model, _dir) = open_temp(&[users_entity()]);
        assert_eq!(model.read_checkpoint().unwrap().get(), 0);
        let tx = model.begin().unwrap();
        model.write_checkpoint(Position::new(42), &tx).unwrap();
        tx.commit().unwrap();
        assert_eq!(model.read_checkpoint().unwrap().get(), 42);
    }

    #[test]
    fn get_returns_none_then_the_row() {
        let entity = users_entity();
        let (model, _dir) = open_temp(slice::from_ref(&entity));
        assert!(model.get(&entity, "u1").unwrap().is_none());
        put(&model, &entity, "u1", "a@b.c");
        let row = model.get(&entity, "u1").unwrap().unwrap();
        assert_eq!(row["email"], "a@b.c");
        assert_eq!(row["user_id"], "u1");
    }

    #[test]
    fn scan_paginates_by_key_covering_every_row_once() {
        let entity = users_entity();
        let (model, _dir) = open_temp(slice::from_ref(&entity));
        for i in 0..5 {
            put(&model, &entity, &format!("u{i}"), &format!("{i}@x"));
        }
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = model.scan(&entity, None, after.as_deref(), 2).unwrap();
            if page.is_empty() {
                break;
            }
            for row in &page {
                seen.push(row["user_id"].as_str().unwrap().to_owned());
            }
            after = Some(page.last().unwrap()["user_id"].as_str().unwrap().to_owned());
            if page.len() < 2 {
                break;
            }
        }
        assert_eq!(seen, vec!["u0", "u1", "u2", "u3", "u4"]);
    }

    #[test]
    fn scan_filters_on_a_column() {
        let entity = users_entity();
        let (model, _dir) = open_temp(slice::from_ref(&entity));
        put(&model, &entity, "u1", "match@x");
        put(&model, &entity, "u2", "other@x");
        let rows = model
            .scan(&entity, Some(("email", "match@x")), None, 50)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["user_id"], "u1");
    }

    #[test]
    fn scan_filters_on_a_bool_column() {
        // A bool filter must bind as INTEGER 0/1, matching how the column is stored;
        // binding it as the text `"true"` would match nothing (the phase-3 bug).
        let entity = EntityDef {
            id: 1,
            name: "flags".to_owned(),
            key: "user_id".to_owned(),
            fields: vec![
                ("user_id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                ("active".to_owned(), FieldMeta::plain(FieldKind::Bool)),
            ],
            indexes: vec![],
        };
        let (model, _dir) = open_temp(slice::from_ref(&entity));
        for (id, active) in [("u1", true), ("u2", false), ("u3", true)] {
            model
                .apply_one(
                    &entity,
                    EntityOpKind::Put(json!({ "user_id": id, "active": active }).to_string()),
                )
                .unwrap();
        }
        let active = model
            .scan(&entity, Some(("active", "true")), None, 50)
            .unwrap();
        assert_eq!(active.len(), 2);
        let inactive = model
            .scan(&entity, Some(("active", "false")), None, 50)
            .unwrap();
        assert_eq!(inactive.len(), 1);
        assert_eq!(inactive[0]["user_id"], "u2");
    }

    #[test]
    fn keyword_named_identifiers_survive_every_generated_statement() {
        // Table, key, filter column and plain column are all SQLite keywords. Each op
        // below builds a different statement (CREATE, CREATE INDEX, INSERT, SELECT,
        // UPDATE, DELETE), so a single unquoted identifier anywhere is a syntax error.
        let entity = EntityDef {
            id: 1,
            name: "order".to_owned(),
            key: "group".to_owned(),
            fields: vec![
                ("group".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                (
                    "select".to_owned(),
                    FieldMeta::plain(FieldKind::Text { max_length: None }),
                ),
            ],
            indexes: vec![IndexDef {
                name: "by_select".to_owned(),
                columns: vec!["select".to_owned()],
            }],
        };
        let (model, _dir) = open_temp(slice::from_ref(&entity));

        for (key, value) in [("g1", "a"), ("g2", "b")] {
            model
                .apply_one(
                    &entity,
                    EntityOpKind::Put(json!({ "group": key, "select": value }).to_string()),
                )
                .unwrap();
        }
        assert_eq!(model.get(&entity, "g1").unwrap().unwrap()["select"], "a");
        let filtered = model
            .scan(&entity, Some(("select", "b")), None, 50)
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["group"], "g2");
        // The cursor branch adds `key > ?` and the ORDER BY on the keyword key.
        let after = model.scan(&entity, None, Some("g1"), 50).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["group"], "g2");

        model
            .apply_one(
                &entity,
                EntityOpKind::Patch {
                    key: "g1".to_owned(),
                    changes: json!({ "select": "patched" }).to_string(),
                },
            )
            .unwrap();
        assert_eq!(
            model.get(&entity, "g1").unwrap().unwrap()["select"],
            "patched"
        );

        model
            .apply_one(&entity, EntityOpKind::Delete("g1".to_owned()))
            .unwrap();
        assert!(model.get(&entity, "g1").unwrap().is_none());
        assert_eq!(model.rows(&entity).unwrap().len(), 1);
    }

    #[test]
    fn a_read_model_created_with_unquoted_ddl_still_reads_and_writes() {
        // Quoting is a parsing change, not a schema change: SQLite resolves `"group"`
        // and `group` to the same column. A model built by the pre-quoting generator
        // must keep working with no rebuild, so build one by hand and drive it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let mut entity = users_entity();
        entity.indexes.push(IndexDef {
            name: "by_email".to_owned(),
            columns: vec!["email".to_owned()],
        });
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (user_id TEXT PRIMARY KEY NOT NULL, email TEXT NOT NULL);
                 CREATE INDEX users_by_email ON users (email);
                 INSERT INTO users (user_id, email) VALUES ('u0', 'old@x');",
            )
            .unwrap();
        }

        // `IF NOT EXISTS` leaves the hand-built table and index alone; the quoted DDL
        // must resolve to the same names, not conflict with them or duplicate them.
        let model = ReadModel::open(&path, slice::from_ref(&entity)).unwrap();
        assert_eq!(model.get(&entity, "u0").unwrap().unwrap()["email"], "old@x");
        put(&model, &entity, "u1", "new@x");
        assert_eq!(model.rows(&entity).unwrap().len(), 2);
        assert_eq!(
            model
                .scan(&entity, Some(("email", "old@x")), None, 50)
                .unwrap()
                .len(),
            1
        );

        // The stored names carry no quotes: unquoted SQL still resolves them.
        let names: Vec<String> = model
            .conn
            .prepare("SELECT name FROM pragma_table_info('users')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(names, vec!["user_id", "email"]);
        // `sql IS NOT NULL` skips the implicit index behind the TEXT primary key.
        let indexes: Vec<String> = model
            .conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'index' AND tbl_name = 'users' AND sql IS NOT NULL",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            indexes,
            vec!["users_by_email"],
            "the quoted CREATE INDEX names the existing index rather than adding one"
        );
    }

    #[test]
    fn quote_ident_doubles_an_embedded_quote() {
        assert_eq!(quote_ident("group"), "\"group\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    /// Quoting identifiers is only safe with SQLite's double-quoted-string fallback
    /// off. With it on (the default), `SELECT "nope" FROM t` yields the text `nope`
    /// for every row instead of failing, so a read model whose shape predates a field
    /// addition would serve the column's own name as data.
    #[test]
    fn an_unknown_quoted_column_errors_instead_of_becoming_a_string_literal() {
        let (model, _dir) = open_temp(slice::from_ref(&users_entity()));
        let err = model
            .conn
            .query_row(
                &format!("SELECT {} FROM users", quote_ident("nope")),
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("no such column"),
            "expected a missing column to fail loudly, got {err}"
        );
    }

    #[test]
    fn a_read_only_connection_also_rejects_double_quoted_strings() {
        let (model, dir) = open_temp(slice::from_ref(&users_entity()));
        drop(model);
        let reopened = ReadModel::open_readonly(&dir.path().join("m.db")).unwrap();
        let err = reopened
            .conn
            .query_row(
                &format!("SELECT {} FROM users", quote_ident("nope")),
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_err();
        assert!(err.to_string().contains("no such column"), "{err}");
    }

    #[test]
    fn coerce_value_rejects_type_mismatches() {
        assert!(coerce_value(&FieldKind::I64, "42").is_ok());
        assert!(coerce_value(&FieldKind::I64, "abc").is_err());
        assert!(coerce_value(&FieldKind::Bool, "true").is_ok());
        assert!(coerce_value(&FieldKind::Bool, "maybe").is_err());
        // Optional unwraps to the inner kind.
        let opt_int = FieldKind::Optional(Box::new(FieldKind::I64));
        assert!(coerce_value(&opt_int, "7").is_ok());
        assert!(coerce_value(&opt_int, "seven").is_err());
    }

    #[test]
    fn a_json_column_that_does_not_parse_reads_back_as_its_raw_text() {
        // A column declared str() in one deploy and json() in the next, with no
        // rebuild: the surviving plaintext rows must read back as the raw string.
        // Decoding them as null would drop the key from the row entirely, reporting
        // bad data as absent data.
        let as_text = EntityDef {
            id: 1,
            name: "docs".to_owned(),
            key: "doc_id".to_owned(),
            fields: vec![
                ("doc_id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                (
                    "body".to_owned(),
                    FieldMeta::plain(FieldKind::Text { max_length: None }),
                ),
            ],
            indexes: vec![],
        };
        let (model, _dir) = open_temp(slice::from_ref(&as_text));
        model
            .apply_one(
                &as_text,
                EntityOpKind::Put(json!({ "doc_id": "d1", "body": "not json" }).to_string()),
            )
            .unwrap();

        let mut as_json = as_text.clone();
        as_json.fields[1].1 = FieldMeta::plain(FieldKind::Json);
        let row = model.get(&as_json, "d1").unwrap().unwrap();
        assert_eq!(row["body"], "not json");
    }
}
