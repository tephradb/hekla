//! The operational database (`kiln.db`).
//!
//! One shared SQLite database holding runtime bookkeeping that is not domain
//! truth and never belongs in the event log: the effect journal and its per-effect
//! cursor, effect invocations, and deployed-module metadata. Command idempotency is
//! not here: it lives in the event log itself, guarded by a per-request tag on the
//! append (see [`crate::dispatch`]). This module owns the schema and its migrations,
//! and exposes the short, single-statement operations the effect runtime calls under
//! a shared lock.

use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

/// The current schema version, tracked in SQLite's `user_version`. Bump it and
/// add a migration arm when the schema changes.
pub const SCHEMA_VERSION: i64 = 2;

/// How many rows a single sweep statement deletes, so a retention sweep never
/// holds the connection across a long scan. The sweeper loops until a call
/// deletes fewer than this.
pub const SWEEP_CHUNK: usize = 1000;

/// The operational database handle.
pub struct OpDb {
    conn: Connection,
}

/// The state of an effect invocation after reserving it, deciding whether the
/// driver runs (or replays) the handler or skips a position already completed.
pub enum InvocationState {
    /// New, or left `running` by a crash: run the handler (journaled calls
    /// replay, the unjournaled tail runs live).
    Running,
    /// A prior run already reached `terminal`: skip this position.
    AlreadyTerminal,
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
        // WAL keeps a journal write from blocking a concurrent read once the
        // operational DB grows beyond a single connection. A no-op (stays `memory`)
        // for the in-memory database used in tests.
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

    // --- effect invocations and journal ------------------------------------

