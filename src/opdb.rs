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

use std::collections::HashMap;
use std::iter;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};

use crate::crypto;

/// The current schema version, tracked in SQLite's `user_version`. Bump it and
/// add a migration arm when the schema changes.
pub const SCHEMA_VERSION: i64 = 10;

/// How many rows a single sweep statement deletes, so a retention sweep never
/// holds the connection across a long scan. The sweeper loops until a call
/// deletes fewer than this.
pub const SWEEP_CHUNK: usize = 1000;

/// The operational database handle.
pub struct OpDb {
    conn: Connection,
}

/// The schema version an existing `hekla.db` records, without opening it through
/// [`OpDb::open`].
///
/// [`OpDb::open`] migrates, which is the wrong move for a reader: `hekla plan` runs
/// against a live production directory, and silently upgrading its schema is a write
/// nothing asked for. Reading `user_version` over a read-only connection lets a caller
/// refuse an older database rather than quietly changing it.
pub fn recorded_schema_version(path: &Path) -> anyhow::Result<i64> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening operational database {} read-only", path.display()))?;
    let version = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("reading schema version")?;
    Ok(version)
}

/// Positions are stored signed, so a `u64::MAX` sentinel would bind as `-1` and match
/// nothing. Saturate instead, which is the bound the caller meant.
fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The statement [`OpDb::invocations_at`] runs, built once so the test that explains
/// the plan and the code that executes it cannot describe different queries.
fn invocations_at_sql(effects: usize, positions: usize) -> String {
    let mut next = 0;
    let mut placeholders = |count: usize| {
        (0..count)
            .map(|_| {
                next += 1;
                format!("?{next}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let effect_list = placeholders(effects);
    let position_list = placeholders(positions);
    let limit = placeholders(1);
    format!(
        "SELECT effect, position, status FROM effect_invocation \
         WHERE effect IN ({effect_list}) AND position IN ({position_list}) LIMIT {limit}"
    )
}

/// The column list every [`DeclarationRow`] read selects, written once so the two
/// queries over this table cannot drift into reading different columns in the same
/// positions.
const DECLARATION_COLUMNS: &str = "SELECT kind, name, hash, signature_hash, form, signature, \
                                   module, first_seen, last_seen, current FROM declaration";

fn row_to_declaration(row: &rusqlite::Row) -> rusqlite::Result<DeclarationRow> {
    Ok(DeclarationRow {
        kind: row.get(0)?,
        name: row.get(1)?,
        hash: row.get(2)?,
        signature_hash: row.get(3)?,
        form: row.get(4)?,
        signature: row.get(5)?,
        module: row.get(6)?,
        first_seen: row.get(7)?,
        last_seen: row.get(8)?,
        current: row.get::<_, i64>(9)? != 0,
    })
}

fn row_to_invocation(row: &rusqlite::Row) -> rusqlite::Result<InvocationRow> {
    let position: i64 = row.get(0)?;
    Ok(InvocationRow {
        position: position as u64,
        status: row.get(1)?,
        script_hash: row.get(2)?,
        created_at: row.get(3)?,
        completed_at: row.get(4)?,
        skipped_at: row.get(5)?,
    })
}

/// How a stored key row is wrapped. Exactly one of these, which the table's `CHECK`
/// enforces rather than this type hoping for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wrapping {
    /// A root, wrapped under the master with this id.
    Master(String),
    /// A child, wrapped under a key derived from its parent's secret. Deleting the
    /// parent's row is what makes this one unopenable, for good.
    Parent(ParentRef),
    /// Neither, which the `CHECK` forbids. Only a hand-edited database reaches it, and
    /// it is a value rather than a panic so the operator who did it gets a message.
    Neither,
}

impl Wrapping {
    /// The three nullable columns this wrapping writes.
    pub fn columns(&self) -> (Option<&str>, Option<&str>, Option<&str>, Option<&str>) {
        match self {
            Wrapping::Master(id) => (Some(id), None, None, None),
            Wrapping::Parent(parent) => (
                None,
                Some(&parent.subject),
                Some(&parent.value),
                Some(&parent.fingerprint),
            ),
            Wrapping::Neither => (None, None, None, None),
        }
    }
}

/// The row a child's key is wrapped under, and which generation of it.
///
/// The fingerprint is what separates two failures that are otherwise identical bytes: a
/// parent erased and written to again has a new secret, so nothing wrapped under the old
/// one will ever open, and that is an ordinary shred. A wrapping that will not open while
/// the fingerprint still matches is something else, because the key it was wrapped under
/// has not changed: the bytes were altered outside hekla.
///
/// Without it those collapse into one answer, and the runtime would have to pick: report
/// tampering as an erasure and lose the alarm, or report an erasure as tampering and 500
/// every read of a legitimately shredded row. Neither is acceptable, so it records enough
/// to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentRef {
    pub subject: String,
    pub value: String,
    /// A domain-separated digest of the parent's secret as it was when this key was
    /// wrapped. A digest rather than the secret, so the column carries no key material,
    /// and domain-separated from the wrapping key derived from the same secret.
    pub fingerprint: String,
}

/// One subject-key row, without the identity it was read by.
#[derive(Debug, Clone)]
pub struct SubjectKey {
    pub wrapped: Vec<u8>,
    pub wrapping: Wrapping,
}

/// One root subject key, for a master-rotation rewrap:
/// `(subject, subject_value, wrapped_key, master_key_id)`.
pub type SubjectKeyRow = (String, String, Vec<u8>, String);

/// One rewrap for a master rotation: a [`SubjectKeyRow`] with the new wrapped key and
/// new master id, plus the master id the row was expected to be under (a compare-and-set
/// guard so a concurrent erase-then-recreate of the same subject is not clobbered).
/// `(subject, subject_value, new_wrapped_key, new_master_id, expected_master_id)`.
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
    ///
    /// `collapsed_from` is rule 15's batch collapse: the oldest position this invocation
    /// folded into itself, or `None` when it covers only its own. **It is set by the same
    /// statement that makes the row terminal, and that is the whole of why it can be
    /// trusted.** The dispatcher retires the folded positions from its in-flight set the
    /// moment this returns, so the mark moves past them; a completion that landed without
    /// the range would leave them covered by nothing and say so nowhere.
    pub fn complete_invocation(
        &self,
        effect: &str,
        position: u64,
        now: &str,
        collapsed_from: Option<u64>,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE effect_invocation SET status = 'terminal', completed_at = ?3, \
                 collapsed_from = ?4 WHERE effect = ?1 AND position = ?2",
                params![
                    effect,
                    position as i64,
                    now,
                    collapsed_from.map(|from| from as i64)
                ],
            )
            .context("completing effect invocation")?;
        Ok(())
    }

    /// Mark an invocation `terminal` *and* record that an operator skipped it.
    ///
    /// Split from [`complete_invocation`](Self::complete_invocation) rather than given a
    /// flag, because the two are different events that happen to leave the same status: a
    /// completion is a handler reaching its own end, a skip is an operator stepping over
    /// one that could not. Only the second is a position nothing ran to a conclusion for,
    /// and a replay that cannot tell them apart has to guess from the journal.
    ///
    /// `collapsed_from` carries the same range
    /// [`complete_invocation`](Self::complete_invocation) does, and for the same reason: a
    /// skipped invocation is still the one that retires its whole group, so the positions
    /// it folded have to be recorded by the statement that retires them.
    pub fn skip_invocation(
        &self,
        effect: &str,
        position: u64,
        now: &str,
        collapsed_from: Option<u64>,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE effect_invocation \
                 SET status = 'terminal', completed_at = ?3, skipped_at = ?3, \
                 collapsed_from = ?4 WHERE effect = ?1 AND position = ?2",
                params![
                    effect,
                    position as i64,
                    now,
                    collapsed_from.map(|from| from as i64)
                ],
            )
            .context("recording an operator skip")?;
        Ok(())
    }

    /// Whether an operator skipped this invocation rather than it reaching an end.
    ///
    /// `false` for a row written before schema v7, which had nowhere to record it. That
    /// is the honest answer rather than a safe one: those rows keep the journal-shape
    /// inference they have always had, and only rows written since can be answered
    /// outright.
    pub fn invocation_skipped(&self, effect: &str, position: u64) -> anyhow::Result<bool> {
        let skipped: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT skipped_at FROM effect_invocation WHERE effect = ?1 AND position = ?2",
                params![effect, position as i64],
                |row| row.get(0),
            )
            .optional()
            .context("reading an invocation skip marker")?;
        Ok(skipped.flatten().is_some())
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
    #[allow(clippy::too_many_arguments)]
    pub fn journal_put(
        &self,
        effect: &str,
        position: u64,
        call_hash: &str,
        disambiguator: u64,
        kind: &str,
        result: &str,
        now: &str,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO effect_journal \
                 (effect, position, call_hash, disambiguator, kind, result, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    effect,
                    position as i64,
                    call_hash,
                    disambiguator as i64,
                    kind,
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
    pub fn terminal_invocations(&self, effect: &str) -> anyhow::Result<Vec<TerminalInvocation>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT position, script_hash, collapsed_from FROM effect_invocation \
                 WHERE effect = ?1 AND status = 'terminal' ORDER BY position",
            )
            .context("preparing the terminal invocation query")?;
        let rows = stmt
            .query_map(params![effect], |row| {
                let position: i64 = row.get(0)?;
                let script_hash: String = row.get(1)?;
                let collapsed_from: Option<i64> = row.get(2)?;
                Ok(TerminalInvocation {
                    position: position as u64,
                    script_hash,
                    collapsed_from: collapsed_from.map(|from| from as u64),
                })
            })
            .context("querying terminal invocations")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting terminal invocations")
    }

    /// The most recent invocations of `effect` that `script_hash` recorded, at or below
    /// `upto`, newest first, at most `limit` of them.
    ///
    /// For a replay against a *live* deployment, which is the difference from
    /// [`terminal_invocations`](Self::terminal_invocations). `upto` is the tip of the
    /// prefix the reader pinned: the server goes on appending and completing
    /// invocations while a plan runs, and one recorded after the pin has no event the
    /// reader can see, so replaying it would report a divergence that is really a race.
    /// `limit` bounds the work, because a busy effect's seven days of history is
    /// unbounded in a way a deploy gate is not.
    ///
    /// `script_hash` is the *deployed* program's, and filtering on it is what keeps a
    /// plan honest. The retention window outlives an edit, so rows written by versions
    /// this deploy is not replacing are still here; replaying those against the candidate
    /// reports differences the running code already has, which is a finding about last
    /// week rather than about this deploy.
    ///
    /// Newest first because recency is what a plan is about: if only some of the history
    /// can be covered, the invocations closest to what is running now are the ones worth
    /// covering.
    pub fn recent_terminal_invocations(
        &self,
        effect: &str,
        script_hash: &str,
        upto: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT position FROM effect_invocation \
                 WHERE effect = ?1 AND script_hash = ?2 AND status = 'terminal' \
                 AND position <= ?3 ORDER BY position DESC LIMIT ?4",
            )
            .context("preparing the recent terminal invocation query")?;
        // Saturating rather than wrapping: `usize::MAX` means "everything", and `LIMIT -1`
        // is how SQLite spells that, but arriving there by two's complement would be an
        // accident rather than a decision.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![effect, script_hash, upto as i64, limit], |row| {
                let position: i64 = row.get(0)?;
                Ok(position as u64)
            })
            .context("querying recent terminal invocations")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting recent terminal invocations")
    }

    /// How many rows [`recent_terminal_invocations`](Self::recent_terminal_invocations)
    /// would have to choose from, unbounded by any limit.
    ///
    /// For a caller reporting invocations it is deliberately *not* replaying, where the
    /// honest number is the whole set rather than the part a cap on the replay would
    /// have covered.
    pub fn count_terminal_invocations(
        &self,
        effect: &str,
        script_hash: &str,
        upto: u64,
    ) -> anyhow::Result<usize> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM effect_invocation \
                 WHERE effect = ?1 AND script_hash = ?2 AND status = 'terminal' \
                 AND position <= ?3",
                params![effect, script_hash, upto as i64],
                |row| row.get(0),
            )
            .context("counting terminal invocations")?;
        Ok(count as usize)
    }

    /// Positions above `after` that this effect already has an invocation row for, in any
    /// status.
    ///
    /// Rule 15's batch collapse is what wants this, and the rule it enforces is narrow:
    /// such a position may not be **superseded**. It still takes over the group it lands in
    /// and becomes what runs, which is how the partition an earlier process built is
    /// reproduced; what it may not do is let a later position fold it away, because then
    /// nothing would run at it. A crash leaves `running` rows for invocations that began,
    /// and a completion whose mark never advanced leaves `terminal` ones: fold the first
    /// away and its row is never completed, fold the second away and it sits inside a range
    /// that also claims to cover it. After a restart the dispatcher cannot tell either from
    /// memory, which is what this answers.
    ///
    /// Starts above the mark, because the subscription resumes there and nothing at or
    /// below it is offered again. Read once, at resume: every row written after that is for
    /// a position this process admitted, which its own lane high-water already answers for.
    ///
    /// **`limit` is a real bound and the caller has to handle reaching it.** Rows above the
    /// mark are never swept and the mark is held down by the oldest unfinished lane, so
    /// their number is bounded by how long a lane has been wedged rather than by anything: a
    /// day of one wedged shop beside a busy plain arm is millions of rows, materialised on
    /// the restart an operator reached for to clear the wedge. Answering a short list is why
    /// this returns them rather than a count.
    pub fn invocations_above(
        &self,
        effect: &str,
        after: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT position FROM effect_invocation WHERE effect = ?1 AND position > ?2 \
                 ORDER BY position LIMIT ?3",
            )
            .context("preparing the recorded position query")?;
        let rows = stmt
            .query_map(params![effect, after as i64, limit as i64], |row| {
                let position: i64 = row.get(0)?;
                Ok(position as u64)
            })
            .context("querying recorded positions")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting recorded positions")
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

    /// The lane watermarks recorded above this effect's global mark, by lane id.
    ///
    /// The dispatcher consults this as it admits positions after a resume: a lane whose
    /// recorded position is at or above the one being admitted has already finished it.
    /// A missing row costs correctness nothing, only one `begin_invocation` round trip
    /// per position to be told `AlreadyTerminal`, which is why this table may lag the
    /// truth but must never lead it.
    pub fn effect_lanes(&self, effect: &str) -> anyhow::Result<HashMap<String, u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT lane, position FROM effect_lane WHERE effect = ?1")
            .context("preparing the lane read")?;
        let rows = stmt
            .query_map(params![effect], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .context("reading effect lanes")?;
        let mut lanes = HashMap::new();
        for row in rows {
            let (lane, position) = row.context("reading an effect lane")?;
            lanes.insert(lane, position);
        }
        Ok(lanes)
    }

    /// Record that `lane` has terminalised every position up to `position`.
    ///
    /// Written *after* the invocation reaches `terminal`, never before: a crash between
    /// the two replays the position into `begin_invocation`'s `AlreadyTerminal`, which
    /// is free, while the reverse order would let a lane row claim a position that never
    /// completed and skip it forever.
    ///
    /// `max` rather than assignment because the two writes are not in one transaction,
    /// so taking the higher of them is the only outcome that is right under a retry.
    pub fn record_effect_lane(
        &self,
        effect: &str,
        lane: &str,
        position: u64,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO effect_lane (effect, lane, position) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(effect, lane) DO UPDATE SET position = max(position, excluded.position)",
                params![effect, lane, position as i64],
            )
            .context("recording an effect lane")?;
        Ok(())
    }

    /// Drop the lane rows the global mark has caught up with, and report how many went.
    ///
    /// A lane at or below the mark says nothing the mark does not already say, and
    /// keeping it is how a per-user key turns into a row per user forever.
    pub fn sweep_effect_lanes(&self, effect: &str, watermark: u64) -> anyhow::Result<usize> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM effect_lane WHERE effect = ?1 AND position <= ?2",
                params![effect, watermark as i64],
            )
            .context("sweeping effect lanes")?;
        Ok(deleted)
    }

    /// How many lanes are still ahead of the global mark.
    ///
    /// Zero means the effect has drained, which is what makes a partition-key change
    /// safe to accept (see [`OpDb::set_effect_lane_scheme`]).
    pub fn effect_lanes_outstanding(&self, effect: &str) -> anyhow::Result<usize> {
        // Bounded by the cursor rather than counting the table, so a row the sweep has not
        // reached yet (a crash between advancing the mark and deleting, say) does not read
        // as work outstanding. Its two callers refuse to start an effect and warn on a
        // deploy, so a stale row would turn a routine key change into a stopped effect.
        let count: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM effect_lane WHERE effect = ?1 AND position > \
                 coalesce((SELECT watermark FROM effect_cursor WHERE effect = ?1), 0)",
                params![effect],
                |row| row.get(0),
            )
            .context("counting effect lanes")?;
        Ok(count as usize)
    }

    /// Resolve this effect's first activation against this data directory, or read back
    /// the one already recorded.
    ///
    /// `docs/effects.md` rule 15: `on live` is not a position constant in source, it is
    /// a number the runtime resolves once and keeps, so dev, staging and production each
    /// resolve correctly and `hek digest` does not move when an operator makes an
    /// operational decision.
    ///
    /// Insert-or-ignore then read is what makes "once" true. The insert is a single
    /// autocommit statement, so the row either holds the first-ever head or does not
    /// exist; a crash between the two re-reads the same row next boot. `head` is only
    /// consulted the first time, which is what makes the boundary mean "after this
    /// effect first ran here" rather than "after the last restart".
    pub fn activate_effect(
        &self,
        effect: &str,
        head: u64,
        lane_scheme: &str,
        now: &str,
    ) -> anyhow::Result<Activation> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO effect_activation \
                 (effect, live_boundary, lane_scheme, activated_at) VALUES (?1, ?2, ?3, ?4)",
                params![effect, head as i64, lane_scheme, now],
            )
            .context("activating an effect")?;
        self.effect_activation(effect)?
            .ok_or_else(|| anyhow::anyhow!("effect `{effect}` has no activation row after insert"))
    }

    /// The recorded activation, without creating one. `None` before the effect has ever
    /// run against this directory.
    ///
    /// Separate from [`OpDb::activate_effect`] because a reader must not activate an
    /// effect by looking at it: `/admin/effects` and `hekla plan` both ask, and a plan
    /// that resolved a live boundary as a side effect of reporting on one would pin
    /// history at whatever the head was when someone ran a dry run.
    pub fn effect_activation(&self, effect: &str) -> anyhow::Result<Option<Activation>> {
        self.conn
            .query_row(
                "SELECT live_boundary, lane_scheme FROM effect_activation WHERE effect = ?1",
                params![effect],
                |row| {
                    Ok(Activation {
                        live_boundary: row.get::<_, i64>(0)? as u64,
                        lane_scheme: row.get(1)?,
                    })
                },
            )
            .optional()
            .context("reading an effect activation")
    }

    /// Accept a new lane scheme for an effect that has drained.
    ///
    /// Only valid with no lane rows outstanding: rows above the mark are keyed under the
    /// old scheme, so a scheme change while they exist would leave them naming lanes the
    /// new key never produces. The caller checks that; this only writes.
    pub fn set_effect_lane_scheme(&self, effect: &str, lane_scheme: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE effect_activation SET lane_scheme = ?2 WHERE effect = ?1",
                params![effect, lane_scheme],
            )
            .context("recording an effect lane scheme")?;
        Ok(())
    }

    /// Move an effect back to `to`, so it reprocesses everything after it.
    ///
    /// **The one write in hekla that moves a watermark backwards**, which is why it is here
    /// rather than through [`OpDb::set_effect_watermark`]: that function's forward-only
    /// contract is what every other caller relies on, and a rewind is the deliberate
    /// exception rather than a reason to weaken it.
    ///
    /// Deleting the invocation rows is the part that makes it a rewind at all. Without it
    /// `begin_invocation` reports `AlreadyTerminal` for every position moved past and the
    /// command silently does nothing. It is also what makes a rewind *re-fire*: the journal
    /// rows cascade with their invocations, so the calls they recorded are performed again
    /// rather than replayed.
    ///
    /// **A target inside a collapsed range loses the record of what covered its older
    /// half.** Rule 15 lets one invocation stand for a batch, so a rewind to 50 that
    /// discards an invocation at 100 covering positions from 3 leaves 3..50 accounted for by
    /// nothing: they are at or below the new cursor, so they are neither re-delivered nor
    /// still covered, and a later sweep can say less about them than it could. The work
    /// itself happened and nothing re-fires twice; what the operator gave up is
    /// auditability, on a command whose whole preview is about what it gives up. Refusing
    /// such a target would be worse: it would make a rewind's legality depend on a
    /// scheduling decision an operator cannot see.
    ///
    /// One transaction, because a half-applied rewind is exactly the shape
    /// [`OpDb::sweep_effect_journal`] warns about: terminal rows above the watermark with
    /// no journal behind them, whose side effects fire a second time.
    ///
    /// `include_live` lowers the `on live` boundary too. Off by default: an author who
    /// wrote `on live` declared that history must not fire, and a rewind aimed at a plain
    /// arm should not quietly re-send every notification that arm declined.
    pub fn rewind_effect(
        &mut self,
        effect: &str,
        to: u64,
        include_live: bool,
    ) -> anyhow::Result<RewindCounts> {
        let tx = self
            .conn
            .transaction()
            .context("beginning a rewind transaction")?;
        let invocations = tx
            .execute(
                "DELETE FROM effect_invocation WHERE effect = ?1 AND position > ?2",
                params![effect, to as i64],
            )
            .context("discarding recorded invocations")?;
        let lanes = tx
            .execute("DELETE FROM effect_lane WHERE effect = ?1", params![effect])
            .context("discarding lane rows")?;
        // Otherwise the effect refuses to start and the rewind looks like a no-op. The
        // invocation the check diverged on is exactly what is being discarded.
        let quarantine = tx
            .execute(
                "DELETE FROM effect_quarantine WHERE effect = ?1 AND position > ?2",
                params![effect, to as i64],
            )
            .context("clearing a quarantine")?;
        tx.execute(
            "INSERT INTO effect_cursor (effect, watermark) VALUES (?1, ?2) \
             ON CONFLICT(effect) DO UPDATE SET watermark = excluded.watermark",
            params![effect, to as i64],
        )
        .context("rewinding the effect cursor")?;
        if include_live {
            tx.execute(
                "UPDATE effect_activation SET live_boundary = ?2 \
                 WHERE effect = ?1 AND live_boundary > ?2",
                params![effect, to as i64],
            )
            .context("lowering the live boundary")?;
        }
        tx.commit().context("committing a rewind")?;
        Ok(RewindCounts {
            invocations,
            lanes,
            quarantine: quarantine > 0,
        })
    }

    /// What a rewind to `to` would discard, without discarding it.
    pub fn rewind_preview(&self, effect: &str, to: u64) -> anyhow::Result<RewindCounts> {
        let invocations: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM effect_invocation WHERE effect = ?1 AND position > ?2",
                params![effect, to as i64],
                |row| row.get(0),
            )
            .context("counting invocations to discard")?;
        let quarantine: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM effect_quarantine WHERE effect = ?1 AND position > ?2",
                params![effect, to as i64],
                |row| row.get(0),
            )
            .context("checking for a quarantine")?;
        // Every row, not the ones above the mark: `rewind_effect` deletes the lot, and the
        // summary is the number an operator answers the prompt against.
        let lanes: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM effect_lane WHERE effect = ?1",
                params![effect],
                |row| row.get(0),
            )
            .context("counting lane rows to discard")?;
        Ok(RewindCounts {
            invocations: invocations as usize,
            lanes: lanes as usize,
            quarantine: quarantine > 0,
        })
    }

    /// Positions of this effect's still-`running` invocations that were recorded under
    /// a *known* other version of it, for the restart warning.
    ///
    /// A hash `declaration` has never held is excluded rather than reported. Those exist
    /// only from before the digest, when `script_hash` was a hash of a file's bytes, and
    /// nothing can be concluded by comparing one to an entry hash: they are not the same
    /// measurement. Warning about them would name every in-flight invocation on the first
    /// boot after that migration and say "the code changed", which is not what happened.
    pub fn running_with_hash_mismatch(
        &self,
        effect: &str,
        current_hash: &str,
    ) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT position FROM effect_invocation \
                 WHERE effect = ?1 AND status = 'running' AND script_hash <> ?2 \
                 AND EXISTS (SELECT 1 FROM declaration \
                             WHERE kind = 'effect' AND name = ?1 AND hash = script_hash) \
                 ORDER BY position",
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

    /// Record what this boot loaded: clear the previous `current` set, then upsert one
    /// row per declaration.
    ///
    /// The insert conflicts on `(kind, name, hash)`, so re-loading a declaration hekla
    /// has seen before touches `last_seen` and writes no new row. That is what makes the
    /// table grow with edits rather than with boots, and what keeps a restart loop from
    /// filling it. Both halves run in one transaction: a crash between them would leave
    /// no version marked current and the inventory empty.
    pub fn set_current_declarations(
        &mut self,
        declarations: &[DeclarationRow],
        now: &str,
    ) -> anyhow::Result<()> {
        let tx = self
            .conn
            .transaction()
            .context("beginning the declaration write")?;
        tx.execute("UPDATE declaration SET current = 0 WHERE current = 1", [])
            .context("clearing the previous current declarations")?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO declaration \
                     (kind, name, hash, signature_hash, form, signature, module, \
                      first_seen, last_seen, current) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 1) \
                     ON CONFLICT(kind, name, hash) DO UPDATE SET \
                     last_seen = excluded.last_seen, current = 1, module = excluded.module",
                )
                .context("preparing the declaration insert")?;
            for row in declarations {
                stmt.execute(params![
                    row.kind,
                    row.name,
                    row.hash,
                    row.signature_hash,
                    row.form,
                    row.signature,
                    row.module,
                    now,
                ])
                .with_context(|| format!("recording declaration `{}`", row.name))?;
            }
        }
        tx.commit().context("committing the declaration write")?;
        Ok(())
    }

    /// The declarations this process loaded, in `(kind, name)` order.
    pub fn current_declarations(&self) -> anyhow::Result<Vec<DeclarationRow>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "{DECLARATION_COLUMNS} WHERE current = 1 ORDER BY kind, name"
            ))
            .context("preparing the current declaration query")?;
        let rows = stmt
            .query_map([], row_to_declaration)
            .context("querying current declarations")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting current declarations")
    }

    /// Every version of every declaration this directory has ever had deployed, in
    /// `(kind, name)` order.
    ///
    /// Not only the current ones, and the difference is the whole reason this exists: a
    /// field removed in one deploy and put back in the next leaves events in between
    /// that carry neither, and a comparison against the current declaration alone sees
    /// the two ends agree and misses them.
    pub fn all_declarations(&self) -> anyhow::Result<Vec<DeclarationRow>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "{DECLARATION_COLUMNS} ORDER BY kind, name, first_seen"
            ))
            .context("preparing the declaration history query")?;
        let rows = stmt
            .query_map([], row_to_declaration)
            .context("querying every declaration")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting every declaration")
    }

    /// Every recorded declaration, read back as the digest entry it was written from.
    ///
    /// A row whose packed form does not reproduce its own hash is skipped rather than
    /// failing the call: it was written under a different `heklang::digest::VERSION`, so
    /// what it declared cannot be read off it by this build, and a caller asking what a
    /// past deployment's fields were is better off knowing less than guessing.
    pub fn recorded_entries(&self) -> anyhow::Result<Vec<heklang::Entry>> {
        Ok(self
            .all_declarations()?
            .iter()
            .filter_map(|row| {
                let entry =
                    heklang::Entry::from_packed(&row.form, row.signature.as_deref()).ok()?;
                (entry.hash.to_string() == row.hash).then_some(entry)
            })
            .collect())
    }

    /// One recorded version of a declaration, by its hash.
    ///
    /// `None` means hekla has no record of that hash at all, which is a different fact
    /// from "a different version is current": it says the hash was written under a
    /// scheme this table never held, so nothing can be concluded by comparing it.
    pub fn declaration_by_hash(
        &self,
        kind: &str,
        name: &str,
        hash: &str,
    ) -> anyhow::Result<Option<DeclarationRow>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "{DECLARATION_COLUMNS} WHERE kind = ?1 AND name = ?2 AND hash = ?3"
            ))
            .context("preparing the declaration lookup")?;
        let mut rows = stmt
            .query_map(params![kind, name, hash], row_to_declaration)
            .context("looking up a declaration")?;
        rows.next().transpose().context("reading a declaration row")
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

    /// How a stored key row is wrapped: under a master (a root) or under another
    /// subject's key (a child).
    ///
    /// Exactly one, which the table's `CHECK` enforces rather than this type hoping for
    /// it. A row that is neither would be unopenable and a row that is both would be
    /// ambiguous about which key actually holds it.
    pub fn get_subject_key(
        &self,
        subject: &str,
        subject_value: &str,
    ) -> anyhow::Result<Option<SubjectKey>> {
        self.conn
            .query_row(
                "SELECT wrapped_key, master_key_id, parent_subject, parent_value, parent_fingerprint \
                 FROM subject_key WHERE subject = ?1 AND subject_value = ?2",
                params![subject, subject_value],
                |row| {
                    let wrapped: Vec<u8> = row.get(0)?;
                    let master: Option<String> = row.get(1)?;
                    let parent_subject: Option<String> = row.get(2)?;
                    let parent_value: Option<String> = row.get(3)?;
                    let fingerprint: Option<String> = row.get(4)?;
                    Ok(SubjectKey {
                        wrapped,
                        wrapping: match (master, parent_subject, parent_value, fingerprint) {
                            (Some(id), _, _, _) => Wrapping::Master(id),
                            (None, Some(subject), Some(value), Some(fingerprint)) => {
                                Wrapping::Parent(ParentRef {
                                    subject,
                                    value,
                                    fingerprint,
                                })
                            }
                            // Unreachable while the CHECK holds, and reported rather
                            // than panicked on because a hand-edited database is a
                            // thing an operator can do.
                            (None, _, _, _) => Wrapping::Neither,
                        },
                    })
                },
            )
            .optional()
            .context("reading a subject key")
    }

    /// Get a subject's wrapped key, inserting the caller's candidate if none exists,
    /// and return whichever now persists. The insert and the read happen under the one
    /// connection lock the caller holds, so a concurrent create races cleanly (first
    /// writer wins, both encrypt under the winner) and a concurrent erase cannot slip
    /// between them and leave the caller with nothing.
    ///
    /// `replacing` is the row a caller found unopenable and means to overwrite: a child
    /// whose parent was erased holds ciphertext nobody can ever read again, so it reads
    /// as absent and is replaced.
    ///
    /// **The compare-and-set is on the wrapped bytes, and it has to be.** Keying it on the
    /// parent instead looks equivalent and is not: the replacement hangs from the same
    /// parent as the row it replaced, so two writers racing to replace one stale row would
    /// each match the *other's* fresh row and delete a key already sealed under. The bytes
    /// are unique per mint (a random wrapping nonce), so this names exactly the row the
    /// caller read and nothing else. A writer that arrives second matches nothing, its
    /// `INSERT OR IGNORE` no-ops, and it reads back the winner's key, which is the
    /// insert-race behaviour every other path here already has.
    pub fn get_or_insert_subject_key(
        &self,
        subject: &str,
        subject_value: &str,
        candidate: &SubjectKey,
        replacing: Option<&SubjectKey>,
    ) -> anyhow::Result<SubjectKey> {
        let tx = self
            .conn
            .unchecked_transaction()
            .context("beginning a subject-key insert")?;
        if let Some(stale) = replacing {
            tx.execute(
                "DELETE FROM subject_key WHERE subject = ?1 AND subject_value = ?2 \
                 AND wrapped_key = ?3",
                params![subject, subject_value, stale.wrapped],
            )
            .context("replacing an unwrappable subject key")?;
        }
        let (master, parent_subject, parent_value, fingerprint) = candidate.wrapping.columns();
        tx.execute(
            "INSERT OR IGNORE INTO subject_key \
             (subject, subject_value, wrapped_key, master_key_id, parent_subject, parent_value, \
              parent_fingerprint) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                subject,
                subject_value,
                candidate.wrapped,
                master,
                parent_subject,
                parent_value,
                fingerprint
            ],
        )
        .context("inserting a subject key")?;
        tx.commit().context("committing a subject-key insert")?;
        // The row is guaranteed present under the held lock after insert-or-ignore.
        self.get_subject_key(subject, subject_value)?
            .ok_or_else(|| anyhow::anyhow!("subject key missing immediately after insert"))
    }

    /// Delete a subject's key, shredding every value encrypted under it. Returns
    /// whether a row was removed (`false` if it was already absent).
    ///
    /// Still one row even when the subject has children: a child's key is wrapped under
    /// this one, so deleting this row makes every key beneath it unwrappable at the same
    /// instant. The rows beneath survive as unreadable bytes until the sweep reclaims
    /// them, which is storage rather than reachability.
    pub fn delete_subject_key(&self, subject: &str, subject_value: &str) -> anyhow::Result<bool> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM subject_key WHERE subject = ?1 AND subject_value = ?2",
                params![subject, subject_value],
            )
            .context("deleting a subject key")?;
        Ok(changed == 1)
    }

    /// How many key rows sit beneath this one, transitively.
    ///
    /// What `hekla erase` prints before it asks. Needs no master key, because
    /// reachability here is structural: the walk reads parent pointers and never unwraps
    /// anything, which is what keeps the CLI usable without one.
    pub fn descendant_key_count(&self, subject: &str, subject_value: &str) -> anyhow::Result<u64> {
        self.conn
            .query_row(
                "WITH RECURSIVE beneath(subject, subject_value) AS ( \
                     SELECT subject, subject_value FROM subject_key \
                       WHERE parent_subject = ?1 AND parent_value = ?2 \
                     UNION \
                     SELECT k.subject, k.subject_value FROM subject_key k \
                       JOIN beneath b ON k.parent_subject = b.subject \
                                     AND k.parent_value = b.subject_value \
                 ) SELECT count(*) FROM beneath",
                params![subject, subject_value],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as u64)
            .context("counting the keys beneath a subject")
    }

    /// Delete up to `limit` child rows whose parent row is gone.
    ///
    /// Orphans are unreadable the moment their parent is deleted, so this reclaims
    /// storage rather than enforcing anything. One pass only reaches direct orphans: a
    /// grandchild becomes one once its parent is swept, so the caller repeats until a
    /// pass deletes nothing. The `rowid IN (SELECT ... LIMIT ?)` form rather than
    /// `DELETE ... LIMIT`, which needs a compile-time option this build does not set.
    pub fn sweep_orphan_subject_keys(&self, limit: usize) -> anyhow::Result<usize> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM subject_key WHERE rowid IN ( \
                     SELECT k.rowid FROM subject_key k \
                      WHERE k.parent_subject IS NOT NULL \
                        AND NOT EXISTS ( \
                          SELECT 1 FROM subject_key p \
                           WHERE p.subject = k.parent_subject \
                             AND p.subject_value = k.parent_value) \
                      LIMIT ?1)",
                params![limit as i64],
            )
            .context("sweeping orphaned subject keys")?;
        Ok(deleted)
    }

    /// Every distinct master-key id referenced by a stored subject key. Boot checks
    /// each is configured before serving: a missing one means those rows cannot be
    /// unwrapped (a wrong or rotated-away master), which should fail fast rather than
    /// surface later as a read error.
    ///
    /// Roots only, because only a root is wrapped under a master. A child's wrapping key
    /// is derived from its parent's secret, so it references no master of its own and a
    /// rotation never touches it.
    pub fn distinct_master_key_ids(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT master_key_id FROM subject_key WHERE master_key_id IS NOT NULL",
            )
            .context("preparing master-key id scan")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .context("scanning master key ids")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting master key ids")
    }
    /// Every **root** subject key, for a master-rotation rewrap. Returns
    /// `(subject, subject_value, wrapped_key, master_key_id)` rows.
    ///
    /// Roots only, and that is the whole of what a hierarchy costs a rotation: a child's
    /// wrapping key is derived from its parent's secret, and a rotation changes no
    /// secret, so every child stays wrapped correctly without being touched.
    pub fn all_subject_keys(&self) -> anyhow::Result<Vec<SubjectKeyRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT subject, subject_value, wrapped_key, master_key_id FROM subject_key \
                 WHERE master_key_id IS NOT NULL",
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
        for (subject, subject_value, wrapped_key, master_key_id, expected_master_id) in updates {
            rewrapped += tx
                .execute(
                    "UPDATE subject_key SET wrapped_key = ?3, master_key_id = ?4 \
                     WHERE subject = ?1 AND subject_value = ?2 AND master_key_id = ?5",
                    params![
                        subject,
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

    // --- introspection readers ---------------------------------------------
    //
    // These share one mutex with every effect's hot path, so an unbounded scan would
    // stall live work; the retention sweeper chunks for the same reason. Every reader
    // over a table that grows with traffic takes a caller-supplied limit. Two do not,
    // and say why: [`OpDb::current_declarations`] is bounded by the declaration count,
    // which is fixed at boot, and [`OpDb::subject_key_counts`] is an aggregate that
    // cannot be paged, so its caller runs it once per listing rather than once per page.
    //
    // `declaration` itself is *not* bounded that way: it keeps every version of every
    // declaration and nothing sweeps it. That is why the reader filters on `current`
    // rather than selecting the table, and why the index leads with that column. It
    // grows with edits rather than with traffic or with boots, so it stays small enough
    // to want no pagination, but the predicate is load-bearing rather than incidental.

    /// One effect's invocations, newest first, strictly below `before`. Pass
    /// `u64::MAX` for the first page and the oldest position seen for the next.
    pub fn invocations(
        &self,
        effect: &str,
        before: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<InvocationRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT position, status, script_hash, created_at, completed_at, skipped_at \
                 FROM effect_invocation WHERE effect = ?1 AND position < ?2 \
                 ORDER BY position DESC LIMIT ?3",
            )
            .context("preparing the invocation page query")?;
        let rows = stmt
            .query_map(
                params![effect, clamp_i64(before), limit as i64],
                row_to_invocation,
            )
            .context("querying invocations")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting invocations")
    }

    /// Which of `effects` ran at which of `positions`, sorted by `(position, effect)`.
    ///
    /// This is what lets a correlation trace say *which* effect produced an event
    /// rather than only that one did: the envelope records the triggering event, and
    /// the journal is keyed by `(effect, position)`, so the join is exact and needs
    /// no second durable field on the log.
    ///
    /// Both columns are constrained, and that is not belt-and-braces: the table's
    /// only index is its `(effect, position)` primary key, so filtering on `position`
    /// alone would scan a table that grows with traffic, behind the mutex every
    /// journaled call contends for. With both, SQLite walks the key as nested `IN`
    /// loops and touches at most `effects x positions` rows, which is also the
    /// `LIMIT`. `effects` is fixed at boot and `positions` is one clamped page, so
    /// the bind count stays a few hundred, far under SQLite's parameter ceiling.
    /// `explain_invocations_at` pins the plan so this stays true.
    ///
    /// An invocation the retention sweeper has already reclaimed is simply absent,
    /// and is indistinguishable from one that never existed. That is deliberate: the
    /// alternative is dating every position against the cutoff and being wrong at the
    /// boundary. Callers that care read the window from `/admin/system`.
    pub fn invocations_at(
        &self,
        effects: &[&str],
        positions: &[u64],
    ) -> anyhow::Result<Vec<InvocationAt>> {
        // `IN ()` is a syntax error, and both are legitimately empty: a project may
        // declare no effects, and a trace page may hold no events.
        if effects.is_empty() || positions.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(&invocations_at_sql(effects.len(), positions.len()))
            .context("preparing the invocations-at-position query")?;
        let limit = (effects.len() * positions.len()) as i64;
        let bound = params_from_iter(
            effects
                .iter()
                .map(|effect| SqlValue::from((*effect).to_owned()))
                .chain(
                    positions
                        .iter()
                        .copied()
                        .map(|position| SqlValue::from(clamp_i64(position))),
                )
                .chain(iter::once(SqlValue::from(limit))),
        );
        let rows = stmt
            .query_map(bound, |row| {
                let position: i64 = row.get(1)?;
                Ok(InvocationAt {
                    effect: row.get(0)?,
                    position: position as u64,
                    status: row.get(2)?,
                })
            })
            .context("querying invocations by position")?;
        let mut found = rows
            .collect::<Result<Vec<_>, _>>()
            .context("collecting invocations by position")?;
        // The nested-loop order is an implementation detail of the query planner, and
        // this is rendered into a response that must not churn between runs.
        found.sort_by(|left, right| {
            (left.position, &left.effect).cmp(&(right.position, &right.effect))
        });
        Ok(found)
    }

    /// The query plan [`OpDb::invocations_at`] actually gets, for the test that pins
    /// it to a key search rather than a table scan.
    #[cfg(test)]
    fn explain_invocations_at(&self, effects: usize, positions: usize) -> anyhow::Result<String> {
        let sql = format!(
            "EXPLAIN QUERY PLAN {}",
            invocations_at_sql(effects, positions)
        );
        let mut stmt = self.conn.prepare(&sql).context("preparing the explain")?;
        // The planner sees bound values, so they have to be present and of the right
        // types for the plan to be the one the real call gets. Bound in the same three
        // groups the statement declares (effects, positions, then the limit) rather
        // than as one run of `positions + 1` integers that happens to add up: an edit
        // that adds a parameter to the real query should fail where it was made.
        let bound = params_from_iter(
            (0..effects)
                .map(|index| SqlValue::from(format!("effect-{index}")))
                .chain((0..positions).map(|index| SqlValue::from(index as i64)))
                .chain(iter::once(SqlValue::from((effects * positions) as i64))),
        );
        let rows = stmt
            .query_map(bound, |row| row.get::<_, String>(3))
            .context("explaining")?;
        Ok(rows
            .collect::<Result<Vec<_>, _>>()
            .context("collecting the plan")?
            .join("\n"))
    }

    /// One invocation by position.
    pub fn invocation(&self, effect: &str, position: u64) -> anyhow::Result<Option<InvocationRow>> {
        self.conn
            .query_row(
                "SELECT position, status, script_hash, created_at, completed_at, skipped_at \
                 FROM effect_invocation WHERE effect = ?1 AND position = ?2",
                params![effect, position as i64],
                row_to_invocation,
            )
            .optional()
            .context("reading an invocation")
    }

    /// Every journaled call for one invocation, in the order it was made, with its
    /// recorded result. `journal_keys` answers the replay check's question (which
    /// calls happened); this answers an operator's (what each one returned).
    ///
    /// Paged by `offset` rather than a key cursor, which is sound here and nowhere
    /// else in the codebase: one invocation's journal rows are append-only and the
    /// sweeper deletes whole invocations by cascade, never a row from the middle of
    /// one, so a row's ordinal does not shift under a reader.
    pub fn journal_entries(
        &self,
        effect: &str,
        position: u64,
        offset: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<JournalRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT call_hash, disambiguator, kind, result, created_at \
                 FROM effect_journal WHERE effect = ?1 AND position = ?2 \
                 ORDER BY rowid LIMIT ?3 OFFSET ?4",
            )
            .context("preparing the journal entry query")?;
        let rows = stmt
            .query_map(
                params![effect, position as i64, limit as i64, clamp_i64(offset)],
                |row| {
                    let disambiguator: i64 = row.get(1)?;
                    Ok(JournalRow {
                        call_hash: row.get(0)?,
                        disambiguator: disambiguator as u64,
                        kind: row.get(2)?,
                        result: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )
            .context("querying journal entries")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting journal entries")
    }

    /// Every effect's durable runtime state in one statement: the resume watermark and
    /// the quarantine record, keyed by effect name.
    ///
    /// Unbounded, and bounded in practice by the effect count, which is fixed at boot.
    /// It exists so listing effects takes the shared mutex once rather than twice per
    /// effect: reading the two tables separately put a listing's lock traffic in
    /// proportion to the module count, on the mutex every journal write contends for.
    pub fn effect_states(&self) -> anyhow::Result<HashMap<String, EffectState>> {
        let mut states: HashMap<String, EffectState> = HashMap::new();
        let mut cursors = self
            .conn
            .prepare("SELECT effect, watermark FROM effect_cursor")
            .context("preparing the effect cursor query")?;
        let rows = cursors
            .query_map([], |row| {
                let watermark: i64 = row.get(1)?;
                Ok((row.get::<_, String>(0)?, watermark as u64))
            })
            .context("querying effect cursors")?;
        for row in rows {
            let (effect, watermark) = row.context("collecting effect cursors")?;
            states.entry(effect).or_default().watermark = Some(watermark);
        }

        let mut quarantines = self
            .conn
            .prepare("SELECT effect, position, reason, at FROM effect_quarantine")
            .context("preparing the effect quarantine query")?;
        let rows = quarantines
            .query_map([], |row| {
                let position: i64 = row.get(1)?;
                Ok((
                    row.get::<_, String>(0)?,
                    QuarantineRow {
                        position: position as u64,
                        reason: row.get(2)?,
                        at: row.get(3)?,
                    },
                ))
            })
            .context("querying effect quarantines")?;
        for row in rows {
            let (effect, quarantine) = row.context("collecting effect quarantines")?;
            states.entry(effect).or_default().quarantine = Some(quarantine);
        }
        Ok(states)
    }

    /// Live subject-key counts per subject field. The reserved global uniqueness
    /// secret is excluded: it is not a subject, and it can never be erased.
    ///
    /// The one reader here that scans without a limit, because a count of a group is
    /// not something a limit can bound. It is an index-only aggregate over
    /// `subject_key`'s primary key, and the caller takes it once for a listing rather
    /// than once per page, since the counts do not change between pages of one.
    pub fn subject_key_counts(&self) -> anyhow::Result<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT subject, count(*) FROM subject_key \
                 WHERE subject <> ?1 GROUP BY subject ORDER BY subject",
            )
            .context("preparing the subject key count query")?;
        let rows = stmt
            .query_map(params![crypto::GLOBAL_SUBJECT], |row| {
                let count: i64 = row.get(1)?;
                Ok((row.get(0)?, count as u64))
            })
            .context("querying subject key counts")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("collecting subject key counts")
    }

    /// One page of live subject keys in `(field, value)` order, never the key
    /// material. `after` is the last pair of the previous page.
    pub fn subject_keys_page(
        &self,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<SubjectInfo>> {
        const COLUMNS: &str = "SELECT subject, subject_value, master_key_id, parent_subject, parent_value, created_at \
             FROM subject_key WHERE subject <> ?1";
        const ORDER: &str = " ORDER BY subject, subject_value LIMIT ";
        let read = |row: &rusqlite::Row| {
            let parent_subject: Option<String> = row.get(3)?;
            let parent_value: Option<String> = row.get(4)?;
            Ok(SubjectInfo {
                subject: row.get(0)?,
                subject_value: row.get(1)?,
                master_key_id: row.get(2)?,
                parent: parent_subject.zip(parent_value),
                created_at: row.get(5)?,
            })
        };
        // Two statements rather than one with a NULL-guarded keyset predicate: the
        // guarded form makes the planner choose between the primary key and a scan on
        // a value it cannot see until bind time.
        let rows = match after {
            Some((field, value)) => {
                let sql = format!(
                    "{COLUMNS} AND (subject > ?2 OR (subject = ?2 AND subject_value > ?3)){ORDER}?4"
                );
                let mut stmt = self
                    .conn
                    .prepare(&sql)
                    .context("preparing the subject key page query")?;
                let rows = stmt
                    .query_map(
                        params![crypto::GLOBAL_SUBJECT, field, value, limit as i64],
                        read,
                    )
                    .context("querying subject keys")?;
                rows.collect::<Result<Vec<_>, _>>()
            }
            None => {
                let sql = format!("{COLUMNS}{ORDER}?2");
                let mut stmt = self
                    .conn
                    .prepare(&sql)
                    .context("preparing the subject key page query")?;
                let rows = stmt
                    .query_map(params![crypto::GLOBAL_SUBJECT, limit as i64], read)
                    .context("querying subject keys")?;
                rows.collect::<Result<Vec<_>, _>>()
            }
        };
        rows.context("collecting subject keys")
    }

    /// Whether a subject still has a key. `false` covers both "erased" and "never
    /// had one": erasure deletes the row, so the two are the same state on disk.
    ///
    /// Excludes the reserved global uniqueness secret, as the listing readers do. It is
    /// not a subject, and a point lookup that reported it would contradict the inventory
    /// that hides it.
    /// Whether a key row is reachable: it exists, and so does every row above it.
    ///
    /// Reachability rather than existence, because that is what "is this subject erased"
    /// means once a key can hang from another. A child outlives its parent's deletion by
    /// however long the sweep takes to reclaim it, and for that whole window its row is
    /// present and nothing can open it. Answering from existence would report `live` for a
    /// subject whose data is permanently unreadable, and would change its answer when the
    /// sweeper happened to run, which is not a thing an operator should be able to observe.
    ///
    /// Structural, so it needs no master key: the walk reads parent pointers and unwraps
    /// nothing, which is what keeps `hekla erase` usable without one.
    ///
    /// `UNION` rather than `UNION ALL`, and it is not a preference: a hand-edited store
    /// with a cycle in its parent pointers would make `UNION ALL` walk for ever and hang
    /// the request. Deduplicating bounds the walk by the table, so a cycle reaches no root
    /// and answers `false`, which is the safe direction for a question that gates reads.
    pub fn subject_key_reachable(&self, subject: &str, value: &str) -> anyhow::Result<bool> {
        self.conn
            .query_row(
                "WITH RECURSIVE above(subject, subject_value, parent_subject, parent_value) AS ( \
                     SELECT subject, subject_value, parent_subject, parent_value \
                       FROM subject_key WHERE subject = ?1 AND subject_value = ?2 \
                     UNION \
                     SELECT p.subject, p.subject_value, p.parent_subject, p.parent_value \
                       FROM subject_key p JOIN above a ON p.subject = a.parent_subject \
                                                      AND p.subject_value = a.parent_value \
                 ) SELECT count(*) FROM above WHERE parent_subject IS NULL",
                params![subject, value],
                |row| row.get::<_, i64>(0),
            )
            // Exactly one row of the walk is a root, and only if the chain is unbroken:
            // a missing link ends the recursion before it reaches one.
            .map(|roots| roots == 1)
            .context("checking whether a subject key is reachable")
    }

    /// Whether a key row is on disk, reachable or not. The storage question, which the
    /// sweep and its tests ask; [`OpDb::subject_key_reachable`] is the erasure one.
    pub fn subject_key_exists(&self, field: &str, value: &str) -> anyhow::Result<bool> {
        if field == crypto::GLOBAL_SUBJECT {
            return Ok(false);
        }
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM subject_key WHERE subject = ?1 AND subject_value = ?2",
                params![field, value],
                |row| row.get(0),
            )
            .optional()
            .context("checking for a subject key")?;
        Ok(found.is_some())
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
                4 => tx.execute_batch(SCHEMA_V5).context("applying schema v5")?,
                5 => tx.execute_batch(SCHEMA_V6).context("applying schema v6")?,
                6 => tx.execute_batch(SCHEMA_V7).context("applying schema v7")?,
                7 => tx.execute_batch(SCHEMA_V8).context("applying schema v8")?,
                8 => tx.execute_batch(SCHEMA_V9).context("applying schema v9")?,
                9 => {
                    // The rows are carried across, but their namespace is not what it
                    // was: a subject used to be spelled with the *field* name the
                    // annotation pointed at and is now the declared type's name, and
                    // nothing here knows the mapping between them. So a carried row is
                    // reachable only if the project happened to name its subject exactly
                    // what the field was called. Said out loud rather than left to be
                    // discovered, because the symptom is every pre-existing sealed value
                    // reading back absent, which is indistinguishable from an erasure.
                    let carried: i64 = tx
                        .query_row("SELECT count(*) FROM subject_key", [], |row| row.get(0))
                        .context("counting subject keys before the v10 rebuild")?;
                    tx.execute_batch(SCHEMA_V10)
                        .context("applying schema v10")?;
                    if carried > 0 {
                        tracing::warn!(
                            "carried {carried} subject key row(s) into schema v10. a subject is \
                             now named by its declared type rather than by the field the \
                             annotation named, so a row whose old spelling differs is \
                             unreachable and everything sealed under it reads as erased"
                        );
                    }
                }
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
/// Schema v5 records which builtin a journaled call came from. The call hash is a
/// hash of the pre-image, so the kind is otherwise unrecoverable from a stored row:
/// an introspection view could show what a call returned but not what it was. Rows
/// written before this migration read `NULL`, which is why the column is nullable
/// rather than `NOT NULL DEFAULT ''` (an invented value that reads as a real one).
const SCHEMA_V5: &str = "
ALTER TABLE effect_journal ADD COLUMN kind TEXT;
";

/// Schema v6 replaces `module_metadata` with `declaration`, one row per heklang
/// declaration rather than per `.hk` file, keyed by what the declaration *does*.
///
/// `module_metadata` recorded a hash of a file's raw bytes, which made a reformat
/// indistinguishable from a rewrite and made two declarations sharing a file share a
/// hash. heklang's digest hashes the lowered IR instead, so the identity here is
/// `(kind, name, hash)`: a boot that loads unchanged code writes no new row, and a
/// restart loop costs nothing. Every version a declaration has ever had is kept, which
/// is what lets an invocation's `script_hash` be resolved back to the form that ran.
///
/// The old table is dropped rather than migrated. Its hashes are of a different thing
/// and cannot be translated into these.
const SCHEMA_V6: &str = "
DROP TABLE module_metadata;

CREATE TABLE declaration (
    kind           TEXT    NOT NULL,  -- event|enum|record|function|command|projector|effect
    name           TEXT    NOT NULL,  -- heklang's entry name, verbatim (an event keeps its `@`)
    hash           TEXT    NOT NULL,  -- what this declaration does
    signature_hash TEXT,              -- what of it is visible outside; NULL for a `fn`
    form           TEXT    NOT NULL,  -- the packed digest line, which reads back
    signature      TEXT,
    -- The file it was declared in. Outside the digest's identity on purpose: heklang
    -- treats a module as a label, so moving a declaration updates this and moves no hash.
    module         TEXT,
    first_seen     TEXT    NOT NULL,
    last_seen      TEXT    NOT NULL,
    -- Whether this is the version the running process loaded. Exactly one row per
    -- (kind, name) carries it, and `WHERE current = 1` is the deployed inventory.
    current        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (kind, name, hash)
);
CREATE INDEX declaration_current ON declaration (current, kind, name);
";

/// Schema v7 records *why* an invocation reached `terminal`, for the one case a reader
/// cannot otherwise recover: an operator skip.
///
/// A skip completes a wedged invocation without running it to a conclusion, and until
/// this column it left exactly the row a success leaves. Every reader that needed the
/// distinction had to infer it from the shape of the journal, which cannot carry it: a
/// skip before the first call and a run that genuinely called nothing are both an empty
/// journal, and a skip after two calls reads as a run that made two. Pre-v7 rows are
/// NULL, which reads as "not known to be a skip" and leaves them to that inference
/// rather than asserting a fact the column was not there to record.
const SCHEMA_V7: &str = "
ALTER TABLE effect_invocation ADD COLUMN skipped_at TEXT;
";

/// Schema v8 is rule 15's half of the effect runtime: `docs/effects.md` makes an arm
/// declare the lane it runs in and how much of history it wants, and neither fits in
/// the one integer `effect_cursor` holds.
///
/// **`effect_lane` is what a watermark cannot say.** `effect_cursor.watermark` becomes
/// the *global low-water mark* (the highest position every event at or below is
/// terminal) and keeps its job as the resume point and the sweeper's bound. But lanes
/// complete out of order, so one integer cannot also say that lane A is at 900 while
/// lane B is at 500. These rows carry that, and only for lanes *ahead* of the mark:
/// they are deleted as it passes them, because a `@key user_id` effect would otherwise
/// accumulate one permanent row per user, nearly all of them lanes that saw one event
/// and will never see another.
///
/// They are a resume *index*, not the correctness boundary. `begin_invocation`
/// returning `AlreadyTerminal` is what actually stops a position running twice, and a
/// boot with this table empty would be correct, only slower. What they buy is that a
/// resume from a mark far behind a fast lane skips those positions in memory instead of
/// asking the op-DB about each one, and that per-lane progress outlives the process.
///
/// **`effect_activation` is the `on live` boundary, and it is deliberately not a column
/// on `effect_cursor`.** The boundary is written at first activation, before the effect
/// has advanced anything, and a cursor row means "this effect has persisted a
/// watermark": `/admin/effects` reports a missing one as `null` for "never ran", and
/// `sweep_effect_journal` relies on the same absence to sweep nothing. Writing a cursor
/// row early would quietly destroy both. `lane_scheme` rides along because it has the
/// same lifetime: one row per effect, written when it first runs here.
const SCHEMA_V8: &str = "
CREATE TABLE effect_lane (
    effect   TEXT    NOT NULL,
    lane     TEXT    NOT NULL,   -- `crate::lane::encode` of the arm's `@key` values
    position INTEGER NOT NULL,   -- every position in this lane at or below it is terminal
    PRIMARY KEY (effect, lane)
);
-- The sweep deletes by `(effect, position <= mark)`, which the primary key cannot serve.
CREATE INDEX effect_lane_by_position ON effect_lane (effect, position);

CREATE TABLE effect_activation (
    effect        TEXT    NOT NULL PRIMARY KEY,
    -- The log head the first time this effect ran against this data directory. `on live`
    -- arms decline every position at or below it, for as long as the row exists.
    live_boundary INTEGER NOT NULL,
    -- Which lane each event type lands in, as canonical JSON. Compared at every boot:
    -- a key that changed repartitions the lanes, so the rows above are meaningless.
    lane_scheme   TEXT    NOT NULL,
    activated_at  TEXT    NOT NULL
);
";

/// Schema v9 is rule 15's batch collapse: an `on latest` arm runs once per key per
/// dispatch batch, at the newest matching position in it, and this column is where the
/// positions it folded away are recorded.
///
/// **A range rather than a list, and the range is exact.** An invocation's collapsed set
/// is every position in `[collapsed_from, position]` whose arm and lane match this
/// invocation's, so the two integers reconstruct the grouping and the positions between
/// them carry nothing the range does not. The dispatcher folds forward as it admits, so
/// `collapsed_from` is the *first admitted* position of the group: a matching position an
/// earlier invocation already covered was dropped before the group existed, and is
/// therefore outside the range rather than counted twice.
///
/// **Re-deriving membership depends on two guards that live elsewhere**, so this says so
/// rather than leaving a later reader to find them. `crate::effect`'s `lane_of` has to
/// answer the same later, and it would not if the effect's `@key` had moved.
/// `effect_activation.lane_scheme` is compared at every boot and stops an effect whose key
/// repartitioned the lanes, and a key change also moves the effect's digest, so
/// `crate::verify` skips the invocation as edited before it reads this column at all.
///
/// NULL is what a row with nothing folded into it holds, and what every pre-v9 row reads
/// back as. Both are the same honest answer: this invocation covers its own position and
/// no more. There is no index, because the only read is by primary key.
const SCHEMA_V9: &str = "
ALTER TABLE effect_invocation ADD COLUMN collapsed_from INTEGER;
";

/// Schema v10 rebuilds the subject key store for two changes at once, because SQLite
/// cannot relax a `NOT NULL` in place and one rebuild is cheaper than two.
///
/// The column is `subject` rather than `subject_field`: a subject stopped being a field
/// name when heklang made it a declared type, so the old spelling names the wrong thing.
/// And a row gains a parent, so a child's key can be wrapped under its parent's and
/// deleting the parent makes every key beneath it unwrappable in one row delete.
///
/// The `CHECK` is the reason this is a rebuild rather than two `ADD COLUMN`s: a row is
/// wrapped under exactly one thing, a master (a root) or a parent subject (a child).
/// Neither would be unopenable and both would be ambiguous, so it is a database rule
/// rather than a convention this module has to remember.
const SCHEMA_V10: &str = "
CREATE TABLE subject_key_new (
    subject        TEXT NOT NULL,
    subject_value  TEXT NOT NULL,
    wrapped_key    BLOB NOT NULL,  -- AEAD-wrapped subject secret (nonce || ciphertext)
    master_key_id  TEXT,           -- set iff this row is a root
    parent_subject TEXT,           -- set iff this row is a child
    parent_value   TEXT,
    parent_fingerprint TEXT,       -- which generation of that parent, see `ParentRef`
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (subject, subject_value),
    CHECK ((master_key_id IS NULL) <> (parent_subject IS NULL)),
    CHECK ((parent_subject IS NULL) = (parent_value IS NULL)),
    CHECK ((parent_subject IS NULL) = (parent_fingerprint IS NULL))
);
INSERT INTO subject_key_new (subject, subject_value, wrapped_key, master_key_id, created_at)
    SELECT subject_field, subject_value, wrapped_key, master_key_id, created_at FROM subject_key;
DROP TABLE subject_key;
ALTER TABLE subject_key_new RENAME TO subject_key;
-- The boot-time master check and a rotation both read the roots only, which the primary
-- key does not cover; this keeps them from full-scanning subject_key.
CREATE INDEX subject_key_by_master ON subject_key (master_key_id);
-- The sweep looks for children whose parent is gone, and a cascading erase counts the
-- descendants it is about to make unreadable. Both walk this way.
CREATE INDEX subject_key_by_parent ON subject_key (parent_subject, parent_value);
";

/// What a rewind would discard, and what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RewindCounts {
    /// Recorded invocations above the target, whose journal rows cascade with them.
    pub invocations: usize,
    /// Per-lane rows, all of which describe progress the rewind repudiates.
    pub lanes: usize,
    /// Whether a quarantine was cleared.
    pub quarantine: bool,
}

/// What an effect's first activation against this data directory recorded: the position
/// `on live` arms decline at or below, and the lane scheme its rows above the watermark
/// are keyed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub live_boundary: u64,
    pub lane_scheme: String,
}

/// One effect invocation, as an introspection reader sees it. `status` is `running` or
/// `terminal`, and `skipped_at` is set when an operator skipped a wedged invocation
/// rather than it reaching a conclusion of its own. A terminal `reveal()` stays plain
/// completion: that is the handler reaching its own documented end, not an operator
/// stepping over one.
pub struct InvocationRow {
    pub position: u64,
    pub status: String,
    pub script_hash: String,
    pub created_at: String,
    pub completed_at: Option<String>,
    /// When an operator skipped this wedged invocation. `None` for a run that reached
    /// its own end, and for every row written before schema v7.
    pub skipped_at: Option<String>,
}

/// One invocation found by position, carrying the effect it belongs to. Narrower
/// than [`InvocationRow`] on purpose: a trace joins many positions at once and needs
/// only enough to name the invocation and link to it.
pub struct InvocationAt {
    pub effect: String,
    pub position: u64,
    pub status: String,
}

/// One journaled call, in the order it was made. `kind` is `None` for a row written
/// before schema v5. The call arguments are deliberately absent: they are not stored,
/// only hashed.
pub struct JournalRow {
    pub call_hash: String,
    pub disambiguator: u64,
    pub kind: Option<String>,
    pub result: String,
    pub created_at: String,
}

/// One declaration, as recorded at boot. A row is one *version* of a declaration, so
/// several may share a `(kind, name)` and at most one of those carries `current`.
pub struct DeclarationRow {
    pub kind: String,
    pub name: String,
    pub hash: String,
    pub signature_hash: Option<String>,
    pub form: String,
    pub signature: Option<String>,
    pub module: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
    pub current: bool,
}

/// One recorded invocation that reached a terminal state, as the replay check sweeps it.
pub struct TerminalInvocation {
    pub position: u64,
    /// What the module hashed to when it ran, so a replay can skip an invocation whose
    /// code has since been edited rather than report it as a divergence.
    pub script_hash: String,
    /// Rule 15: the oldest position folded into this invocation by an `on latest` arm's
    /// batch collapse, or `None` when it covers only its own position.
    pub collapsed_from: Option<u64>,
}

/// A durable effect quarantine, with the time it was recorded.
pub struct QuarantineRow {
    pub position: u64,
    pub reason: String,
    pub at: String,
}

/// One effect's durable runtime state: where it resumes from, and whether a verify
/// check has quarantined it. Both absent is an effect that has never run.
#[derive(Default)]
pub struct EffectState {
    pub watermark: Option<u64>,
    pub quarantine: Option<QuarantineRow>,
}

/// One live subject key, without any key material. A subject with no row has either
/// been erased or never had a value encrypted under it; the two are indistinguishable
/// here by construction, since erasure deletes the row.
pub struct SubjectInfo {
    pub subject: String,
    pub subject_value: String,
    /// The master this row is wrapped under, for a root. `None` for a child, whose
    /// wrapping key is derived from its parent's secret and references no master.
    pub master_key_id: Option<String>,
    /// The row this one's key is wrapped under, for a child. Deleting it makes this row
    /// unopenable, which is what the operator reading this list wants to see.
    pub parent: Option<(String, String)>,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A root key row, which is what every one of these is: the hierarchy has its own
    /// tests and these are about the store beneath it.
    fn root(wrapped: &[u8], master: &str) -> SubjectKey {
        SubjectKey {
            wrapped: wrapped.to_vec(),
            wrapping: Wrapping::Master(master.to_owned()),
        }
    }

    /// The master a root row is wrapped under, for a test that only ever makes roots.
    fn master_of(row: &SubjectKey) -> String {
        match &row.wrapping {
            Wrapping::Master(id) => id.clone(),
            other => panic!("expected a root, got {other:?}"),
        }
    }

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
            "declaration",
            "effect_cursor",
            "effect_quarantine",
            "effect_lane",
            "effect_activation",
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
        db.complete_invocation("e", 1, "t2", None).unwrap();
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
        db.journal_put("e", 1, "abc", 0, "http", r#"{"n":1}"#, "t1")
            .unwrap();
        db.journal_put("e", 1, "abc", 1, "http", r#"{"n":2}"#, "t2")
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
        let mut db = OpDb::open_in_memory().unwrap();
        // Both versions of `e` are on record, which is what makes "old" a *known* other
        // version rather than an unrecognised hash.
        db.set_current_declarations(&[decl("effect", "e", "old")], "t0")
            .unwrap();
        db.set_current_declarations(&[decl("effect", "e", "new")], "t1")
            .unwrap();

        db.begin_invocation("e", 1, "old", "t0").unwrap(); // running, stale hash
        db.begin_invocation("e", 2, "new", "t0").unwrap(); // running, current hash
        db.begin_invocation("e", 3, "old", "t0").unwrap();
        db.complete_invocation("e", 3, "t1", None).unwrap(); // terminal, ignored
        assert_eq!(db.running_with_hash_mismatch("e", "new").unwrap(), vec![1]);
    }

    /// The third outcome, and the one the `EXISTS` clause exists for.
    ///
    /// `script_hash` used to hold a hash of the effect file's bytes. Those values are
    /// still in the table and will never match an entry hash, but they are not evidence
    /// that anything changed: they are a different measurement. Reporting them would
    /// name every in-flight invocation on the first boot after the migration and blame
    /// the code.
    #[test]
    fn an_unrecognised_script_hash_is_not_reported_as_a_change() {
        let mut db = OpDb::open_in_memory().unwrap();
        db.set_current_declarations(&[decl("effect", "e", "new")], "t0")
            .unwrap();
        db.begin_invocation("e", 1, "a-hash-of-some-file-bytes", "t0")
            .unwrap();

        assert!(
            db.running_with_hash_mismatch("e", "new")
                .unwrap()
                .is_empty(),
            "a hash this table never held says nothing about the code"
        );
        assert!(
            db.declaration_by_hash("effect", "e", "a-hash-of-some-file-bytes")
                .unwrap()
                .is_none(),
            "and it is distinguishable from a known older version by exactly that"
        );
    }
    /// The v10 rebuild carries existing key rows across, as roots.
    ///
    /// The ladder runs on every fresh open, so the SQL is exercised constantly; what is
    /// not is the `INSERT ... SELECT` doing its job over rows that are actually there,
    /// which is the only part an upgrade depends on. This stands the table back up in its
    /// v3 shape, puts rows in it, winds `user_version` back and reopens.
    ///
    /// What it deliberately does not claim is that the carried rows are *reachable*. The
    /// namespace changed meaning in this version, from the field an annotation named to
    /// the declared subject's name, and nothing here knows the mapping; the migration logs
    /// a warning saying so. This is about not losing the bytes.
    #[test]
    fn the_v10_rebuild_carries_existing_key_rows_across_as_roots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hekla.db");
        drop(OpDb::open(&path).unwrap());

        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TABLE subject_key;
                 CREATE TABLE subject_key (
                     subject_field TEXT NOT NULL,
                     subject_value TEXT NOT NULL,
                     wrapped_key   BLOB NOT NULL,
                     master_key_id TEXT NOT NULL,
                     created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                     PRIMARY KEY (subject_field, subject_value)
                 );
                 CREATE INDEX subject_key_by_master ON subject_key (master_key_id);
                 INSERT INTO subject_key (subject_field, subject_value, wrapped_key, master_key_id, created_at)
                     VALUES ('customer_id', '42', x'DEADBEEF', 'master-A', '2020-01-01T00:00:00.000Z'),
                            ('shop_id', '7', x'C0FFEE', 'master-B', '2020-01-02T00:00:00.000Z');
                 PRAGMA user_version = 9;",
            )
            .unwrap();
        }

        let db = OpDb::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        let carried = db.get_subject_key("customer_id", "42").unwrap().unwrap();
        assert_eq!(
            carried.wrapped,
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            "the bytes are the point"
        );
        assert_eq!(
            carried.wrapping,
            Wrapping::Master("master-A".to_owned()),
            "an existing row predates the hierarchy, so it is a root"
        );
        assert_eq!(
            db.get_subject_key("shop_id", "7")
                .unwrap()
                .unwrap()
                .wrapping,
            Wrapping::Master("master-B".to_owned()),
            "every row, and each under the master that actually wrapped it"
        );

        // `created_at` survives, so an operator's inventory does not reset to today.
        let page = db.subject_keys_page(None, 10).unwrap();
        assert_eq!(page.len(), 2);
        assert!(
            page.iter()
                .all(|row| row.created_at.starts_with("2020-01-"))
        );
        assert!(page.iter().all(|row| row.parent.is_none()));

        // And the rebuilt table carries the constraint the old one had no room for.
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert!(
            conn.execute(
                "UPDATE subject_key SET master_key_id = NULL WHERE subject_value = '42'",
                [],
            )
            .is_err(),
            "the CHECK came with the rebuild"
        );
    }

    /// Replacing an unreachable row is a compare-and-set on the bytes, so the second
    /// writer to arrive keeps the first one's key instead of deleting it.
    ///
    /// Keying it on the parent looks equivalent and is not, which is the whole reason this
    /// exists: a replacement hangs from the *same* parent as the row it replaced, so a
    /// parent-keyed delete matches the other writer's fresh row rather than the stale one
    /// they both read. Each would then destroy a key the other had already sealed under,
    /// with no error anywhere. Written against the two calls rather than two threads
    /// because the failure is about what the statement matches, not about timing.
    #[test]
    fn replacing_an_unreachable_row_is_a_compare_and_set_on_its_bytes() {
        let db = OpDb::open_in_memory().unwrap();
        let child = |wrapped: &[u8]| SubjectKey {
            wrapped: wrapped.to_vec(),
            wrapping: Wrapping::Parent(ParentRef {
                subject: "Tenant".to_owned(),
                value: "7".to_owned(),
                fingerprint: "fp".to_owned(),
            }),
        };
        let stale = child(b"wrapped-under-the-old-tenant");
        db.get_or_insert_subject_key("Member", "1", &stale, None)
            .unwrap();

        // Both writers read the same stale row, then each mints a replacement that hangs
        // from the same parent.
        let first = db
            .get_or_insert_subject_key("Member", "1", &child(b"minted-by-A"), Some(&stale))
            .unwrap();
        assert_eq!(first.wrapped, b"minted-by-A", "the first replacement lands");

        let second = db
            .get_or_insert_subject_key("Member", "1", &child(b"minted-by-B"), Some(&stale))
            .unwrap();
        assert_eq!(
            second.wrapped, b"minted-by-A",
            "the second writer's stale CAS misses, so it reads back the winner's key \
             rather than clobbering content already sealed under it"
        );
    }

    #[test]
    fn rewrap_is_a_compare_and_set_on_the_master_id() {
        let db = OpDb::open_in_memory().unwrap();
        db.get_or_insert_subject_key("Customer", "1", &root(b"wrapped-under-A", "master-A"), None)
            .unwrap();
        // A rotation snapshots the row under master-A, but the subject is erased and
        // recreated under a different master before the rewrap lands. The rewrap expects
        // master-A, so its compare-and-set misses and the recreated secret is preserved
        // rather than clobbered by the stale rewrap.
        let rewrapped = db
            .rewrap_subject_keys(&[(
                "Customer".into(),
                "1".into(),
                b"stale-rewrap".to_vec(),
                "master-C".into(),
                "master-B".into(),
            )])
            .unwrap();
        assert_eq!(rewrapped, 0, "a mismatched expected master rewraps nothing");
        let row = db.get_subject_key("Customer", "1").unwrap().unwrap();
        let (wrapped, master) = (row.wrapped.clone(), master_of(&row));
        assert_eq!(
            master, "master-A",
            "a mismatched expected master is a no-op"
        );
        assert_eq!(wrapped, b"wrapped-under-A");
        // A rewrap that expects the row's real current master applies.
        let rewrapped = db
            .rewrap_subject_keys(&[(
                "Customer".into(),
                "1".into(),
                b"wrapped-under-C".to_vec(),
                "master-C".into(),
                "master-A".into(),
            )])
            .unwrap();
        assert_eq!(rewrapped, 1, "a matching expected master rewraps the row");
        let row = db.get_subject_key("Customer", "1").unwrap().unwrap();
        let (wrapped, master) = (row.wrapped.clone(), master_of(&row));
        assert_eq!(master, "master-C");
        assert_eq!(wrapped, b"wrapped-under-C");
    }

    #[test]
    fn distinct_master_key_ids_lists_each_once() {
        let db = OpDb::open_in_memory().unwrap();
        db.get_or_insert_subject_key("Customer", "1", &root(b"w", "master-A"), None)
            .unwrap();
        db.get_or_insert_subject_key("Customer", "2", &root(b"w", "master-A"), None)
            .unwrap();
        db.get_or_insert_subject_key("Customer", "3", &root(b"w", "master-B"), None)
            .unwrap();
        let mut ids = db.distinct_master_key_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["master-A".to_owned(), "master-B".to_owned()]);
    }

    /// A declaration row with only the columns a test cares about set.
    fn decl(kind: &str, name: &str, hash: &str) -> DeclarationRow {
        DeclarationRow {
            kind: kind.to_owned(),
            name: name.to_owned(),
            hash: hash.to_owned(),
            signature_hash: None,
            form: format!("({kind} {name})"),
            signature: None,
            module: None,
            first_seen: String::new(),
            last_seen: String::new(),
            current: true,
        }
    }

    fn count_declarations(db: &OpDb) -> i64 {
        db.connection()
            .query_row("SELECT count(*) FROM declaration", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_declaration_that_did_not_change_writes_no_new_row() {
        let mut db = OpDb::open_in_memory().unwrap();
        db.set_current_declarations(&[decl("effect", "m", "h1")], "t0")
            .unwrap();
        db.set_current_declarations(&[decl("effect", "m", "h1")], "t1")
            .unwrap();

        assert_eq!(
            count_declarations(&db),
            1,
            "the same declaration re-loaded is the same row, so a restart costs nothing"
        );
        let row = db
            .declaration_by_hash("effect", "m", "h1")
            .unwrap()
            .unwrap();
        assert_eq!(
            row.first_seen, "t0",
            "the first sighting is not overwritten"
        );
        assert_eq!(row.last_seen, "t1", "the latest one is");
    }

    #[test]
    fn a_changed_declaration_keeps_the_version_it_replaced() {
        let mut db = OpDb::open_in_memory().unwrap();
        db.set_current_declarations(&[decl("effect", "m", "h1")], "t0")
            .unwrap();
        db.set_current_declarations(&[decl("effect", "m", "h2")], "t1")
            .unwrap();

        assert_eq!(count_declarations(&db), 2, "both versions are kept");
        let current = db.current_declarations().unwrap();
        assert_eq!(
            current
                .iter()
                .map(|row| row.hash.as_str())
                .collect::<Vec<_>>(),
            vec!["h2"],
            "only the version this boot loaded is current"
        );
        // The point of keeping the old row: an invocation recorded under `h1` can still
        // be resolved to the form that ran, which is what tells "a known older version"
        // apart from "a hash this table never held".
        assert!(
            db.declaration_by_hash("effect", "m", "h1")
                .unwrap()
                .is_some()
        );
        assert!(
            db.declaration_by_hash("effect", "m", "h9")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_declaration_that_is_gone_stops_being_current() {
        let mut db = OpDb::open_in_memory().unwrap();
        db.set_current_declarations(
            &[decl("effect", "kept", "h1"), decl("effect", "gone", "h2")],
            "t0",
        )
        .unwrap();
        db.set_current_declarations(&[decl("effect", "kept", "h1")], "t1")
            .unwrap();

        let current = db.current_declarations().unwrap();
        assert_eq!(
            current
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            vec!["kept"],
            "a declaration deleted from the project is no longer deployed"
        );
        assert_eq!(
            count_declarations(&db),
            2,
            "but its row survives, so what it was is still answerable"
        );
    }

    /// The three bounds a replay against a *live* deployment needs, and the order it
    /// wants them in.
    ///
    /// `upto` is the tip of the prefix the reader pinned. A server appending while a
    /// plan runs completes invocations the reader has no event for, and replaying one
    /// would report a race as a behaviour change. `script_hash` keeps the baseline to
    /// one program: retention outlives an edit, so rows from a version already replaced
    /// would otherwise be replayed against a candidate that is not replacing them.
    /// `limit` bounds the work, and takes the newest rows because those are the history
    /// closest to what is running.
    #[test]
    fn recent_terminal_invocations_stops_at_the_prefix_the_hash_and_the_limit() {
        let db = OpDb::open_in_memory().unwrap();
        for position in 1..=5u64 {
            db.begin_invocation("e", position, "h", "t0").unwrap();
            db.complete_invocation("e", position, "t1", None).unwrap();
        }
        // Still running, so not a candidate at all.
        db.begin_invocation("e", 6, "h", "t0").unwrap();
        // The same effect, one edit ago.
        db.begin_invocation("e", 7, "older", "t0").unwrap();
        db.complete_invocation("e", 7, "t1", None).unwrap();

        assert_eq!(
            db.recent_terminal_invocations("e", "h", 100, 100).unwrap(),
            vec![5, 4, 3, 2, 1],
            "newest first, and a running invocation is not one"
        );
        assert_eq!(
            db.recent_terminal_invocations("e", "h", 3, 100).unwrap(),
            vec![3, 2, 1],
            "nothing above the pinned prefix, however terminal it is"
        );
        assert_eq!(
            db.recent_terminal_invocations("e", "h", 100, 2).unwrap(),
            vec![5, 4],
            "the limit keeps the most recent, not the first it happens to read"
        );
        assert_eq!(
            db.recent_terminal_invocations("e", "older", 100, 100)
                .unwrap(),
            vec![7],
            "a row belongs to the program that wrote it, not to the effect's name"
        );
        assert!(
            db.recent_terminal_invocations("other", "h", 100, 100)
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            db.count_terminal_invocations("e", "h", 100).unwrap(),
            5,
            "the count answers the same question with no limit on it"
        );
        assert_eq!(db.count_terminal_invocations("e", "h", 3).unwrap(), 3);
        assert_eq!(db.count_terminal_invocations("e", "older", 100).unwrap(), 1);
    }
    #[test]
    fn sweep_removes_only_old_terminal_and_cascades_the_journal() {
        let db = OpDb::open_in_memory().unwrap();
        // The cursor is past both completed positions, so age and status are the only
        // things under test here.
        db.set_effect_watermark("e", 2).unwrap();
        // Old terminal: swept, journal cascades.
        db.begin_invocation("e", 1, "h", "t0").unwrap();
        db.journal_put("e", 1, "abc", 0, "http", "{}", "t0")
            .unwrap();
        db.complete_invocation("e", 1, "2026-01-01T00:00:00Z", None)
            .unwrap();
        // Recent terminal: kept.
        db.begin_invocation("e", 2, "h", "t0").unwrap();
        db.complete_invocation("e", 2, "2026-12-31T00:00:00Z", None)
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
        db.journal_put("e", 5, "abc", 0, "http", "{}", "t0")
            .unwrap();
        db.complete_invocation("e", 5, "2020-01-01T00:00:00Z", None)
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
            db.journal_put(effect, 1, "abc", 0, "http", "{}", "t0")
                .unwrap();
            db.complete_invocation(effect, 1, "2020-01-01T00:00:00Z", None)
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
            db.journal_put("e", position, "abc", 0, "http", "{}", "t0")
                .unwrap();
            db.complete_invocation("e", position, "2020-01-01T00:00:00Z", None)
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

    // --- effect lanes and activation ---------------------------------------

    #[test]
    fn a_lane_records_its_own_position_and_never_moves_backwards() {
        let db = OpDb::open_in_memory().unwrap();
        db.record_effect_lane("e", "i:1", 5).unwrap();
        db.record_effect_lane("e", "i:2", 9).unwrap();
        // A retry can write an older position; `max` is what makes that harmless.
        db.record_effect_lane("e", "i:1", 3).unwrap();

        let lanes = db.effect_lanes("e").unwrap();
        assert_eq!(lanes.get("i:1"), Some(&5));
        assert_eq!(lanes.get("i:2"), Some(&9));
        assert_eq!(db.effect_lanes_outstanding("e").unwrap(), 2);
    }

    #[test]
    fn lanes_belong_to_one_effect() {
        let db = OpDb::open_in_memory().unwrap();
        db.record_effect_lane("one", "i:1", 5).unwrap();
        db.record_effect_lane("two", "i:1", 7).unwrap();
        assert_eq!(db.effect_lanes("one").unwrap().get("i:1"), Some(&5));
        assert_eq!(db.effect_lanes("two").unwrap().get("i:1"), Some(&7));
    }

    /// The rows exist only to describe lanes *ahead* of the global mark. Once it passes
    /// one, the row says nothing the mark does not, and keeping it is how a `@key
    /// user_id` effect grows a permanent row per user.
    #[test]
    fn the_sweep_drops_only_the_lanes_the_mark_has_passed() {
        let db = OpDb::open_in_memory().unwrap();
        db.record_effect_lane("e", "behind", 4).unwrap();
        db.record_effect_lane("e", "at", 7).unwrap();
        db.record_effect_lane("e", "ahead", 9).unwrap();

        assert_eq!(db.sweep_effect_lanes("e", 7).unwrap(), 2);
        let lanes = db.effect_lanes("e").unwrap();
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes.get("ahead"), Some(&9));
    }

    /// The whole point of resolving the boundary once: a later boot at a longer log
    /// reads back the head the *first* boot saw, so `on live` means "after this effect
    /// first ran here" rather than "after the last restart".
    #[test]
    fn an_activation_keeps_the_head_the_first_one_saw() {
        let db = OpDb::open_in_memory().unwrap();
        let first = db.activate_effect("e", 100, "{}", "t0").unwrap();
        assert_eq!(first.live_boundary, 100);

        let second = db.activate_effect("e", 900, "{}", "t1").unwrap();
        assert_eq!(
            second.live_boundary, 100,
            "a restart does not re-resolve it"
        );
    }

    #[test]
    fn reading_an_activation_does_not_create_one() {
        let db = OpDb::open_in_memory().unwrap();
        assert_eq!(db.effect_activation("e").unwrap(), None);
        db.activate_effect("e", 42, "{\"@a.b\":[\"k\"]}", "t0")
            .unwrap();
        let seen = db.effect_activation("e").unwrap().unwrap();
        assert_eq!(seen.live_boundary, 42);
        assert_eq!(seen.lane_scheme, "{\"@a.b\":[\"k\"]}");
    }

    #[test]
    fn a_lane_scheme_is_replaceable_without_moving_the_boundary() {
        let db = OpDb::open_in_memory().unwrap();
        db.activate_effect("e", 42, "old", "t0").unwrap();
        db.set_effect_lane_scheme("e", "new").unwrap();
        let seen = db.effect_activation("e").unwrap().unwrap();
        assert_eq!(seen.lane_scheme, "new");
        assert_eq!(
            seen.live_boundary, 42,
            "accepting a new key is not a rewind"
        );
    }

    /// v8 adds tables rather than touching one, so the rows that were already there have
    /// to come through untouched, including the cursor, whose *meaning* changes without
    /// its contents changing.
    #[test]
    fn rows_written_before_v8_survive_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hekla.db");
        {
            let conn = Connection::open(&path).unwrap();
            for schema in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
            ] {
                conn.execute_batch(schema).unwrap();
            }
            conn.pragma_update(None, "user_version", 7i64).unwrap();
            conn.execute(
                "INSERT INTO effect_cursor (effect, watermark) VALUES ('e', 42)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO effect_invocation (effect, position, script_hash, status, created_at) \
                 VALUES ('e', 7, 'h', 'terminal', 't0')",
                [],
            )
            .unwrap();
        }

        let db = OpDb::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(db.effect_resume_after("e").unwrap(), 42);
        // A directory that predates lanes has no lane rows and no activation, which is
        // exactly what a first boot under v8 should find: nothing to resume past, and a
        // boundary still to resolve.
        assert!(db.effect_lanes("e").unwrap().is_empty());
        assert_eq!(db.effect_activation("e").unwrap(), None);
    }

    /// v9 adds a column, so the question is what a row written before it says now. Null
    /// rather than the row's own position: an invocation recorded before batch collapse
    /// existed covered one position, and "nothing was folded into this" is exactly what
    /// null means for a row written after it too.
    #[test]
    fn an_invocation_written_before_v9_reads_back_with_nothing_collapsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hekla.db");
        {
            let conn = Connection::open(&path).unwrap();
            for schema in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
                SCHEMA_V8,
            ] {
                conn.execute_batch(schema).unwrap();
            }
            conn.pragma_update(None, "user_version", 8i64).unwrap();
            conn.execute(
                "INSERT INTO effect_invocation (effect, position, script_hash, status, created_at) \
                 VALUES ('e', 7, 'h', 'terminal', 't0')",
                [],
            )
            .unwrap();
        }

        let db = OpDb::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let recorded = db.terminal_invocations("e").unwrap();
        assert_eq!(recorded.len(), 1, "the migration preserves existing rows");
        assert_eq!(recorded[0].position, 7);
        assert_eq!(recorded[0].collapsed_from, None);
    }

    /// The range is written by the statement that makes the row terminal, so there is no
    /// window in which a completion is durable and what it folded is not.
    #[test]
    fn a_completion_records_the_range_it_folded() {
        let db = OpDb::open_in_memory().unwrap();
        db.begin_invocation("e", 9, "h", "t0").unwrap();
        db.complete_invocation("e", 9, "t1", Some(4)).unwrap();
        db.begin_invocation("e", 10, "h", "t0").unwrap();
        db.complete_invocation("e", 10, "t1", None).unwrap();

        let recorded = db.terminal_invocations("e").unwrap();
        assert_eq!(recorded[0].collapsed_from, Some(4));
        assert_eq!(
            recorded[1].collapsed_from, None,
            "an invocation that folded nothing says so rather than naming itself"
        );
    }

    // --- introspection readers ---------------------------------------------

    #[test]
    fn a_journal_row_written_before_v5_reads_back_with_no_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hekla.db");
        {
            // A database at v4: the journal exists, but without the kind column.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute_batch(SCHEMA_V3).unwrap();
            conn.execute_batch(SCHEMA_V4).unwrap();
            conn.pragma_update(None, "user_version", 4i64).unwrap();
            conn.execute(
                "INSERT INTO effect_invocation (effect, position, script_hash, status, created_at) \
                 VALUES ('e', 1, 'h', 'running', 't0')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO effect_journal (effect, position, call_hash, disambiguator, result, created_at) \
                 VALUES ('e', 1, 'old', 0, '{}', 't0')",
                [],
            )
            .unwrap();
        }

        let db = OpDb::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        db.journal_put("e", 1, "new", 0, "http", "{}", "t1")
            .unwrap();

        let calls = db.journal_entries("e", 1, 0, 10).unwrap();
        assert_eq!(calls.len(), 2, "the migration preserves existing rows");
        // Null rather than an invented value: the kind lives only inside the hash
        // pre-image, so a pre-v5 row genuinely does not record it.
        assert_eq!(calls[0].kind, None);
        assert_eq!(calls[0].call_hash, "old");
        assert_eq!(calls[1].kind.as_deref(), Some("http"));
    }

    #[test]
    fn the_invocation_join_reads_the_primary_key_rather_than_scanning() {
        // The table's only index is its `(effect, position)` primary key, so a join
        // constrained on `position` alone would scan a table that grows with traffic,
        // behind the mutex every journaled call contends for. This is the assertion
        // that keeps the query honest: a later edit that drops the `effect IN (...)`
        // half would still return the right rows and would still pass every other
        // test in this file.
        let db = OpDb::open_in_memory().unwrap();
        let plan = db.explain_invocations_at(2, 3).unwrap();
        assert!(
            plan.contains("SEARCH"),
            "the join must reach rows through the primary key: {plan}"
        );
        assert!(
            !plan.contains("SCAN effect_invocation"),
            "a full scan of a traffic-scaled table is what the bounded-reader \
             contract above forbids: {plan}"
        );
    }

    #[test]
    fn the_invocation_join_returns_only_the_asked_for_pairs_in_a_stable_order() {
        let db = OpDb::open_in_memory().unwrap();
        for effect in ["b-effect", "a-effect", "unasked"] {
            for position in 1..=4 {
                db.begin_invocation(effect, position, "h", "t0").unwrap();
            }
        }
        db.complete_invocation("a-effect", 2, "t9", None).unwrap();

        let found = db
            .invocations_at(&["a-effect", "b-effect"], &[2, 1])
            .unwrap();
        let pairs: Vec<(&str, u64, &str)> = found
            .iter()
            .map(|row| (row.effect.as_str(), row.position, row.status.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("a-effect", 1, "running"),
                ("b-effect", 1, "running"),
                ("a-effect", 2, "terminal"),
                ("b-effect", 2, "running"),
            ],
            "sorted by position then effect regardless of the order asked for or the \
             order the planner walks, because this is rendered into a response"
        );
        assert!(
            !found.iter().any(|row| row.effect == "unasked"),
            "an effect the caller did not name must not leak in"
        );
    }

    #[test]
    fn the_invocation_join_is_empty_rather_than_a_syntax_error_when_asked_for_nothing() {
        // `IN ()` does not parse, and both sides are legitimately empty: a project can
        // declare no effects, and a trace page can come back with no events.
        let db = OpDb::open_in_memory().unwrap();
        db.begin_invocation("e", 1, "h", "t0").unwrap();
        assert!(db.invocations_at(&[], &[1]).unwrap().is_empty());
        assert!(db.invocations_at(&["e"], &[]).unwrap().is_empty());
        assert!(db.invocations_at(&[], &[]).unwrap().is_empty());
    }

    #[test]
    fn invocations_page_newest_first_and_carry_their_lifecycle_timestamps() {
        let db = OpDb::open_in_memory().unwrap();
        for position in 1..=5 {
            db.begin_invocation("e", position, "h", "t0").unwrap();
        }
        db.complete_invocation("e", 2, "t9", None).unwrap();

        let first = db.invocations("e", u64::MAX, 2).unwrap();
        assert_eq!(
            first.iter().map(|row| row.position).collect::<Vec<_>>(),
            vec![5, 4],
            "u64::MAX must saturate rather than bind as -1 against a signed column"
        );

        let next = db
            .invocations("e", first.last().unwrap().position, 2)
            .unwrap();
        assert_eq!(
            next.iter().map(|row| row.position).collect::<Vec<_>>(),
            vec![3, 2],
            "the cursor is exclusive"
        );
        let completed = next.iter().find(|row| row.position == 2).unwrap();
        assert_eq!(completed.status, "terminal");
        assert_eq!(completed.completed_at.as_deref(), Some("t9"));

        let running = db.invocation("e", 1).unwrap().unwrap();
        assert_eq!(running.status, "running");
        assert_eq!(running.completed_at, None);
        assert!(db.invocation("e", 99).unwrap().is_none());
        assert!(db.invocation("other", 1).unwrap().is_none());
    }

    #[test]
    fn effect_state_distinguishes_never_ran_from_ran_to_zero() {
        let db = OpDb::open_in_memory().unwrap();
        assert!(!db.effect_states().unwrap().contains_key("e"));
        assert_eq!(db.effect_resume_after("e").unwrap(), 0);

        db.set_effect_watermark("e", 0).unwrap();
        let states = db.effect_states().unwrap();
        assert_eq!(
            states.get("e").and_then(|state| state.watermark),
            Some(0),
            "the driver flattens both to zero; an operator needs them apart"
        );
        assert!(states["e"].quarantine.is_none());
    }

    #[test]
    fn effect_states_answers_both_tables_for_every_effect_at_once() {
        let db = OpDb::open_in_memory().unwrap();
        db.set_effect_watermark("ran", 4).unwrap();
        db.quarantine_effect("broken", 7, "replay diverged")
            .unwrap();

        let states = db.effect_states().unwrap();
        assert_eq!(
            states.len(),
            2,
            "an effect appears if either table knows it"
        );
        assert_eq!(states["ran"].watermark, Some(4));
        assert!(states["ran"].quarantine.is_none());
        // Quarantined before it ever advanced: no cursor row, but a quarantine row.
        assert_eq!(states["broken"].watermark, None);
        let quarantine = states["broken"].quarantine.as_ref().unwrap();
        assert_eq!(quarantine.position, 7);
        assert_eq!(quarantine.reason, "replay diverged");
        assert!(!quarantine.at.is_empty());
    }

    #[test]
    fn current_declarations_read_back_in_a_stable_order() {
        let mut db = OpDb::open_in_memory().unwrap();
        db.set_current_declarations(
            &[
                decl("command", "zeta", "h1"),
                decl("projector", "alpha", "h2"),
                decl("command", "alpha", "h3"),
            ],
            "t0",
        )
        .unwrap();

        let rows = db.current_declarations().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row.kind.as_str(), row.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("command", "alpha"),
                ("command", "zeta"),
                ("projector", "alpha")
            ],
            "a declaration is identified by kind and name together"
        );
        assert_eq!(rows[0].hash, "h3");
    }

    #[test]
    fn the_subject_inventory_pages_and_excludes_the_global_secret() {
        let db = OpDb::open_in_memory().unwrap();
        for (field, value) in [
            ("customer_id", "1"),
            ("customer_id", "2"),
            ("shop_id", "9"),
            (crypto::GLOBAL_SUBJECT, "global"),
        ] {
            db.get_or_insert_subject_key(field, value, &root(b"wrapped", "m1"), None)
                .unwrap();
        }

        assert_eq!(
            db.subject_key_counts().unwrap(),
            vec![("customer_id".to_owned(), 2), ("shop_id".to_owned(), 1)],
            "the global uniqueness secret is not a subject and cannot be erased"
        );

        let first = db.subject_keys_page(None, 2).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|row| (row.subject.as_str(), row.subject_value.as_str()))
                .collect::<Vec<_>>(),
            vec![("customer_id", "1"), ("customer_id", "2")]
        );
        let last = first.last().unwrap();
        let next = db
            .subject_keys_page(Some((&last.subject, &last.subject_value)), 2)
            .unwrap();
        assert_eq!(
            next.iter()
                .map(|row| (row.subject.as_str(), row.subject_value.as_str()))
                .collect::<Vec<_>>(),
            vec![("shop_id", "9")],
            "the keyset cursor crosses a field boundary"
        );

        assert!(db.subject_key_exists("customer_id", "1").unwrap());
        assert!(!db.subject_key_exists("customer_id", "404").unwrap());
        db.delete_subject_key("customer_id", "1").unwrap();
        assert!(
            !db.subject_key_exists("customer_id", "1").unwrap(),
            "erasure deletes the row, so erased and never-existed are one state"
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
