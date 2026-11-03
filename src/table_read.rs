//! The stored folds, read: what `GET /rows`, `GET /row` and `GET /folds/{fold}` serve.
//!
//! The sibling of [`crate::read_api`] for [`crate::fold_store`]. **A reader names folds and
//! never a table.** Every fold is kept in exactly one table, so a list of folds that share
//! a key is enough to find their row, and the table stays a storage detail with a hashed
//! name. A key signature's main table is joined to its side table here, so a collection
//! fold is just another value of the row, and a sealed cell is opened on the way out.
//!
//! **Spoken in the first fold's vocabulary.** A request's key parameters are the first
//! listed fold's own parameter names, its values are keyed by fold name, and nothing a
//! caller sees is a name `layout` derived.
//!
//! The discipline is `read_api`'s: a scan filters on what an index serves and anything else
//! is a 400, never a table scan. What serves a filter is the table's primary key, the
//! indexes [`crate::tables`] derived for range arms, and the ones a fold's `@index`
//! declares. A filter is a *set* of pinned parameters, so which order the key happens to be
//! stored in is not something a caller has to know: the refusal names the sets that serve.

use std::iter;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::crypto::KeyStore;
use crate::fold_store::SealedCell;
use crate::heklang_host::{column_form, unsealed_json};
use crate::read_api::{RANGE_OPERATORS, Range};
use crate::read_model::{coerce_value, from_sql, quote_ident, reject_double_quoted_strings};
use crate::schema::{FieldKind, FieldMeta};
use crate::tables::{ColumnDef, SortDef, TableDef, main_of, side_of};

/// Query parameters a row read takes that are not a fold's parameter, which a fold's
/// parameter therefore may not be named.
pub const RESERVED: [&str; 8] = [
    "folds",
    "limit",
    "cursor",
    "after",
    "timeout_ms",
    "order_by",
    "before",
    "since",
];

/// The parameter that lists a row read's folds.
pub const FOLDS: &str = "folds";

/// How long a read waits on the single writer's lock before failing.
const BUSY: Duration = Duration::from_secs(5);

/// A request the store will not serve, which is a 400. `code` is what a client branches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn invalid(message: String) -> Refusal {
        Refusal {
            code: "invalid_input",
            message,
        }
    }

    fn unindexed(message: String) -> Refusal {
        Refusal {
            code: "unindexed_filter",
            message,
        }
    }
}

/// A table a key signature's rows are recorded in, which is a main table. A side table is
/// storage joined in, never a table of its own.
pub fn find<'a>(tables: &'a [TableDef], name: &str) -> Option<&'a TableDef> {
    readable(tables).find(|table| table.name == name)
}

/// The main tables, in `layout`'s order.
pub fn readable(tables: &[TableDef]) -> impl Iterator<Item = &TableDef> {
    tables.iter().filter(|table| table.source == table.name)
}

/// The folds a row read names, which share one key, and the main table that records it.
#[derive(Debug, Clone)]
pub struct Selection<'a> {
    pub main: &'a TableDef,
    /// In the order the caller listed them; never empty.
    pub folds: Vec<&'a ColumnDef>,
}

/// Why a fold list selects nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unselected {
    /// A fold no table keeps. Undeclared or unstored is the caller's to tell apart, since
    /// it needs the program and this needs only the tables.
    Missing(String),
    Refused(Refusal),
}

impl<'a> Selection<'a> {
    /// What the query string calls each key part, in key order: the first fold's own
    /// parameter names.
    pub fn params(&self) -> &'a [String] {
        &self.folds[0].args
    }

    /// The fold whose parameters a request speaks in.
    pub fn first(&self) -> &'a str {
        &self.folds[0].fold
    }

    /// The key part a parameter names.
    fn part(&self, param: &str) -> Option<&'a str> {
        let at = self.params().iter().position(|name| name == param)?;
        Some(self.main.key[at].0.as_str())
    }

    /// The parameter a key part is called.
    fn spoken(&self, part: &str) -> &'a str {
        let at = self
            .main
            .key
            .iter()
            .position(|(name, _)| name == part)
            .expect("a key part of the selection's table");
        &self.params()[at]
    }

    fn spoken_list<'b>(&self, parts: impl IntoIterator<Item = &'b str>) -> String {
        parts
            .into_iter()
            .map(|part| self.spoken(part))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The fold kept in a column of the main table.
    fn fold_of<'b>(&'b self, column: &'b str) -> &'b str {
        self.main
            .columns
            .iter()
            .find(|held| held.name == column)
            .map_or(column, |held| held.fold.as_str())
    }
}

