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
use rusqlite::{Connection, params};

/// The current schema version, tracked in SQLite's `user_version`. Bump it and
/// add a migration arm when the schema changes.
pub const SCHEMA_VERSION: i64 = 1;

/// The operational database handle.
pub struct OpDb {
    conn: Connection,
}

/// The result of trying to reserve an idempotency key.
pub enum Reserve {
    /// This request now owns the key: run the command, then `finalize`.
    Acquired,
    /// A previous execution already completed; replay its stored outcome.
    Done { status: u16, outcome: String },
    /// Another execution holds the key and has not finished yet.
    Pending,
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
        // WAL keeps a finalize write from blocking a concurrent reserve read once
        // the operational DB grows beyond a single connection. A no-op (stays
        // `memory`) for the in-memory database used in tests.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_row| Ok(()))
            .context("enabling WAL")?;
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

    /// Reserve an idempotency key for a command. The caller holds the DB lock only
    /// for this call; the reserved `pending` row is what excludes a concurrent
    /// duplicate while the command runs. On `Acquired` the caller must eventually
    /// `finalize` (success) or `release` (internal error).
    pub fn reserve(&self, command: &str, key: &str, now: &str) -> anyhow::Result<Reserve> {
        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO idempotency (command, key, state, created_at) \
                 VALUES (?1, ?2, 'pending', ?3)",
                params![command, key, now],
            )
            .context("reserving idempotency key")?;
        if inserted == 1 {
            return Ok(Reserve::Acquired);
        }
        let row = self
            .conn
            .query_row(
                "SELECT state, status, outcome FROM idempotency WHERE command = ?1 AND key = ?2",
                params![command, key],
                |row| {
                    let state: String = row.get(0)?;
                    let status: Option<i64> = row.get(1)?;
                    let outcome: Option<String> = row.get(2)?;
                    Ok((state, status, outcome))
                },
            )
            .context("reading idempotency key")?;
        match row.0.as_str() {
            "done" => Ok(Reserve::Done {
                status: row.1.unwrap_or(500) as u16,
                outcome: row.2.unwrap_or_default(),
            }),
            _ => Ok(Reserve::Pending),
        }
    }

    /// Record the terminal outcome for a reserved key, so a later replay returns
    /// exactly this status and body.
    pub fn finalize(
        &self,
        command: &str,
        key: &str,
        status: u16,
        outcome: &str,
        now: &str,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE idempotency SET state = 'done', status = ?3, outcome = ?4, \
                 completed_at = ?5 WHERE command = ?1 AND key = ?2",
                params![command, key, status as i64, outcome, now],
            )
            .context("finalizing idempotency key")?;
        Ok(())
    }

    /// Drop a reservation whose command failed with an internal error, so a retry
    /// can proceed rather than caching a transient failure.
    pub fn release(&self, command: &str, key: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "DELETE FROM idempotency WHERE command = ?1 AND key = ?2 AND state = 'pending'",
                params![command, key],
            )
            .context("releasing idempotency key")?;
        Ok(())
    }

    /// Clear all `pending` reservations. Run at startup: a pending row can only be
    /// stale (the process that owned it is gone), and clearing it frees the key.
    pub fn clear_pending(&self) -> anyhow::Result<usize> {
        let cleared = self
            .conn
            .execute("DELETE FROM idempotency WHERE state = 'pending'", [])
            .context("clearing pending idempotency keys")?;
        Ok(cleared)
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
-- nothing in the boundary to find. A row is reserved `pending` before the command
-- runs (the mutual-exclusion token for concurrent duplicates) and moved to `done`
-- with the outcome once it finishes; on a hit the runtime returns the original
-- outcome, including rejections.
CREATE TABLE idempotency (
    command      TEXT    NOT NULL,
    key          TEXT    NOT NULL,
    state        TEXT    NOT NULL CHECK (state IN ('pending', 'done')),
    status       INTEGER,            -- HTTP status of the original outcome, set on finalize
    outcome      TEXT,               -- JSON of the original response body, set on finalize
    created_at   TEXT    NOT NULL,   -- ISO-8601, drives the retention sweeper
    completed_at TEXT,               -- set on finalize
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

    #[test]
    fn first_reserve_acquires_second_is_pending() {
        let db = OpDb::open_in_memory().unwrap();
        assert!(matches!(
            db.reserve("c", "k", "t0").unwrap(),
            Reserve::Acquired
        ));
        assert!(matches!(
            db.reserve("c", "k", "t1").unwrap(),
            Reserve::Pending
        ));
    }

    #[test]
    fn finalize_makes_replay_return_the_stored_outcome() {
        let db = OpDb::open_in_memory().unwrap();
        db.reserve("c", "k", "t0").unwrap();
        db.finalize("c", "k", 200, r#"{"ok":true}"#, "t1").unwrap();
        match db.reserve("c", "k", "t2").unwrap() {
            Reserve::Done { status, outcome } => {
                assert_eq!(status, 200);
                assert_eq!(outcome, r#"{"ok":true}"#);
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn release_frees_the_key() {
        let db = OpDb::open_in_memory().unwrap();
        db.reserve("c", "k", "t0").unwrap();
        db.release("c", "k").unwrap();
        assert!(matches!(
            db.reserve("c", "k", "t1").unwrap(),
            Reserve::Acquired
        ));
    }

    #[test]
    fn clear_pending_frees_reservations_but_not_done() {
        let db = OpDb::open_in_memory().unwrap();
        db.reserve("c", "pending", "t0").unwrap();
        db.reserve("c", "done", "t0").unwrap();
        db.finalize("c", "done", 200, "{}", "t1").unwrap();
        assert_eq!(db.clear_pending().unwrap(), 1);
        assert!(matches!(
            db.reserve("c", "pending", "t2").unwrap(),
            Reserve::Acquired
        ));
        assert!(matches!(
            db.reserve("c", "done", "t2").unwrap(),
            Reserve::Done { .. }
        ));
    }
}
