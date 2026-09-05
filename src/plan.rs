//! What deploying this project would change, before it changes anything.
//!
//! `hekla plan` answers the question a deploy raises and nothing else could: this code
//! is not what is running, so what is different, and what would booting it do? It reads
//! and it changes nothing. The candidate side is the project's digest, exactly as
//! [`crate::loader`] computes it at boot. The recorded side is the `declaration` table,
//! which keeps the packed form of every version of every declaration, so the deployed
//! program reads back with no source tree in reach.
//!
//! Two properties make that half worth trusting. It opens no event log and takes no
//! data-directory lock, so it runs against a live deployment rather than only a copy;
//! and every hash it compares is heklang's digest, so a reformat is not a change and a
//! handler fix is.
//!
//! # What it would *do*
//!
//! A declaration diff says an effect changed. It cannot say whether the change matters,
//! and "would this now send a different HTTP request" is the question a deploy actually
//! turns on. `--replay` answers it: every recorded invocation of every affected effect is
//! re-run against the candidate code and the journal the original run left behind.
//!
//! Nothing is mocked, which is the point. The journal holds the responses the recorded
//! run really received, so a candidate that branches differently on a response body
//! reaches a call the journal has no entry for, and that miss *is* the finding. The
//! machinery is the sealed replay [`crate::verify`] already runs, with one difference:
//! the program that goes in has not been deployed yet. `verify` asks "did this reproduce
//! itself"; `plan` asks "would this still do what happened".
//!
//! This half opens the log, through a tephra [`Follower`](tephra::Follower): read-only
//! descriptors, nothing created, nothing deleted, and no lock, so it too runs against a
//! deployment that is serving traffic.
//!
//! # What replay cannot see, and says so
//!
//! Every limit here is counted or named rather than papered over, because a replay that
//! concluded nothing must not read like one that concluded everything:
//!
//! - **An erased subject.** A handler that branches on revealed plaintext cannot be
//!   re-run once its key is shredded, and journaling the plaintext to make it replayable
//!   would defeat the erasure. Reported as unreplayable, never as a divergence.
//! - **An invocation that journaled no call**, where the candidate reaches one. An
//!   operator skip and a run that took a callless branch leave the same row, so there is
//!   nothing to compare against. (When the candidate also calls nothing the two agree,
//!   and that *is* a check.)
//! - **An effect that reveals, with no usable master key.** Counted per effect, so one
//!   sealed field somewhere does not blind an effect that never touches a key.
//! - **Retention.** A reclaimed invocation loses its row and its journal together, so it
//!   is invisible here rather than skipped. Nothing can count what is gone, so the
//!   window itself is reported and the reader judges the coverage against it. One that is
//!   reclaimed *while this runs* is counted, because a server sweeping under a replay is
//!   the case `--replay` exists for.
//! - **[`DEFAULT_REPLAY_LIMIT`]**, per effect, and any effect the cap bit is named.
//!
//! One more thing it deliberately does not look at: invocations recorded by a version of
//! the effect that is *already* superseded. The baseline is what is running, and nothing
//! else. See [`OpDb::recent_terminal_invocations`].
//!
//! And one thing it reports that [`crate::verify`] cannot: a candidate that would end in
//! a terminal `fail`. The row a `fail` leaves is the row a success leaves, so the record
//! cannot say whether the deployed code gave up here too, which makes it news to a
//! candidate and nothing at all to the program that wrote it. See [`effect::Asked`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use heklang::digest::Sexp;
use heklang::ir::{Command, Literal, MessagePart, RefusalDef};
use heklang::{Entry, Kind, Program};

use crate::crypto::MasterKeys;
use crate::effect::{self, Asked, Replayed, Uncovered};
use crate::loader::{self, EffectUnit, LoadedProject};
use crate::opdb::{self, DeclarationRow, OpDb, SCHEMA_VERSION};
use crate::projector::{Reconcile, reconcile_from};
use crate::read_model::ReadModel;
use crate::runtime::Runtime;

/// How one declaration differs from what is deployed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not deployed at all.
    Added,
    /// Deployed, and gone from the candidate.
    Removed,
    /// It does something different behind a contract that did not move, so nothing
    /// outside the program can tell.
    Behaviour,
    /// What is visible from outside changed: a caller could notice.
    Contract,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Added => "added",
            Verdict::Removed => "removed",
            Verdict::Behaviour => "behaviour",
            Verdict::Contract => "contract",
        }
    }
}

/// One declaration that would not survive the deploy unchanged.
#[derive(Debug, Clone)]
pub struct Change {
    pub kind: Kind,
    pub name: String,
    pub verdict: Verdict,
    pub module: Option<String>,
    /// The deployed form, expanded. `None` for an addition.
    pub before: Option<String>,
    /// The candidate form, expanded. `None` for a removal.
    pub after: Option<String>,
}

/// What booting this project would do to one projector's read model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebuild {
    /// Its definition is unchanged: it resumes from its checkpoint.
    Resume,
    /// Nothing is recorded for it here, so it builds from the start of the log.
    Fresh,
    /// Its definition changed and `auto_rebuild` is on: the model is rebuilt from zero.
    Rebuild,
    /// The same change with `auto_rebuild` off, so it serves rows the old logic built
    /// until someone replays it by hand.
    Stale,
}

impl Rebuild {
    fn of(plan: Reconcile) -> Rebuild {
        match plan {
            Reconcile::UpToDate => Rebuild::Resume,
            Reconcile::Stamp => Rebuild::Fresh,
            Reconcile::Rebuild => Rebuild::Rebuild,
            Reconcile::Stale => Rebuild::Stale,
        }
    }
}

/// What one projector would do on the next boot, and from where.
#[derive(Debug, Clone)]
pub struct Forecast {
    pub name: String,
    pub outcome: Rebuild,
    /// The position its model has reached, which is what a rebuild would redo.
    pub checkpoint: u64,
}

/// Why a group of declarations changed together.
///
/// `const`, `refusal` and `guard` are spliced into what names them before a program
/// exists, so none has a digest entry of its own and editing one moves every hash that
/// reaches it. Without this, a one-line edit to a shared guard reads as an unexplained
/// wall of diffs.
#[derive(Debug, Clone)]
pub enum Cause {
    /// Every command in `commands` names this guard, and no command outside the group
    /// does. The second half is what makes it evidence rather than coincidence.
    Guard { name: String, commands: Vec<String> },
    /// The same edit appears in several declarations, so one inlined declaration most
    /// likely produced all of them. `likely` names it when exactly one candidate fits,
    /// and is `None` rather than a guess when several do.
    SharedEdit {
        removed: Vec<String>,
        added: Vec<String>,
        declarations: Vec<String>,
        likely: Option<String>,
    },
}