/// Where a stored fold is kept: the main table of its key signature, and its own column,
/// which may be in the side table.
fn locate<'a>(tables: &'a [TableDef], fold: &str) -> Option<(&'a TableDef, &'a ColumnDef)> {
    let (table, column) = tables.iter().find_map(|table| {
        table
            .columns
            .iter()
            .find(|column| column.fold == fold)
            .map(|column| (table, column))
    })?;
    Some((main_of(tables, &table.source)?, column))
}

/// The folds named, checked to share one key.
pub fn pick<'a>(tables: &'a [TableDef], folds: &[&str]) -> Result<Selection<'a>, Unselected> {
    let Some((first, rest)) = folds.split_first() else {
        return Err(Unselected::Refused(Refusal::invalid(format!(
            "`{FOLDS}` names no fold; list the folds to read, comma separated"
        ))));
    };
    let (main, column) =
        locate(tables, first).ok_or_else(|| Unselected::Missing((*first).into()))?;
    let mut selection = Selection {
        main,
        folds: vec![column],
    };
    for fold in rest {
        let (held, column) =
            locate(tables, fold).ok_or_else(|| Unselected::Missing((*fold).into()))?;
        if selection.folds.iter().any(|kept| kept.fold == *fold) {
            return Err(Unselected::Refused(Refusal::invalid(format!(
                "`{fold}` is listed twice"
            ))));
        }
        if held.name != main.name {
            return Err(Unselected::Refused(Refusal::invalid(format!(
                "`{fold}` is not keyed like `{first}`, so the two have no row in common; \
                 read them separately"
            ))));
        }
        selection.folds.push(column);
    }
    Ok(selection)
}

/// A query string, as name and value pairs in the order given.
pub type Params = Vec<(String, String)>;

/// The `folds` parameter split off a query string: the folds it lists, and the rest.
pub fn split_folds(params: &[(String, String)]) -> Result<(Vec<String>, Params), Refusal> {
    let mut folds = None;
    let mut rest = Vec::new();
    for (name, value) in params {
        if name != FOLDS {
            rest.push((name.clone(), value.clone()));
            continue;
        }
        if folds.is_some() {
            return Err(Refusal::invalid(format!(
                "`{FOLDS}` is given twice; list every fold in one, comma separated"
            )));
        }
        folds = Some(
            value
                .split(',')
                .map(str::trim)
                .filter(|fold| !fold.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
    }
    let folds = folds.ok_or_else(|| {
        Refusal::invalid(format!(
            "`{FOLDS}` is required: name the folds to read, comma separated, e.g. \
             `?{FOLDS}=OrderTotal,OrderClosed`"
        ))
    })?;
    Ok((folds, rest))
}

/// What a scan was asked for, in the table's own part and column names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// Key parts pinned to a value, as the texts the store binds.
    pub equals: Vec<(String, String)>,
    /// At most one key part bounded at either end or both: the part right after the
    /// pinned ones, in the key or in an index, which is what keeps it a seek.
    pub range: Option<(String, Range)>,
    /// A column a fold's `@index` declares, to order by instead of the key.
    pub order: Option<Order>,
    /// Where the previous page ended.
    pub after: Option<After>,
    pub limit: usize,
}

/// An ordering by a declared sort column, ascending or descending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub column: String,
    pub descending: bool,
}

impl Order {
    /// What a cursor records, so it is not read under another ordering.
    fn token(&self) -> String {
        format!("{}{}", if self.descending { "-" } else { "" }, self.column)
    }
}

/// The last row of the previous page: its key, and its sort value when the scan is
/// ordered by one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct After {
    pub value: Option<String>,
    pub key: Vec<String>,
}

/// A page cursor. Carries its table so one taken over another key, whose parts compare
/// differently, is refused rather than read as a position in this one, and the ordering it
/// was taken under for the same reason.
#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    #[serde(rename = "t")]
    table: String,
    #[serde(rename = "k")]
    key: Vec<String>,
    #[serde(rename = "o", default, skip_serializing_if = "Option::is_none")]
    order: Option<String>,
    #[serde(rename = "v", default, skip_serializing_if = "Option::is_none")]
    value: Option<String>,
}

fn encode_cursor(table: &str, order: Option<&Order>, after: After) -> String {
    let cursor = Cursor {
        table: table.to_owned(),
        key: after.key,
        order: order.map(Order::token),
        value: after.value,
    };
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).expect("a cursor serializes"))
}

