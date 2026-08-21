//! The operational database (`kiln.db`).
//!
//! One shared SQLite database holding runtime bookkeeping that is not domain
//! truth and never belongs in the event log: command idempotency keys, the
//! effect journal, and deployed-module metadata. This module owns the schema and
//! its migrations. The rows are written and read by the command and effect
//! runtimes in later phases; landing the schema now means those phases build on a
//! stable shape rather than reshaping tables under live data.

use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

/// The current schema version, tracked in SQLite's `user_version`. Bump it and
/// add a migration arm when the schema changes.
pub const SCHEMA_VERSION: i64 = 1;

/// The operational database handle.
pub struct OpDb {
    conn: Connection,
}

impl OpDb {
    /// Open (or create) `kiln.db` at `path` and bring its schema up to date.
    /// Idempotent: opening an already-migrated database is a no-op.
    pub fn open(path: &Path) -> anyhow::Result<OpDb> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening operational database {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// Open an in-memory operational database. For tests.
    pub fn open_in_memory() -> anyhow::Result<OpDb> {
        let conn =
            Connection::open_in_memory().context("opening in-memory operational database")?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<OpDb> {
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enabling foreign keys")?;
        let mut db = OpDb { conn };
        db.migrate()?;
        Ok(db)
    }

    /// The connection, for the runtimes that read and write these tables.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// The schema version recorded in the database.
    pub fn schema_version(&self) -> anyhow::Result<i64> {
        let version = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("reading schema version")?;
        Ok(version)
    }

    fn migrate(&mut self) -> anyhow::Result<()> {
        let mut version: i64 = self.schema_version()?;
        while version < SCHEMA_VERSION {
            match version {
                0 => self
                    .conn
                    .execute_batch(SCHEMA_V1)
                    .context("applying schema v1")?,
                other => anyhow::bail!("no migration from schema version {other}"),
            }
            version += 1;
            self.conn
                .pragma_update(None, "user_version", version)
                .context("recording schema version")?;
        }
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "operational database is at schema version {version}, newer than this build ({SCHEMA_VERSION})"
            );
        }
        Ok(())
    }
}

/// The initial schema. Table shapes anticipate the command and effect runtimes
/// so those phases add behaviour, not columns.
const SCHEMA_V1: &str = "
-- Command idempotency keys. The lookup is global per command, not scoped to a
-- boundary: a retry of a rejected command produced no event, so there would be
-- nothing in the boundary to find. On a hit the runtime returns the original
-- outcome, including rejections.
CREATE TABLE idempotency (
    command    TEXT    NOT NULL,
    key        TEXT    NOT NULL,
    status     INTEGER NOT NULL,   -- HTTP status of the original outcome
    outcome    TEXT    NOT NULL,   -- JSON of the original response body
    created_at TEXT    NOT NULL,   -- ISO-8601, drives the retention sweeper
    PRIMARY KEY (command, key)
);

-- One effect invocation: an effect reacting to a single event position. The
-- recorded script hash lets a restart warn when in-flight code changed under it.
CREATE TABLE effect_invocation (
    effect       TEXT    NOT NULL,
    position     INTEGER NOT NULL,  -- tephra position of the triggering event
    script_hash  TEXT    NOT NULL,
    status       TEXT    NOT NULL,  -- 'running' | 'terminal'
    created_at   TEXT    NOT NULL,
    completed_at TEXT,              -- set once terminal, drives the sweeper
    PRIMARY KEY (effect, position)
);

-- The journal of side-effect calls within an invocation, keyed by the content
-- hash of the call plus a disambiguator for legitimately-identical repeats, so
-- editing or reordering the script does not corrupt replay.
CREATE TABLE effect_journal (
    effect        TEXT    NOT NULL,
    position      INTEGER NOT NULL,
    call_hash     TEXT    NOT NULL,
    disambiguator INTEGER NOT NULL DEFAULT 0,
    result        TEXT    NOT NULL,  -- JSON of the recorded call result
    created_at    TEXT    NOT NULL,
    PRIMARY KEY (effect, position, call_hash, disambiguator),
    FOREIGN KEY (effect, position) REFERENCES effect_invocation (effect, position)
);

-- What is deployed: one row per loaded module, its source hash and kind. Lets a
-- restart detect changed modules and anchors future version pinning.
CREATE TABLE module_metadata (
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    loaded_at   TEXT NOT NULL,
    PRIMARY KEY (name, kind)
);
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_at_current_schema_version() {
        let db = OpDb::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn creates_every_table() {
        let db = OpDb::open_in_memory().unwrap();
        for table in [
            "idempotency",
            "effect_invocation",
            "effect_journal",
            "module_metadata",
        ] {
            let count: i64 = db
                .connection()
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table `{table}` should exist");
        }
    }

    #[test]
    fn reopening_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kiln.db");
        OpDb::open(&path).unwrap();
        let reopened = OpDb::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
    }
}