/// One recorded invocation the candidate code would not reproduce.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub effect: String,
    /// The log position of the event that triggered the recorded run.
    pub position: u64,
    pub outcome: Replayed,
}

/// How much of the recorded history the replay actually spoke for.
///
/// Every field here exists so a reader can tell "nothing would change" apart from
/// "nothing was looked at". A replay that covered none of the history and found no
/// divergence is not reassurance, and without these counts it reads exactly like one
/// that covered all of it.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    /// Effects whose behaviour the candidate could have moved: their own digest changed,
    /// or the digest of something they name did. See `affected_effects`.
    pub effects_affected: usize,
    /// Invocations the replay reached a conclusion about, divergent or not.
    pub replayed: usize,
    /// Of those, the ones the candidate reproduces exactly.
    pub reproduced: usize,
    /// Invocations whose subject key has been erased, so the plaintext the handler
    /// branches on is gone and nothing can be concluded. Not fixable: journaling it
    /// would defeat the erasure.
    pub subject_erased: usize,
    /// Invocations that journaled no call, where the candidate reaches one, recorded
    /// before the row could say whether an operator skipped it. Only pre-v7 rows land
    /// here; since then the row answers and `operator_skipped` counts it.
    pub no_journal: usize,
    /// Invocations an operator skipped. Nothing ran them to an end, so their journal is
    /// the prefix of a run that stopped where it wedged and there is no complete record
    /// to compare a candidate against.
    pub operator_skipped: usize,
    /// Invocations whose record could not be read. A busy op-DB is ordinary against a
    /// live directory, and a failed read is not evidence about the candidate.
    pub unreadable: usize,
    /// Effects whose recorded history could not be listed at all, so none of it was
    /// replayed. Named rather than counted, because the count is what could not be read.
    pub unreadable_history: Vec<String>,
    /// Why the replay could not be started at all, when it could not.
    ///
    /// Opening the data directory to follow it can fail on its own (a locked op-DB, a
    /// transient i/o error on the segment set), and that failure says nothing about the
    /// declaration diff and the projector forecast, which are already computed and still
    /// true. Reported as coverage of nothing rather than as an exit code, so `--replay`
    /// against a busy directory degrades to what a plan without it would have said.
    pub unavailable: Option<String>,
    /// Invocations the deployment's own retention sweeper reclaimed mid-run. `--replay`
    /// is built to run against a directory a server is still sweeping, so the row can go
    /// between being listed and being read.
    pub reclaimed: usize,
    /// Invocations of an effect that reveals, where no usable master key is configured.
    /// Counted per effect from its form, so an effect that never reveals is replayed
    /// even in a project that seals fields elsewhere.
    pub no_master_key: usize,
    /// Why the master key that *was* configured could not be used, when one was.
    ///
    /// A half-configured rotation (the current master set, the previous one forgotten)
    /// must not be worse than no key at all: the declaration diff and the projector
    /// forecast are already computed and still true, so the plan reports them and says
    /// what it could not do. `None` covers both a key that works and no key at all,
    /// which `no_master_key` tells apart.
    pub unusable_master_key: Option<String>,
    /// Effects whose history was longer than `limit`, so only the most recent `limit`
    /// invocations were replayed. Named rather than counted: a cap a reader cannot see
    /// reads as full coverage.
    pub truncated: Vec<String>,
    /// The per-effect cap that produced `truncated`.
    pub limit: usize,
    /// The candidate project's `retention.effect_journal_days`.
    ///
    /// An upper bound on the replay horizon, and only that. Retention reclaims an
    /// invocation's row and its journal together, so what it took is invisible here
    /// rather than counted, and nothing can report what is gone. It is also the
    /// *candidate's* window: the deployed configuration is what actually governed the
    /// sweeping, and hekla does not record it. Printed so a reader can judge the counts
    /// above against something rather than against nothing.
    pub horizon_days: u32,
}

impl Coverage {
    /// Whether the replay concluded anything at all.
    pub fn is_blind(&self) -> bool {
        self.replayed == 0
    }
}

/// What deploying a project over a data directory would change.
///
/// `declarations_compared` is part of the result, not decoration: a plan with no changes
/// and nothing compared is a directory that has never been deployed to, and it must not
/// read the same as a project that genuinely matches what is running.
#[derive(Debug, Default)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub projectors: Vec<Forecast>,
    pub causes: Vec<Cause>,
    pub declarations_compared: usize,
    /// Every recorded form failed to reproduce its own hash, so the deployment was
    /// written under a different `heklang::digest::VERSION` and no comparison here
    /// means anything.
    pub digest_version_mismatch: bool,
    /// Recorded invocations the candidate code would not reproduce. Empty unless replay
    /// was asked for, and empty when it was and found nothing.
    pub divergences: Vec<Divergence>,
    /// How much history the replay spoke for. `None` when replay was not asked for,
    /// which is not the same as a replay that covered nothing.
    pub coverage: Option<Coverage>,
}

