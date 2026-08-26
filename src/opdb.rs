//! The operational database (`hekla.db`).
//!
//! One shared SQLite database holding runtime bookkeeping that is not domain
//! truth and never belongs in the event log: the effect journal and its per-effect
//! cursor, effect invocations, deployed-module metadata, and the per-subject
//! encryption keys behind field-level erasure. Command idempotency is not here: it
//! lives in the event log itself, guarded by a per-request tag on the append (see
//! [`crate::dispatch`]). This module owns the schema and its migrations, and exposes
//! the short, single-statement operations the effect runtime calls under a shared
//! lock.

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

/// The current schema version, tracked in SQLite's `user_version`. Bump it and
/// add a migration arm when the schema changes.
pub const SCHEMA_VERSION: i64 = 4;

/// How many rows a single sweep statement deletes, so a retention sweep never
/// holds the connection across a long scan. The sweeper loops until a call
/// deletes fewer than this.
pub const SWEEP_CHUNK: usize = 1000;

/// The operational database handle.
pub struct OpDb {
    conn: Connection,
}

/// One subject-key row: `(subject_field, subject_value, wrapped_key, master_key_id)`.
pub type SubjectKeyRow = (String, String, Vec<u8>, String);

/// One rewrap for a master rotation: a [`SubjectKeyRow`] with the new wrapped key and
/// new master id, plus the master id the row was expected to be under (a compare-and-set
/// guard so a concurrent erase-then-recreate of the same subject is not clobbered).
/// `(subject_field, subject_value, new_wrapped_key, new_master_id, expected_master_id)`.
pub type RewrapUpdate = (String, String, Vec<u8>, String, String);

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
    /// Open (or create) `hekla.db` at `path` and bring its schema up to date.
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
        // A second connection (the `hekla erase`/`rotate` CLI against a live server)
        // waits for the write lock rather than failing immediately with SQLITE_BUSY.
        conn.busy_timeout(Duration::from_secs(5))
            .context("setting busy timeout")?;
        let mut db = OpDb { conn };
        db.migrate()?;
        Ok(db)
    }

    /// The raw connection, for tests that assert directly on the schema.
    #[cfg(test)]
    fn connection(&self) -> &Connection {
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

    /// Record a verify-mode quarantine, so a restart honours it instead of resuming
    /// the effect as though nothing had been found.
    pub fn quarantine_effect(
        &self,
        effect: &str,
        position: u64,
        reason: &str,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO effect_quarantine (effect, position, reason) \
                 VALUES (?1, ?2, ?3)",
                params![effect, position as i64, reason],
            )
            .context("recording an effect quarantine")?;
        Ok(())
    }

    /// The recorded quarantine for an effect, if it has one.
    pub fn effect_quarantine(&self, effect: &str) -> anyhow::Result<Option<(u64, String)>> {
        let row = self
            .conn
            .query_row(
                "SELECT position, reason FROM effect_quarantine WHERE effect = ?1",
                params![effect],
                |row| {
                    let position: i64 = row.get(0)?;
                    let reason: String = row.get(1)?;
                    Ok((position as u64, reason))
                },
            )
            .optional()
            .context("reading an effect quarantine")?;
        Ok(row)
    }

    /// Every journaled call recorded for one invocation, in the order it was made.
    ///
    /// The replay check needs the recorded set, not the results: a faithful replay
    /// reaches exactly these calls, so a journal entry the handler no longer makes
    /// is as much a divergence as a call with no entry.
    pub fn journal_keys(&self, effect: &str, position: u64) -> anyhow::Result<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT call_hash, disambiguator FROM effect_journal \
                 WHERE effect = ?1 AND position = ?2 ORDER BY rowid",
            )
            .context("preparing the effect journal key query")?;
        let rows = stmt
            .query_map(params![effect, position as i64], |row| {
                let hash: String = row.get(0)?;
                let disambiguator: i64 = row.get(1)?;
                Ok((hash, disambiguator as u64))
            })
            .context("querying effect journal keys")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting effect journal keys")
    }

    /// Every invocation recorded for `effect` that reached a terminal state, with
    /// the script hash it ran under. The replay check sweeps these; the hash is what
    /// lets it skip invocations whose module has since been edited, which diverge
    /// legitimately rather than in error.
    pub fn terminal_invocations(&self, effect: &str) -> anyhow::Result<Vec<(u64, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT position, script_hash FROM effect_invocation \
                 WHERE effect = ?1 AND status = 'terminal' ORDER BY position",
            )
            .context("preparing the terminal invocation query")?;
        let rows = stmt
            .query_map(params![effect], |row| {
                let position: i64 = row.get(0)?;
                let hash: String = row.get(1)?;
                Ok((position as u64, hash))
            })
            .context("querying terminal invocations")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting terminal invocations")
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
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting running invocation positions")
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
    ///
    /// A position is only reclaimable once the effect's cursor has passed it. The
    /// driver completes invocations one position at a time but persists the
    /// watermark per batch (and not at all when a shutdown interrupts a batch), so
    /// `terminal` rows routinely sit above the watermark. Those are exactly the rows
    /// the next boot replays: dropping them makes `begin_invocation` report `Running`
    /// against an empty journal and every recorded side effect fires a second time.
    pub fn sweep_effect_journal(&self, cutoff: &str, limit: usize) -> anyhow::Result<usize> {
        // No cursor row means the effect has never persisted a watermark, so
        // `effect_resume_after` resumes it from 0 and every position replays. The
        // subquery is NULL there, the comparison is NULL, and nothing is swept for
        // that effect: intended, and it starts sweeping once a cursor exists.
        let deleted = self
            .conn
            .execute(
                "DELETE FROM effect_invocation WHERE rowid IN (\
                 SELECT rowid FROM effect_invocation \
                 WHERE status = 'terminal' AND completed_at < ?1 \
                 AND position <= (SELECT watermark FROM effect_cursor \
                 WHERE effect = effect_invocation.effect) LIMIT ?2)",
                params![cutoff, limit as i64],
            )
            .context("sweeping effect journal")?;
        Ok(deleted)
    }

    // --- subject keys (field-level erasure) --------------------------------

    /// The wrapped key material and the id of the master that wrapped it for a
    /// subject, or `None` if the subject has no key (never created, or erased).
    pub fn get_subject_key(
        &self,
        subject_field: &str,
        subject_value: &str,
    ) -> anyhow::Result<Option<(Vec<u8>, String)>> {
        self.conn
            .query_row(
                "SELECT wrapped_key, master_key_id FROM subject_key \
                 WHERE subject_field = ?1 AND subject_value = ?2",
                params![subject_field, subject_value],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .context("reading a subject key")
    }

    /// Get a subject's wrapped key, inserting the caller's candidate if none exists,
    /// and return whichever now persists. The insert and the read happen under the one
    /// connection lock the caller holds, so a concurrent create races cleanly (first
    /// writer wins, both encrypt under the winner) and a concurrent erase cannot slip
    /// between them and leave the caller with nothing.
    pub fn get_or_insert_subject_key(
        &self,
        subject_field: &str,
        subject_value: &str,
        candidate_wrapped: &[u8],
        master_key_id: &str,
    ) -> anyhow::Result<(Vec<u8>, String)> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO subject_key \
                 (subject_field, subject_value, wrapped_key, master_key_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    subject_field,
                    subject_value,
                    candidate_wrapped,
                    master_key_id
                ],
            )
            .context("inserting a subject key")?;
        // The row is guaranteed present under the held lock after insert-or-ignore.
        self.get_subject_key(subject_field, subject_value)?
            .ok_or_else(|| anyhow::anyhow!("subject key missing immediately after insert"))
    }

    /// Delete a subject's key, shredding every value encrypted under it. Returns
    /// whether a row was removed (`false` if it was already absent).
    pub fn delete_subject_key(
        &self,
        subject_field: &str,
        subject_value: &str,
    ) -> anyhow::Result<bool> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM subject_key WHERE subject_field = ?1 AND subject_value = ?2",
                params![subject_field, subject_value],
            )
            .context("deleting a subject key")?;
        Ok(changed == 1)
    }

    /// Every distinct master-key id referenced by a stored subject key. Boot checks
    /// each is configured before serving: a missing one means those rows cannot be
    /// unwrapped (a wrong or rotated-away master), which should fail fast rather than
    /// surface later as a read error.
    pub fn distinct_master_key_ids(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT master_key_id FROM subject_key")
            .context("preparing master-key id scan")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .context("scanning master key ids")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting master key ids")
    }

    /// Every subject key, for a master-rotation rewrap. Returns
    /// `(subject_field, subject_value, wrapped_key, master_key_id)` rows.
    pub fn all_subject_keys(&self) -> anyhow::Result<Vec<SubjectKeyRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT subject_field, subject_value, wrapped_key, master_key_id FROM subject_key",
            )
            .context("preparing subject-key scan")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("scanning subject keys")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting subject keys")
    }

    /// Replace many subjects' wrapped key material and wrapping master id in one
    /// transaction, so a master rotation is all-or-nothing (a crash mid-rotation
    /// leaves every row on its original master, still unwrappable) and costs one
    /// fsync rather than one per row.
    ///
    /// Each update is a compare-and-set on the expected master id: if the subject was
    /// erased and recreated between the rotation's snapshot and this write (so the row
    /// now sits under a different master, holding a fresh secret), the `WHERE` misses
    /// and the stale rewrap is skipped rather than clobbering the new secret. Returns
    /// the number of rows actually rewrapped, which is below `updates.len()` when a CAS
    /// skips a concurrently recreated row.
    pub fn rewrap_subject_keys(&self, updates: &[RewrapUpdate]) -> anyhow::Result<usize> {
        let tx = self
            .conn
            .unchecked_transaction()
            .context("beginning a rotation transaction")?;
        let mut rewrapped = 0;
        for (subject_field, subject_value, wrapped_key, master_key_id, expected_master_id) in
            updates
        {
            rewrapped += tx
                .execute(
                    "UPDATE subject_key SET wrapped_key = ?3, master_key_id = ?4 \
                     WHERE subject_field = ?1 AND subject_value = ?2 AND master_key_id = ?5",
                    params![
                        subject_field,
                        subject_value,
                        wrapped_key,
                        master_key_id,
                        expected_master_id
                    ],
                )
                .context("rewrapping a subject key")?;
        }
        tx.commit().context("committing a rotation")?;
        Ok(rewrapped)
    }

    /// Bring the schema up to [`SCHEMA_VERSION`], one version per transaction. The
    /// DDL and the `user_version` bump commit together, so a crash or a failure
    /// mid-migration leaves the database exactly at the version it was at rather
    /// than half-migrated and unopenable forever after.
    fn migrate(&mut self) -> anyhow::Result<()> {
        let mut version: i64 = self.schema_version()?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "operational database is at schema version {version}, newer than this build ({SCHEMA_VERSION})"
            );
        }
        while version < SCHEMA_VERSION {
            let tx = self
                .conn
                .unchecked_transaction()
                .context("beginning a migration transaction")?;
            match version {
                0 => tx.execute_batch(SCHEMA_V1).context("applying schema v1")?,
                1 => tx.execute_batch(SCHEMA_V2).context("applying schema v2")?,
                2 => tx.execute_batch(SCHEMA_V3).context("applying schema v3")?,
                3 => tx.execute_batch(SCHEMA_V4).context("applying schema v4")?,
                other => anyhow::bail!("no migration from schema version {other}"),
            }
            version += 1;
            tx.pragma_update(None, "user_version", version)
                .context("recording schema version")?;
            tx.commit().context("committing a migration")?;
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

/// Schema v3 adds the per-subject key store for field-level erasure. Each row holds
/// one subject's key material, wrapped under a master key (identified by
/// `master_key_id` so masters can rotate online). Deleting a row shreds every value
/// encrypted under it, across the log, the tag index, and every read model at once.
/// The subject id itself stays plaintext (it is needed to find the key). A reserved
/// row holds the global secret behind `unique` uniqueness tags.
const SCHEMA_V3: &str = "
CREATE TABLE subject_key (
    subject_field TEXT NOT NULL,
    subject_value TEXT NOT NULL,
    wrapped_key   BLOB NOT NULL,  -- AEAD-wrapped subject secret (nonce || ciphertext)
    master_key_id TEXT NOT NULL,  -- which master wrapped this row
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (subject_field, subject_value)
);
-- The boot-time master check and a rotation both group by master_key_id, which the
-- primary key does not cover; this keeps those from full-scanning subject_key.
CREATE INDEX subject_key_by_master ON subject_key (master_key_id);
";

const SCHEMA_V4: &str = "
-- A verify-mode quarantine. Durable because the whole point is that it does not
-- clear on its own: an in-memory flag would be wiped by the restart that a wedged
-- effect invites, and the effect would resume as if nothing had been found.
CREATE TABLE effect_quarantine (
    effect   TEXT    NOT NULL PRIMARY KEY,
    position INTEGER NOT NULL,  -- where the invariant broke
    reason   TEXT    NOT NULL,
    at       TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
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
            "effect_invocation",
            "effect_journal",
            "module_metadata",
            "effect_cursor",
            "subject_key",
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
        let path = dir.path().join("hekla.db");
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
    fn rewrap_is_a_compare_and_set_on_the_master_id() {
        let db = OpDb::open_in_memory().unwrap();
        db.get_or_insert_subject_key("customer_id", "1", b"wrapped-under-A", "master-A")
            .unwrap();
        // A rotation snapshots the row under master-A, but the subject is erased and
        // recreated under a different master before the rewrap lands. The rewrap expects
        // master-A, so its compare-and-set misses and the recreated secret is preserved
        // rather than clobbered by the stale rewrap.
        let rewrapped = db
            .rewrap_subject_keys(&[(
                "customer_id".into(),
                "1".into(),
                b"stale-rewrap".to_vec(),
                "master-C".into(),
                "master-B".into(),
            )])
            .unwrap();
        assert_eq!(rewrapped, 0, "a mismatched expected master rewraps nothing");
        let (wrapped, master) = db.get_subject_key("customer_id", "1").unwrap().unwrap();
        assert_eq!(
            master, "master-A",
            "a mismatched expected master is a no-op"
        );
        assert_eq!(wrapped, b"wrapped-under-A");
        // A rewrap that expects the row's real current master applies.
        let rewrapped = db
            .rewrap_subject_keys(&[(
                "customer_id".into(),
                "1".into(),
                b"wrapped-under-C".to_vec(),
                "master-C".into(),
                "master-A".into(),
            )])
            .unwrap();
        assert_eq!(rewrapped, 1, "a matching expected master rewraps the row");
        let (wrapped, master) = db.get_subject_key("customer_id", "1").unwrap().unwrap();
        assert_eq!(master, "master-C");
        assert_eq!(wrapped, b"wrapped-under-C");
    }

    #[test]
    fn distinct_master_key_ids_lists_each_once() {
        let db = OpDb::open_in_memory().unwrap();
        db.get_or_insert_subject_key("customer_id", "1", b"w", "master-A")
            .unwrap();
        db.get_or_insert_subject_key("customer_id", "2", b"w", "master-A")
            .unwrap();
        db.get_or_insert_subject_key("customer_id", "3", b"w", "master-B")
            .unwrap();
        let mut ids = db.distinct_master_key_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["master-A".to_owned(), "master-B".to_owned()]);
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
        // The cursor is past both completed positions, so age and status are the only
        // things under test here.
        db.set_effect_watermark("e", 2).unwrap();
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

    /// A mid-batch shutdown (`Progress::Interrupted`) leaves positions that are
    /// already `terminal` sitting above the effect's persisted watermark: the driver
    /// only advances the cursor once the whole batch is through. If those rows age
    /// past the retention window while the process is down, the sweep must leave them
    /// alone, or the next boot resumes from the watermark, finds no invocation row,
    /// and re-fires every side effect it already performed.
    #[test]
    fn sweep_keeps_terminal_invocations_above_the_effect_watermark() {
        let db = OpDb::open_in_memory().unwrap();
        db.set_effect_watermark("e", 0).unwrap();
        db.begin_invocation("e", 5, "h", "t0").unwrap();
        db.journal_put("e", 5, "abc", 0, "{}", "t0").unwrap();
        db.complete_invocation("e", 5, "2020-01-01T00:00:00Z")
            .unwrap();

        let deleted = db
            .sweep_effect_journal("2026-06-01T00:00:00Z", SWEEP_CHUNK)
            .unwrap();
        assert_eq!(
            deleted, 0,
            "a terminal invocation the cursor has not passed is still needed"
        );
        assert!(
            db.journal_get("e", 5, "abc", 0).unwrap().is_some(),
            "the journal cascaded away under a watermark that never reached it"
        );
        assert!(
            matches!(
                db.begin_invocation("e", 5, "h", "t9").unwrap(),
                InvocationState::AlreadyTerminal
            ),
            "the position re-runs after the sweep, so its side effects fire twice"
        );
    }

    /// One effect's cursor must not license sweeping another's positions, and an
    /// effect with no cursor row at all resumes from 0, so nothing of its is
    /// reclaimable until it persists a watermark.
    #[test]
    fn sweep_bounds_each_effect_by_its_own_cursor() {
        let db = OpDb::open_in_memory().unwrap();
        db.set_effect_watermark("swept", 1).unwrap();
        for effect in ["swept", "no-cursor"] {
            db.begin_invocation(effect, 1, "h", "t0").unwrap();
            db.journal_put(effect, 1, "abc", 0, "{}", "t0").unwrap();
            db.complete_invocation(effect, 1, "2020-01-01T00:00:00Z")
                .unwrap();
        }

        let deleted = db
            .sweep_effect_journal("2026-06-01T00:00:00Z", SWEEP_CHUNK)
            .unwrap();
        assert_eq!(deleted, 1, "only the effect with a cursor past 1 is swept");
        assert_eq!(db.journal_get("swept", 1, "abc", 0).unwrap(), None);
        assert!(
            db.journal_get("no-cursor", 1, "abc", 0).unwrap().is_some(),
            "an effect that has never persisted a cursor still replays position 1"
        );
    }

    #[test]
    fn sweep_returns_exactly_the_limit_when_more_rows_remain() {
        let db = OpDb::open_in_memory().unwrap();
        // The cursor is past every position here, so the chunk boundary is the only
        // thing under test.
        db.set_effect_watermark("e", 3).unwrap();
        for position in 1..=3 {
            db.begin_invocation("e", position, "h", "t0").unwrap();
            db.journal_put("e", position, "abc", 0, "{}", "t0").unwrap();
            db.complete_invocation("e", position, "2020-01-01T00:00:00Z")
                .unwrap();
        }

        let first = db.sweep_effect_journal("2026-06-01T00:00:00Z", 2).unwrap();
        assert_eq!(first, 2, "a full chunk keeps `run_sweep` looping");
        let second = db.sweep_effect_journal("2026-06-01T00:00:00Z", 2).unwrap();
        assert_eq!(second, 1, "a short chunk ends `run_sweep`'s loop");
        let third = db.sweep_effect_journal("2026-06-01T00:00:00Z", 2).unwrap();
        assert_eq!(third, 0, "nothing is left to reclaim");
        assert_eq!(
            db.journal_get("e", 3, "abc", 0).unwrap(),
            None,
            "the last chunk cascaded its journal rows too"
        );
    }

    /// Drives the real `OpDb::open` (and so `migrate`) rather than SQLite's
    /// transaction semantics: it fails a migration mid-batch and asserts the DDL and
    /// the `user_version` bump roll back together. Without the transaction the DDL
    /// survives and the next open dies on `table already exists`, forever.
    #[test]
    fn a_migration_that_fails_partway_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hekla.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
            // Collides with the index v3 creates after its CREATE TABLE, so the batch
            // fails with one statement already applied.
            conn.execute_batch("CREATE INDEX subject_key_by_master ON module_metadata (kind);")
                .unwrap();
        }

        assert!(OpDb::open(&path).is_err(), "the v3 batch cannot succeed");

        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2, "a failed migration does not bump the version");
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'subject_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "a failed migration rolls back its applied DDL");
    }
}
