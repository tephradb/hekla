//! A stored fold's history: the positions where its column changed, read back off the log.
//!
//! A derived table holds a fold's value now. Its history is every record that moved it,
//! which the store does not keep and does not need to: heklang's `fold_steps` folds the
//! key's own slice of the log and reports the value after each record, and this keeps the
//! records that changed it.
//!
//! **Read from the log and not the store**, so a history is never behind and never waits:
//! it reads to the log's head as of the request. The cost is a fold of the key's history
//! per request, which for a fold keyed straight on event fields is an indexed read of that
//! key's own events.

use std::error::Error as StdError;
use std::sync::Arc;
use std::{fmt, mem, slice};

use anyhow::Context;
use heklang::host::Log;
use heklang::interp::{ErrorKind, coerce, eval_pure};
use heklang::value::{Record, Value};
use heklang::{Interpreter, Program};
use rusqlite::types::ValueRef;
use serde_json::{Map, Value as Json, json};

use crate::crypto::KeyStore;
use crate::fold_store::cell_of;
use crate::heklang_host::{HeklaHost, served_micros};
use crate::schema::{EventDefs, event_type};
use crate::store::Store;
use crate::table_read::{Refusal, rendered};
use crate::tables::{ColumnDef, TableDef, key_value};

/// The most records one history request folds, across every column it asks for.
///
/// A history is folded from the start of the key's log on every request, and a fold that
/// looks another up refolds that one at each record, so an unbounded request is one a
/// caller can make arbitrarily expensive. Past this the request is refused rather than
/// slow; a key with this much history wants the fold behind it narrowed.
pub const MAX_FOLDED: u64 = 100_000;

const TOO_LONG: &str = "the history is longer than one request folds";

/// A history over [`MAX_FOLDED`] records, which a handler answers as a 400.
#[derive(Debug)]
pub struct TooLong;

impl fmt::Display for TooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "this history folds more than {MAX_FOLDED} records; ask for fewer columns, or \
             narrow the fold"
        )
    }
}

impl StdError for TooLong {}

/// The parameters a history read takes besides the key.
pub const CONTROL: [&str; 4] = ["columns", "limit", "before", "since"];

/// Which part of a history to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Changes at positions below this one, which is how a page resumes.
    pub before: Option<u64>,
    /// Changes at or above this position.
    pub since: Option<u64>,
    pub limit: usize,
    /// The columns asked for by name, or `None` for every one with a history.
    pub columns: Option<Vec<String>>,
}

/// The control parameters off a query string, and the key parameters left over.
pub fn parse_window(
    params: &[(String, String)],
) -> Result<(Window, Vec<(String, String)>), Refusal> {
    let mut window = Window {
        before: None,
        since: None,
        limit: crate::read_api::DEFAULT_LIMIT,
        columns: None,
    };
    let mut rest = Vec::new();
    let position = |name: &str, value: &str| {
        value.parse::<u64>().map_err(|_| Refusal {
            code: "invalid_input",
            message: format!("`{name}` must be a log position, a non-negative integer"),
        })
    };
    for (name, value) in params {
        match name.as_str() {
            "limit" => {
                window.limit = value
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| *limit > 0)
                    .ok_or_else(|| Refusal {
                        code: "invalid_input",
                        message: "limit must be a positive integer".into(),
                    })?
                    .min(crate::read_api::MAX_LIMIT);
            }
            "before" => window.before = Some(position(name, value)?),
            "since" => window.since = Some(position(name, value)?),
            "columns" => {
                window.columns = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                        .collect(),
                )
            }
            _ => rest.push((name.clone(), value.clone())),
        }
    }
    Ok((window, rest))
}

/// The columns a history is read over: the ones asked for, or every column that has one.
pub fn chosen<'a>(
    columns: &'a [&'a ColumnDef],
    asked: Option<&[String]>,
    table: &str,
) -> Result<Vec<&'a ColumnDef>, Refusal> {
    if let Some(asked) = asked
        && let Some((_, name)) = asked
            .iter()
            .enumerate()
            .find(|(at, name)| asked[..*at].contains(name))
    {
        return Err(Refusal {
            code: "invalid_input",
            message: format!("`{name}` is asked for twice"),
        });
    }
    let Some(asked) = asked else {
        let kept: Vec<&ColumnDef> = columns.iter().copied().filter(|c| c.history).collect();
        if kept.is_empty() {
            return Err(no_history(table, columns));
        }
        return Ok(kept);
    };
    asked
        .iter()
        .map(|name| {
            let column = columns
                .iter()
                .find(|column| &column.name == name)
                .ok_or_else(|| Refusal {
                    code: "invalid_input",
                    message: format!("`{name}` is not a column of `{table}`"),
                })?;
            if !column.history {
                return Err(no_history(table, slice::from_ref(column)));
            }
            Ok(*column)
        })
        .collect()
}

fn no_history(table: &str, columns: &[&ColumnDef]) -> Refusal {
    let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
    Refusal {
        code: "no_history",
        message: format!(
            "{} of `{table}` has no history: its fold has an arm that leaves a key part open, \
             so a key made after such a record never saw it, and a history read off the log \
             would report a change the stored column never took",
            names.join(", ")
        ),
    }
}

/// One page of a history, newest first.
pub struct Page {
    pub items: Vec<Json>,
    /// Pass as `before` for the next page; `None` on the last.
    pub next_before: Option<u64>,
    /// The log length the history was read to.
    pub head: u64,
}

