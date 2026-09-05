//! What deploying this project would change, before it changes anything.
//!
//! `hekla plan` answers the question a deploy raises and nothing else could: this code
//! is not what is running, so what is different, and what would booting it do? It reads
//! two things and changes neither. The candidate side is the project's digest, exactly as
//! [`crate::loader`] computes it at boot. The recorded side is the `declaration` table,
//! which keeps the packed form of every version of every declaration, so the deployed
//! program reads back with no source tree in reach.
//!
//! Two properties make this worth trusting. It opens no event log and takes no
//! data-directory lock, so it runs against a live deployment rather than only a copy;
//! and every hash it compares is heklang's digest, so a reformat is not a change and a
//! handler fix is.
//!
//! Effect replay is deliberately absent. Reporting that an effect would now make
//! different HTTP calls needs a baseline that can be *executed*, and a packed form is a
//! rendering rather than a serialisation. The journal is where that baseline lives, and
//! reaching it means opening the log.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use heklang::ir::{Command, Literal, MessagePart, RefusalDef};
use heklang::{Entry, Kind, Program};

use crate::loader::{self, LoadedProject};
use crate::opdb::{self, DeclarationRow, OpDb, SCHEMA_VERSION};
use crate::projector::{Reconcile, reconcile_from};
use crate::read_model::ReadModel;

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
}

impl Plan {
    /// Whether deploying would change nothing at all.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
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

        if self.is_empty() {
            return write!(f, "ok: nothing would change");
        }
        let added = self.count(Verdict::Added);
        let removed = self.count(Verdict::Removed);
        let changed = self.count(Verdict::Behaviour) + self.count(Verdict::Contract);
        write!(
            f,
            "{added} added, {removed} removed, {changed} changed; \
             {} projector(s) would rebuild",
            self.rebuilding()
        )
    }
}

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

/// Compare a candidate project against what a data directory records as deployed.
///
/// Changes nothing. The operational database is opened only after its schema version is
/// read separately and found to match, because [`OpDb::open`] migrates and silently
/// upgrading a live deployment is not a reader's business. Read models are opened
/// read-only for the same reason: [`ReadModel::open`] would create tables for a
/// projector that has never run.
///
/// The one mark it can leave is SQLite's: a read-only connection to a WAL database maps
/// a shared-memory index and cannot remove it on close, so an empty `-wal` and `-shm`
/// pair may outlive the call. No database's contents change.
pub fn compute(project: &LoadedProject, data_dir: &Path) -> anyhow::Result<Plan> {
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
    let db = OpDb::open(&db_path)?;
    let recorded = db.current_declarations()?;

    let mut plan = Plan::default();
    let changes = diff(project, &recorded, &mut plan)?;
    plan.changes = changes;
    plan.projectors = forecast(project, data_dir)?;
    if !plan.digest_version_mismatch {
        plan.causes = attribute(&project.program, &plan.changes);
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
