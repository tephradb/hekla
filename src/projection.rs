//! `hekla project`: a projector that is not deployed, folded over the log once.
//!
//! The read API serves what a *deployed* projector chose to materialise and `/admin`
//! serves raw events filtered by type and tag. Neither answers "count these, grouped by
//! that", and the only route to that answer was writing a projector, deploying it and
//! waiting for a rebuild: a schema change, a new declaration hash and a read model on
//! disk, for a question asked once. This is that question without the deploy.
//!
//! It sits beside [`crate::plan`] rather than under the runtime, for the same reason:
//! both run code that has not been deployed against a directory that may be serving
//! traffic. The log is read through a tephra follower, so no lock is taken and nothing is
//! created; the rows land in a throwaway read model in a temporary directory, the way
//! `hekla test` and [`crate::verify`] already build one.
//!
//! **The rows go through the real sink.** [`crate::heklang_host::RowWriter`] writes them,
//! not an in-memory map, and that is a correctness requirement rather than a convenience.
//! It re-seals a moved seal under the column's own field name, because a key store binds
//! the field name into the ciphertext and a column stores a `Timestamp` in a shape an
//! event does not; it writes through `encrypt_subject_existing`, so re-projecting an
//! erased subject cannot mint the key the erasure destroyed; and it decrypts on read, so
//! a `patch` over a sealed column reads plaintext back. A sink that skipped any of the
//! three would answer a different question than the deployment does, which is the one
//! thing this must not do.
//!
//! # What it cannot see
//!
//! - **Only the event log.** The operational database is opened for one thing, subject
//!   keys, and only when the projector declares a sealed column. Effect invocations,
//!   journals, retry counts and checkpoints stay `/admin`'s to answer.
//! - **No `reveal`.** A projector holds no host, so it cannot branch on subject plaintext,
//!   here or when deployed. It can key and group on a sealed column, because the
//!   encryption is deterministic.
//! - **No checkpoint.** Every run folds from the start of its window into a fresh model
//!   and deletes it. A question asked on every request is a deployment, not a projection.
//! - **Nothing appended during the run.** A follower pins one committed prefix when it
//!   opens, so the result names the position it is the answer as of.
//! - **Nothing is recorded.** No declaration row, no database under `data/projectors/`,
//!   no route, no metric. A later `hekla plan` cannot tell this ran.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use tephra::Position;

use crate::crypto::{KeyStore, MasterKeys};
use crate::heklang_host::Shredded;
use crate::loader::{LoadedProject, ProjectorUnit};
use crate::opdb::{self, OpDb, SCHEMA_VERSION};
use crate::progress::thousands;
use crate::projector::{self, Stopped, Window};
use crate::read_api;
use crate::read_model::ReadModel;
use crate::runtime;
use crate::schema::{EntityDef, ModuleDef, scalar_to_string};

/// Rows kept per entity when the caller names no limit.
///
/// `read_api::DEFAULT_LIMIT` deliberately: an operator comparing an ad-hoc projection
/// against `GET /read/...` should be shown the same amount of it by both.
pub const DEFAULT_ROWS: usize = read_api::DEFAULT_LIMIT;

/// What to project, and over how much of the log.
pub struct Request<'a> {
    /// Which projector, when the scratch module declares more than one.
    pub projector: Option<&'a str>,
    /// Which entity to report, or every one of them in declaration order.
    pub entity: Option<&'a str>,
    /// The inclusive position window. `None` is the start of the log, and the tip the
    /// follower pinned.
    pub from: Option<u64>,
    pub upto: Option<u64>,
    /// Matching events this run may spend. `None` is the whole window.
    pub max_events: Option<u64>,
    /// Whether to open sealed columns on the way out.
    ///
    /// A rendering choice and nothing else. The *fold* holds the key whenever the
    /// projector seals a column at all, because it has to re-seal under the column's own
    /// field name and read its own stored loads back; this withholds only the last step,
    /// the way `/admin/events?decrypt=false` does.
    pub decrypt: bool,
    /// Rows kept per entity.
    pub rows: usize,
    /// The master key the subject keys are wrapped under, when the projection needs one.
    ///
    /// Passed in rather than read from the environment here: `hekla plan` resolves its
    /// own the same way, and a library call that reached for a process variable would
    /// make the one thing this command can refuse over untestable from anywhere but a
    /// subprocess.
    pub master: Option<MasterKeys>,
}