/// Everything a history read needs from the running deployment.
pub struct Source {
    pub program: Arc<Program>,
    pub events: Arc<EventDefs>,
    pub store: Store,
    pub keystore: Option<Arc<KeyStore>>,
}

/// One change, before it is rendered.
struct Step {
    position: u64,
    event_type: String,
    at: i64,
    column: usize,
    before: Value,
    after: Value,
}

/// The history of `columns` at `key` (texts, in `table`'s key order).
pub fn read(
    source: &Source,
    table: &TableDef,
    columns: &[&ColumnDef],
    key: &[String],
    window: &Window,
) -> anyhow::Result<Page> {
    let host = HeklaHost::reader(
        Arc::clone(&source.program),
        Arc::clone(&source.events),
        source.store.clone(),
        source.keystore.clone(),
    );
    let head = host
        .head()
        .map_err(|err| anyhow::anyhow!("reading the log head: {err}"))?;
    let interpreter = Interpreter::with_host(source.program.as_ref(), host);
    let keystore = source.keystore.as_deref();

    let mut steps: Vec<Step> = Vec::new();
    let mut folded: u64 = 0;
    for (at, column) in columns.iter().enumerate() {
        let args = column
            .args
            .iter()
            .zip(key)
            .zip(&table.key)
            .map(|((name, text), (_, meta))| key_value(&meta.kind, text).map(|v| (name.clone(), v)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let def = source
            .program
            .fold(&column.fold)
            .with_context(|| format!("no fold `{}`", column.fold))?;
        // The seed, which is what the column holds before its first record, fitted to the
        // accumulator's type the way the fold fits it: an optional fold seeded `"free"`
        // holds `some("free")`, and comparing the bare literal to that is a change that
        // never happened.
        let seed = def
            .stage
            .folds
            .iter()
            .find(|var| var.slot == def.slot)
            .with_context(|| format!("`{}` has no accumulator", column.fold))?;
        let mut held = coerce(
            eval_pure(source.program.as_ref(), &def.exprs, def.frame, seed.init)
                .map_err(|err| anyhow::anyhow!("seeding `{}`: {err}", column.fold))?,
            &seed.ty,
        );
        let mut visit = |record: &Record, value: &Value| {
            folded += 1;
            if folded > MAX_FOLDED {
                return Err(heklang::interp::Error::new(ErrorKind::Host(
                    TOO_LONG.into(),
                )));
            }
            if value.same(&held) {
                return Ok(());
            }
            // Two seals of one plaintext differ when they came in through different fields,
            // so a sealed column is compared as it is served before it counts as moved.
            if column.meta.sealed_under.is_some() {
                let opened = |value: &Value| {
                    render(table, column, value, keystore).map_err(|why| {
                        heklang::interp::Error::new(ErrorKind::Host(format!("{why:#}")))
                    })
                };
                if opened(value)? == opened(&held)? {
                    held = value.clone();
                    return Ok(());
                }
            }
            steps.push(Step {
                position: record.position + 1,
                event_type: event_type(&record.event.path),
                at: record.at,
                column: at,
                before: mem::replace(&mut held, value.clone()),
                after: value.clone(),
            });
            Ok(())
        };
        interpreter
            .fold_steps(&column.fold, args, head, &mut visit)
            .map_err(|err| match &err.kind {
                ErrorKind::Host(why) if why == TOO_LONG => anyhow::Error::new(TooLong),
                ErrorKind::NoHistory(_) => anyhow::anyhow!("`{}` has no history", column.fold),
                _ => anyhow::anyhow!("reading the history of `{}`: {err}", column.fold),
            })?;
    }

    // Newest first, and one entry per record however many columns it moved.
    steps.sort_by(|a, b| b.position.cmp(&a.position).then(a.column.cmp(&b.column)));
    let mut items = Vec::new();
    let mut next_before = None;
    let mut steps = steps
        .into_iter()
        .filter(|step| window.before.is_none_or(|before| step.position < before))
        .filter(|step| window.since.is_none_or(|since| step.position >= since))
        .peekable();
    while let Some(first) = steps.next() {
        if items.len() == window.limit {
            next_before = items
                .last()
                .and_then(|item: &Json| item["position"].as_u64());
            break;
        }
        let mut changed = Map::new();
        let mut group = vec![first];
        while steps
            .peek()
            .is_some_and(|step| step.position == group[0].position)
        {
            group.push(steps.next().expect("peeked"));
        }
        for step in &group {
            let column = columns[step.column];
            changed.insert(
                column.name.clone(),
                json!({
                    "before": render(table, column, &step.before, keystore)?,
                    "after": render(table, column, &step.after, keystore)?,
                }),
            );
        }
        // The record named, not rendered: its payload is `/admin/events`', which a
        // deployment can keep behind its admin boundary while serving this.
        items.push(json!({
            "position": group[0].position,
            "event": {
                "type": group[0].event_type,
                "at": served_micros(group[0].at),
            },
            "changed": changed,
        }));
    }
    Ok(Page {
        items,
        next_before,
        head,
    })
}

/// A value as its column serves it: the same bind a write makes, then the same read a
/// stored cell takes, so a sealed value is opened and an erased one is `null` here too.
fn render(
    table: &TableDef,
    column: &ColumnDef,
    value: &Value,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Json> {
    let cell = cell_of(&table.name, column, value)?;
    rendered(&column.meta, ValueRef::from(&cell), keystore)
}