fn decode_cursor(sel: &Selection, raw: &str, order: Option<&Order>) -> Result<After, Refusal> {
    let table = sel.main;
    let bad = || Refusal::invalid("cursor is not valid".to_owned());
    // A one-row table never hands out a cursor, so one for it was not this server's.
    if table.key.is_empty() {
        return Err(bad());
    }
    let bytes = URL_SAFE_NO_PAD.decode(raw.as_bytes()).map_err(|_| bad())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    if cursor.table != table.name || cursor.key.len() != table.key.len() {
        return Err(Refusal::invalid(format!(
            "this cursor was taken over folds keyed otherwise than `{}`",
            sel.first()
        )));
    }
    let asked = order.map(Order::token);
    if cursor.order != asked {
        let spelled = |token: Option<&str>, unordered: &str| match token {
            None => unordered.to_owned(),
            Some(token) => {
                let (sign, column) = match token.strip_prefix('-') {
                    Some(column) => ("-", column),
                    None => ("", token),
                };
                format!("`order_by={sign}{}`", sel.fold_of(column))
            }
        };
        return Err(Refusal::invalid(format!(
            "this cursor was taken {} and this request asks for {}",
            match cursor.order.as_deref() {
                None => "in key order".to_owned(),
                token => format!("under {}", spelled(token, "")),
            },
            spelled(asked.as_deref(), "key order")
        )));
    }
    // Each part through the same check a filter takes, so a cursor somebody edited is a
    // 400 here rather than a bind that fails as a 500.
    let key = table
        .key
        .iter()
        .zip(&cursor.key)
        .map(|((part, _), value)| bound(table, part, part, value).map_err(|_| bad()))
        .collect::<Result<Vec<_>, _>>()?;
    let value = match (order, cursor.value) {
        (None, None) => None,
        (Some(order), Some(value)) => {
            let column = table
                .columns
                .iter()
                .find(|column| column.name == order.column)
                .ok_or_else(bad)?;
            coerce_value(&column.meta.kind, &value).map_err(|_| bad())?;
            Some(value)
        }
        _ => return Err(bad()),
    };
    Ok(After { value, key })
}

/// The parameters an `after` wait reads, which a scan and a row read both pass through
/// to the server rather than reading as key parts.
pub const WAIT: [&str; 2] = ["after", "timeout_ms"];

/// Read a scan off the query string, `folds` already taken off it, or say why it is not
/// one.
pub fn parse_scan(sel: &Selection, params: &[(String, String)]) -> Result<Scan, Refusal> {
    let mut scan = Scan {
        limit: crate::read_api::DEFAULT_LIMIT,
        ..Scan::default()
    };
    let mut cursor = None;
    let mut order_by = None;
    for (name, value) in params {
        match name.as_str() {
            "limit" => {
                scan.limit = value
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| *limit > 0)
                    .ok_or_else(|| Refusal::invalid("limit must be a positive integer".into()))?
                    .min(crate::read_api::MAX_LIMIT);
            }
            "cursor" => cursor = Some(value.as_str()),
            "order_by" => order_by = Some(value.as_str()),
            wait if WAIT.contains(&wait) => {}
            ranged if ranged.contains('.') => ranged_bound(sel, &mut scan, ranged, value)?,
            param => {
                let part = sel.part(param).ok_or_else(|| not_a_param(sel, param))?;
                if scan.equals.iter().any(|(given, _)| given == part) {
                    return Err(Refusal::invalid(format!("`{param}` is given twice")));
                }
                scan.equals
                    .push((part.to_owned(), bound(sel.main, part, param, value)?));
            }
        }
    }
    let ranged = scan.range.as_ref().map(|(part, _)| part.as_str());
    if let Some(part) = ranged
        && scan.equals.iter().any(|(given, _)| given == part)
    {
        return Err(Refusal::invalid(format!(
            "`{}` is pinned and bounded at once; filter it one way",
            sel.spoken(part)
        )));
    }
    if !servable(sel.main, &scan.equals, ranged) {
        return Err(Refusal::unindexed(unindexed(sel, &scan.equals, ranged)));
    }
    if let Some(raw) = order_by {
        scan.order = Some(ordered(sel, &scan, raw)?);
    }
    if let Some(raw) = cursor {
        scan.after = Some(decode_cursor(sel, raw, scan.order.as_ref())?);
    }
    Ok(scan)
}

