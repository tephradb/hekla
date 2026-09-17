//! The generated read API over projector read models.
//!
//! Reads open a fresh read-only connection to the projector's database per
//! request (WAL lets them run concurrently with the projector's single writer)
//! and read the projector's log position in the same snapshot as the rows, so a
//! response's `position` is consistent with its data. Filters and orderings are both
//! restricted to declared indexes, and pagination is by an opaque cursor over the
//! ordering's own tuple, never an offset.

use std::iter;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::crypto::{KeyStore, RowDecryptor};
use crate::read_model::{ReadModel, coerce_value};
use crate::schema::{EntityDef, FieldKind, IndexDef, scalar_to_string};

/// Default page size for a scan when the request does not set `limit`.
pub const DEFAULT_LIMIT: usize = 50;
/// Largest page a scan will return; a larger `limit` is clamped to this.
pub const MAX_LIMIT: usize = 500;

/// Query params the read endpoints consume as controls (pagination plus the
/// read-your-writes wait), never as an indexed filter. The single source of truth
/// for both the scan handler (which must not treat one as a filter) and `hekla
/// check` (which rejects an entity field that would collide with one). Keep in sync
/// with the keys the read handlers read off the query string.
pub const RESERVED_QUERY_PARAMS: [&str; 5] = ["limit", "cursor", "after", "timeout_ms", "order_by"];

/// One page of a scan: the rows, the cursor to resume after them (absent at the
/// end), and the projector's log position at read time.
pub struct Page {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
    pub position: u64,
}

/// The entity named `name` in a projector's declared set.
pub fn find_entity<'a>(entities: &'a [EntityDef], name: &str) -> Option<&'a EntityDef> {
    entities.iter().find(|entity| entity.name == name)
}

/// Every field a scan may mention in a filter, in declaration order: the primary key,
/// then every column of every declared index.
///
/// Naming a column here does not mean it can be filtered *alone*. A filter has to be a
/// prefix of one index (see [`choose_index`]), so the second column of `index (a, b)` is
/// reachable only alongside the first. This is the set a query parameter may be named
/// after, which is what the OpenAPI generator and the reserved-param gate both need;
/// which *combinations* are admissible is `choose_index`'s question.
///
/// May repeat a name, when two indexes share a column or one contains the key. A caller
/// that turns each into something name-addressed (an OpenAPI query parameter) has to
/// deduplicate, and under a prefix rule that is load-bearing rather than tidiness: every
/// index contributes all of its columns. A caller asking a membership question does not,
/// and leaving it lazy keeps [`is_filterable`] allocation-free on the read path.
pub fn filterable_fields(entity: &EntityDef) -> impl Iterator<Item = &str> {
    iter::once(entity.key.as_str()).chain(
        entity
            .indexes
            .iter()
            .flat_map(|index| index.columns.iter())
            .map(String::as_str),
    )
}

/// Whether `field` may appear in a filter at all: the primary key, or a column of some
/// declared index. A column no index covers would be a table scan, which the read API
/// refuses; the caller returns a 400 telling the author to declare the index.
///
/// Shares [`filterable_fields`] so the runtime's 400 and the parameters the OpenAPI
/// generator documents cannot drift apart. Being filterable is necessary and not
/// sufficient: [`choose_index`] decides whether the combination asked for is servable.
pub fn is_filterable(entity: &EntityDef, field: &str) -> bool {
    filterable_fields(entity).any(|name| name == field)
}

/// One end of a range, and whether it includes its own value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Inclusive,
    Exclusive,
}

impl Bound {
    /// The SQL comparison for this bound at the given end. `lower` picks `>`/`>=` over
    /// `<`/`<=`.
    pub(crate) fn operator(self, lower: bool) -> &'static str {
        match (lower, self) {
            (true, Bound::Inclusive) => ">=",
            (true, Bound::Exclusive) => ">",
            (false, Bound::Inclusive) => "<=",
            (false, Bound::Exclusive) => "<",
        }
    }
}

