//! SQLite-backed read model for projectors.
//!
//! Each declared `entity(...)` becomes a real SQLite table (created from the
//! `EntityDef`'s generated DDL), and the projector's `put`/`patch`/`delete` ops
//! map onto `INSERT OR REPLACE` / `UPDATE` / `DELETE`. Values are bound as typed
//! columns per the entity's declared `FieldKind`s.
//!
//! Alongside the entity tables sits a single-row `_kiln_checkpoint`, so a batch
//! of ops and the position it advances to commit in one transaction: state and
//! progress can never disagree. The read API opens the same file read-only.

use std::path::Path;
use std::str;

use anyhow::Context;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, Row, Transaction, params_from_iter};
use tephra::Position;

use crate::starlark_builtins::{EntityDef, EntityOpKind, FieldKind};

/// The projector checkpoint, co-located with the read-model tables so it commits
/// in the same transaction as the state it describes. `completed_above` records
/// positions processed above `position`; it is always empty under the sequential
/// model, reserved so parallel lanes need no schema migration.
const CHECKPOINT_DDL: &str = "\
CREATE TABLE IF NOT EXISTS _kiln_checkpoint (
    id              INTEGER PRIMARY KEY CHECK (id = 0),
    position        INTEGER NOT NULL,
    completed_above TEXT    NOT NULL DEFAULT '[]'
);
INSERT OR IGNORE INTO _kiln_checkpoint (id, position) VALUES (0, 0);";

/// A projector's materialised read model: one SQLite table per entity.
pub struct ReadModel {
    conn: Connection,
}

