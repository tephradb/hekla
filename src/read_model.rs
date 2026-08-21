//! SQLite-backed read model for projectors.
//!
//! Each declared `entity(...)` becomes a real SQLite table (created from the
//! `EntityDef`'s generated DDL), and the projector's `put`/`patch`/`delete` ops
//! map onto `INSERT OR REPLACE` / `UPDATE` / `DELETE`. Values are bound as typed
//! columns per the entity's declared `FieldKind`s.

use std::path::Path;
use std::str;

use anyhow::Context;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, params_from_iter};

use crate::starlark_builtins::{EntityDef, EntityOpKind, FieldKind};

/// A projector's materialised read model: one SQLite table per entity.
pub struct ReadModel {
    conn: Connection,
}

impl ReadModel {
    /// Open (or create) the database at `path` and create each entity's table
    /// and indexes. Pass a filesystem path; the tables use `IF NOT EXISTS`, so
    /// reopening an existing read model is fine.
    pub fn open(path: &Path, entities: &[EntityDef]) -> anyhow::Result<ReadModel> {
        let conn = Connection::open(path).context("opening read-model database")?;
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

    /// Apply one entity op: `put` → `INSERT OR REPLACE`, `patch` → `UPDATE`
    /// (naturally a no-op when zero rows match), `delete` → `DELETE`.
    pub fn apply(&self, entity: &EntityDef, op: EntityOpKind) -> anyhow::Result<()> {
        match op {
            EntityOpKind::Put(row_json) => {
                let row: serde_json::Value = serde_json::from_str(&row_json)?;
                let obj = row.as_object().context("put row is not a JSON object")?;
                let columns: Vec<&str> = entity.fields.iter().map(|(n, _)| n.as_str()).collect();
                let placeholders = vec!["?"; columns.len()].join(", ");
                let sql = format!(
                    "INSERT OR REPLACE INTO {} ({}) VALUES ({})",
                    entity.name,
                    columns.join(", "),
                    placeholders
                );
                let mut values = Vec::with_capacity(columns.len());
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
        let columns: Vec<&str> = entity.fields.iter().map(|(n, _)| n.as_str()).collect();
        let sql = format!(
            "SELECT {} FROM {} ORDER BY {}",
            columns.join(", "),
            entity.name,
            entity.key
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut obj = serde_json::Map::new();
            for (i, (name, kind)) in entity.fields.iter().enumerate() {
                let value = from_sql(kind, row.get_ref(i)?)?;
                if !value.is_null() {
                    obj.insert(name.clone(), value);
                }
            }
            out.push(serde_json::Value::Object(obj));
        }
        Ok(out)
    }
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

/// Bind an op's key string as the key column's type (keys arrive as strings).
fn key_to_sql(kind: &FieldKind, key: &str) -> SqlValue {
    match kind.base() {
        FieldKind::I64 | FieldKind::U64 | FieldKind::Money => key
            .parse::<i64>()
            .map(SqlValue::Integer)
            .unwrap_or_else(|_| SqlValue::Text(key.to_owned())),
        _ => SqlValue::Text(key.to_owned()),
    }
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