/// The suffixes that spell a range bound in a query string, with the end and bound each
/// one means. A declared field can never carry a `.`, because `is_sql_identifier`
/// restricts a name to ascii letters, digits and underscores, so none of these can
/// collide with a column and none of them has to be reserved.
pub const RANGE_OPERATORS: [(&str, bool, Bound); 4] = [
    ("gte", true, Bound::Inclusive),
    ("gt", true, Bound::Exclusive),
    ("lte", false, Bound::Inclusive),
    ("lt", false, Bound::Exclusive),
];

/// A range over one column: either end, both, or (transiently, while parsing) neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Range {
    pub lower: Option<(Bound, String)>,
    pub upper: Option<(Bound, String)>,
}

impl Range {
    /// Both ends, lower first, which is the order a statement binds them in.
    pub(crate) fn bounds(&self) -> impl Iterator<Item = &(Bound, String)> {
        self.lower.iter().chain(self.upper.iter())
    }
}

/// What a scan was asked to match: equality on some columns, plus at most one column
/// carrying a range.
///
/// That is the shape a B-tree index serves, and the reason the read API can promise it
/// never scans a table: equality on columns 1 to n-1 of an index seeks straight to a
/// contiguous run, and a range on column n walks a slice of it.
///
/// Owns its strings. A filter is parsed out of a request, validated, then carried onto a
/// blocking thread to run, and borrowing across that would cost the handler an owned
/// mirror of this whole shape to borrow *from*.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub equals: Vec<(String, String)>,
    pub range: Option<(String, Range)>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.equals.is_empty() && self.range.is_none()
    }

    /// The range column, if one was asked for.
    fn range_column(&self) -> Option<&str> {
        self.range.as_ref().map(|(column, _)| column.as_str())
    }

    /// Every column mentioned, equality first then the range, for a diagnostic that has
    /// to name what was asked.
    pub fn columns(&self) -> Vec<&str> {
        self.equals
            .iter()
            .map(|(column, _)| column.as_str())
            .chain(self.range_column())
            .collect()
    }
}

/// How a scan will reach its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access<'a> {
    /// Through the primary key, which has no index to name. An `INTEGER` key *is* the
    /// rowid and a text key's index is SQLite's own `sqlite_autoindex_*`, so there is
    /// nothing stable to write in an `INDEXED BY` and the pin is left off. The planner
    /// has no scan to fall back to here in any case.
    Key,
    /// Through a declared index, pinned by this name.
    Index(&'a str),
}

/// The index a filter will be served by, or `None` when nothing declared can serve it.
///
/// The rule is the one a B-tree can actually answer: the equality columns must be a
/// **prefix** of the index, and the range column (if any) must be the column right after
/// that prefix. `index (shop_id, status, month)` answers `shop_id`, `(shop_id, status)`
/// and `(shop_id, status, month)`, plus a range on `status` under `shop_id` and one on
/// `month` under both. It cannot answer `(status, month)`, because reaching those rows
/// means visiting every `shop_id`, which is the table scan this API exists to refuse.
///
/// Equality is matched as a **set**: a query string carries no order, so
/// `?status=x&shop_id=1` and `?shop_id=1&status=x` are the same question.
///
/// **An explicit ordering decides the index outright.** `ORDER BY` and the filter have to
/// agree on one index or the rows come out of one and get sorted for the other, which is
/// the in-memory sort this API exists to refuse. So when `order` names an index, that
/// index must be the one that serves the filter, and nothing else is considered.
///
/// **Otherwise, where several indexes qualify, the narrowest wins**, and that is a
/// performance decision rather than a cosmetic one. The scan orders by the key, and the
/// key sits at the end of every generated index (`EntityDef::index_columns`), so the rows
/// only arrive already in key order when the filter uses up *all* of an index's declared
/// columns. Filtering `shop_id` through `index (shop_id, status)` leaves `status` ahead of
/// the key and costs a sort of the whole match set on every page; through `index (shop_id)`
/// it costs nothing. Taking the shortest qualifying index is how an entity that declares
/// both gets the better plan. Ties go to declaration order, so the choice is stable: it
/// is pinned into the SQL and asserted by a query-plan test.
///
/// A prefix shorter than every index that serves it still sorts. That is a real cost and
/// there is no way around it short of generating an index per prefix; declaring the
/// narrower index is the author's lever, and `EXPLAIN QUERY PLAN` is where it shows.
pub fn choose_index<'a>(
    entity: &'a EntityDef,
    filter: &Filter,
    order: &Order,
) -> Option<Access<'a>> {
    if let Some(name) = &order.index {
        let index = entity.indexes.iter().find(|index| &index.name == name)?;
        return serves(index, filter).then_some(Access::Index(index.name.as_str()));
    }
    choose_for_key_order(entity, filter)
}

