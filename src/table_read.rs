//! The derived tables, read: what `GET /tables/{table}` and `GET /tables/{table}/row`
//! serve.
//!
//! The sibling of [`crate::read_api`] for [`crate::fold_store`]. **A reader sees one row per
//! key whatever the storage split**: a key signature's main table is joined to its side
//! table here, so a collection column is just another column of the row, and a sealed cell
//! is opened on the way out.
//!
//! The discipline is `read_api`'s: a scan filters on what an index serves and anything else
//! is a 400, never a table scan. A derived table has no declared indexes yet, so what serves
//! a filter is its primary key, whose leading parts are a prefix, and the indexes
//! [`crate::tables`] derived for range arms, which are the other way in that something
//! already needed.

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
use crate::tables::{ColumnDef, TableDef, main_of, side_of};

/// Query parameters that are not a key part.
pub const RESERVED: [&str; 4] = ["limit", "cursor", "after", "timeout_ms"];

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
}

/// A table a reader can name, which is a main table. A side table is storage, joined in
/// rather than addressed, so `_side_order` is not a route.
pub fn find<'a>(tables: &'a [TableDef], name: &str) -> Option<&'a TableDef> {
    tables
        .iter()
        .find(|table| table.name == name && table.source == table.name)
}

/// The tables a reader can name, in `layout`'s order.
pub fn readable(tables: &[TableDef]) -> impl Iterator<Item = &TableDef> {
    tables.iter().filter(|table| table.source == table.name)
}

/// What a scan was asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// Key parts pinned to a value, as the texts the store binds.
    pub equals: Vec<(String, String)>,
    /// At most one key part bounded at either end or both: the part right after the
    /// pinned ones, in the key or in an index, which is what keeps it a seek.
    pub range: Option<(String, Range)>,
    /// The key of the last row of the previous page.
    pub after: Option<Vec<String>>,
    pub limit: usize,
}

/// A page cursor. Carries its table so one taken over another table, whose key compares
/// different columns, is refused rather than read as a position in this one.
#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    #[serde(rename = "t")]
    table: String,
    #[serde(rename = "k")]
    key: Vec<String>,
}

fn encode_cursor(table: &str, key: Vec<String>) -> String {
    let cursor = Cursor {
        table: table.to_owned(),
        key,
    };
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).expect("a cursor serializes"))
}

fn decode_cursor(table: &TableDef, raw: &str) -> Result<Vec<String>, Refusal> {
    let bad = || Refusal::invalid("cursor is not valid".to_owned());
    // A one-row table never hands out a cursor, so one for it was not this server's.
    if table.key.is_empty() {
        return Err(bad());
    }
    let bytes = URL_SAFE_NO_PAD.decode(raw.as_bytes()).map_err(|_| bad())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    if cursor.table != table.name || cursor.key.len() != table.key.len() {
        return Err(Refusal::invalid(format!(
            "this cursor was taken over another table than `{}`",
            table.name
        )));
    }
    // Each part through the same check a filter takes, so a cursor somebody edited is a
    // 400 here rather than a bind that fails as a 500.
    table
        .key
        .iter()
        .zip(&cursor.key)
        .map(|((part, _), value)| bound(table, part, value).map_err(|_| bad()))
        .collect()
}

/// The parameters an `after` wait reads, which a scan and a row read both pass through
/// to the server rather than reading as key parts.
pub const WAIT: [&str; 2] = ["after", "timeout_ms"];

/// Read a scan off the query string, or say why it is not one.
pub fn parse_scan(table: &TableDef, params: &[(String, String)]) -> Result<Scan, Refusal> {
    let mut scan = Scan {
        limit: crate::read_api::DEFAULT_LIMIT,
        ..Scan::default()
    };
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
            "cursor" => scan.after = Some(decode_cursor(table, value)?),
            wait if WAIT.contains(&wait) => {}
            ranged if ranged.contains('.') => ranged_bound(table, &mut scan, ranged, value)?,
            part => {
                if scan.equals.iter().any(|(given, _)| given == part) {
                    return Err(Refusal::invalid(format!("`{part}` is given twice")));
                }
                if !table.key.iter().any(|(name, _)| name == part) {
                    return Err(Refusal {
                        code: "unindexed_filter",
                        message: format!(
                            "`{part}` is not a key part of `{}`; its key is ({}), and a \
                             derived table filters on its key",
                            table.name,
                            key_names(table)
                        ),
                    });
                }
                scan.equals
                    .push((part.to_owned(), bound(table, part, value)?));
            }
        }
    }
    let ranged = scan.range.as_ref().map(|(part, _)| part.as_str());
    if ranged.is_some_and(|part| scan.equals.iter().any(|(given, _)| given == part)) {
        return Err(Refusal::invalid(format!(
            "`{}` is pinned and bounded at once; filter it one way",
            ranged.unwrap_or_default()
        )));
    }
    if !servable(table, &scan.equals, ranged) {
        return Err(Refusal {
            code: "unindexed_filter",
            message: unindexed(table, &scan.equals, ranged),
        });
    }
    Ok(scan)
}