impl Plan {
    /// Whether deploying would change nothing at all.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
            && self.divergences.is_empty()
            && self
                .projectors
                .iter()
                .all(|forecast| forecast.outcome == Rebuild::Resume)
    }

    fn rebuilding(&self) -> usize {
        self.projectors
            .iter()
            .filter(|forecast| matches!(forecast.outcome, Rebuild::Rebuild | Rebuild::Fresh))
            .count()
    }

    /// The plan as JSON, for a caller gating a deploy on it.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "declarations_compared": self.declarations_compared,
            "digest_version_mismatch": self.digest_version_mismatch,
            "changes": self.changes.iter().map(|change| serde_json::json!({
                "kind": change.kind.name(),
                "name": change.name,
                "verdict": change.verdict.label(),
                "module": change.module,
                "before": change.before,
                "after": change.after,
            })).collect::<Vec<_>>(),
            "projectors": self.projectors.iter().map(|forecast| serde_json::json!({
                "name": forecast.name,
                "outcome": match forecast.outcome {
                    Rebuild::Resume => "resume",
                    Rebuild::Fresh => "fresh",
                    Rebuild::Rebuild => "rebuild",
                    Rebuild::Stale => "stale",
                },
                "checkpoint": forecast.checkpoint,
            })).collect::<Vec<_>>(),
            "causes": self.causes.iter().map(|cause| match cause {
                Cause::Guard { name, commands } => serde_json::json!({
                    "cause": "guard", "guard": name, "declarations": commands,
                }),
                Cause::SharedEdit { removed, added, declarations, likely } => serde_json::json!({
                    "cause": "shared_edit",
                    "removed": removed,
                    "added": added,
                    "declarations": declarations,
                    "likely": likely,
                }),
            }).collect::<Vec<_>>(),
            // `null` when no replay ran, for the reason `Display` prints no divergence
            // clause there: an empty list is a clean replay result, and a gate reading one
            // off a run that never opened the log would pass on the strength of a check
            // nobody made. A gate wanting "no divergences" has to see a list.
            "divergences": self.coverage.as_ref().map(|_| {
                self.divergences.iter().map(|divergence| serde_json::json!({
                    "effect": divergence.effect,
                    "position": divergence.position,
                    "outcome": divergence.outcome.label(),
                    "detail": divergence.outcome.to_string(),
                })).collect::<Vec<_>>()
            }),
            "coverage": self.coverage.as_ref().map(|coverage| serde_json::json!({
                "effects_affected": coverage.effects_affected,
                "replayed": coverage.replayed,
                "reproduced": coverage.reproduced,
                "subject_erased": coverage.subject_erased,
                "no_journal": coverage.no_journal,
                "operator_skipped": coverage.operator_skipped,
                "unreadable": coverage.unreadable,
                "unreadable_history": coverage.unreadable_history,
                "reclaimed": coverage.reclaimed,
                "no_master_key": coverage.no_master_key,
                "unusable_master_key": coverage.unusable_master_key,
                "unavailable": coverage.unavailable,
                "truncated": coverage.truncated,
                "limit": coverage.limit,
                "horizon_days": coverage.horizon_days,
                // So a gate can tell "nothing would change" from "nothing was looked
                // at" without re-deriving it from six counters.
                "blind": coverage.is_blind(),
            })),
        })
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "compared {} declaration(s) against what is deployed",
            self.declarations_compared
        )?;

        // Every hash differs for one reason, so listing them all would bury it.
        if self.digest_version_mismatch {
            return write!(
                f,
                "failed: the deployment was recorded under a different digest version, \
                 so nothing here is comparable; deploy once with this build to re-record it"
            );
        }

        for change in &self.changes {
            writeln!(
                f,
                "  {:<9} {} {} ({})",
                change.verdict.label(),
                change.kind.name(),
                change.name,
                change.module.as_deref().unwrap_or("no module"),
            )?;
        }

        for forecast in &self.projectors {
            match forecast.outcome {
                Rebuild::Resume => {}
                Rebuild::Fresh => writeln!(
                    f,
                    "  projector {} builds from the start of the log",
                    forecast.name
                )?,
                Rebuild::Rebuild => writeln!(
                    f,
                    "  projector {} rebuilds from zero, redoing {} position(s)",
                    forecast.name, forecast.checkpoint
                )?,
                Rebuild::Stale => writeln!(
                    f,
                    "  projector {} goes stale: auto_rebuild is off, so it serves rows \
                     the old logic built until it is replayed",
                    forecast.name
                )?,
            }
        }

        for cause in &self.causes {
            match cause {
                Cause::Guard { name, commands } => writeln!(
                    f,
                    "  because `guard {}` changed: {}",
                    name,
                    commands.join(", ")
                )?,
                Cause::SharedEdit {
                    removed,
                    added,
                    declarations,
                    likely,
                } => {
                    match likely {
                        Some(likely) => writeln!(
                            f,
                            "  the same edit in {} declaration(s), likely `{}`: {}",
                            declarations.len(),
                            likely,
                            declarations.join(", ")
                        )?,
                        None => writeln!(
                            f,
                            "  the same edit in {} declaration(s): {}",
                            declarations.len(),
                            declarations.join(", ")
                        )?,
                    }
                    write_lines(f, '-', removed)?;
                    write_lines(f, '+', added)?;
                }
            }
        }

        // Capped like every other list here. A candidate that diverges on all of a
        // 1000-invocation history, across several effects, would otherwise push the
        // coverage summary and the change counts thousands of lines off the top of the
        // terminal. `--json` carries the whole set.
        for divergence in self.divergences.iter().take(DIVERGENCE_LINES) {
            writeln!(
                f,
                "  effect {} @ {}: {}",
                divergence.effect, divergence.position, divergence.outcome
            )?;
        }
        if self.divergences.len() > DIVERGENCE_LINES {
            writeln!(
                f,
                "  ... and {} more divergence(s)",
                self.divergences.len() - DIVERGENCE_LINES
            )?;
        }

        // Printed whether or not anything diverged, and before the summary, because a
        // replay that concluded nothing is the one result that must not be mistaken for
        // a clean one.
        if let Some(coverage) = &self.coverage {
            write_coverage(f, coverage)?;
        }

        if self.is_empty() {
            return write!(f, "ok: nothing would change");
        }
        let added = self.count(Verdict::Added);
        let removed = self.count(Verdict::Removed);
        let changed = self.count(Verdict::Behaviour) + self.count(Verdict::Contract);
        // Only when a replay ran. Without `--replay` nothing opened the log, and "0
        // recorded invocation(s) would diverge" would report a clean result for a check
        // that was never made, which is the one thing this command must not do.
        let diverged = match self.coverage {
            Some(_) => format!(
                ", {} recorded invocation(s) would diverge",
                self.divergences.len()
            ),
            None => String::new(),
        };
        write!(
            f,
            "{added} added, {removed} removed, {changed} changed; \
             {} projector(s) would rebuild{diverged}",
            self.rebuilding()
        )
    }
}