impl ReadModel {
    /// Open (or create) the database at `path` and create each entity's table
    /// and indexes, plus the checkpoint. Pass a filesystem path; the tables use
    /// `IF NOT EXISTS`, so reopening an existing read model is fine.
    pub fn open(path: &Path, entities: &[EntityDef]) -> anyhow::Result<ReadModel> {
        let conn = Connection::open(path).context("opening read-model database")?;
        // WAL lets the read API read concurrently with this single writer.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_row| Ok(()))
            .context("enabling WAL")?;
        conn.execute_batch(CHECKPOINT_DDL)
            .context("creating the checkpoint table")?;
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
                "SELECT position FROM _kiln_checkpoint WHERE id = 0",
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
            "UPDATE _kiln_checkpoint SET position = ?1, completed_above = '[]' WHERE id = 0",
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
                    "INSERT OR REPLACE INTO {} ({}) VALUES ({})",
                    entity.name,
                    column_list(entity),
                    placeholders
                );
                let mut values = Vec::with_capacity(entity.fields.len());
                for (name, kind) in &entity.fields {
                    let value = obj.get(name).unwrap_or(&serde_json::Value::Null);
                    values.push(to_sql(kind, value).with_context(|| format!("column `{name}`"))?);
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
                for (name, kind) in &entity.fields {
                    if let Some(value) = obj.get(name) {
                        assignments.push(format!("{name} = ?"));
                        values
                            .push(to_sql(kind, value).with_context(|| format!("column `{name}`"))?);
                    }
                }
                if assignments.is_empty() {
                    return Ok(());
                }
                values.push(key_to_sql(key_kind(entity), &key));
                let sql = format!(
                    "UPDATE {} SET {} WHERE {} = ?",
                    entity.name,
                    assignments.join(", "),
                    entity.key
                );
                self.conn.execute(&sql, params_from_iter(values))?;
            }
            EntityOpKind::Delete(key) => {
                let sql = format!("DELETE FROM {} WHERE {} = ?", entity.name, entity.key);
                self.conn
                    .execute(&sql, params_from_iter([key_to_sql(key_kind(entity), &key)]))?;
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
            entity.name, entity.key
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter([key_to_sql(key_kind(entity), key)]))?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_json(entity, row)?)),
            None => Ok(None),
        }
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
            clauses.push(format!("{column} = ?"));
            let kind = entity
                .fields
                .iter()
                .find(|(name, _)| name == column)
                .map(|(_, kind)| kind);
            binds.push(match kind {
                Some(kind) => coerce_value(kind, value).unwrap_or_else(|_| text(value)),
                None => text(value),
            });
        }
        if let Some(after) = after_key {
            clauses.push(format!("{} > ?", entity.key));
            binds.push(key_to_sql(key_kind(entity), after));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let sql = format!(
            "SELECT {columns} FROM {}{where_clause} ORDER BY {} LIMIT ?",
            entity.name, entity.key
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
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reconstruct one selected row as a JSON object, NULL columns omitted, typed per
/// the entity's declared kinds. Columns must be selected in `entity.fields` order.
fn row_to_json(entity: &EntityDef, row: &Row) -> anyhow::Result<serde_json::Value> {
    let mut obj = serde_json::Map::new();
    for (i, (name, kind)) in entity.fields.iter().enumerate() {
        let value = from_sql(kind, row.get_ref(i)?)?;
        if !value.is_null() {
            obj.insert(name.clone(), value);
        }
    }
    Ok(serde_json::Value::Object(obj))
}

fn key_kind(entity: &EntityDef) -> &FieldKind {
    entity
        .fields
        .iter()
        .find(|(name, _)| name == &entity.key)
        .map(|(_, kind)| kind)
        .expect("the key is a declared field")
}

/// Bind a JSON value as a typed SQLite value per the field's declared kind.
fn to_sql(kind: &FieldKind, value: &serde_json::Value) -> anyhow::Result<SqlValue> {
    if value.is_null() {
        return Ok(SqlValue::Null);
    }
    Ok(match kind.base() {
        FieldKind::Bool => {
            let n = value
                .as_bool()
                .map(|b| b as i64)
                .or_else(|| value.as_i64())
                .context("expected a boolean")?;
            SqlValue::Integer(n)
        }
        FieldKind::I64 | FieldKind::U64 | FieldKind::Money => {
            SqlValue::Integer(value.as_i64().context("expected an integer")?)
        }
        FieldKind::Text { .. } | FieldKind::Uuid | FieldKind::Timestamp | FieldKind::OneOf(_) => {
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
        FieldKind::I64 | FieldKind::U64 | FieldKind::Money => {
            SqlValue::Integer(raw.parse::<i64>().context("expected an integer")?)
        }
        _ => SqlValue::Text(raw.to_owned()),
    })
}

/// Bind an op's key string as the key column's type, falling back to text when it
/// does not parse. Internal ops supply well-typed keys; the external filter path
/// validates the value first (via [`coerce_value`]) so a mismatch 400s rather than
/// reaching here.
fn key_to_sql(kind: &FieldKind, key: &str) -> SqlValue {
    coerce_value(kind, key).unwrap_or_else(|_| text(key))
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

/// Reconstruct a JSON value from a stored column per the field's declared kind.
fn from_sql(kind: &FieldKind, value: ValueRef) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value as J;
    Ok(match value {
        ValueRef::Null => J::Null,
        ValueRef::Integer(i) => match kind.base() {
            FieldKind::Bool => J::Bool(i != 0),
            _ => J::Number(i.into()),
        },
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(J::Number)
            .unwrap_or(J::Null),
        ValueRef::Text(bytes) => {
            let text = str::from_utf8(bytes).context("non-UTF-8 text column")?;
            match kind.base() {
                FieldKind::Json => serde_json::from_str(text).unwrap_or(J::Null),
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

    fn users_entity() -> EntityDef {
        EntityDef {
            id: 1,
            name: "users".to_owned(),
            key: "user_id".to_owned(),
            fields: vec![
                ("user_id".to_owned(), FieldKind::Uuid),
                ("email".to_owned(), FieldKind::Text { max_length: None }),
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
                ("user_id".to_owned(), FieldKind::Uuid),
                ("active".to_owned(), FieldKind::Bool),
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
}