fn not_a_param(sel: &Selection, param: &str) -> Refusal {
    Refusal::invalid(format!(
        "`{param}` is not a parameter of `{}`, which takes ({}); a read names the key in the \
         first listed fold's parameters",
        sel.first(),
        sel.params().join(", ")
    ))
}

/// An `order_by` against what the folds declare: a fold whose `@index` orders this key,
/// `-` before it to reverse, with exactly the parameters that lead that index pinned.
/// Anything else would be a sort over the rows the filter matched rather than a walk of one
/// index.
fn ordered(sel: &Selection, scan: &Scan, raw: &str) -> Result<Order, Refusal> {
    let (descending, fold) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let table = sel.main;
    let declared: Vec<&SortDef> = table
        .sorts
        .iter()
        .filter(|sort| sel.fold_of(&sort.column) == fold)
        .collect();
    if declared.is_empty() {
        let known: Vec<String> = table
            .sorts
            .iter()
            .map(|sort| spelled_sort(sel, sort))
            .collect();
        return Err(Refusal::invalid(if known.is_empty() {
            format!(
                "`{fold}` does not order rows keyed like `{}`, and nothing does; a fold \
                 orders its rows with `@index`",
                sel.first()
            )
        } else {
            format!(
                "`{fold}` does not order rows keyed like `{}`; they order by {}, each \
                 optionally prefixed with `-` to reverse it",
                sel.first(),
                known.join(", ")
            )
        }));
    }
    let pinned: Vec<&str> = scan.equals.iter().map(|(part, _)| part.as_str()).collect();
    let fits = |sort: &&SortDef| {
        sort.lead.len() == pinned.len()
            && sort.lead.iter().all(|part| pinned.contains(&part.as_str()))
    };
    if scan.range.is_some() || !declared.iter().any(fits) {
        let ways: Vec<String> = declared
            .iter()
            .map(|sort| spelled_sort(sel, sort))
            .collect();
        return Err(Refusal::unindexed(format!(
            "ordering by `{fold}` with ({}) pinned{} is not one index walk: {}. Pin exactly the \
             parameters its `@index` names, and no range.",
            sel.spoken_list(pinned.iter().copied()),
            if scan.range.is_some() {
                " and a range"
            } else {
                ""
            },
            ways.join(", ")
        )));
    }
    let column = &declared[0].column;
    Ok(Order {
        column: column.clone(),
        descending,
    })
}

fn spelled_sort(sel: &Selection, sort: &SortDef) -> String {
    let fold = sel.fold_of(&sort.column);
    if sort.lead.is_empty() {
        format!("`{fold}`")
    } else {
        format!(
            "`{fold}` with ({}) pinned",
            sel.spoken_list(sort.lead.iter().map(String::as_str))
        )
    }
}

/// One end of a range, `param.gte=` and its siblings, onto the scan's one ranged part.
fn ranged_bound(sel: &Selection, scan: &mut Scan, name: &str, value: &str) -> Result<(), Refusal> {
    let (param, op) = name.rsplit_once('.').expect("checked to hold a dot");
    let Some(&(_, lower, bound_kind)) = RANGE_OPERATORS.iter().find(|(word, ..)| *word == op)
    else {
        let words: Vec<&str> = RANGE_OPERATORS.iter().map(|(word, ..)| *word).collect();
        return Err(Refusal::invalid(format!(
            "`{name}` is not a filter: a range is `{param}.` followed by one of {}",
            words.join(", ")
        )));
    };
    let part = sel.part(param).ok_or_else(|| not_a_param(sel, param))?;
    let (_, meta) = sel
        .main
        .key
        .iter()
        .find(|(known, _)| known == part)
        .expect("a key part of the selection's table");
    if !meta.kind.base().is_comparable() {
        return Err(Refusal::invalid(format!(
            "`{param}` is {}, which has no order a range could use; filter it for equality \
             instead",
            meta.kind.describe()
        )));
    }
    let (given, range) = scan
        .range
        .get_or_insert_with(|| (part.to_owned(), Range::default()));
    if given != part {
        return Err(Refusal::invalid(format!(
            "a scan bounds one parameter, and `{}` is already bounded",
            sel.spoken(given)
        )));
    }
    let end = if lower {
        &mut range.lower
    } else {
        &mut range.upper
    };
    if end.is_some() {
        return Err(Refusal::invalid(format!(
            "`{param}` has two {} bounds",
            if lower { "lower" } else { "upper" }
        )));
    }
    *end = Some((bound_kind, bound(sel.main, part, param, value)?));
    Ok(())
}