/// The replay's own coverage, in the shape of `verify::Report`'s counts: what it looked
/// at, then everything it did not.
fn write_coverage(f: &mut fmt::Formatter<'_>, coverage: &Coverage) -> fmt::Result {
    writeln!(
        f,
        "replayed {} invocation(s) across {} affected effect(s); {} reproduce, {} diverge",
        coverage.replayed,
        coverage.effects_affected,
        coverage.reproduced,
        coverage.replayed - coverage.reproduced,
    )?;
    // First, and in its own words. A replay that concluded nothing prints the same zeroes
    // as one that examined a clean history, and the difference is the whole reason these
    // counts exist. The reasons below say why; this says that.
    if coverage.is_blind() && coverage.effects_affected > 0 {
        writeln!(
            f,
            "this replay concluded nothing: no recorded invocation was compared"
        )?;
    }
    if let Some(err) = &coverage.unavailable {
        writeln!(
            f,
            "the replay could not be started, so none of the history was read: {err}"
        )?;
    }
    // Whether or not anything needed it. A key that cannot unwrap what is stored will
    // fail the next `serve` or `verify` outright, and reporting it only when some effect
    // happened to reveal withheld a fact this run had already established.
    if let Some(err) = &coverage.unusable_master_key {
        writeln!(
            f,
            "the configured master key cannot unwrap what is stored: {err}"
        )?;
    }
    if coverage.no_master_key > 0 {
        match &coverage.unusable_master_key {
            Some(_) => writeln!(
                f,
                "not replayable: {} invocation(s) of an effect that reveals, because the \
                 master key cannot unwrap what is stored",
                coverage.no_master_key
            )?,
            None => writeln!(
                f,
                "not replayable: {} invocation(s) of an effect that reveals, because \
                 HEKLA_MASTER_KEY is not set",
                coverage.no_master_key
            )?,
        }
    }
    if coverage.subject_erased > 0 {
        writeln!(
            f,
            "not replayable: {} invocation(s) whose subject has been erased",
            coverage.subject_erased
        )?;
    }
    if coverage.no_journal > 0 {
        writeln!(
            f,
            "not replayable: {} invocation(s) recorded before a skipped run could be told \
             from one that called nothing, that journaled no call",
            coverage.no_journal
        )?;
    }
    if coverage.operator_skipped > 0 {
        writeln!(
            f,
            "not replayable: {} invocation(s) an operator skipped, which nothing ran to \
             an end",
            coverage.operator_skipped
        )?;
    }
    if coverage.unreadable > 0 {
        writeln!(
            f,
            "not replayable: {} invocation(s) whose record could not be read",
            coverage.unreadable
        )?;
    }
    if !coverage.unreadable_history.is_empty() {
        writeln!(
            f,
            "history could not be read at all for: {}",
            coverage.unreadable_history.join(", ")
        )?;
    }
    if coverage.reclaimed > 0 {
        writeln!(
            f,
            "not replayable: {} invocation(s) whose record retention reclaimed while this \
             was reading it",
            coverage.reclaimed
        )?;
    }
    if !coverage.truncated.is_empty() {
        writeln!(
            f,
            "capped at the {} most recent invocation(s) of: {}",
            coverage.limit,
            coverage.truncated.join(", ")
        )?;
    }
    // Named rather than counted, because what retention reclaimed is gone: the row and
    // its journal go together, so a swept invocation is invisible here and cannot be
    // added to a total. It is the candidate's window, not necessarily the one the
    // deployment swept under, so it bounds the horizon rather than measuring it.
    if coverage.effects_affected > 0 {
        writeln!(
            f,
            "this project retains {} day(s) of journals; anything older was reclaimed \
             before the replay could see it",
            coverage.horizon_days
        )?;
    }
    Ok(())
}

/// How many diverging invocations the report names before summarising the rest. A
/// candidate that moved one effect moves it for every recorded invocation of it, so the
/// list length measures recorded history rather than how much went wrong.
const DIVERGENCE_LINES: usize = 20;

/// How many lines of one side of an edit the report prints before summarising the rest.
/// A wider form reflows when a literal grows, so an edit of one atom can surface as
/// several lines and a large one as many. `--json` carries them all.
const EDIT_LINES: usize = 3;

fn write_lines(f: &mut fmt::Formatter<'_>, sign: char, lines: &[String]) -> fmt::Result {
    for line in lines.iter().take(EDIT_LINES) {
        writeln!(f, "      {sign} {}", line.trim())?;
    }
    if lines.len() > EDIT_LINES {
        writeln!(
            f,
            "      {sign} ... and {} more line(s)",
            lines.len() - EDIT_LINES
        )?;
    }
    Ok(())
}

impl Plan {
    fn count(&self, verdict: Verdict) -> usize {
        self.changes
            .iter()
            .filter(|change| change.verdict == verdict)
            .count()
    }
}

/// Whether to re-run recorded effect invocations, and with what.
pub enum Replay {
    /// Declaration diff and rebuild forecast only. No event log is opened, no key is
    /// read, and [`Plan::coverage`] stays `None`.
    Off,
    On {
        /// The master key, when one is configured. `None` is not fatal: an effect that
        /// reveals is counted as unreplayable instead, so a plan against production does
        /// not need the production key to produce a diff.
        master: Option<MasterKeys>,
        /// The most recent invocations of each effect to replay. See
        /// [`DEFAULT_REPLAY_LIMIT`].
        limit: usize,
    },
}

