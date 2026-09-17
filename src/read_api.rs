//! The generated read API over projector read models.
//!
//! Reads open a fresh read-only connection to the projector's database per
//! request (WAL lets them run concurrently with the projector's single writer)
//! and read the projector's log position in the same snapshot as the rows, so a
//! response's `position` is consistent with its data. Filters are restricted to
//! declared indexes, and pagination is by an opaque key cursor, never an offset.

use std::iter;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use crate::crypto::{KeyStore, RowDecryptor};
use crate::read_model::{ReadModel, coerce_value};
use crate::schema::{EntityDef, FieldKind, scalar_to_string};

/// Default page size for a scan when the request does not set `limit`.
pub const DEFAULT_LIMIT: usize = 50;
/// Largest page a scan will return; a larger `limit` is clamped to this.
pub const MAX_LIMIT: usize = 500;

/// Query params the read endpoints consume as controls (pagination plus the
/// read-your-writes wait), never as an indexed filter. The single source of truth
/// for both the scan handler (which must not treat one as a filter) and `hekla
/// check` (which rejects an entity field that would collide with one). Keep in sync
/// with the keys the read handlers read off the query string.
pub const RESERVED_QUERY_PARAMS: [&str; 4] = ["limit", "cursor", "after", "timeout_ms"];

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
/// **Where several indexes qualify, the narrowest wins**, and that is a performance
/// decision rather than a cosmetic one. The scan orders by the key, and the key sits at
/// the end of every generated index (`EntityDef::index_columns`), so the rows only arrive
/// already in key order when the filter uses up *all* of an index's declared columns.
/// Filtering `shop_id` through `index (shop_id, status)` leaves `status` ahead of the key
/// and costs a sort of the whole match set on every page; through `index (shop_id)` it
/// costs nothing. Taking the shortest qualifying index is how an entity that declares
/// both gets the better plan. Ties go to declaration order, so the choice is stable: it
/// is pinned into the SQL and asserted by a query-plan test.
///
/// A prefix shorter than every index that serves it still sorts. That is a real cost and
/// there is no way around it short of generating an index per prefix; declaring the
/// narrower index is the author's lever, and `EXPLAIN QUERY PLAN` is where it shows.
pub fn choose_index<'a>(entity: &'a EntityDef, filter: &Filter) -> Option<Access<'a>> {
    let equals: Vec<&str> = filter
        .equals
        .iter()
        .map(|(column, _)| column.as_str())
        .collect();
    let range = filter.range_column();
    // The key is its own index, and the only one that can serve a bare scan. It is
    // tried first so an entity that also declares an index leading with its key does
    // not change which plan a key filter gets.
    let key_only = equals.iter().all(|column| *column == entity.key)
        && range.is_none_or(|column| column == entity.key);
    if key_only {
        return Some(Access::Key);
    }
    entity
        .indexes
        .iter()
        .filter(|index| {
            let Some(prefix) = index.columns.get(..equals.len()) else {
                return false;
            };
            let covered = equals
                .iter()
                .all(|column| prefix.iter().any(|declared| declared == column));
            covered
                && match range {
                    Some(column) => index
                        .columns
                        .get(equals.len())
                        .is_some_and(|next| next == column),
                    None => true,
                }
        })
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
        if !kind.is_comparable() {
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

/// Encode a row key as an opaque forward cursor.
fn encode_cursor(key: &str) -> String {
    URL_SAFE_NO_PAD.encode(key.as_bytes())
}

/// Decode an opaque cursor back to a row key.
pub fn decode_cursor(cursor: &str) -> anyhow::Result<String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .context("cursor is not valid base64url")?;
    String::from_utf8(bytes).context("cursor is not valid UTF-8")
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

/// Scan an entity, filtered over a declared index and resumed after a cursor, plus the
/// projector position, in one read snapshot. `filter` must already have been through
/// [`choose_index`] and [`check_filter`]; this re-chooses the index rather than taking
/// one, so the statement cannot be built against a different index than the handler
/// validated against.
pub fn scan(
    db_path: &Path,
    entity: &EntityDef,
    filter: &Filter,
    after_key: Option<&str>,
    limit: usize,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Page> {
    let model = open_with_retry(db_path)?;
    let snapshot = model.begin()?;
    let position = model.read_checkpoint()?.get();
    // Over-fetch one row to learn whether another page follows.
    let mut items = model.scan(entity, filter, after_key, limit + 1)?;
    drop(snapshot);

    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items
            .last()
            .and_then(|row| row.get(&entity.key))
            .and_then(key_string)
            .map(|key| encode_cursor(&key))
    } else {
        None
    };
    // The cursor is computed from the plaintext key (a key is never encrypted), so
    // decrypting the rows afterward does not affect pagination. One decryptor for the
    // whole page unwraps each subject's key once, not per row.
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

/// The string form of a key value for cursor encoding: strings as-is, numbers by
/// their canonical decimal form (so integer-keyed entities paginate too).
fn key_string(value: &Value) -> Option<String> {
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
        let cursor = encode_cursor("u1");
        assert_ne!(cursor, "u1");
        assert_eq!(decode_cursor(&cursor).unwrap(), "u1");
        assert!(decode_cursor("not valid base64!!").is_err());
    }
}