    /// Reserve an effect invocation for `position`, then report whether to run it.
    /// The `running` row is inserted once and left in place across replays; a
    /// crash leaves it `running` so the next boot re-runs the handler (its
    /// journaled calls replay). A row already `terminal` means the position is
    /// done and the driver skips it.
    pub fn begin_invocation(
        &self,
        effect: &str,
        position: u64,
        script_hash: &str,
        now: &str,
    ) -> anyhow::Result<InvocationState> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO effect_invocation \
                 (effect, position, script_hash, status, created_at) \
                 VALUES (?1, ?2, ?3, 'running', ?4)",
                params![effect, position as i64, script_hash, now],
            )
            .context("beginning effect invocation")?;
        let status: String = self
            .conn
            .query_row(
                "SELECT status FROM effect_invocation WHERE effect = ?1 AND position = ?2",
                params![effect, position as i64],
                |row| row.get(0),
            )
            .context("reading effect invocation status")?;
        match status.as_str() {
            "terminal" => Ok(InvocationState::AlreadyTerminal),
            _ => Ok(InvocationState::Running),
        }
    }

    /// Mark an invocation `terminal`. This is the journaled terminal step: it is
    /// idempotent, and the sweeper only reclaims rows once they reach it.
    pub fn complete_invocation(
        &self,
        effect: &str,
        position: u64,
        now: &str,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE effect_invocation SET status = 'terminal', completed_at = ?3 \
                 WHERE effect = ?1 AND position = ?2",
                params![effect, position as i64, now],
            )
            .context("completing effect invocation")?;
        Ok(())
    }

    /// The recorded result of a journaled call, or `None` on a miss. A hit lets a
    /// replay return the original result instead of performing the side effect.
    pub fn journal_get(
        &self,
        effect: &str,
        position: u64,
        call_hash: &str,
        disambiguator: u64,
    ) -> anyhow::Result<Option<String>> {
        let result = self
            .conn
            .query_row(
                "SELECT result FROM effect_journal \
                 WHERE effect = ?1 AND position = ?2 AND call_hash = ?3 AND disambiguator = ?4",
                params![effect, position as i64, call_hash, disambiguator as i64],
                |row| row.get(0),
            )
            .optional()
            .context("reading effect journal")?;
        Ok(result)
    }

    /// Record a journaled call's result. Runs after the real side effect succeeded
    /// and commits on its own, so it survives a crash and replay skips the call.
    pub fn journal_put(
        &self,
        effect: &str,
        position: u64,
        call_hash: &str,
        disambiguator: u64,
        result: &str,
        now: &str,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO effect_journal \
                 (effect, position, call_hash, disambiguator, result, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    effect,
                    position as i64,
                    call_hash,
                    disambiguator as i64,
                    result,
                    now
                ],
            )
            .context("recording effect journal entry")?;
        Ok(())
    }

    /// The effect's durable resume point: the watermark it has processed every
    /// matching event up to. `0` if it has never run.
    pub fn effect_resume_after(&self, effect: &str) -> anyhow::Result<u64> {
        let watermark: Option<i64> = self
            .conn
            .query_row(
                "SELECT watermark FROM effect_cursor WHERE effect = ?1",
                params![effect],
                |row| row.get(0),
            )
            .optional()
            .context("reading effect cursor")?;
        Ok(watermark.unwrap_or(0) as u64)
    }

    /// Advance the effect's watermark. Only valid once every matching position up
    /// to `watermark` is `terminal`; the sequential driver guarantees that.
    pub fn set_effect_watermark(&self, effect: &str, watermark: u64) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO effect_cursor (effect, watermark) VALUES (?1, ?2) \
                 ON CONFLICT(effect) DO UPDATE SET watermark = excluded.watermark",
                params![effect, watermark as i64],
            )
            .context("advancing effect cursor")?;
        Ok(())
    }

    /// Positions of this effect's still-`running` invocations whose recorded
    /// script hash differs from `current_hash`, for the restart warning.
    pub fn running_with_hash_mismatch(
        &self,
        effect: &str,
        current_hash: &str,
    ) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT position FROM effect_invocation \
                 WHERE effect = ?1 AND status = 'running' AND script_hash <> ?2 ORDER BY position",
            )
            .context("preparing effect hash-mismatch query")?;
        let rows = stmt
            .query_map(params![effect, current_hash], |row| {
                let position: i64 = row.get(0)?;
                Ok(position as u64)
            })
            .context("querying running invocations")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("reading running invocation position")?);
        }
        Ok(out)
    }

    /// Record what is deployed: one row per loaded module, keyed by name and kind.
    pub fn upsert_module_metadata(
        &self,
        name: &str,
        kind: &str,
        source_hash: &str,
        now: &str,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO module_metadata (name, kind, source_hash, loaded_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(name, kind) DO UPDATE SET \
                 source_hash = excluded.source_hash, loaded_at = excluded.loaded_at",
                params![name, kind, source_hash, now],
            )
            .context("recording module metadata")?;
        Ok(())
    }

    /// Delete up to `limit` `terminal` effect invocations completed before
    /// `cutoff`, cascading to their journal rows. Returns the count deleted, so
    /// the sweeper loops until a call clears fewer than `limit`.
    pub fn sweep_effect_journal(&self, cutoff: &str, limit: usize) -> anyhow::Result<usize> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM effect_invocation WHERE rowid IN (\
                 SELECT rowid FROM effect_invocation \
                 WHERE status = 'terminal' AND completed_at < ?1 LIMIT ?2)",
                params![cutoff, limit as i64],
            )
            .context("sweeping effect journal")?;
        Ok(deleted)
    }

    fn migrate(&mut self) -> anyhow::Result<()> {
        let mut version: i64 = self.schema_version()?;
        while version < SCHEMA_VERSION {
            match version {
                0 => self
                    .conn
                    .execute_batch(SCHEMA_V1)
                    .context("applying schema v1")?,
                1 => self
                    .conn
                    .execute_batch(SCHEMA_V2)
                    .context("applying schema v2")?,
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

/// The initial schema. Table shapes anticipate the effect runtime so later phases
/// add behaviour, not columns. Command idempotency has no table here: it lives in
/// the event log, guarded by a per-request append tag (see `crate::dispatch`).
const SCHEMA_V1: &str = "
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

/// Schema v2 adds the effect runtime's durable cursor and lets the sweeper reclaim
/// a completed invocation's journal in one delete. `effect_journal` is recreated
/// with `ON DELETE CASCADE` (safe: v1 never wrote it, so it is empty here).
const SCHEMA_V2: &str = "
-- The effect's durable resume point: the watermark position it has processed
-- every matching event up to. Advanced only once a batch is fully terminal, so a
-- crash resumes by re-scanning from the last completed batch, never skipping.
CREATE TABLE effect_cursor (
    effect    TEXT    NOT NULL PRIMARY KEY,
    watermark INTEGER NOT NULL
);

DROP TABLE effect_journal;
CREATE TABLE effect_journal (
    effect        TEXT    NOT NULL,
    position      INTEGER NOT NULL,
    call_hash     TEXT    NOT NULL,
    disambiguator INTEGER NOT NULL DEFAULT 0,
    result        TEXT    NOT NULL,  -- JSON of the recorded call result
    created_at    TEXT    NOT NULL,
    PRIMARY KEY (effect, position, call_hash, disambiguator),
    FOREIGN KEY (effect, position) REFERENCES effect_invocation (effect, position)
        ON DELETE CASCADE
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
        for table in ["effect_invocation", "effect_journal", "module_metadata"] {
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
    fn begin_invocation_is_running_then_terminal_then_skipped() {
        let db = OpDb::open_in_memory().unwrap();
        assert!(matches!(
            db.begin_invocation("e", 1, "h", "t0").unwrap(),
            InvocationState::Running
        ));
        // A second begin before completion still runs (crash replay).
        assert!(matches!(
            db.begin_invocation("e", 1, "h", "t1").unwrap(),
            InvocationState::Running
        ));
        db.complete_invocation("e", 1, "t2").unwrap();
        assert!(matches!(
            db.begin_invocation("e", 1, "h", "t3").unwrap(),
            InvocationState::AlreadyTerminal
        ));
    }

    #[test]
    fn journal_round_trips_by_hash_and_disambiguator() {
        let db = OpDb::open_in_memory().unwrap();
        db.begin_invocation("e", 1, "h", "t0").unwrap();
        assert_eq!(db.journal_get("e", 1, "abc", 0).unwrap(), None);
        db.journal_put("e", 1, "abc", 0, r#"{"n":1}"#, "t1")
            .unwrap();
        db.journal_put("e", 1, "abc", 1, r#"{"n":2}"#, "t2")
            .unwrap();
        assert_eq!(
            db.journal_get("e", 1, "abc", 0).unwrap().as_deref(),
            Some(r#"{"n":1}"#)
        );
        assert_eq!(
            db.journal_get("e", 1, "abc", 1).unwrap().as_deref(),
            Some(r#"{"n":2}"#)
        );
    }

    #[test]
    fn effect_cursor_defaults_to_zero_then_advances() {
        let db = OpDb::open_in_memory().unwrap();
        assert_eq!(db.effect_resume_after("e").unwrap(), 0);
        db.set_effect_watermark("e", 7).unwrap();
        assert_eq!(db.effect_resume_after("e").unwrap(), 7);
        db.set_effect_watermark("e", 12).unwrap();
        assert_eq!(db.effect_resume_after("e").unwrap(), 12);
    }

    #[test]
    fn running_with_hash_mismatch_lists_only_stale_running() {
        let db = OpDb::open_in_memory().unwrap();
        db.begin_invocation("e", 1, "old", "t0").unwrap(); // running, stale hash
        db.begin_invocation("e", 2, "new", "t0").unwrap(); // running, current hash
        db.begin_invocation("e", 3, "old", "t0").unwrap();
        db.complete_invocation("e", 3, "t1").unwrap(); // terminal, ignored
        assert_eq!(db.running_with_hash_mismatch("e", "new").unwrap(), vec![1]);
    }

    #[test]
    fn upsert_module_metadata_replaces_on_conflict() {
        let db = OpDb::open_in_memory().unwrap();
        db.upsert_module_metadata("m", "effect", "h1", "t0")
            .unwrap();
        db.upsert_module_metadata("m", "effect", "h2", "t1")
            .unwrap();
        let hash: String = db
            .connection()
            .query_row(
                "SELECT source_hash FROM module_metadata WHERE name = 'm' AND kind = 'effect'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash, "h2");
    }

    #[test]
    fn sweep_removes_only_old_terminal_and_cascades_the_journal() {
        let db = OpDb::open_in_memory().unwrap();
        // Old terminal: swept, journal cascades.
        db.begin_invocation("e", 1, "h", "t0").unwrap();
        db.journal_put("e", 1, "abc", 0, "{}", "t0").unwrap();
        db.complete_invocation("e", 1, "2026-01-01T00:00:00Z")
            .unwrap();
        // Recent terminal: kept.
        db.begin_invocation("e", 2, "h", "t0").unwrap();
        db.complete_invocation("e", 2, "2026-12-31T00:00:00Z")
            .unwrap();
        // Old but still running: kept (never sweep in-flight work).
        db.begin_invocation("e", 3, "h", "2026-01-01T00:00:00Z")
            .unwrap();

        let deleted = db
            .sweep_effect_journal("2026-06-01T00:00:00Z", SWEEP_CHUNK)
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(db.journal_get("e", 1, "abc", 0).unwrap(), None);
        assert!(matches!(
            db.begin_invocation("e", 2, "h", "t9").unwrap(),
            InvocationState::AlreadyTerminal
        ));
        assert!(matches!(
            db.begin_invocation("e", 3, "h", "t9").unwrap(),
            InvocationState::Running
        ));
    }
}