/// Whether one index can serve a filter: the equality columns are exactly its leading
/// ones, and the range column (if any) is the one directly after them.
fn serves(index: &IndexDef, filter: &Filter) -> bool {
    let equals: Vec<&str> = filter
        .equals
        .iter()
        .map(|(column, _)| column.as_str())
        .collect();
    let Some(prefix) = index.columns.get(..equals.len()) else {
        return false;
    };
    // `prefix` holds exactly `equals.len()` columns and the caller has already refused a
    // repeated one, so every equality column being in it makes the two equal as sets.
    let covered = equals
        .iter()
        .all(|column| prefix.iter().any(|declared| declared == column));
    covered
        && match filter.range_column() {
            Some(column) => index
                .columns
                .get(equals.len())
                .is_some_and(|next| next == column),
            None => true,
        }
}

/// The access path for a filter under the default key ordering.
fn choose_for_key_order<'a>(entity: &'a EntityDef, filter: &Filter) -> Option<Access<'a>> {
    // The key is its own index, and the only one that can serve a bare scan. It is
    // tried first so an entity that also declares an index leading with its key does
    // not change which plan a key filter gets.
    let on_key = |column: &str| column == entity.key;
    let key_only = filter.equals.iter().all(|(column, _)| on_key(column))
        && filter.range_column().is_none_or(on_key);
    if key_only {
        return Some(Access::Key);
    }
    entity
        .indexes
        .iter()
        .filter(|index| serves(index, filter))
        // `min_by_key` keeps the first of equal keys, so declaration order breaks ties.
        .min_by_key(|index| index.columns.len())
        .map(|index| Access::Index(index.name.as_str()))
}

/// Validate that a filter's values parse as their columns' declared types, so a mismatch
/// (`?count=abc`, `?active=maybe`) is a 400 up front rather than a scan that silently
/// matches nothing. A column that is not declared is left for [`is_filterable`] to
/// reject, which has the better message for it.
///
/// A range carries the extra requirement that the column's order has to *mean*
/// something: a `Money` column holds its decimal string, so `?fee.gte=10` would put
/// `"2"` above it. [`FieldKind::is_comparable`] is that question, and `describe()` puts
/// the declared type in the message so the answer is actionable.
pub fn check_filter(entity: &EntityDef, filter: &Filter) -> anyhow::Result<()> {
    let kind = |field: &str| {
        entity
            .fields
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, meta)| &meta.kind)
    };
    for (field, value) in &filter.equals {
        if let Some(kind) = kind(field) {
            coerce_value(kind, value).with_context(|| format!("filter `{field}`"))?;
        }
    }
    if let Some((field, range)) = &filter.range {
        let Some(kind) = kind(field) else {
            return Ok(());
        };
        // `base()`, so an optional column ranges like the one underneath it. A range over
        // a nullable column is well defined and useful: `>=` simply does not match NULL,
        // which is what a reader asking for one wants. It is an *ordering* over one that
        // breaks, because a row-value comparison against NULL loses rows at a page
        // boundary rather than excluding them from an answer, and `ordering_problem`
        // refuses that separately.
        if !kind.base().is_comparable() {
            anyhow::bail!(
                "filter `{field}` is {}, which has no order a range could use; filter it for equality instead",
                kind.describe()
            );
        }
        for (_, value) in range.bounds() {
            coerce_value(kind, value).with_context(|| format!("filter `{field}`"))?;
        }
    }
    Ok(())
}

