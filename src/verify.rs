//! Invariant checks over a running or stopped project.
//!
//! The test suite covers cases someone thought of; these check the properties the
//! design rests on, against whatever state the runtime actually reached.
//!
//! The checks correspond to the claims that cannot be recovered from if they are
//! false:
//!
//! - **Rebuild equivalence**: a projector rebuilt from position 0 matches the live
//!   one. Every "just replay it" recovery depends on this. Offline only: it needs the
//!   directory to itself, so [`sweep`] is the only thing that runs it.
//! - **Replay equivalence**: an invocation re-run from its journal makes the same
//!   calls and performs none of them again. This is the exactly-once promise, and the
//!   one check both entry points share: [`sweep`] runs it over a stopped directory and
//!   `serve --verify` runs it after each completed invocation.
//! - **Checkpoint monotonicity**: no component's position moves backwards. Enforced
//!   where a position is written rather than reported here, because the write is the
//!   only place that has both numbers: see `projector::advance_position`. Nothing in a
//!   sweep can observe it after the fact.
//!
//! Fold determinism used to be a fourth. It is gone with the Starlark port: heklang
//! removes its sources by construction (ordered maps, a clock a fold cannot reach, no
//! randomness, no read of anything but the log), and the check itself lived inside the
//! chunked fold that went with it.
//!
//! **A check must never be able to cause the fault it looks for.** That constraint
//! shapes the replay check in particular, and sealing it takes more than refusing to
//! send: heklang performs a journal miss for real, so the host it replays against
//! refuses to append and to erase as well.

use std::cmp::Ordering;
use std::fmt;

use crate::invariant::{Mismatch, Violation};
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;

use serde_json::Value;
use tephra::{Event, Position, Query, WriteHandle};

use heklang::Program;

use crate::crypto::{KeyStore, MasterKeys, RowDecryptor};
use crate::effect;
use crate::envelope;
use crate::loader::{EffectUnit, LoadedProject, ProjectorUnit};
use crate::projector;
use crate::read_model::ReadModel;
use crate::runtime::Runtime;
use crate::schema::{EntityDef, ModuleDef, scalar_to_string};

/// What a run of the checks found, and how much it covered.
///
/// The counts matter as much as the violations: a report with no violations and
/// nothing checked is a passing run that proved nothing, and it should not read the
/// same as a clean sweep.
#[derive(Debug, Default)]
pub struct Report {
    pub violations: Vec<Violation>,
    pub projectors_checked: usize,
    pub invocations_checked: usize,
    pub invocations_skipped: usize,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn absorb(&mut self, violations: impl IntoIterator<Item = Violation>) {
        self.violations.extend(violations);
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "checked {} projector(s) and {} invocation(s); skipped {}",
            self.projectors_checked, self.invocations_checked, self.invocations_skipped
        )?;
        if self.violations.is_empty() {
            write!(f, "ok: no violations")
        } else {
            for violation in &self.violations {
                writeln!(f, "violation: {violation}")?;
            }
            write!(f, "failed: {} violation(s)", self.violations.len())
        }
    }
}

/// Rebuild a projector from position 0 into a throwaway model and compare it, row
/// for row, against the live one.
///
/// The comparison is exact rather than approximate because two things hold: `rows`
/// orders by key, and a subject column re-encrypted from the same plaintext under the
/// same key is the same bytes, because AES-SIV derives its IV from the two rather than
/// drawing one. A nonce-carrying cipher here would make every encrypted column differ
/// between the live model and the rebuild, so that property is load-bearing rather
/// than incidental. What it does *not* survive is erasure, which leaves the live row
/// holding ciphertext and the rebuild writing NULL; [`drop_shredded`] is what reconciles
/// the two. Nothing here writes to the live model or the log.
///
/// **The rebuild is bounded at the live model's own checkpoint.** Building to head
/// instead would compare a shadow that has absorbed the whole log against a live
/// model that stopped wherever it stopped, so every event in the gap would surface as
/// a mismatch. A projector that was merely lagging when the server stopped is the
/// ordinary case, not a corrupt one.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_equivalence(
    store: &WriteHandle,
    unit: &ProjectorUnit,
    program: &Program,
    keystore: Option<&KeyStore>,
    live_db: &Path,
    scratch: &Path,
) -> anyhow::Result<Vec<Violation>> {
    let ModuleDef::Projector { name, entities, .. } = &unit.def else {
        anyhow::bail!("rebuild_equivalence called on a non-projector module");
    };

    let live = ReadModel::open_readonly(live_db)?;
    let upto = live.read_checkpoint()?;

    let rebuilt_path = scratch.join(format!("{name}.verify.db"));
    let rebuilt = ReadModel::open(&rebuilt_path, entities.as_slice())?;
    projector::project_to(store, unit, program, keystore, &rebuilt, Some(upto))?;

    let mut violations = Vec::new();
    for entity in entities {
        violations.extend(compare_entity(
            name.as_str(),
            entity,
            &live,
            &rebuilt,
            keystore,
        )?);
    }
    Ok(violations)
}