/// Read a whole key off the query string, `folds` already taken off it: every parameter
/// of the first fold, once, and nothing else.
pub fn parse_key(sel: &Selection, params: &[(String, String)]) -> Result<Vec<String>, Refusal> {
    let table = sel.main;
    let names = sel.params();
    let owner = format!("`{}`", sel.first());
    let mut given: Vec<Option<String>> = vec![None; names.len()];
    for (name, value) in params {
        if WAIT.contains(&name.as_str()) {
            continue;
        }
        let Some(index) = names.iter().position(|known| known == name) else {
            return Err(Refusal::invalid(format!(
                "`{name}` is not one of {owner}'s parameters ({}), which are all it is named \
                 by",
                names.join(", ")
            )));
        };
        if given[index].is_some() {
            return Err(Refusal::invalid(format!("`{name}` is given twice")));
        }
        let (part, _) = &table.key[index];
        given[index] = Some(bound(table, part, name, value)?);
    }
    given
        .into_iter()
        .zip(names)
        .map(|(value, name)| {
            value.ok_or_else(|| {
                Refusal::invalid(format!(
                    "{owner} is named by all of ({}), and `{name}` is missing",
                    names.join(", ")
                ))
            })
        })
        .collect()
}

/// One key part's value, checked against its type, so a malformed value is a 400 rather
/// than a row that silently is not there. A timestamp is also respelled the way the store
/// holds it, since one instant has many RFC 3339 spellings; a UUID is not, since the store
/// holds it as the event spelled it and a case-folded compare could not use the key.
///
/// `named` is what the caller called the part, for the message.
fn bound(table: &TableDef, part: &str, named: &str, value: &str) -> Result<String, Refusal> {
    let (_, meta) = table
        .key
        .iter()
        .find(|(name, _)| name == part)
        .expect("checked to be a key part by the caller");
    let wrong = |what: &str| Refusal::invalid(format!("`{named}` must be {what}"));
    match meta.kind.base() {
        // Checked and kept as spelled: heklang keeps a UUID the way it arrived, so the
        // store holds whatever case the event carried.
        FieldKind::Uuid => Uuid::parse_str(value)
            .map(|_| value.to_owned())
            .map_err(|_| wrong("a UUID")),
        // The column form, which is what the store wrote: one instant has many RFC 3339
        // spellings, and a text comparison only finds the one.
        FieldKind::Timestamp => heklang::value::timestamp(value)
            .and_then(|micros| {
                column_form(&meta.kind, Value::from(micros))
                    .as_str()
                    .map(str::to_owned)
            })
            .ok_or_else(|| wrong("an RFC 3339 timestamp with an offset")),
        FieldKind::OneOf(variants) => {
            if variants.iter().any(|variant| variant == value) {
                Ok(value.to_owned())
            } else {
                Err(wrong(&format!("one of {}", variants.join(", "))))
            }
        }
        kind => coerce_value(kind, value)
            .map(|_| value.to_owned())
            .map_err(|why| Refusal::invalid(format!("`{named}`: {why}"))),
    }
}

/// The column orders a filter can lead: the key, then every index.
fn leads(table: &TableDef) -> impl Iterator<Item = Vec<&str>> {
    iter::once(table.key.iter().map(|(name, _)| name.as_str()).collect()).chain(
        table
            .indexes
            .iter()
            .map(|index| index.columns.iter().map(String::as_str).collect()),
    )
}

/// Whether the key or an index serves the filter: the pinned parts are a leading run of
/// it, in any order, and a ranged part, if any, is the column right after them.
fn servable(table: &TableDef, equals: &[(String, String)], ranged: Option<&str>) -> bool {
    let asked: Vec<&str> = equals.iter().map(|(name, _)| name.as_str()).collect();
    leads(table).any(|columns| {
        asked.len() <= columns.len()
            && columns[..asked.len()].iter().all(|c| asked.contains(c))
            && ranged.is_none_or(|part| columns.get(asked.len()) == Some(&part))
    })
}