impl Default for Request<'_> {
    fn default() -> Request<'static> {
        Request {
            projector: None,
            entity: None,
            from: None,
            upto: None,
            max_events: None,
            decrypt: true,
            rows: DEFAULT_ROWS,
            master: None,
        }
    }
}

/// One entity's rows, and what could not be read in them.
pub struct Entity {
    pub name: String,
    pub key: String,
    /// Every declared column, in declaration order: the header, and the order a row's
    /// values print in.
    pub columns: Vec<String>,
    /// `(column, subject field)` for each sealed column. A static fact off the
    /// declaration, printed so a reader knows which blank cells *could* be an erasure and
    /// which cannot.
    pub sealed: Vec<(String, String)>,
    /// Every row the entity holds, which is not the same as how many are kept.
    pub row_count: u64,
    pub rows: Vec<Value>,
    /// Cells holding a value written under a key that has since been superseded. Also
    /// unreadable, but not the permanent, total loss an erasure is.
    ///
    /// **Counted over `rows` alone**, because decrypting is what finds one and only the
    /// kept rows are decrypted. That makes it a different scope from
    /// [`Projection::shredded`], which the fold counts over every write it made, so both
    /// the report and the JSON say which rows this is out of rather than leaving the two
    /// numbers to read as one.
    pub stale: usize,
}

impl Entity {
    /// Whether rows were left out of `rows`.
    pub fn truncated(&self) -> bool {
        self.row_count > self.rows.len() as u64
    }
}

/// What one projection found.
pub struct Projection {
    pub projector: String,
    /// The module name the scratch source was compiled under: the path the operator
    /// typed.
    pub source: String,
    /// The projector's heklang digest entry hash. A result is attributable to exact code
    /// even though nothing about it was deployed or recorded.
    pub digest: String,
    /// The event types the handlers select, which is the projector's whole subscription.
    pub sources: Vec<String>,
    pub from: u64,
    pub upto: u64,
    /// The tip the follower pinned when it opened.
    pub head: u64,
    /// Matching events folded.
    pub events: usize,
    /// The position these rows are the answer as of.
    pub position: u64,
    /// Why the fold stopped short, if it did. `None` means the window was covered, and a
    /// reader has to be able to tell that from a bounded run without re-deriving it.
    pub stopped: Option<Stopped>,
    pub entities: Vec<Entity>,
    /// Sealed column writes the fold dropped, counted where they were dropped.
    pub shredded: Shredded,
    pub decrypt: bool,
    pub elapsed: Duration,
    pub row_limit: usize,
}