/// Compare a candidate project against a data directory, optionally re-running the
/// effect invocations it has on record.
///
/// Changes nothing. The operational database is opened only after its schema version is
/// read separately and found to match, because [`OpDb::open`] migrates and silently
/// upgrading a live deployment is not a reader's business. Read models are opened
/// read-only for the same reason: [`ReadModel::open`] would create tables for a
/// projector that has never run. With replay on, the log is opened through a tephra
/// [`Follower`](tephra::Follower), which takes no lock and creates nothing, so this
/// still runs against a deployment that is serving traffic.
///
/// The one mark it can leave is SQLite's: a read-only connection to a WAL database maps
/// a shared-memory index and cannot remove it on close, so an empty `-wal` and `-shm`
/// pair may outlive the call. No database's contents change.
pub fn compute_with(
    project: &LoadedProject,
    data_dir: &Path,
    replay: Replay,
) -> anyhow::Result<Plan> {
    let db_path = data_dir.join("hekla.db");
    if !db_path.exists() {
        anyhow::bail!(
            "no operational database at {}; nothing is deployed there to compare against",
            db_path.display()
        );
    }
    let recorded_version = opdb::recorded_schema_version(&db_path)?;
    if recorded_version != SCHEMA_VERSION {
        anyhow::bail!(
            "the data directory is at schema version {recorded_version} and this build \
             expects {SCHEMA_VERSION}; run `hekla serve` against it once to migrate, or \
             plan against a directory this build wrote"
        );
    }

    let mut plan = Plan::default();
    // The hash each effect is *running* under, which is the only baseline a replay may
    // use: see `OpDb::recent_terminal_invocations`.
    let deployed_effects: BTreeMap<String, String>;
    // Scoped, so the connection closes before a replay opens its own. Two connections to
    // one SQLite file work, but a plan against a live deployment has no reason to hold
    // a second one open for the whole run.
    {
        let db = OpDb::open(&db_path)?;
        let recorded = db.current_declarations()?;
        deployed_effects = recorded
            .iter()
            .filter(|row| Kind::lookup(&row.kind) == Some(Kind::Effect))
            .map(|row| (row.name.clone(), row.hash.clone()))
            .collect();
        plan.changes = diff(project, &recorded, &mut plan)?;
    }
    plan.projectors = forecast(project, data_dir)?;
    if !plan.digest_version_mismatch {
        plan.causes = attribute(&project.program, &plan.changes);
    }

    if let Replay::On { master, limit } = replay {
        if plan.digest_version_mismatch {
            // Nothing here is comparable, so there is no honest way to say which effects
            // the candidate moved, and replaying a set chosen at random would produce
            // findings about nothing. Reported as coverage of zero rather than as no
            // coverage at all: `None` means the caller did not ask, and a gate keying on
            // that must not read a refusal as a question it never posed.
            plan.coverage = Some(Coverage {
                horizon_days: project.config.retention.effect_journal_days,
                limit,
                ..Coverage::default()
            });
        } else {
            // Degraded, not propagated. Everything above this point is computed and
            // correct, and handing back an exit code because the replay could not start
            // would lose a diff the operator asked for, over a failure in the half they
            // asked for as well as it. The coverage says plainly that nothing was
            // looked at.
            let replayed = replay_effects(
                project,
                data_dir,
                &plan.changes,
                &deployed_effects,
                master,
                limit,
            );
            match replayed {
                Ok((divergences, coverage)) => {
                    plan.divergences = divergences;
                    plan.coverage = Some(coverage);
                }
                Err(err) => {
                    plan.coverage = Some(Coverage {
                        horizon_days: project.config.retention.effect_journal_days,
                        limit,
                        unavailable: Some(format!("{err:#}")),
                        ..Coverage::default()
                    });
                }
            }
        }
    }
    Ok(plan)
}

/// Merge the candidate digest against the recorded rows on `(kind, name)`.
///
/// Not a merge join over the two sorted sequences, despite both being sorted: SQLite
/// orders the `kind` column as text and `Kind` orders by declaration kind, so the two
/// sequences interleave differently.
fn diff(
    project: &LoadedProject,
    recorded: &[DeclarationRow],
    plan: &mut Plan,
) -> anyhow::Result<Vec<Change>> {
    let modules = loader::module_paths(&project.program);

    let mut deployed: BTreeMap<(Kind, &str), (Entry, &DeclarationRow)> = BTreeMap::new();
    let mut unreproduced = 0usize;
    for row in recorded {
        let kind = Kind::lookup(&row.kind).ok_or_else(|| {
            anyhow::anyhow!(
                "the declaration table holds kind `{}`, which this build does not know",
                row.kind
            )
        })?;
        // Rule 13: a form that was truncated or half-migrated fails loudly rather than
        // decoding into a plausible wrong answer, because what reads it next is deciding
        // whether a deployment is safe.
        let entry = Entry::from_packed(&row.form, row.signature.as_deref()).map_err(|err| {
            anyhow::anyhow!(
                "the recorded form of {} `{}` does not read back: {err}",
                row.kind,
                row.name
            )
        })?;
        // The version line is hashed into every entry, so a form that no longer
        // reproduces its own hash was written under a different digest version.
        if entry.hash.to_string() != row.hash {
            unreproduced += 1;
        }
        deployed.insert((kind, row.name.as_str()), (entry, row));
    }
    plan.declarations_compared = deployed.len();
    plan.digest_version_mismatch = !deployed.is_empty() && unreproduced == deployed.len();

    let mut changes = Vec::new();
    let mut seen: BTreeSet<(Kind, &str)> = BTreeSet::new();

    for entry in project.digest.entries() {
        let key = (entry.kind, entry.name.as_str());
        seen.insert(key);
        let module = modules.get(&(entry.kind, entry.name.clone())).cloned();
        let Some((was, _row)) = deployed.get(&key) else {
            changes.push(Change {
                kind: entry.kind,
                name: entry.name.clone(),
                verdict: Verdict::Added,
                module,
                before: None,
                after: Some(entry.form.expanded()),
            });
            continue;
        };
        if was.hash == entry.hash {
            continue;
        }
        let verdict = if was.signature_hash == entry.signature_hash {
            Verdict::Behaviour
        } else {
            Verdict::Contract
        };
        changes.push(Change {
            kind: entry.kind,
            name: entry.name.clone(),
            verdict,
            module,
            before: Some(was.form.expanded()),
            after: Some(entry.form.expanded()),
        });
    }

    for ((kind, name), (was, row)) in &deployed {
        if seen.contains(&(*kind, *name)) {
            continue;
        }
        changes.push(Change {
            kind: *kind,
            name: (*name).to_owned(),
            verdict: Verdict::Removed,
            module: row.module.clone(),
            before: Some(was.form.expanded()),
            after: None,
        });
    }

    changes.sort_by(|left, right| (left.kind, &left.name).cmp(&(right.kind, &right.name)));
    Ok(changes)
}

/// What each projector would do on the next boot.
fn forecast(project: &LoadedProject, data_dir: &Path) -> anyhow::Result<Vec<Forecast>> {
    let projectors_dir = data_dir.join("projectors");
    let auto_rebuild = project.config.projectors.auto_rebuild;
    let mut forecasts = Vec::new();
    for unit in &project.projectors {
        let name = unit.def.name();
        let path = projectors_dir.join(format!("{name}.db"));
        // A model that is not there has never run, which `reconcile_from` reads as a
        // fresh one. Opening it to find that out would create it.
        let (previous, checkpoint) = if path.exists() {
            let model = ReadModel::open_readonly(&path)?;
            (model.read_definition()?, model.read_checkpoint()?.get())
        } else {
            (None, 0)
        };
        let outcome = reconcile_from(
            previous.as_deref(),
            checkpoint,
            &unit.digest_hash,
            auto_rebuild,
        );
        forecasts.push(Forecast {
            name: name.to_owned(),
            outcome: Rebuild::of(outcome),
            checkpoint,
        });
    }
    Ok(forecasts)
}