/// Why a filter is refused, naming what would be served instead as sets of parameters: a
/// caller pins a set, and which order the store keeps it in is not theirs to know.
fn unindexed(sel: &Selection, equals: &[(String, String)], ranged: Option<&str>) -> String {
    let table = sel.main;
    let is_part = |column: &&str| table.key.iter().any(|(part, _)| part == column);
    let asked = sel.spoken_list(equals.iter().map(|(name, _)| name.as_str()));
    // Every leading run of key parts, as a sorted set so two orders of one set are one way.
    let mut sets: Vec<(Vec<&str>, Option<&str>)> = Vec::new();
    for columns in leads(table) {
        let parts: Vec<&str> = columns.iter().copied().take_while(is_part).collect();
        // Pinning nothing is a scan in key order, not a fix for a filter, so an equality
        // refusal starts at one part; a range may still lead a key with nothing pinned.
        let shortest = usize::from(ranged.is_none());
        for len in shortest..=parts.len() {
            let mut set: Vec<&str> = parts[..len].iter().map(|part| sel.spoken(part)).collect();
            set.sort_unstable();
            let next = match ranged {
                Some(_) => match parts.get(len) {
                    Some(part) => Some(sel.spoken(part)),
                    None => continue,
                },
                None => None,
            };
            if !sets.contains(&(set.clone(), next)) {
                sets.push((set, next));
            }
        }
    }
    let ways: Vec<String> = sets
        .iter()
        .map(|(set, next)| match next {
            Some(next) => format!("`{next}` with ({}) pinned", set.join(", ")),
            None => format!("({})", set.join(", ")),
        })
        .collect();
    match ranged {
        Some(part) => format!(
            "a range on `{}` with ({asked}) pinned is not one index seek for folds keyed like \
             `{}`; one of these is: {}",
            sel.spoken(part),
            sel.first(),
            ways.join(", ")
        ),
        None => format!(
            "pinning ({asked}) is not one index seek for folds keyed like `{}`; pin one of \
             these sets instead: {}",
            sel.first(),
            ways.join(", ")
        ),
    }
}

/// One page of rows, plus the position every row of it was read at.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
    pub position: u64,
}

/// One fold's value for one key, plus the store's position, in one read snapshot.
///
/// `None` for a key no event has named, whose value would be the fold's seed. `Some(Null)`
/// for a fold answering `none`, or a sealed one whose subject is gone.
pub fn get_fold(
    path: &Path,
    tables: &[TableDef],
    sel: &Selection,
    key: &[String],
    keystore: Option<&KeyStore>,
) -> anyhow::Result<(Option<Value>, u64)> {
    let (row, position) = get_one(path, tables, sel, key, keystore)?;
    Ok((
        row.map(|row| row.get(sel.first()).cloned().unwrap_or(Value::Null)),
        position,
    ))
}

/// Read one row by its whole key, plus the store's position, in one read snapshot.
pub fn get_one(
    path: &Path,
    tables: &[TableDef],
    sel: &Selection,
    key: &[String],
    keystore: Option<&KeyStore>,
) -> anyhow::Result<(Option<Value>, u64)> {
    let mut conn = open(path)?;
    let tx = conn.transaction()?;
    let position = checkpoint(&tx)?;
    let equals: Vec<(String, String)> = sel
        .main
        .key
        .iter()
        .map(|(name, _)| name.clone())
        .zip(key.iter().cloned())
        .collect();
    let scan = Scan {
        equals,
        limit: 1,
        ..Scan::default()
    };
    let mut rows = select(&tx, tables, sel, &scan, keystore)?;
    Ok((rows.pop().map(|(row, _)| row), position))
}

/// A page of rows in key order, filtered and resumed, in one read snapshot.
pub fn scan(
    path: &Path,
    tables: &[TableDef],
    sel: &Selection,
    scan: &Scan,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Page> {
    let mut conn = open(path)?;
    let tx = conn.transaction()?;
    let position = checkpoint(&tx)?;
    // One more than asked, to learn whether another page follows.
    let probe = Scan {
        limit: scan.limit + 1,
        ..scan.clone()
    };
    let mut rows = select(&tx, tables, sel, &probe, keystore)?;
    let next_cursor = if rows.len() > scan.limit {
        rows.truncate(scan.limit);
        rows.last()
            .map(|(_, after)| encode_cursor(&sel.main.name, scan.order.as_ref(), after.clone()))
    } else {
        None
    };
    Ok(Page {
        items: rows.into_iter().map(|(row, _)| row).collect(),
        next_cursor,
        position,
    })
}

/// How many rows a table holds, which is how many keys any event has named.
pub fn count(path: &Path, table: &TableDef) -> anyhow::Result<u64> {
    let conn = open(path)?;
    let rows: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {}", quote_ident(&table.name)),
        [],
        |row| row.get(0),
    )?;
    Ok(rows as u64)
}