/// One end of a range, `part.gte=` and its siblings, onto the scan's one ranged part.
fn ranged_bound(table: &TableDef, scan: &mut Scan, name: &str, value: &str) -> Result<(), Refusal> {
    let (part, op) = name.rsplit_once('.').expect("checked to hold a dot");
    let Some(&(_, lower, bound_kind)) = RANGE_OPERATORS.iter().find(|(word, ..)| *word == op)
    else {
        let words: Vec<&str> = RANGE_OPERATORS.iter().map(|(word, ..)| *word).collect();
        return Err(Refusal::invalid(format!(
            "`{name}` is not a filter: a range is `{part}.` followed by one of {}",
            words.join(", ")
        )));
    };
    let Some((_, meta)) = table.key.iter().find(|(known, _)| known == part) else {
        return Err(Refusal {
            code: "unindexed_filter",
            message: format!(
                "`{part}` is not a key part of `{}`; its key is ({}), and a derived table \
                 filters on its key",
                table.name,
                key_names(table)
            ),
        });
    };
    if !meta.kind.base().is_comparable() {
        return Err(Refusal::invalid(format!(
            "`{part}` is {}, which has no order a range could use; filter it for equality \
             instead",
            meta.kind.describe()
        )));
    }
    let (given, range) = scan
        .range
        .get_or_insert_with(|| (part.to_owned(), Range::default()));
    if given != part {
        return Err(Refusal::invalid(format!(
            "a scan bounds one key part, and `{given}` is already bounded"
        )));
    }
    let end = if lower {
        &mut range.lower
    } else {
        &mut range.upper
    };
    if end.is_some() {
        return Err(Refusal::invalid(format!(
            "`{part}` has two {} bounds",
            if lower { "lower" } else { "upper" }
        )));
    }
    *end = Some((bound_kind, bound(table, part, value)?));
    Ok(())
}

/// Read a whole key off the query string: every key part, once, and nothing else.
pub fn parse_key(table: &TableDef, params: &[(String, String)]) -> Result<Vec<String>, Refusal> {
    let parts: Vec<&str> = table.key.iter().map(|(part, _)| part.as_str()).collect();
    parse_whole(table, &parts, &format!("a row of `{}`", table.name), params)
}