/// How a scan is ordered: by the primary key, or by a declared index, either direction.
///
/// One direction for the whole tuple, because the cursor resumes with a row-value
/// comparison and a row-value comparison has one. `(a, b, key) > (?, ?, ?)` is the shape
/// an index serves; per-column directions are not expressible in it, so an index is read
/// as declared or reversed as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Order {
    /// The declared name of the index to order by, or `None` for the primary key.
    pub index: Option<String>,
    pub descending: bool,
}

impl Order {
    /// The `order_by` spelling of this ordering, which is also what a cursor records so
    /// that resuming under a different one can be refused.
    pub fn token(&self, entity: &EntityDef) -> String {
        let name = self.index.as_deref().unwrap_or(&entity.key);
        let sign = if self.descending { "-" } else { "" };
        format!("{sign}{name}")
    }

    /// The columns this ordering sorts by, outermost first. Always ends in the key, which
    /// is what makes it unique: an index tuple is not, and without the tiebreak two rows
    /// sharing one would be skipped or repeated at a page boundary.
    ///
    /// **The one definition of what an ordering sorts by.** The cursor's values come from
    /// here and so do the `ORDER BY`, the row-value comparison and its binds; two copies
    /// drifting apart is exactly the "a tuple compared against the wrong columns" failure
    /// that [`Cursor::ordering`] exists to prevent, so there is only ever one.
    pub(crate) fn columns(&self, entity: &EntityDef) -> Vec<String> {
        match &self.index {
            None => vec![entity.key.clone()],
            Some(name) => entity
                .indexes
                .iter()
                .find(|index| &index.name == name)
                .map(|index| entity.index_columns(index))
                .unwrap_or_else(|| vec![entity.key.clone()]),
        }
    }
}

/// Read an `order_by` value against what the entity declares.
///
/// A leading `-` reverses. The rest names a declared index, or the key column, which is
/// the default ordering and is nameable so that it can be reversed. An index is matched
/// first: index names are generated as `by_<columns>` and never authored, so the only way
/// the two could name the same thing is a key column spelled `by_<something>`, which
/// `EntityDef::validate` refuses at load precisely so this order does not have to be a
/// judgement call.
pub fn parse_order(entity: &EntityDef, raw: &str) -> Result<Order, String> {
    let (descending, name) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    if entity.indexes.iter().any(|index| index.name == name) {
        return Ok(Order {
            index: Some(name.to_owned()),
            descending,
        });
    }
    if name == entity.key {
        return Ok(Order {
            index: None,
            descending,
        });
    }
    let mut known: Vec<&str> = entity
        .indexes
        .iter()
        .map(|index| index.name.as_str())
        .collect();
    known.push(&entity.key);
    Err(format!(
        "`{name}` is not an ordering of entity `{}`; order by one of {}, each optionally \
         prefixed with `-` to reverse it",
        entity.name,
        known.join(", ")
    ))
}

/// Why a declared index cannot back an ordering, if it cannot.
///
/// Refused at request time rather than at load, because it depends on which index was
/// asked for: an index that cannot sort can still filter perfectly well, and rejecting it
/// at load would make a useful declaration unloadable over a question nobody asked.
///
/// The appended key is not checked. The ordering an author asked for is the declared
/// columns; the key is the tiebreak underneath them, and it already had to be orderable
/// and present to be a key at all.
pub fn ordering_problem(entity: &EntityDef, index: &IndexDef) -> Option<String> {
    for column in &index.columns {
        // Fails closed. `EntityDef::validate` refuses an index naming an undeclared
        // column, so a miss here is a broken invariant, and returning `None` would read
        // as "this index can order rows" and let the SQL name a column that is not there:
        // a 500 from the database rather than a 400 from the handler.
        let Some((_, meta)) = entity.fields.iter().find(|(name, _)| name == column) else {
            return Some(format!(
                "index `{}` cannot order rows: it names `{column}`, which entity `{}` does \
                 not declare",
                index.name, entity.name
            ));
        };
        // Before the comparability check, which also refuses an optional but for a
        // reason the author cannot act on: here the fix is the `?`, not the type.
        if meta.is_nullable() {
            return Some(format!(
                "index `{}` cannot order rows: `{column}` is optional, and a row-value \
                 comparison against NULL matches nothing, so a page boundary would drop \
                 every row whose `{column}` is absent. It stays filterable.",
                index.name
            ));
        }
        if !meta.kind.base().is_comparable() {
            return Some(format!(
                "index `{}` cannot order rows: `{column}` is {}, which has no order an \
                 ordering could use. It stays filterable.",
                index.name,
                meta.kind.describe()
            ));
        }
    }
    None
}