/// Fold the project's scratch projector over the log and read its rows back.
///
/// `project` must have been loaded with a [`crate::loader::Scratch`], which is what puts
/// the ad-hoc projector into `project.projectors` beside the deployed ones, built the
/// same way and special-cased nowhere below this.
///
/// `progress` is called once a batch with the position reached, the position the window
/// ends at, and the events folded so far. The window end comes from here rather than
/// from the caller because only this function has opened the follower, and the tip it
/// pinned is the only honest denominator: a caller guessing one would be reporting
/// against a number the fold is not working towards.
pub fn run(
    project: &LoadedProject,
    data_dir: &Path,
    request: &Request<'_>,
    progress: &mut dyn FnMut(Position, Position, usize),
) -> anyhow::Result<Projection> {
    let started = Instant::now();
    let unit = select(project, request.projector)?;
    let ModuleDef::Projector { name, .. } = &unit.def else {
        anyhow::bail!("the scratch module is not a projector");
    };

    // A projector with no handler lowers to a query that matches nothing, so every entity
    // would come back empty and read exactly like a question with no answer. heklang has
    // no rule against one, so this is hekla's.
    if unit.sources.is_empty() {
        anyhow::bail!("`projector {name}` declares no handler, so it selects no events");
    }
    if let Some(wanted) = request.entity
        && !unit.entities.iter().any(|one| one.name == wanted)
    {
        let known: Vec<&str> = unit.entities.iter().map(|one| one.name.as_str()).collect();
        anyhow::bail!(
            "`projector {name}` declares no entity `{wanted}`\n       declared here: {}",
            known.join(", ")
        );
    }

    let keystore = open_keystore(&unit.entities, data_dir, request.master.clone())?;
    let Some(store) = runtime::follow(data_dir)? else {
        anyhow::bail!(
            "no event log at {}, so there is nothing to project over",
            data_dir.join("events").display()
        );
    };

    // The tip the follower pinned when it opened bounds everything: a position above it
    // is not readable through this handle, so accepting one would report a window that
    // was never covered.
    let head = store.head();
    let from = Position::new(request.from.unwrap_or_default());
    // A window that cannot hold anything is refused rather than folded. Both of these
    // come back as a clean `ok: 0 row(s)` otherwise, which an operator who typed one
    // digit too many reads as "the projector matched nothing in the whole log": the
    // worst answer this command could give, because it is a wrong one that looks right.
    if let Some(upto) = request.upto
        && from.get() > upto
    {
        anyhow::bail!(
            "--from {} is above --upto {upto}, so the window holds nothing",
            from.get()
        );
    }
    if from.get() > head.get() {
        anyhow::bail!(
            "--from {} is above position {}, which is the whole log this follower can \
             see, so the window holds nothing",
            from.get(),
            head.get()
        );
    }
    // Clamped rather than refused: a tip moves, so asking for one past it is an ordinary
    // thing to do with a position read a moment ago. The report says it was clamped.
    let upto = request
        .upto
        .map_or(head, |upto| Position::new(upto).min(head));
    let window = Window {
        from,
        upto,
        max_events: request.max_events,
    };

    let dir = TempDir::new()?;
    let model = ReadModel::open(&dir.path().join("projection.db"), &unit.entities)?;
    let mut shredded = Shredded::default();
    let scanned = projector::project_reading(
        &store,
        unit,
        &project.program,
        keystore.as_deref(),
        &model,
        window,
        &mut shredded,
        &mut |position, matched| progress(position, upto, matched),
    )?;

    let mut entities = Vec::new();
    for entity in &unit.entities {
        if request.entity.is_some_and(|wanted| wanted != entity.name) {
            continue;
        }
        entities.push(read_entity(
            &model,
            entity,
            keystore.as_deref(),
            request.decrypt,
            request.rows,
        )?);
    }

    Ok(Projection {
        projector: name.clone(),
        source: unit.rel_path.clone(),
        digest: unit.digest_hash.clone(),
        sources: unit.sources.clone(),
        from: from.get(),
        upto: upto.get(),
        head: head.get(),
        events: scanned.events,
        position: scanned.position.get(),
        stopped: scanned.stopped,
        entities,
        shredded,
        decrypt: request.decrypt,
        elapsed: started.elapsed(),
        row_limit: request.rows,
    })
}

/// The one projector this run folds, or an error saying why there is not exactly one.
///
/// Only the scratch module's own projectors are candidates. `hekla project` folds
/// undeployed code, and a deployed projector's rows are already served at `/read`.
pub fn select<'a>(
    project: &'a LoadedProject,
    named: Option<&str>,
) -> anyhow::Result<&'a ProjectorUnit> {
    let scratch = project.scratch.as_deref().unwrap_or_default();
    let declared: Vec<&ProjectorUnit> = project
        .projectors
        .iter()
        .filter(|unit| unit.rel_path == scratch)
        .collect();
    let names = |units: &[&ProjectorUnit]| {
        units
            .iter()
            .map(|unit| unit.def.name())
            .collect::<Vec<&str>>()
            .join(", ")
    };
    match (named, declared.as_slice()) {
        (_, []) => anyhow::bail!("{scratch} declares no projector, so there is nothing to fold"),
        (None, [only]) => Ok(only),
        (None, several) => anyhow::bail!(
            "{scratch} declares {} projector(s); name one with --projector\n       declared here: {}",
            several.len(),
            names(several)
        ),
        (Some(wanted), several) => match several.iter().find(|unit| unit.def.name() == wanted) {
            Some(unit) => Ok(unit),
            // A deployed name is the likely mistake, and saying where its rows already
            // are answers the question behind it rather than only refusing.
            None if project.projectors.iter().any(|u| u.def.name() == wanted) => anyhow::bail!(
                "`projector {wanted}` is deployed, not declared in {scratch}; \
                 `hekla project` folds undeployed code, and a deployed projector's rows \
                 are already served at GET /read/{wanted}/..."
            ),
            None => anyhow::bail!(
                "{scratch} declares no `projector {wanted}`\n       declared here: {}",
                names(several)
            ),
        },
    }
}

