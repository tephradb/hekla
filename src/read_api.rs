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

/// Every field a scan may filter on, in declaration order: the primary key, then the
/// leftmost column of each declared index.
///
/// The single source of truth for both the 400 the scan handler returns and the query
/// parameters the OpenAPI generator documents, so the two cannot drift apart.
///
/// May repeat a name, when two indexes lead with the same column or one leads with the
/// key. A caller that turns each into something name-addressed (an OpenAPI query
/// parameter) has to deduplicate; a caller asking a membership question does not, and
/// leaving it lazy keeps [`is_filterable`] allocation-free on the read path.
pub fn filterable_fields(entity: &EntityDef) -> impl Iterator<Item = &str> {
    iter::once(entity.key.as_str()).chain(
        entity
            .indexes
            .iter()
            .filter_map(|index| index.columns.first())
            .map(String::as_str),
    )
}

/// Whether `field` can be filtered on: the primary key, or the leftmost column of
/// some declared index. Anything else would be a table scan, which the read API
/// refuses; the caller returns a 400 telling the author to declare the index.
///
/// Shares [`filterable_fields`] so the runtime's 400 and the parameters the OpenAPI
/// generator documents cannot drift apart.
pub fn is_filterable(entity: &EntityDef, field: &str) -> bool {
    filterable_fields(entity).any(|name| name == field)
}

/// Validate that a filter value parses as its column's declared type, so a
/// mismatch (`?count=abc`, `?active=maybe`) is a 400 up front rather than a scan
/// that silently matches nothing. `field` must already be validated as filterable,
/// so it is a declared field; an unknown field is left for that check to reject.
pub fn check_filter(entity: &EntityDef, field: &str, value: &str) -> anyhow::Result<()> {
    match entity.fields.iter().find(|(name, _)| name == field) {
        Some((_, meta)) => coerce_value(&meta.kind, value).map(|_| ()),
        None => Ok(()),
    }
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
        decrypt_row(entity, row, &ks.row_decryptor())?;
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
fn decrypt_row(
    entity: &EntityDef,
    row: &mut Value,
    decryptor: &RowDecryptor<'_>,
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
        let plaintext = match obj.get(subject_field).and_then(scalar_to_string) {
            Some(subject_value) => decryptor
                .decrypt(subject_field, &subject_value, name, &ciphertext)
                .with_context(|| format!("decrypting column `{name}`"))?,
            // No subject id to key on: the value is unreadable.
            None => None,
        };
        match plaintext {
            Some(text) => {
                obj.insert(name.clone(), typed_from_string(&meta.kind, text));
            }
            None => {
                obj.remove(name);
            }
        }
    }
    Ok(())
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

/// Scan an entity, optionally filtered by one indexed column and resumed after a
/// cursor, plus the projector position, in one read snapshot. `filter`'s column
/// must already be validated as filterable.
pub fn scan(
    db_path: &Path,
    entity: &EntityDef,
    filter: Option<(&str, &str)>,
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
            decrypt_row(entity, row, &decryptor)?;
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