/// An opaque forward cursor: which ordering the page was taken under, and where in that
/// ordering it ended.
///
/// Carrying the ordering is what makes reading one back under a different `order_by` a
/// 400 instead of a wrong page. When the cursor was a bare key that hardly mattered,
/// because every scan was in key order; a tuple read against the wrong ordering compares
/// the wrong columns and silently returns the wrong rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// [`Order::token`] for the page this came from.
    #[serde(rename = "o")]
    pub ordering: String,
    /// The ordering's columns for the last row of that page, outermost first, ending in
    /// the key. Strings, because that is what a filter value is and what `bind_or_text`
    /// re-types against the column.
    #[serde(rename = "v")]
    pub values: Vec<String>,
}

/// Encode a cursor opaquely. Base64url of its JSON, so it stays one URL-safe token.
fn encode_cursor(cursor: &Cursor) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).expect("a cursor serialises"))
}

/// Decode an opaque cursor. A cursor issued before orderings existed was the bare key,
/// which is not JSON, so it fails here rather than being read as a tuple of one.
pub fn decode_cursor(cursor: &str) -> anyhow::Result<Cursor> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .context("cursor is not valid base64url")?;
    serde_json::from_slice(&bytes).context("cursor is not a cursor this server issued")
}

/// Everything a scan was asked for beyond the entity: what to match, in what order, where
/// to resume, and how much.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub filter: Filter,
    pub order: Order,
    pub cursor: Option<Cursor>,
    pub limit: usize,
}

impl Query {
    /// Every row, in key order. What a caller reading a whole entity wants.
    pub fn all(limit: usize) -> Query {
        Query {
            limit,
            ..Query::default()
        }
    }
}