/// Compare one entity's rows between the live and rebuilt models.
///
/// A merge join rather than a nested scan: one pass, no lookups. [`keyed`] is what puts
/// both sides in the order this walks them in, and its doc says why reading `scan`'s own
/// ordering is not enough.
fn compare_entity(
    projector: &str,
    entity: &EntityDef,
    live: &ReadModel,
    rebuilt: &ReadModel,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Vec<Violation>> {
    let mut live_rows = keyed(entity, live.rows(entity)?);
    let mut rebuilt_rows = keyed(entity, rebuilt.rows(entity)?);
    // One decryptor for the whole entity: it caches the unwrapped secret per subject, so
    // an entity whose rows share a handful of subjects pays the opdb lock and the key
    // unwrap once each rather than twice per row.
    let decryptor = keystore.map(KeyStore::row_decryptor);
    for rows in [&mut live_rows, &mut rebuilt_rows] {
        for (_, row) in rows.iter_mut() {
            drop_shredded(entity, row, decryptor.as_ref());
        }
    }

    let mut violations = Vec::new();
    let mut mismatch = |key: &str, detail: Mismatch| {
        violations.push(Violation::RebuildMismatch {
            projector: projector.to_owned(),
            entity: entity.name.clone(),
            key: key.to_owned(),
            detail,
        });
    };

    let (mut l, mut r) = (0, 0);
    while l < live_rows.len() && r < rebuilt_rows.len() {
        let (live_key, live_row) = &live_rows[l];
        let (rebuilt_key, rebuilt_row) = &rebuilt_rows[r];
        match live_key.cmp(rebuilt_key) {
            Ordering::Equal => {
                if live_row != rebuilt_row {
                    mismatch(
                        live_key,
                        Mismatch::Differs {
                            live: live_row.to_string(),
                            rebuilt: rebuilt_row.to_string(),
                        },
                    );
                }
                l += 1;
                r += 1;
            }
            Ordering::Less => {
                mismatch(live_key, Mismatch::OnlyLive(live_row.to_string()));
                l += 1;
            }
            Ordering::Greater => {
                mismatch(rebuilt_key, Mismatch::OnlyRebuilt(rebuilt_row.to_string()));
                r += 1;
            }
        }
    }
    for (key, row) in &live_rows[l..] {
        mismatch(key, Mismatch::OnlyLive(row.to_string()));
    }
    for (key, row) in &rebuilt_rows[r..] {
        mismatch(key, Mismatch::OnlyRebuilt(row.to_string()));
    }
    Ok(violations)
}
/// Blank every sealed column that no longer reads back, on both sides.
///
/// The comparison is over stored bytes, which is sound only while the two sides *have*
/// the same bytes to hold. Erasure breaks that: the live row keeps the ciphertext it
/// was written with, because a shred rewrites nothing, while a rebuild re-encrypts from
/// the log and cannot read the payload, so it writes NULL rather than minting the key
/// the erasure destroyed. Neither is readable and the read API reports both as absent,
/// so they are equivalent everywhere it matters and differ only at rest.
///
/// Without this every erasure makes `verify` report that projector as corrupt forever,
/// which is the worst failure a tool whose value is being believed can have.
///
/// **The question is whether the bytes decrypt, not whether a key exists**, and the
/// difference is not pedantry: the append path mints a subject key on first use, so a
/// subject written to again after an erasure has a key again, under which the rows
/// written before the erasure still do not decrypt. Asking `KeyStore::erased` said "not
/// erased" and compared the stale ciphertext against a rebuild that had written NULL.
/// This is the same question [`read_api::decrypt_row`](crate::read_api) asks, which is
/// what makes "equivalent everywhere it matters" true rather than nearly true.
///
/// Tampering stays caught, and by a sharper edge than before: a column that will not
/// decrypt is dropped from the side holding it while the other side's readable one
/// stays, so the two rows differ. `a_tampered_subject_column_is_still_caught` pins it.
fn drop_shredded(entity: &EntityDef, row: &mut Value, decryptor: Option<&RowDecryptor<'_>>) {
    let Some(decryptor) = decryptor else { return };
    let Some(object) = row.as_object().cloned() else {
        return;
    };
    for (name, meta) in &entity.fields {
        let Some(subject_field) = &meta.subject else {
            continue;
        };
        let Some(id) = object
            .get(subject_field.as_str())
            .and_then(scalar_to_string)
        else {
            continue;
        };
        // A column that is absent or not text was never ciphertext, so there is nothing
        // to read back and nothing to drop.
        let Some(ciphertext) = object.get(name.as_str()).and_then(Value::as_str) else {
            continue;
        };
        let readable = decryptor
            .decrypt(subject_field, &id, name, ciphertext)
            .unwrap_or(None)
            .is_some();
        if !readable && let Some(target) = row.as_object_mut() {
            target.remove(name.as_str());
        }
    }
}

/// Pair each row with its key column, rendered as text so keys of any declared type
/// compare the same way, and sorted by that text.
///
/// **The sort is what makes the merge join sound.** `ReadModel::scan` orders in SQL, so
/// an `Int @key` comes back numerically while the join below compares with `String::cmp`.
/// The two disagree the moment keys differ in digit count (`10` sorts before `2` as
/// text), and a join walking two differently-ordered sequences reports keys present on
/// both sides as missing from one. Sorting here means the ordering the join assumes is
/// the ordering it was given, whatever the column's SQL type.
fn keyed(entity: &EntityDef, rows: Vec<Value>) -> Vec<(String, Value)> {
    let mut keyed: Vec<(String, Value)> = rows
        .into_iter()
        .map(|row| {
            let key = match row.get(&entity.key) {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                // A row with no key column is itself worth surfacing, and an empty
                // key makes it compare as a distinct row rather than panicking.
                None => String::new(),
            };
            (key, row)
        })
        .collect();
    keyed.sort_by(|(left, _), (right, _)| left.cmp(right));
    keyed
}

/// Run every offline check against a project and its data directory.
///
/// The runtime it opens holds the data-directory lock for the whole sweep, so this
/// refuses to run against a directory a server is using rather than racing it. The
/// intended shape is to copy the data directory and verify the copy, which
/// exercises the backup and the invariants in one pass.
pub fn sweep(
    project: &LoadedProject,
    data_dir: &Path,
    master: Option<MasterKeys>,
) -> anyhow::Result<Report> {
    // The lock comes with the runtime, which holds it for its lifetime. Taking it
    // here too would fail against ourselves: SQLite's exclusive lock is per
    // connection, so a second acquire in one process is refused exactly as a second
    // process would be.
    let (runtime, coordinator) = Runtime::open_quiescent(project, data_dir, master)?;
    // Every path shuts the writer down, including a failing check. Leaving the
    // coordinator running would hold the lock and strand its thread, which for a
    // library call (and for the tests below) is a leak rather than a tidy exit.
    let result = run_checks(&runtime, project, data_dir);
    coordinator.shutdown();
    result
}

fn run_checks(
    runtime: &Arc<Runtime>,
    project: &LoadedProject,
    data_dir: &Path,
) -> anyhow::Result<Report> {
    let scratch = tempfile::tempdir().context("creating a scratch directory for the rebuild")?;
    let mut report = Report::default();

    let projectors_dir = data_dir.join("projectors");
    for unit in &project.projectors {
        let name = unit.def.name();
        let live_db = projectors_dir.join(format!("{name}.db"));
        // A projector with no model on disk has never run. There is nothing to
        // disagree with, and building one just to compare it against itself would
        // report a clean check that tested nothing.
        if !live_db.exists() {
            continue;
        }
        report.projectors_checked += 1;
        report.absorb(rebuild_equivalence(
            runtime.store(),
            unit,
            runtime.program(),
            runtime.keystore(),
            &live_db,
            scratch.path(),
        )?);
    }

    for unit in &project.effects {
        sweep_effect(runtime, unit, &mut report)?;
    }
    Ok(report)
}

/// Replay every recorded invocation of one effect against a sealed host.
fn sweep_effect(
    runtime: &Arc<Runtime>,
    unit: &EffectUnit,
    report: &mut Report,
) -> anyhow::Result<()> {
    let name = unit.def.name();
    let ModuleDef::Effect { sources, .. } = &unit.def else {
        anyhow::bail!("sweep_effect called on a non-effect module");
    };
    // Sources filter on plaintext fields only, so lowering them needs no key store,
    // exactly as the live driver does it.
    let query =
        crate::heklang_host::query_of_types(sources).map_err(|err| anyhow::anyhow!("{err}"))?;

    for (position, script_hash) in runtime.terminal_invocations(name)? {
        // An edited effect diverges legitimately: the recorded run and the current
        // code are different programs, and the journal is keyed to the old one.
        if script_hash != unit.source_hash {
            report.invocations_skipped += 1;
            continue;
        }
        // The retention sweeper reclaims journals for completed invocations, so an
        // older one has nothing left to replay against. Absent is not divergent.
        if runtime.journal_keys(name, position)?.is_empty() {
            report.invocations_skipped += 1;
            continue;
        }
        let Some((event_position, event)) = read_at(runtime, &query, position)? else {
            report.invocations_skipped += 1;
            continue;
        };
        debug_assert_eq!(event_position, position);
        let (_env, _data) = envelope::decode(event.data())
            .with_context(|| format!("reading the event effect `{name}` ran at {position}"))?;
        report.invocations_checked += 1;
        report.absorb(effect::verify_replay(
            name,
            position,
            runtime.program(),
            runtime,
        ));
    }
    Ok(())
}

/// Read the single event at `position` that `query` matches, if it is still there.
fn read_at(
    runtime: &Arc<Runtime>,
    query: &Query,
    position: u64,
) -> anyhow::Result<Option<(u64, Event)>> {
    let mut reads = runtime
        .store()
        .read(query, Position::new(position.saturating_sub(1)), Some(1));
    match reads.next() {
        Some(item) => {
            let seq = item.map_err(|err| anyhow::anyhow!("reading event {position}: {err}"))?;
            if seq.position.get() != position {
                return Ok(None);
            }
            Ok(Some((seq.position.get(), seq.event.to_owned())))
        }
        None => Ok(None),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_report_reads_as_clean() {
        let report = Report {
            projectors_checked: 2,
            ..Report::default()
        };
        assert!(report.is_clean());
        assert!(report.to_string().contains("ok: no violations"));
    }

    #[test]
    fn a_report_with_violations_names_the_count() {
        let mut report = Report::default();
        report.absorb([Violation::CheckpointRegression {
            component: "projector `users`".to_owned(),
            from: 9,
            to: 4,
        }]);
        assert!(!report.is_clean());
        let text = report.to_string();
        assert!(text.contains("failed: 1 violation(s)"), "{text}");
        assert!(text.contains("backwards, from 9 to 4"), "{text}");
    }

    #[test]
    fn a_rebuild_mismatch_renders_both_sides() {
        let violation = Violation::RebuildMismatch {
            projector: "users".to_owned(),
            entity: "user".to_owned(),
            key: "alice".to_owned(),
            detail: Mismatch::Differs {
                live: r#"{"name":"old"}"#.to_owned(),
                rebuilt: r#"{"name":"new"}"#.to_owned(),
            },
        };
        let text = violation.to_string();
        assert!(text.contains("live {\"name\":\"old\"}"), "{text}");
        assert!(text.contains("rebuild gives {\"name\":\"new\"}"), "{text}");
    }
}