/// The key store, if this projection needs one.
///
/// It needs one exactly when the projector seals a column, and then it is not optional:
/// [`crate::heklang_host::RowWriter`] refuses a sealed column with no key rather than
/// storing plaintext, so without it the *fold* fails and not just the rendering. Saying
/// so before the scan beats discovering it a million events in.
///
/// A projection over plaintext columns opens no operational database at all, so it needs
/// nothing but the event log.
fn open_keystore(
    entities: &[EntityDef],
    data_dir: &Path,
    master: Option<MasterKeys>,
) -> anyhow::Result<Option<Arc<KeyStore>>> {
    let sealed = entities.iter().find_map(|entity| {
        entity
            .fields
            .iter()
            .find_map(|(name, meta)| meta.subject.as_ref().map(|subject| (name, subject)))
    });
    let Some((column, subject)) = sealed else {
        return Ok(None);
    };
    let Some(master) = master else {
        anyhow::bail!(
            "this projection seals column `{column}` under `{subject}`, so folding it \
             needs HEKLA_MASTER_KEY\n       a projector re-seals a column under the \
             column's own field name, so the fold cannot carry the log's ciphertext \
             through untouched"
        );
    };

    let db_path = data_dir.join("hekla.db");
    if !db_path.exists() {
        anyhow::bail!(
            "no operational database at {}, so the subject keys this projection needs \
             are not there",
            db_path.display()
        );
    }
    // The version is read separately first, because `OpDb::open` migrates and silently
    // upgrading a directory a server has open is not a reader's business. The same guard
    // `hekla plan` takes, for the same reason.
    let recorded = opdb::recorded_schema_version(&db_path)?;
    if recorded != SCHEMA_VERSION {
        anyhow::bail!(
            "the data directory is at schema version {recorded} and this build expects \
             {SCHEMA_VERSION}; run `hekla serve` against it once to migrate, or project \
             against a directory this build wrote"
        );
    }
    let opdb = Arc::new(Mutex::new(OpDb::open(&db_path)?));
    Ok(Some(Arc::new(KeyStore::new(opdb, master))))
}