// --- attribution -----------------------------------------------------------

/// Group the changes that share one cause.
///
/// The unit of evidence is the *edit*: two declarations that changed by the same
/// removed and added lines almost certainly changed because one thing they both name
/// changed. Grouping first and identifying second is what keeps a guard from being
/// blamed merely because the one command that changed happens to call it.
fn attribute(program: &Program, changes: &[Change]) -> Vec<Cause> {
    let mut groups: BTreeMap<(Vec<String>, Vec<String>), Vec<&Change>> = BTreeMap::new();
    for change in changes {
        let (Some(before), Some(after)) = (&change.before, &change.after) else {
            continue;
        };
        let edit = line_edit(before, after);
        if edit.0.is_empty() && edit.1.is_empty() {
            continue;
        }
        groups.entry(edit).or_default().push(change);
    }

    let mut causes = Vec::new();
    for ((removed, added), members) in groups {
        // One declaration changing is not a fan-out; it is just that declaration.
        if members.len() < 2 {
            continue;
        }
        let names: Vec<String> = members.iter().map(|change| change.name.clone()).collect();
        // The most specific cause wins. A refusal is spliced into the guard that
        // rejects with it, which is spliced into the commands, so a reworded message
        // satisfies the guard test too; naming the guard there would send a reader to
        // the fold when what moved was the string beside it.
        if let Some(likely) = likely_inline(program, &removed, &added) {
            causes.push(Cause::SharedEdit {
                removed,
                added,
                declarations: names,
                likely: Some(likely),
            });
            continue;
        }
        if let Some(guard) = shared_guard(program, &members) {
            causes.push(Cause::Guard {
                name: guard,
                commands: names,
            });
            continue;
        }
        causes.push(Cause::SharedEdit {
            removed,
            added,
            declarations: names,
            likely: None,
        });
    }
    causes
}

/// The lines one form gained and lost relative to another.
///
/// A multiset difference rather than a real diff, which is enough because
/// `Sexp::expanded` puts one child per line: an edit deep inside a form leaves every
/// other line exactly where it was.
fn line_edit(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let mut old: Vec<&str> = before.lines().collect();
    let mut removed = Vec::new();
    for line in after.lines() {
        match old.iter().position(|held| *held == line) {
            Some(at) => {
                old.remove(at);
            }
            None => removed.push(line),
        }
    }
    let mut added: Vec<String> = removed.into_iter().map(str::to_owned).collect();
    let mut gone: Vec<String> = old.into_iter().map(str::to_owned).collect();
    gone.sort();
    added.sort();
    (gone, added)
}

/// The guard every changed command in the group names, when exactly one fits.
///
/// The group must be the guard's whole caller set. A guard that some unchanged command
/// also names cannot be the reason this group changed, because that command would have
/// changed too.
fn shared_guard(program: &Program, members: &[&Change]) -> Option<String> {
    let group: BTreeSet<&str> = members
        .iter()
        .filter(|change| change.kind == Kind::Command)
        .map(|change| change.name.as_str())
        .collect();
    if group.len() != members.len() {
        return None;
    }
    let mut found = None;
    for guard in &program.guards {
        let callers: BTreeSet<&str> = program
            .commands
            .iter()
            .filter(|command| reaches(program, command, &guard.name))
            .map(|command| command.name.as_str())
            .collect();
        if callers.is_empty() || callers != group {
            continue;
        }
        if found.is_some() {
            // Two guards with identical caller sets: nothing here distinguishes them.
            return None;
        }
        found = Some(guard.name.clone());
    }
    found
}

/// Whether `command` names `guard`, directly or through another guard.
fn reaches(program: &Program, command: &Command, guard: &str) -> bool {
    let mut stack: Vec<&str> = command
        .calls
        .iter()
        .map(|call| call.guard.as_str())
        .collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    while let Some(name) = stack.pop() {
        if name == guard {
            return true;
        }
        if !seen.insert(name) {
            continue;
        }
        if let Some(next) = program.guard(name) {
            stack.extend(next.calls.iter().map(|call| call.guard.as_str()));
        }
    }
    false
}

/// The inlined declaration a shared edit most likely came from.
///
/// A hint, and only ever a hint: a `const` is spliced in as its value with nothing
/// naming it at the use site, so this matches on what the edit touched. When more than
/// one candidate fits, it names none rather than picking.
fn likely_inline(program: &Program, removed: &[String], added: &[String]) -> Option<String> {
    // A refusal first: its code sits beside its message in the form, so an edited
    // message is an edited line that still carries the code on both sides.
    let mut refusals = program.refusals.iter().filter(|def| {
        let needle = format!("(str \"{}\")", def.code);
        let touched = |lines: &[String]| lines.iter().any(|line| line.contains(&needle));
        touched(removed) && touched(added) && message_changed(def, removed, added)
    });
    if let Some(def) = refusals.next() {
        if refusals.next().is_none() {
            return Some(format!("refusal {}", def.name));
        }
        return None;
    }

    let mut consts = program.consts.iter().filter(|def| {
        let Some(text) = literal_text(&def.value) else {
            return false;
        };
        added.iter().any(|line| line.contains(&text))
            && !removed.iter().any(|line| line.contains(&text))
    });
    let def = consts.next()?;
    if consts.next().is_some() {
        return None;
    }
    Some(format!("const {}", def.name))
}

/// Whether the refusal's own message text appears on the added side, which separates a
/// reworded refusal from a `reject` that merely moved.
fn message_changed(def: &RefusalDef, removed: &[String], added: &[String]) -> bool {
    def.message.iter().any(|part| match part {
        MessagePart::Text(text) if !text.trim().is_empty() => {
            added.iter().any(|line| line.contains(text.as_str()))
                && !removed.iter().any(|line| line.contains(text.as_str()))
        }
        _ => false,
    })
}