/// Read a whole key under the names a caller gives it: `names[i]` is what the query string
/// calls `table.key[i]`, which is the part's own name for a row read and the fold's
/// parameter name for a fold read. `owner` is what the names belong to, for a message.
fn parse_whole(
    table: &TableDef,
    names: &[&str],
    owner: &str,
    params: &[(String, String)],
) -> Result<Vec<String>, Refusal> {
    let mut given: Vec<Option<String>> = vec![None; names.len()];
    for (name, value) in params {
        if WAIT.contains(&name.as_str()) {
            continue;
        }
        let Some(index) = names.iter().position(|known| known == name) else {
            return Err(Refusal::invalid(format!(
                "`{name}` is not one of {owner}'s ({}), which is all it is named by",
                names.join(", ")
            )));
        };
        if given[index].is_some() {
            return Err(Refusal::invalid(format!("`{name}` is given twice")));
        }
        let (part, _) = &table.key[index];
        let text = bound(table, part, value).map_err(|refusal| Refusal {
            // The part's name means nothing to a caller who named the parameter.
            message: refusal
                .message
                .replace(&format!("`{part}`"), &format!("`{name}`")),
            ..refusal
        })?;
        given[index] = Some(text);
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
fn bound(table: &TableDef, part: &str, value: &str) -> Result<String, Refusal> {
    let (_, meta) = table
        .key
        .iter()
        .find(|(name, _)| name == part)
        .expect("checked to be a key part by the caller");
    let wrong = |what: &str| Refusal::invalid(format!("`{part}` must be {what}"));
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
            .map_err(|why| Refusal::invalid(format!("`{part}`: {why}"))),
    }
}

/// Whether the key or an index serves the filter: the pinned parts are a leading run of
/// it, and a ranged part, if any, is the column right after them.
fn servable(table: &TableDef, equals: &[(String, String)], ranged: Option<&str>) -> bool {
    let asked: Vec<&str> = equals.iter().map(|(name, _)| name.as_str()).collect();
    let leading = |columns: Vec<&str>| {
        asked.len() <= columns.len()
            && columns[..asked.len()].iter().all(|c| asked.contains(c))
            && ranged.is_none_or(|part| columns.get(asked.len()) == Some(&part))
    };
    leading(table.key.iter().map(|(name, _)| name.as_str()).collect())
        || table
            .indexes
            .iter()
            .any(|index| leading(index.columns.iter().map(String::as_str).collect()))
}

fn unindexed(table: &TableDef, equals: &[(String, String)], ranged: Option<&str>) -> String {
    let asked: Vec<&str> = equals.iter().map(|(name, _)| name.as_str()).collect();
    if let Some(part) = ranged {
        let mut ways = vec![format!("the key ({})", key_names(table))];
        ways.extend(
            table
                .indexes
                .iter()
                .map(|index| format!("index ({})", index.columns.join(", "))),
        );
        return format!(
            "a range on `{part}` after ({}) is not served by any index of `{}`: {}. Pin a \
             leading run of one of them and range the column right after it.",
            asked.join(", "),
            table.name,
            ways.join(", ")
        );
    }
    let mut ways = vec![format!("the key ({})", key_names(table))];
    ways.extend(
        table
            .indexes
            .iter()
            .map(|index| format!("index ({})", index.columns.join(", "))),
    );
    format!(
        "filter on ({}) is not a leading run of any index of `{}`: {}. Filter on the first \
         parts of one of them.",
        asked.join(", "),
        table.name,
        ways.join(", ")
    )
}

fn key_names(table: &TableDef) -> String {
    table
        .key
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where a named fold is kept: the main table of its key signature, which is what its key
/// is read against, and its own column, which may be in the side table.
#[derive(Debug, Clone, Copy)]
pub struct FoldAt<'a> {
    pub main: &'a TableDef,
    pub column: &'a ColumnDef,
}

/// The stored fold named `fold`, or `None` for one nothing keeps: undeclared, or with a
/// parameter no arm matches. Telling those apart is the caller's, since it needs the program
/// and this needs only the tables.
pub fn find_fold<'a>(tables: &'a [TableDef], fold: &str) -> Option<FoldAt<'a>> {
    let (table, column) = tables.iter().find_map(|table| {
        table
            .columns
            .iter()
            .find(|column| column.fold == fold)
            .map(|column| (table, column))
    })?;
    Some(FoldAt {
        main: main_of(tables, &table.source)?,
        column,
    })
}

/// Read a fold's arguments off the query string, as the key texts the store binds.
///
/// **Named as the fold declares its parameters**, `customer_id` and not the key part
/// `customer` the table spells it as: a caller asking for a fold by name has its signature
/// in front of them and nothing else. `ColumnDef::args` is that mapping, in key order.
pub fn parse_args(at: FoldAt, params: &[(String, String)]) -> Result<Vec<String>, Refusal> {
    let args: Vec<&str> = at.column.args.iter().map(String::as_str).collect();
    parse_whole(at.main, &args, &format!("`{}`", at.column.fold), params)
}

/// One fold's value for one key, plus the store's position, in one read snapshot.
///
/// `None` for a key no event has named, whose value would be the fold's seed. `Some(Null)`
/// for a fold answering `none`, or a sealed one whose subject is gone.
pub fn get_fold(
    path: &Path,
    tables: &[TableDef],
    at: FoldAt,
    key: &[String],
    keystore: Option<&KeyStore>,
) -> anyhow::Result<(Option<Value>, u64)> {
    let (row, position) = get_one(path, tables, at.main, key, keystore)?;
    Ok((
        row.map(|row| row.get(&at.column.name).cloned().unwrap_or(Value::Null)),
        position,
    ))
}