/// One entity's rows, with its sealed columns opened.
///
/// Counted before it is paged, because how many rows there are is part of the answer and
/// a page that does not say what it is a page of is a different claim. `read_api::scan`
/// bounds the read the same way rather than materialising an entity that could be
/// millions of rows wide; `ReadModel::rows` would read every one of them.
///
/// The decrypt below is `read_api::decrypt_row`'s, down to dropping a cell it cannot
/// open so the row reads exactly as the read API would serve it, with one thing added:
/// it separates a value that will not open under a *live* key from one whose key is
/// gone. The read API has no use for the distinction, because a reader of a read model
/// wants a row rather than a report on it. A one-off answer is the report, and a column
/// that vanished because a master was rotated away is not the same finding as one that
/// was erased.
fn read_entity(
    model: &ReadModel,
    entity: &EntityDef,
    keystore: Option<&KeyStore>,
    decrypt: bool,
    limit: usize,
) -> anyhow::Result<Entity> {
    let row_count = model.count(entity)?;
    let mut rows = model.scan(entity, None, None, limit)?;
    let sealed: Vec<(String, String)> = entity
        .fields
        .iter()
        .filter_map(|(name, meta)| meta.subject.as_ref().map(|s| (name.clone(), s.clone())))
        .collect();

    let mut stale = 0;
    if decrypt
        && !sealed.is_empty()
        && let Some(keystore) = keystore
    {
        // One decryptor for the page, so a run of rows sharing a subject unwraps that
        // key once rather than per row.
        let decryptor = keystore.row_decryptor();
        for row in &mut rows {
            let Some(obj) = row.as_object_mut() else {
                continue;
            };
            for (name, meta) in &entity.fields {
                let Some(subject) = &meta.subject else {
                    continue;
                };
                let Some(ciphertext) = obj.get(name).and_then(Value::as_str).map(str::to_owned)
                else {
                    continue;
                };
                let id = obj.get(subject).and_then(scalar_to_string);
                let plaintext = match &id {
                    Some(id) => decryptor.decrypt(subject, id, name, &ciphertext)?,
                    // No subject id to key on, so the value is unreadable.
                    None => None,
                };
                match plaintext {
                    Some(text) => {
                        obj.insert(name.clone(), read_api::typed_from_string(&meta.kind, text));
                    }
                    None => {
                        // The key is live and this value still will not open: it was
                        // written under one that has since been superseded. Worth its own
                        // count, because unlike an erasure it is not permanent.
                        if id.is_some_and(|id| decryptor.key_present(subject, &id) == Some(true)) {
                            stale += 1;
                        }
                        // Removed rather than kept, so a cell reads absent exactly as the
                        // read API would serve it.
                        obj.remove(name);
                    }
                }
            }
        }
    }

    Ok(Entity {
        name: entity.name.clone(),
        key: entity.key.clone(),
        columns: entity.fields.iter().map(|(name, _)| name.clone()).collect(),
        sealed,
        row_count,
        rows,
        stale,
    })
}

impl Projection {
    /// The whole result, for a reader that is a program.
    pub fn json(&self) -> Value {
        json!({
            "projector": self.projector,
            "source": self.source,
            "digest": self.digest,
            "sources": self.sources,
            "window": { "from": self.from, "upto": self.upto, "head": self.head },
            "scanned": {
                "events": self.events,
                "position": self.position,
                // Null for a fold that covered its window. A reader tells a complete
                // answer from a bounded one here rather than by comparing counters.
                "stopped": self.stopped.map(|stopped| match stopped {
                    Stopped::MaxEvents => "max-events",
                }),
            },
            "shredded": { "writes": self.shredded.writes, "subjects": self.shredded.subjects() },
            "decrypt": self.decrypt,
            "row_limit": self.row_limit,
            "truncated": self.truncated(),
            "entities": self.entities.iter().map(|entity| json!({
                "name": entity.name,
                "key": entity.key,
                "columns": entity.columns,
                "sealed": entity.sealed.iter().map(|(column, subject)| json!({
                    "column": column,
                    "subject": subject,
                })).collect::<Vec<Value>>(),
                "row_count": entity.row_count,
                "truncated": entity.truncated(),
                "stale": { "cells": entity.stale, "of_rows_shown": entity.rows.len() },
                "rows": entity.rows,
            })).collect::<Vec<Value>>(),
        })
    }

    /// Whether any entity holds rows this result is not carrying.
    pub fn truncated(&self) -> bool {
        self.entities.iter().any(Entity::truncated)
    }

    fn rows(&self) -> u64 {
        self.entities.iter().map(|entity| entity.row_count).sum()
    }
}