/// The text a literal contributes to a packed form, for the variants distinctive enough
/// to match on. `None` for the rest, which costs the hint and nothing else.
fn literal_text(literal: &Literal) -> Option<String> {
    Some(match literal {
        Literal::Bool(value) => value.to_string(),
        Literal::Int(value) | Literal::Timestamp(value) => value.to_string(),
        Literal::Decimal { units, .. } | Literal::Money { units, .. } => units.to_string(),
        Literal::Str(text) | Literal::Uuid(text) => text.to_string(),
        Literal::JsonNum(text) => text.clone(),
        Literal::Enum { variant, .. } => variant.clone(),
        _ => return None,
    })
}

/// How many recorded invocations of one effect a replay covers unless told otherwise.
///
/// A bound rather than a preference. A busy effect keeps seven days of terminal rows,
/// which is unbounded in a way a deploy gate is not, and each row costs an interpreter
/// run plus several SQLite round-trips. Newest first, so what is covered is the history
/// closest to what is running, and whatever the bound drops is reported rather than
/// dropped quietly.
pub const DEFAULT_REPLAY_LIMIT: u32 = 1000;

/// Re-run recorded invocations of every affected effect against the candidate code.
///
/// The journal is the baseline, and it is the *real* one: it holds the responses the
/// recorded run actually received, so nothing here is mocked. A candidate that branches
/// differently on a response reaches a call the journal has no entry for, and that miss
/// is the finding. Nothing is sent, nothing is appended, nothing is erased: this is the
/// same sealed machinery `verify` uses, pointed at code that has not been deployed.
fn replay_effects(
    project: &LoadedProject,
    data_dir: &Path,
    changes: &[Change],
    deployed_effects: &BTreeMap<String, String>,
    master: Option<MasterKeys>,
    limit: usize,
) -> anyhow::Result<(Vec<Divergence>, Coverage)> {
    let mut coverage = Coverage {
        horizon_days: project.config.retention.effect_journal_days,
        limit,
        ..Coverage::default()
    };
    // Filtered before it is counted, because the count is the denominator the coverage
    // line reports. An effect this deploy *adds* is affected in every sense except the
    // one that matters here: it has no deployed version and so no history, and counting
    // it would say half the affected surface went unreplayed when only half of it could
    // ever have been.
    let affected: Vec<(&EffectUnit, &Sexp, &String)> = affected_effects(project, changes)?
        .into_iter()
        .filter_map(|(unit, form)| {
            let hash = deployed_effects.get(unit.def.name())?;
            Some((unit, form, hash))
        })
        .collect();
    coverage.effects_affected = affected.len();
    if affected.is_empty() {
        return Ok((Vec::new(), coverage));
    }

    let Some(runtime) = Runtime::open_following(project, data_dir, master)? else {
        // A log with no segment has never been written, so nothing could have been
        // recorded against it. Not an error, and not coverage either.
        return Ok((Vec::new(), coverage));
    };
    // Asked here rather than in `open_following`, which refuses nothing: a key that
    // cannot unwrap what is stored costs the replay and nothing else, and a plan that
    // threw away the declaration diff over it would make a half-configured rotation
    // worse than no key at all.
    let has_key = match runtime.keystore() {
        None => false,
        Some(keystore) => match keystore.verify_masters_present() {
            Ok(()) => true,
            Err(err) => {
                coverage.unusable_master_key = Some(format!("{err:#}"));
                false
            }
        },
    };

    let mut divergences = Vec::new();
    for (unit, form, script_hash) in affected {
        let name = unit.def.name();
        // Per effect, not per project. Only an effect that reveals needs a key, so one
        // sealed field somewhere must not zero the coverage of nine effects that never
        // touch it. Decided from the form rather than from the failure, because a
        // `reveal` with no key fails the same way a genuine divergence would. Before the
        // positions are fetched, so an effect that is not replayed at all is not also
        // reported as having had its history capped.
        if !has_key && reveals(form) {
            // The real count, not the capped one: a limit on how much would be replayed
            // says nothing about how much went unreplayed for a different reason. A count
            // that cannot be read costs this effect its tally and nothing else, by the
            // same rule the reads below follow.
            match runtime.count_terminal_invocations(name, script_hash) {
                Ok(total) => coverage.no_master_key += total,
                Err(err) => {
                    tracing::warn!("counting invocations of effect `{name}` failed: {err:#}");
                    coverage.unreadable_history.push(name.to_owned());
                }
            }
            continue;
        }
        // Bounded, and bounded by the prefix this runtime pinned: a live server goes on
        // appending while a plan runs, and an invocation recorded past the pin has no
        // event the reader can see. See `Runtime::recent_terminal_invocations`. One
        // fetch of `limit + 1` both applies the cap and detects that it bit.
        //
        // A failed read costs this effect its history and nothing more. The declaration
        // diff and the projector forecast are already computed and still true, and a
        // plan that threw them away because a live server held the write lock for six
        // seconds would make `--replay` worse than not asking for it. Same rule as the
        // master key two blocks up.
        let positions =
            match runtime.recent_terminal_invocations(name, script_hash, limit.saturating_add(1)) {
                Ok(positions) => positions,
                Err(err) => {
                    tracing::warn!("listing invocations of effect `{name}` failed: {err:#}");
                    coverage.unreadable_history.push(name.to_owned());
                    continue;
                }
            };
        if positions.len() > limit {
            coverage.truncated.push(name.to_owned());
        }
        for position in positions.into_iter().take(limit) {
            // `Candidate`: the program this runtime carries is the one being deployed,
            // not the one that wrote these journals. See `effect::Asked`.
            let outcome = effect::replay(name, position, &runtime, Asked::Candidate);
            match outcome.uncovered() {
                Some(Uncovered::NoJournal) => {
                    coverage.no_journal += 1;
                    continue;
                }
                Some(Uncovered::OperatorSkipped) => {
                    coverage.operator_skipped += 1;
                    continue;
                }
                Some(Uncovered::Unreadable) => {
                    coverage.unreadable += 1;
                    continue;
                }
                Some(Uncovered::SubjectErased) => {
                    coverage.subject_erased += 1;
                    continue;
                }
                Some(Uncovered::Reclaimed) => {
                    coverage.reclaimed += 1;
                    continue;
                }
                None => {}
            }
            coverage.replayed += 1;
            if outcome.reproduces() {
                coverage.reproduced += 1;
            } else {
                divergences.push(Divergence {
                    effect: name.to_owned(),
                    position,
                    outcome,
                });
            }
        }
    }
    divergences
        .sort_by(|left, right| (&left.effect, left.position).cmp(&(&right.effect, right.position)));
    Ok((divergences, coverage))
}