/// Read one row by key, plus the projector position, in one read snapshot.
/// Subject-encrypted columns are decrypted on the way out; a column whose subject
/// key has been erased comes back absent.
pub fn get_one(
    db_path: &Path,
    entity: &EntityDef,
    key: &str,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<(Option<Value>, u64)> {
    let model = open_with_retry(db_path)?;
    let snapshot = model.begin()?;
    let position = model.read_checkpoint()?.get();
    let mut item = model.get(entity, key)?;
    drop(snapshot);
    if let (Some(row), Some(ks)) = (item.as_mut(), keystore) {
        decrypt_row(entity, row, &ks.row_decryptor(), None)?;
    }
    Ok((item, position))
}

/// Decrypt every subject-encrypted column of a read-model row in place, using the
/// sibling subject-id column's value to find the key (via a per-request cache). A
/// column that is unreadable under the current key is removed (reads as absent) rather
/// than erroring: the key is gone (erased or never created), or the ciphertext will not
/// decrypt under the present key (a stale row left under a superseded key). Only a key
/// that cannot be obtained at all (a missing or rotated-away master) is an error, so a
/// misconfigured master surfaces loudly instead of silently blanking every column.
pub(crate) fn decrypt_row(
    entity: &EntityDef,
    row: &mut Value,
    decryptor: &RowDecryptor<'_>,
    mut tally: Option<&mut Revealed>,
) -> anyhow::Result<()> {
    let Some(obj) = row.as_object_mut() else {
        return Ok(());
    };
    for (name, meta) in &entity.fields {
        let Some(subject_field) = &meta.subject else {
            continue;
        };
        let Some(ciphertext) = obj.get(name).and_then(Value::as_str).map(str::to_owned) else {
            continue; // absent / null column
        };
        let subject_value = obj.get(subject_field).and_then(scalar_to_string);
        let plaintext = match &subject_value {
            Some(subject_value) => decryptor
                .decrypt(subject_field, subject_value, name, &ciphertext)
                .with_context(|| format!("decrypting column `{name}`"))?,
            // No subject id to key on: the value is unreadable.
            None => None,
        };
        match plaintext {
            Some(text) => {
                if let Some(tally) = tally.as_deref_mut() {
                    tally.decrypted += 1;
                }
                obj.insert(name.clone(), typed_from_string(&meta.kind, text));
            }
            None => {
                // The key is live and this value still will not open: it was written
                // under one that has since been superseded. Worth separating from an
                // erasure, which is permanent, for a caller that reports rather than
                // serves.
                if let Some(tally) = tally.as_deref_mut()
                    && subject_value
                        .is_some_and(|id| decryptor.key_present(subject_field, &id) == Some(true))
                {
                    tally.stale += 1;
                }
                obj.remove(name);
            }
        }
    }
    Ok(())
}

/// What one decrypting pass found, for a caller that has to report on it.
///
/// The read API serves rows and wants none of this: a reader of a read model wants the
/// row, not an account of it. A one-off projection *is* the account, and it audits what
/// it revealed, so it asks for the counts rather than keeping a second copy of the loop
/// that would drift from this one.
#[derive(Debug, Default)]
pub(crate) struct Revealed {
    /// Cells opened.
    pub decrypted: usize,
    /// Cells that would not open under a key that is still live.
    pub stale: usize,
}

/// Re-type a decrypted **read-model column** back to the field's declared kind, so an
/// encrypted integer reads back as a JSON number. `Money` stays a decimal string, which
/// is both its wire form and its column form.
///
/// Keyed to the column producer, not the payload one: a column seal ran through
/// `column_form` first, so a `Timestamp` in it is RFC 3339 and stays text.
/// `heklang_host::unsealed_json` is the same table for a log payload, where the same
/// field is micros.
pub(crate) fn typed_from_string(kind: &FieldKind, text: String) -> Value {
    match kind.base() {
        FieldKind::I64 => text
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or(Value::String(text)),
        FieldKind::Bool => match text.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(text),
        },
        FieldKind::Json => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        // Money is a decimal string on the wire; Text/Uuid/Timestamp/OneOf are strings.
        _ => Value::String(text),
    }
}