fn open(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening the derived-table database")?;
    conn.busy_timeout(BUSY)?;
    reject_double_quoted_strings(&conn)?;
    conn.pragma_update(None, "query_only", "ON")
        .context("enabling query_only")?;
    Ok(conn)
}

fn checkpoint(tx: &Transaction) -> anyhow::Result<u64> {
    let head: i64 = tx
        .query_row(
            "SELECT position FROM _hekla_checkpoint WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .context("reading the derived-table checkpoint")?;
    Ok(head as u64)
}

/// What one output column is: a key part, or a fold's column from the main or side table.
struct Out<'a> {
    alias: &'static str,
    /// The stored column.
    column: &'a str,
    /// What the reader gets it as: the parameter for a key part, the fold for a value.
    /// `None` for a sort column read only for the cursor.
    shown: Option<&'a str>,
    meta: &'a FieldMeta,
    key: bool,
    /// The seed a side column reads as when its key has no side row yet.
    seed: Option<&'a str>,
}

/// The rows a scan asks for, each with its key as the texts a cursor carries.
fn select(
    tx: &Transaction,
    tables: &[TableDef],
    sel: &Selection,
    scan: &Scan,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Vec<(Value, After)>> {
    let table = sel.main;
    let side = side_of(tables, table);
    let mut outs: Vec<Out> = table
        .key
        .iter()
        .zip(sel.params())
        .map(|((name, meta), param)| Out {
            alias: "m",
            column: name,
            shown: Some(param),
            meta,
            key: true,
            seed: None,
        })
        .collect();
    let mut joins_side = false;
    for fold in &sel.folds {
        let in_main = table.columns.iter().any(|held| held.name == fold.name);
        joins_side |= !in_main;
        outs.push(Out {
            alias: if in_main { "m" } else { "s" },
            column: &fold.name,
            shown: Some(&fold.fold),
            meta: &fold.meta,
            key: false,
            seed: if in_main {
                None
            } else {
                fold.default.as_deref()
            },
        });
    }
    let sorted = match &scan.order {
        Some(order) => {
            let column = table
                .columns
                .iter()
                .find(|column| column.name == order.column)
                .with_context(|| format!("no column `{}`", order.column))?;
            if !sel.folds.iter().any(|fold| fold.name == column.name) {
                outs.push(Out {
                    alias: "m",
                    column: &column.name,
                    shown: None,
                    meta: &column.meta,
                    key: false,
                    seed: None,
                });
            }
            Some((order, &column.meta))
        }
        None => None,
    };
    let selected: Vec<String> = outs
        .iter()
        .map(|out| {
            let cell = format!("{}.{}", out.alias, quote_ident(out.column));
            // A key a collection was never written for has no side row, and the left join
            // reads that as NULL; the fold's answer for it is the seed, as it is for a
            // main-table column the `DEFAULT` filled.
            match out.seed {
                Some(seed) => format!("COALESCE({cell}, {seed})"),
                None => cell,
            }
        })
        .collect();

    // A keyless table joins on its guard column, which is the one key it has.
    let joined: Vec<String> = if table.key.is_empty() {
        vec!["\"_one\"".to_owned()]
    } else {
        table
            .key
            .iter()
            .map(|(name, _)| quote_ident(name))
            .collect()
    };
    let mut sql = format!(
        "SELECT {} FROM {} AS m",
        selected.join(", "),
        quote_ident(&table.name)
    );
    if joins_side && let Some(side) = side {
        let on: Vec<String> = joined
            .iter()
            .map(|column| format!("m.{column} = s.{column}"))
            .collect();
        sql.push_str(&format!(
            " LEFT JOIN {} AS s ON {}",
            quote_ident(&side.name),
            on.join(" AND ")
        ));
    }

    let mut wheres: Vec<String> = Vec::new();
    let mut binds: Vec<SqlValue> = Vec::new();
    for (name, text) in &scan.equals {
        let (_, meta) = table
            .key
            .iter()
            .find(|(part, _)| part == name)
            .with_context(|| format!("no key part `{name}`"))?;
        binds.push(coerce_value(&meta.kind, text)?);
        wheres.push(format!("m.{} = ?{}", quote_ident(name), binds.len()));
    }
    if let Some((name, range)) = &scan.range {
        let (_, meta) = table
            .key
            .iter()
            .find(|(part, _)| part == name)
            .with_context(|| format!("no key part `{name}`"))?;
        for (lower, end) in [(true, &range.lower), (false, &range.upper)] {
            if let Some((bound, text)) = end {
                binds.push(coerce_value(&meta.kind, text)?);
                wheres.push(format!(
                    "m.{} {} ?{}",
                    quote_ident(name),
                    bound.operator(lower),
                    binds.len()
                ));
            }
        }
    }
    // An ordering by a declared column: its absent rows are left out, since a row-value
    // comparison against NULL matches nothing and a page boundary would drop them, and
    // the key breaks ties so every row has one place in the order.
    if let Some((order, _)) = sorted {
        wheres.push(format!("m.{} IS NOT NULL", quote_ident(&order.column)));
    }
    if let Some(after) = &scan.after {
        // A row value comparison, so a page resumes after a whole key in key order rather
        // than after its first part.
        let mut holders = Vec::new();
        let mut columns = Vec::new();
        if let (Some((order, meta)), Some(value)) = (sorted, &after.value) {
            binds.push(coerce_value(&meta.kind, value)?);
            holders.push(format!("?{}", binds.len()));
            columns.push(format!("m.{}", quote_ident(&order.column)));
        }
        for ((name, meta), text) in table.key.iter().zip(&after.key) {
            binds.push(coerce_value(&meta.kind, text)?);
            holders.push(format!("?{}", binds.len()));
            columns.push(format!("m.{}", quote_ident(name)));
        }
        let descending = sorted.is_some_and(|(order, _)| order.descending);
        wheres.push(format!(
            "({}) {} ({})",
            columns.join(", "),
            if descending { "<" } else { ">" },
            holders.join(", ")
        ));
    }
    if !wheres.is_empty() {
        sql.push_str(&format!(" WHERE {}", wheres.join(" AND ")));
    }
    let direction = if sorted.is_some_and(|(order, _)| order.descending) {
        " DESC"
    } else {
        ""
    };
    let mut order: Vec<String> = Vec::new();
    if let Some((sort, _)) = sorted {
        order.push(format!("m.{}{direction}", quote_ident(&sort.column)));
    }
    order.extend(joined.iter().map(|column| format!("m.{column}{direction}")));
    sql.push_str(&format!(
        " ORDER BY {} LIMIT {}",
        order.join(", "),
        scan.limit
    ));

    let mut stmt = tx.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(binds))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut item = Map::new();
        let mut after = After {
            value: None,
            key: Vec::new(),
        };
        for (at, column) in outs.iter().enumerate() {
            let raw = row.get_ref(at)?;
            if column.key {
                after.key.push(key_text(raw));
            } else if sorted.is_some_and(|(sort, _)| sort.column == column.column) {
                after.value = Some(key_text(raw));
            }
            let Some(shown) = column.shown else {
                continue;
            };
            let value = rendered(column.meta, raw, keystore)
                .with_context(|| format!("reading `{shown}`"))?;
            // Absent rather than `null`, as a projector's row serves it: an optional fold
            // answering `none` and an erased seal read the same, which is the point.
            if !value.is_null() {
                item.insert(shown.to_owned(), value);
            }
        }
        out.push((Value::Object(item), after));
    }
    Ok(out)
}