/// Every effect whose behaviour this deploy could move, with its candidate form.
///
/// An effect's own digest hash is not enough, and both ways it falls short are the same
/// shape: heklang gives a declaration its own entry, and an entry's hash covers what is
/// written inside it rather than what it names.
///
/// - A module-level `fn` is an entry of its own, so an edit to the helper that builds the
///   URL moves nothing in the effect that calls it.
/// - An `event`'s fields are an entry of its own, and an arm binds a field by *name*
///   (`heklang::digest`'s `Frame::trigger` emits the name and the slot, never the type),
///   so adding `@subject(...)` to a field changes what the handler receives without
///   moving a byte of the effect. Records and enums reach the same way, through the event
///   or the function that names them.
///
/// So the set is the transitive closure of "names something that changed", over the
/// references the digest already spells out. Conservative on purpose: an effect pulled in
/// by a reference it does not actually depend on costs one replay that reports `Matched`,
/// while one left out costs the finding this command exists for.
fn affected_effects<'a>(
    project: &'a LoadedProject,
    changes: &[Change],
) -> anyhow::Result<Vec<(&'a EffectUnit, &'a Sexp)>> {
    let entries = project.digest.entries();
    let changed: BTreeSet<Decl> = changes
        .iter()
        .map(|change| (change.kind, change.name.clone()))
        .collect();
    let tainted = declarations_reaching(entries, changed);

    let effects: BTreeMap<&str, &Sexp> = entries
        .iter()
        .filter(|entry| entry.kind == Kind::Effect)
        .map(|entry| (entry.name.as_str(), &entry.form))
        .collect();

    let mut affected = Vec::new();
    for unit in &project.effects {
        let name = unit.def.name();
        // Loud. The alternative is dropping the effect from the set, which reports full
        // coverage of a smaller set rather than reporting that an effect could not be
        // matched, and "this deploy cannot have moved it" is the one conclusion this
        // command must never reach by accident.
        let form = effects.get(name).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "effect `{name}` is loaded but has no entry in the project's digest, so \
                 there is no form to decide whether this deploy would move it"
            )
        })?;
        if tainted.contains(&(Kind::Effect, name.to_owned())) {
            affected.push((unit, form));
        }
    }
    Ok(affected)
}

/// One declaration, as the digest names it. Both halves are the key: a `record` and an
/// `event` may share a spelling, and they are not the same declaration.
type Decl = (Kind, String);

/// Every declaration that reaches a changed one, the changed ones included.
///
/// A fixpoint over the reference relation rather than a walk per declaration: the
/// subgraph a set of effects shares would otherwise be expanded once each. Terminates
/// because each round can only add names, and there are finitely many.
fn declarations_reaching(entries: &[Entry], changed: BTreeSet<Decl>) -> BTreeSet<Decl> {
    let mut tainted = changed;
    if tainted.is_empty() {
        return tainted;
    }
    let references: Vec<(Decl, BTreeSet<Decl>)> = entries
        .iter()
        .map(|entry| {
            let mut out = BTreeSet::new();
            references_of(&entry.form, &mut out);
            ((entry.kind, entry.name.clone()), out)
        })
        .collect();
    loop {
        let mut grew = false;
        for (decl, names) in &references {
            if tainted.contains(decl) || names.is_disjoint(&tainted) {
                continue;
            }
            tainted.insert(decl.clone());
            grew = true;
        }
        if !grew {
            return tainted;
        }
    }
}

/// The declarations one form names, as the digest spells them.
///
/// Every shape here is heklang's own, and each names a declaration two ways, because the
/// digest writes a *type* differently from a *value*:
///
/// - `(fn <name> args...)`, a call.
/// - `(events <path>...)`, the events an effect arm selects, and `(slice <path> ...)`,
///   the events a fold reads. Both are ways of depending on an event, and only listing
///   the first would leave a fold blind to the event it folds over.
/// - `(Record <name>)` and `(Enum <name>)` in a type position, and `(new <name> ...)` and
///   `(variant <name> <case>)` in a value position. A body that only ever constructs a
///   record names it exclusively through the second pair, so reading types alone would
///   miss it.
///
/// heklang reads its own forms this way when it collects the refusal codes a command can
/// answer with and the events an effect signature lists, so these are shapes the language
/// defines rather than ones guessed at from outside.
///
/// An `invoke` is deliberately not one: a replay reaches a command through the journal,
/// keyed by name and arguments, so an edit to the command's body cannot change what the
/// effect calls.
fn references_of(form: &Sexp, out: &mut BTreeSet<Decl>) {
    match (form.head(), form.rest().first()) {
        (Some("fn"), Some(Sexp::Atom(name))) => {
            out.insert((Kind::Function, name.clone()));
        }
        (Some("Record" | "new"), Some(Sexp::Atom(name))) => {
            out.insert((Kind::Record, name.clone()));
        }
        (Some("Enum" | "variant"), Some(Sexp::Atom(name))) => {
            out.insert((Kind::Enum, name.clone()));
        }
        (Some("slice"), Some(Sexp::Atom(path))) => {
            out.insert((Kind::Event, path.clone()));
        }
        (Some("events"), _) => {
            for item in form.rest() {
                if let Sexp::Atom(path) = item {
                    out.insert((Kind::Event, path.clone()));
                }
            }
        }
        _ => {}
    }
    if let Sexp::List(items) = form {
        for item in items {
            references_of(item, out);
        }
    }
}

/// Whether `form` reveals anything, which is the one thing a replay needs a key for.
///
/// `reveal` is an expression head in the packed form, and an effect-local `fn` is packed
/// inside its effect, so one walk over the effect's own form covers every place it can
/// appear. A module `fn` cannot reveal at all (heklang forbids it), so nothing is hidden
/// behind a call.
fn reveals(form: &Sexp) -> bool {
    any_node(form, &mut |node| node.head() == Some("reveal"))
}

/// Whether `pred` holds of `form` or of anything inside it.
fn any_node(form: &Sexp, pred: &mut impl FnMut(&Sexp) -> bool) -> bool {
    if pred(form) {
        return true;
    }
    match form {
        Sexp::List(items) => items.iter().any(|item| any_node(item, pred)),
        _ => false,
    }
}