/// One page of a table, plus the position every row of it was read at.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
    pub position: u64,
}

/// Read one row by its whole key, plus the store's position, in one read snapshot.
pub fn get_one(
    path: &Path,
    tables: &[TableDef],
    table: &TableDef,
    key: &[String],
    keystore: Option<&KeyStore>,
) -> anyhow::Result<(Option<Value>, u64)> {
    let mut conn = open(path)?;
    let tx = conn.transaction()?;
    let position = checkpoint(&tx)?;
    let equals: Vec<(String, String)> = table
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
    let mut rows = select(&tx, tables, table, &scan, keystore)?;
    Ok((rows.pop().map(|(row, _)| row), position))
}

/// A page of a table in key order, filtered and resumed, in one read snapshot.
pub fn scan(
    path: &Path,
    tables: &[TableDef],
    table: &TableDef,
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
    let mut rows = select(&tx, tables, table, &probe, keystore)?;
    let next_cursor = if rows.len() > scan.limit {
        rows.truncate(scan.limit);
        rows.last()
            .map(|(_, key)| encode_cursor(&table.name, key.clone()))
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
    name: &'a str,
    meta: &'a FieldMeta,
    key: bool,
    /// The seed a side column reads as when its key has no side row yet.
    seed: Option<&'a str>,
}

/// The rows a scan asks for, each with its key as the texts a cursor carries.
fn select(
    tx: &Transaction,
    tables: &[TableDef],
    table: &TableDef,
    scan: &Scan,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Vec<(Value, Vec<String>)>> {
    let side = side_of(tables, table);
    let mut outs: Vec<Out> = table
        .key
        .iter()
        .map(|(name, meta)| Out {
            alias: "m",
            name,
            meta,
            key: true,
            seed: None,
        })
        .collect();
    outs.extend(table.columns.iter().map(|column| Out {
        alias: "m",
        name: &column.name,
        meta: &column.meta,
        key: false,
        seed: None,
    }));
    if let Some(side) = side {
        outs.extend(side.columns.iter().map(|column| Out {
            alias: "s",
            name: &column.name,
            meta: &column.meta,
            key: false,
            seed: column.default.as_deref(),
        }));
    }
    let selected: Vec<String> = outs
        .iter()
        .map(|out| {
            let cell = format!("{}.{}", out.alias, quote_ident(out.name));
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
        if selected.is_empty() {
            "1".to_owned()
        } else {
            selected.join(", ")
        },
        quote_ident(&table.name)
    );
    if let Some(side) = side {
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
    if let Some(after) = &scan.after {
        // A row value comparison, so a page resumes after a whole key in key order rather
        // than after its first part.
        let mut holders = Vec::new();
        for ((_, meta), text) in table.key.iter().zip(after) {
            binds.push(coerce_value(&meta.kind, text)?);
            holders.push(format!("?{}", binds.len()));
        }
        let columns: Vec<String> = table
            .key
            .iter()
            .map(|(name, _)| format!("m.{}", quote_ident(name)))
            .collect();
        wheres.push(format!(
            "({}) > ({})",
            columns.join(", "),
            holders.join(", ")
        ));
    }
    if !wheres.is_empty() {
        sql.push_str(&format!(" WHERE {}", wheres.join(" AND ")));
    }
    let order: Vec<String> = joined.iter().map(|column| format!("m.{column}")).collect();
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
        let mut key = Vec::new();
        for (at, column) in outs.iter().enumerate() {
            let raw = row.get_ref(at)?;
            if column.key {
                key.push(key_text(raw));
            }
            let value = rendered(column.meta, raw, keystore)
                .with_context(|| format!("reading `{}`.`{}`", table.name, column.name))?;
            // Absent rather than `null`, as a projector's row serves it: an optional fold
            // answering `none` and an erased seal read the same, which is the point.
            if !value.is_null() {
                item.insert(column.name.to_owned(), value);
            }
        }
        out.push((Value::Object(item), key));
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
fn rendered(meta: &FieldMeta, raw: ValueRef, keystore: Option<&KeyStore>) -> anyhow::Result<Value> {
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