/// Scan an entity, filtered and ordered over a declared index and resumed after a cursor,
/// plus the projector position, in one read snapshot. `query` must already have been
/// through [`choose_index`] and [`check_filter`]; this re-chooses the index rather than
/// taking one, so the statement cannot be built against a different index than the handler
/// validated against.
pub fn scan(
    db_path: &Path,
    entity: &EntityDef,
    query: &Query,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Page> {
    let model = open_with_retry(db_path)?;
    let snapshot = model.begin()?;
    let position = model.read_checkpoint()?.get();
    let probe = Query {
        limit: query.limit + 1,
        ..query.clone()
    };
    // Over-fetch one row to learn whether another page follows.
    let mut items = model.scan(entity, &probe)?;
    drop(snapshot);

    let next_cursor = if items.len() > query.limit {
        items.truncate(query.limit);
        match items.last() {
            Some(row) => Some(cursor_at(entity, query, row)?),
            None => None,
        }
    } else {
        None
    };
    // The cursor is computed before this loop, and out of plaintext columns only: a key
    // is never encrypted, and `EntityDef::validate` refuses an index over a sealed
    // column. So decrypting a page cannot affect where the next one starts, and a page
    // whose subject keys were erased paginates exactly like one whose were not. One
    // decryptor for the whole page unwraps each subject's key once, not per row.
    if let Some(ks) = keystore {
        let decryptor = ks.row_decryptor();
        for row in &mut items {
            decrypt_row(entity, row, &decryptor, None)?;
        }
    }
    Ok(Page {
        items,
        next_cursor,
        position,
    })
}

/// The cursor that resumes after `row`, or `None` if any ordering column is missing from
/// it, which would make a row-value comparison compare the wrong things.
fn cursor_at(entity: &EntityDef, query: &Query, row: &Value) -> anyhow::Result<String> {
    let values = query
        .order
        .columns(entity)
        .iter()
        .map(|column| {
            // An error rather than "no more pages". This is only reached once an
            // over-fetched row has *proved* another page exists, so answering
            // `next_cursor: null` would tell the caller the scan was complete while rows
            // remained: a silent truncation, and the one failure a paginating reader
            // cannot detect. An ordering column is declared non-optional and non-blob, so
            // reaching this means the stored row disagrees with the declaration.
            row.get(column)
                .and_then(scalar_string)
                .with_context(|| format!("ordering column `{column}` is not a scalar in this row"))
        })
        .collect::<anyhow::Result<Vec<String>>>()
        .context("cannot describe where this page ended")?;
    Ok(encode_cursor(&Cursor {
        ordering: query.order.token(entity),
        values,
    }))
}

/// The string form of an ordering column for cursor encoding: strings as-is, numbers by
/// their canonical decimal form, so integer-keyed entities paginate too.
///
/// No `Bool` arm, deliberately: `is_keyable` refuses a boolean key and `ordering_problem`
/// refuses a boolean index column, so one cannot reach an ordering. If that ever changes,
/// [`cursor_at`] says so loudly rather than an arm here quietly deciding what `true`
/// sorts as.
fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Open the read model read-only, retrying once after a brief pause. The `.db`
/// path is always present (a replay swaps it in atomically), so this only guards
/// the vanishing window around the rename. Runs on a blocking thread.
pub fn open_with_retry(db_path: &Path) -> anyhow::Result<ReadModel> {
    match ReadModel::open_readonly(db_path) {
        Ok(model) => Ok(model),
        Err(_) => {
            thread::sleep(Duration::from_millis(5));
            ReadModel::open_readonly(db_path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity_with_index() -> EntityDef {
        use crate::schema::{FieldKind, FieldMeta, IndexDef};
        EntityDef {
            name: "users".to_owned(),
            key: "user_id".to_owned(),
            fields: vec![
                ("user_id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                (
                    "email".to_owned(),
                    FieldMeta::plain(FieldKind::Text { max_length: None }),
                ),
                (
                    "name".to_owned(),
                    FieldMeta::plain(FieldKind::Text { max_length: None }),
                ),
            ],
            indexes: vec![IndexDef {
                name: "by_email".to_owned(),
                columns: vec!["email".to_owned()],
            }],
        }
    }

    #[test]
    fn only_the_key_and_indexed_columns_are_filterable() {
        let entity = entity_with_index();
        assert!(is_filterable(&entity, "user_id"));
        assert!(is_filterable(&entity, "email"));
        assert!(!is_filterable(&entity, "name"));
        assert!(!is_filterable(&entity, "nonexistent"));
    }

    #[test]
    fn cursor_round_trips() {
        let taken = Cursor {
            ordering: "-by_bucket_rank".to_owned(),
            values: vec!["a".to_owned(), "5".to_owned(), "u1".to_owned()],
        };
        let encoded = encode_cursor(&taken);
        assert!(!encoded.contains("by_bucket_rank"), "cursors stay opaque");
        assert_eq!(decode_cursor(&encoded).unwrap(), taken);
        assert!(decode_cursor("not valid base64!!").is_err());
        // A cursor issued before orderings existed was the bare key, base64. It is
        // still valid base64, so the refusal has to come from the shape rather than the
        // encoding, and it has to come at all: read as a tuple of one it would compare
        // the key against whatever the ordering's first column is.
        let legacy = URL_SAFE_NO_PAD.encode(b"u1");
        assert!(decode_cursor(&legacy).is_err());
    }
}