/// A key part as the text a cursor carries and [`coerce_value`] binds back.
fn key_text(raw: ValueRef) -> String {
    match raw {
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(n) => n.to_string(),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            String::from_utf8_lossy(bytes).into_owned()
        }
        ValueRef::Null => String::new(),
    }
}

/// One cell as a reader gets it: the column form a projector's row is served in, and a
/// sealed cell opened.
///
/// **An unreadable seal reads as absent**, which is what an erased subject reads as
/// everywhere: the key is gone, or the ciphertext will not open under it. Only a key that
/// cannot be reached at all, a missing master, is an error, so a misconfigured deployment
/// surfaces loudly instead of blanking every personal column it has.
pub(crate) fn rendered(
    meta: &FieldMeta,
    raw: ValueRef,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Value> {
    let json = from_sql(meta, raw)?;
    if meta.sealed_under.is_none() {
        return Ok(json);
    }
    let Some(text) = json.as_str() else {
        return Ok(Value::Null);
    };
    let cell = SealedCell::parse(text)?;
    let keystore = keystore.with_context(|| {
        format!(
            "a column sealed under `{}` cannot be opened without a master key",
            cell.subject
        )
    })?;
    Ok(
        match keystore.decrypt_subject(&cell.subject, &cell.id, &cell.field, &cell.sealed)? {
            Some(plaintext) => column_form(&meta.kind, unsealed_json(&meta.kind, plaintext)),
            None => Value::Null,
        },
    )
}