impl fmt::Display for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let types: Vec<String> = self.sources.iter().map(|ty| format!("@{ty}")).collect();
        writeln!(
            f,
            "folded {} event(s) of {} into `projector {}` in {:.2?}",
            thousands(self.events as u64),
            types.join(", "),
            self.projector,
            self.elapsed
        )?;

        for entity in &self.entities {
            writeln!(
                f,
                "  entity {}, key {}, {} row(s)",
                entity.name,
                entity.key,
                thousands(entity.row_count)
            )?;
            for (column, subject) in &entity.sealed {
                writeln!(f, "    sealed: {column} under {subject}")?;
            }
            render_rows(f, entity)?;
            if entity.stale > 0 {
                // "of the N shown", because only the shown rows were decrypted. The
                // shredded count below is over the whole fold, and two numbers three
                // lines apart must not look like they cover the same ground.
                writeln!(
                    f,
                    "    {} cell(s) of the {} row(s) shown were written under a \
                     superseded key and will not open",
                    thousands(entity.stale as u64),
                    thousands(entity.rows.len() as u64)
                )?;
            }
        }

        // Everything that bounded the answer, before the answer is summarised, which is
        // where `plan` puts its own coverage lines and for the same reason.
        if self.from > 0 {
            writeln!(
                f,
                "the window starts at position {}, so a row whose first event is below \
                 that is missing or partial",
                thousands(self.from)
            )?;
        }
        if self.upto < self.head {
            writeln!(
                f,
                "bounded at position {}, so {} position(s) below the pinned tip were not \
                 folded",
                thousands(self.upto),
                thousands(self.head - self.upto)
            )?;
        }
        if self.shredded.writes > 0 {
            writeln!(
                f,
                "{} sealed column write(s) dropped across {} erased subject(s), so those \
                 cells read absent",
                thousands(self.shredded.writes),
                thousands(self.shredded.subjects() as u64)
            )?;
        }
        // The snapshot claim is only about the tip when the fold actually reached it.
        // A run that stopped short stands at a position of its own, and calling that the
        // tip would say the log ends where this run gave up.
        if self.stopped.is_none() && self.upto == self.head {
            writeln!(
                f,
                "a snapshot at position {}, the tip pinned when this opened; anything \
                 appended since is not in it",
                thousands(self.position)
            )?;
        } else {
            writeln!(
                f,
                "these rows are the answer as of position {}, out of {} the log held when \
                 this opened",
                thousands(self.position),
                thousands(self.head)
            )?;
        }

        match self.stopped {
            None => write!(
                f,
                "ok: {} row(s) from {} event(s)",
                thousands(self.rows()),
                thousands(self.events as u64)
            ),
            // Never `ok:`. These rows are a prefix of the log's answer, and a bounded run
            // that read like a complete one is the worst thing this could print.
            Some(Stopped::MaxEvents) => write!(
                f,
                "partial: {} row(s) from {} event(s); stopped at the --max-events budget \
                 with {} position(s) of the window unread",
                thousands(self.rows()),
                thousands(self.events as u64),
                thousands(self.upto.saturating_sub(self.position))
            ),
        }
    }
}

/// One entity's kept rows as an aligned table.
///
/// Widths come from the data, the way `hekla secrets` sizes its own columns. The full set
/// is in `--json`; a terminal that had to scroll past ten thousand rows to reach the
/// summary would be worse served than one told what it is not being shown.
fn render_rows(f: &mut fmt::Formatter<'_>, entity: &Entity) -> fmt::Result {
    if entity.rows.is_empty() {
        return writeln!(f, "    (no rows)");
    }
    let cells: Vec<Vec<String>> = entity
        .rows
        .iter()
        .map(|row| entity.columns.iter().map(|name| cell(row, name)).collect())
        .collect();
    let widths: Vec<usize> = entity
        .columns
        .iter()
        .enumerate()
        .map(|(column, name)| {
            cells
                .iter()
                .map(|row| row[column].chars().count())
                .chain([name.chars().count()])
                .max()
                .unwrap_or_default()
        })
        .collect();

    writeln!(f, "    {}", padded(&entity.columns, &widths))?;
    for row in &cells {
        writeln!(f, "    {}", padded(row, &widths))?;
    }
    if entity.truncated() {
        writeln!(
            f,
            "    ... and {} more row(s)",
            thousands(entity.row_count - entity.rows.len() as u64)
        )?;
    }
    Ok(())
}

/// One table line: every value left-aligned in its column, two spaces between, with the
/// trailing pad dropped so no line carries invisible width.
fn padded<T: AsRef<str>>(values: &[T], widths: &[usize]) -> String {
    let cells: Vec<String> = values
        .iter()
        .zip(widths)
        .map(|(text, width)| format!("{:<width$}", text.as_ref()))
        .collect();
    cells.join("  ").trim_end().to_owned()
}

/// One cell. A column the row does not carry prints `-`: it is an absent optional, or a
/// sealed value that could not be read, and the lines under the table say which are
/// possible for this entity and how many there were.
fn cell(row: &Value, name: &str) -> String {
    match row.get(name) {
        None | Some(Value::Null) => "-".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
    }
}
