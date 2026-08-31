//! The read-only introspection surface behind `/admin`.
//!
//! Sits above the runtime, alongside [`crate::verify`]: it reads the event log, the
//! operational database and the projector read models, and never writes to any of
//! them. The HTTP handlers stay thin, as they already do for the generated read API,
//! so the shaping lives here and stays testable without a server.
//!
//! Two properties are load-bearing and easy to lose in a refactor:
//!
//! **Every read is bounded.** The operational database is one mutex shared with each
//! effect's hot path, and the event log is read on the caller's thread. A browsing
//! request that scanned without a limit would stall live work.
//!
//! **Decryption is the same kind of boundary the read API already crosses, over a
//! wider surface.** `GET /read/...` decrypts a projector's subject columns and serves
//! the plaintext, so decrypting is not new; what is new is the reach. A read model
//! exposes the columns a projector chose to materialise, while this reaches every field
//! of every event, including ones no projector stores and event types with no read model
//! at all. That is why a decrypting request logs an audit line, and why `?decrypt=false`
//! exists. On an unauthenticated port the bind address remains the only real control,
//! which is a property of the whole API and not of this module.
//!
//! What it is not is a way around erasure: an erased subject has no key, so its
//! ciphertext stays ciphertext here as it does everywhere else.

use std::cell::Cell;

use anyhow::Context;
use serde_json::{Map, Value, json};
use tephra::{Event, EventType, Position, Query, QueryItem, Tag, Tags, WriteHandle};

use crate::crypto::{KeyStore, RowDecryptor};
use crate::effect::EffectShared;
use crate::envelope;
use crate::opdb::{EffectState, InvocationAt, InvocationRow, JournalRow, ModuleRow, SubjectInfo};
use crate::projector::ProjectorShared;
use crate::read_api::{self, filterable_fields};
use crate::read_model::key_kind;
use crate::schema::EventDefs;
use crate::schema::{EntityDef, EventDef, FieldMeta, scalar_to_string};
use crate::tags;
use crate::tags::RESERVED_TAG_PREFIX;

/// Default page size when a request does not set `limit`.
pub const DEFAULT_LIMIT: usize = 50;

/// Largest page any introspection endpoint returns. A larger `limit` is clamped to
/// this rather than rejected, matching the read API.
pub const MAX_LIMIT: usize = 500;

/// Which way a log page walks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Newest first, from `before` downwards. The default: an operator opening the
    /// log wants the tail, and it is the direction `read_back` exists for.
    Back,
    /// Oldest first, from `after` upwards.
    Forward,
}

impl Direction {
    pub fn parse(raw: &str) -> Option<Direction> {
        match raw {
            "back" => Some(Direction::Back),
            "forward" => Some(Direction::Forward),
            _ => None,
        }
    }
}

/// Lower a request's `type` and `tag` filters to a store query.
///
/// The shape matches tephra's own semantics exactly, so nothing is reinterpreted on
/// the way through: types OR together, tags AND together, and the two combine as one
/// item. No filter at all is [`Query::All`], which skips the tag index rather than
/// degenerating to an empty match.
pub fn build_query(types: &[String], tags: &[String]) -> anyhow::Result<Query> {
    if types.is_empty() && tags.is_empty() {
        return Ok(Query::All);
    }
    let types = types
        .iter()
        .map(|raw| {
            EventType::new(raw.as_str())
                .map_err(|err| anyhow::anyhow!("invalid type `{raw}`: {err}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let tags = tags
        .iter()
        .map(|raw| {
            Tag::new(raw.clone()).map_err(|err| anyhow::anyhow!("invalid tag `{raw}`: {err}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let tags = Tags::new(tags).map_err(|err| anyhow::anyhow!("invalid tag set: {err}"))?;
    Ok(Query::item(QueryItem::new(types, tags)))
}

/// The query matching every event of one correlated flow.
///
/// Goes through [`tags::correlation_tag_value`] rather than taking the bare id:
/// the stored tag is `_hekla_corr:<uuid>`, so a probe for the id alone matches
/// nothing at all, which is the shape of bug that reads as "this flow had no events".
pub fn correlation_query(correlation_id: &str) -> anyhow::Result<Query> {
    build_query(&[], &[tags::correlation_tag_value(correlation_id)])
}

/// One page of the log. `cursor` is a position: the exclusive upper bound walking
/// back, the exclusive lower bound walking forward. Positions are dense and 1-based,
/// so the cursor needs no opaque encoding, unlike a read model's key cursor.
///
/// `limit` is taken as given and **not** clamped: callers clamp the page size they
/// will return and then ask for one more, so a clamp here would eat the extra and
/// leave a full page indistinguishable from the last one.
pub fn page(
    store: &WriteHandle,
    query: &Query,
    direction: Direction,
    cursor: Option<u64>,
    limit: usize,
) -> anyhow::Result<Vec<(u64, Event)>> {
    let limit = limit.max(1) as u64;
    let read = match direction {
        Direction::Back => {
            let before = cursor.map_or(Position::MAX, Position::new);
            store.read_back(query, before, Some(limit)).collect_owned()
        }
        Direction::Forward => {
            let after = Position::new(cursor.unwrap_or(0));
            store.read(query, after, Some(limit)).collect_owned()
        }
    };
    let events = read.map_err(|err| anyhow::anyhow!("reading the event log: {err}"))?;
    Ok(events
        .into_iter()
        .map(|(position, event)| (position.get(), event))
        .collect())
}

/// The single event at `position`, or `None`.
///
/// Reads one event from an exclusive lower bound of `position - 1` and checks what
/// came back, the same shape [`crate::verify`] uses: a limit-1 read returns the next
/// matching event, which is the one asked for only when the log actually holds it.
pub fn read_at(store: &WriteHandle, position: u64) -> anyhow::Result<Option<Event>> {
    if position == 0 {
        return Ok(None);
    }
    let found = store
        .read(
            &Query::All,
            Position::new(position.saturating_sub(1)),
            Some(1),
        )
        .collect_owned()
        .map_err(|err| anyhow::anyhow!("reading the event log: {err}"))?;
    Ok(found
        .into_iter()
        .next()
        .filter(|(found, _)| found.get() == position)
        .map(|(_, event)| event))
}

/// Renders stored events as JSON, decrypting subject-scoped fields when it can.
///
/// Holds the decryptor for the whole request so one page of events unwraps each
/// subject's key once rather than per row, and tallies what it decrypted so the
/// request can emit a single audit line.
pub struct Renderer<'a> {
    defs: &'a EventDefs,
    decryptor: Option<RowDecryptor<'a>>,
    decrypted: Cell<usize>,
}

impl<'a> Renderer<'a> {
    /// `decrypt` is the request's opt-out. With no keystore configured there is
    /// nothing to decrypt with, and both cases render a subject field the same way:
    /// as the ciphertext that is actually stored.
    pub fn new(defs: &'a EventDefs, keystore: Option<&'a KeyStore>, decrypt: bool) -> Renderer<'a> {
        Renderer {
            defs,
            decryptor: decrypt
                .then(|| keystore.map(KeyStore::row_decryptor))
                .flatten(),
            decrypted: Cell::new(0),
        }
    }

    /// One stored event as JSON.
    pub fn event(&self, position: u64, event: &Event) -> anyhow::Result<Value> {
        let (envelope, mut data) =
            envelope::decode(event.data()).with_context(|| format!("decoding event {position}"))?;
        let event_type = event.event_type();
        let def = self.defs.get(event_type);
        let subjects = self.reveal_subjects(def, &mut data)?;

        // Stored tags are already sorted, so the split preserves that ordering. The
        // reserved ones are kept rather than dropped: unlike a command response, which
        // reports what an author emitted, this reports what the log holds, and the
        // host's own tags are exactly what an operator is trying to see.
        let (hekla_tags, tags): (Vec<&str>, Vec<&str>) = event
            .tags()
            .partition(|tag| tag.starts_with(RESERVED_TAG_PREFIX));

        let mut out = Map::new();
        out.insert("position".to_owned(), json!(position));
        out.insert("type".to_owned(), json!(event_type));
        // An event whose type the current project does not declare: the log outlives
        // any one deployment, so this is a fact about the two disagreeing, not corruption.
        out.insert("declared".to_owned(), json!(def.is_some()));
        let Value::Object(envelope) = serde_json::to_value(&envelope)? else {
            anyhow::bail!("an envelope did not serialise as an object");
        };
        out.extend(envelope);
        out.insert("data".to_owned(), data);
        out.insert("subjects".to_owned(), Value::Object(subjects));
        out.insert("tags".to_owned(), json!(tags));
        out.insert("hekla_tags".to_owned(), json!(hekla_tags));
        Ok(Value::Object(out))
    }

    /// Replace each subject-scoped field with its plaintext where possible, and
    /// describe every one of them either way.
    ///
    /// This mirrors `read_api`'s `decrypt_row` with one deliberate difference: an
    /// unreadable value keeps its stored ciphertext instead of being removed. Removal
    /// is right for a read model, which has to look like an ordinary row, and wrong
    /// here, where a field silently vanishing is the one thing an operator must not
    /// have to infer.
    fn reveal_subjects(
        &self,
        def: Option<&EventDef>,
        data: &mut Value,
    ) -> anyhow::Result<Map<String, Value>> {
        let mut subjects = Map::new();
        let (Some(def), Some(obj)) = (def, data.as_object_mut()) else {
            return Ok(subjects);
        };
        for (name, meta) in &def.fields {
            let Some(subject_field) = &meta.subject else {
                continue;
            };
            let Some(ciphertext) = obj.get(name).and_then(Value::as_str).map(str::to_owned) else {
                continue; // absent or null in the stored payload
            };
            let subject_value = obj.get(subject_field).and_then(scalar_to_string);
            let state = match (&self.decryptor, &subject_value) {
                (Some(decryptor), Some(subject_value)) => {
                    match decryptor.decrypt(subject_field, subject_value, name, &ciphertext) {
                        Ok(Some(text)) => {
                            obj.insert(name.clone(), read_api::typed_from_string(&meta.kind, text));
                            self.decrypted.set(self.decrypted.get() + 1);
                            "decrypted"
                        }
                        // Two situations reach here, and calling both "erased" tells an
                        // operator that data was irreversibly shredded when it may not
                        // have been. The decryptor just cached the answer, so separating
                        // them costs no further lookup.
                        Ok(None) => match decryptor.key_present(subject_field, subject_value) {
                            // The key is gone. Irreversible, by design.
                            Some(false) | None => "erased",
                            // The key is live, but this value was written under a
                            // superseded one (the subject was erased and a later event
                            // recreated it) or the ciphertext is corrupt. Unreadable, but
                            // not the permanent, total loss `erased` reports.
                            Some(true) => "stale",
                        },
                        // The key could not be obtained at all: a corrupt wrapping, or a
                        // master that is not configured. The read API fails the whole
                        // request on this so a misconfigured master is loud, and that is
                        // right for a read model. Here it would take the log itself
                        // offline: one bad row would 500 every page overlapping it, and
                        // the cursor is a position the caller cannot discover without a
                        // page. So it becomes a per-field state, loud in the log.
                        Err(err) => {
                            tracing::warn!(
                                "introspection could not obtain the key for `{name}` \
                                 (subject {subject_field} = {subject_value}): {err:#}"
                            );
                            "unreadable"
                        }
                    }
                }
                // Either the request opted out, or no master key is configured, or the
                // subject id is not readable to key on. Nothing was attempted.
                _ => "encrypted",
            };
            subjects.insert(
                name.clone(),
                json!({
                    "subject": subject_field,
                    "subject_value": subject_value,
                    "state": state,
                }),
            );
        }
        Ok(subjects)
    }

    /// Emit one audit line if this request decrypted anything.
    ///
    /// `reveal()` audits every decrypt in an effect (`crate::effect`), and this is the
    /// same seam reached over HTTP instead of from a handler. The read API is the one
    /// decrypt path that does not audit, which is a gap this deliberately does not copy.
    pub fn audit(&self, scope: &str) {
        let count = self.decrypted.get();
        if count > 0 {
            tracing::info!("introspection decrypted {count} subject field(s) for {scope}");
        }
    }
}

/// One effect, with everything the operational database knows about it on top of what
/// `/status` already reports.
pub fn effect_detail(shared: &EffectShared, head: u64, state: Option<&EffectState>) -> Value {
    let position = shared.position();
    let watermark = state.and_then(|state| state.watermark);
    let quarantine = state.and_then(|state| state.quarantine.as_ref());
    json!({
        "name": shared.name,
        // One word for what the counters below add up to, derived in the runtime so
        // `/status`, this endpoint and the console cannot disagree about it.
        "state": shared.state(head),
        "position": position,
        "lag": head.saturating_sub(position),
        // A remaining duration rather than an instant, so a reader can count down
        // without its clock having to agree with this process's. Null whenever nothing
        // is waiting, which covers both a healthy effect and one whose attempt is in
        // flight right now: the driver clears the deadline before it retries. So null
        // alongside a non-zero `consecutive_failures` is an attempt in progress.
        "retry_in_ms": shared.retry_in_ms(),
        "sources": shared.sources,
        // `None` is "has never run", which the driver's own resume path flattens to
        // zero because both mean "start from the beginning".
        "watermark": watermark,
        "consecutive_failures": shared.consecutive_failures(),
        "last_error": shared.last_error(),
        // Process-local and reset by a restart. The durable trace of a skipped
        // position is a terminal invocation row, indistinguishable from a completed one.
        "terminal_skips": shared.terminal_skips(),
        "last_terminal_error": shared.last_terminal_error(),
        "quarantined": shared.quarantined(),
        "quarantine": quarantine.map(|row| json!({
            "position": row.position,
            "reason": row.reason,
            "at": row.at,
        })),
    })
}

/// One effect invocation found by position, for a trace to attribute an event to the
/// effect that produced it. Narrower than [`invocation`]: a trace links to the full
/// journal rather than inlining it.
pub fn invocation_at(row: &InvocationAt) -> Value {
    json!({
        "effect": row.effect,
        "position": row.position,
        "status": row.status,
    })
}

/// One invocation row.
pub fn invocation(row: &InvocationRow) -> Value {
    json!({
        "position": row.position,
        "status": row.status,
        "script_hash": row.script_hash,
        "created_at": row.created_at,
        "completed_at": row.completed_at,
    })
}

/// One invocation with its journaled calls, in the order they were made.
///
/// `first_seq` is the ordinal of `calls[0]` in the whole sequence, so `seq` keeps
/// counting across pages: an operator reading "the call it is stuck on is #57" needs
/// that number to mean the same thing on page two as on page one.
pub fn invocation_detail(
    row: &InvocationRow,
    calls: &[JournalRow],
    first_seq: u64,
    next_cursor: Option<u64>,
) -> Value {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            json!({
                "seq": first_seq + index as u64,
                // `None` on a row written before the kind column existed. Rendered as
                // an absent value rather than an invented one.
                "kind": call.kind,
                "call_hash": call.call_hash,
                "disambiguator": call.disambiguator,
                // Stored as a JSON string; re-parsed so a consumer sees the structure
                // rather than an escaped blob. A row that will not parse is surfaced
                // as its raw text instead of failing the whole request.
                "result": serde_json::from_str::<Value>(&call.result)
                    .unwrap_or_else(|_| Value::String(call.result.clone())),
                "created_at": call.created_at,
            })
        })
        .collect();
    let mut out = invocation(row);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("calls".to_owned(), Value::Array(calls));
        obj.insert("next_cursor".to_owned(), json!(next_cursor));
    }
    out
}

/// One projector, with its entities. `counts` is present only when the request asked
/// for it: counting is a full table scan per entity.
pub fn projector_detail(
    shared: &ProjectorShared,
    head: u64,
    definition_hash: Option<String>,
    counts: Option<&[u64]>,
) -> Value {
    let position = shared.position();
    let entities: Vec<Value> = shared
        .entities
        .iter()
        .enumerate()
        .map(|(index, entity)| self_entity(entity, counts.and_then(|counts| counts.get(index))))
        .collect();
    json!({
        "name": shared.name,
        "position": position,
        "lag": head.saturating_sub(position),
        "readiness": shared.readiness().label(),
        "running": shared.running(),
        "failed": shared.failed(),
        "last_error": shared.last_error(),
        "sources": shared.sources,
        "definition_hash": definition_hash,
        "entities": entities,
    })
}

/// One entity's declared shape, plus its row count when one was taken.
fn self_entity(entity: &EntityDef, count: Option<&u64>) -> Value {
    let mut filterable: Vec<&str> = filterable_fields(entity).collect();
    filterable.sort_unstable();
    filterable.dedup();
    let fields: Vec<Value> = entity
        .fields
        .iter()
        .map(|(name, meta)| field(name, meta))
        .collect();
    json!({
        "name": entity.name,
        "key": entity.key,
        "key_kind": key_kind(entity).describe(),
        "fields": fields,
        "indexes": entity.indexes.iter().map(|index| json!({
            "name": index.name,
            "columns": index.columns,
        })).collect::<Vec<_>>(),
        "filterable": filterable,
        "rows": count,
    })
}

/// One declared field, in the vocabulary the author wrote it in.
fn field(name: &str, meta: &FieldMeta) -> Value {
    json!({
        "name": name,
        "kind": meta.kind.describe(),
        "optional": meta.kind.is_nullable(),
        "indexed": meta.indexed,
        "subject": meta.subject,
    })
}

/// One declared event type.
pub fn event_def(def: &EventDef) -> Value {
    json!({
        "type": def.event_type,
        "fields": def.fields.iter().map(|(name, meta)| field(name, meta)).collect::<Vec<_>>(),
    })
}

/// One deployed module as recorded at boot.
pub fn module(row: &ModuleRow) -> Value {
    json!({
        "name": row.name,
        "kind": row.kind,
        "source_hash": row.source_hash,
        "loaded_at": row.loaded_at,
    })
}

/// One live subject key, without any key material.
pub fn subject(info: &SubjectInfo) -> Value {
    json!({
        "subject_field": info.subject_field,
        "subject_value": info.subject_value,
        "master_key_id": info.master_key_id,
        "created_at": info.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_filters_is_the_catch_all_rather_than_an_empty_match() {
        assert_eq!(build_query(&[], &[]).unwrap(), Query::All);
    }

    #[test]
    fn types_or_together_and_tags_and_together_in_one_item() {
        let query = build_query(
            &["a.happened".to_owned(), "b.happened".to_owned()],
            &["x:1".to_owned(), "y:2".to_owned()],
        )
        .unwrap();
        let Query::Items(items) = query else {
            panic!("a filtered query is a set of items");
        };
        // One item, not one per filter: two items would OR the tags, which is the
        // opposite of what a caller passing two tags is asking for.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].types.len(), 2);
        assert_eq!(items[0].tags.len(), 2);
    }

    #[test]
    fn a_repeated_tag_is_rejected_rather_than_silently_deduped() {
        let err = build_query(&[], &["x:1".to_owned(), "x:1".to_owned()]).unwrap_err();
        assert!(err.to_string().contains("invalid tag set"), "got: {err}");
    }

    #[test]
    fn a_direction_is_one_of_two_words() {
        assert!(matches!(Direction::parse("back"), Some(Direction::Back)));
        assert!(matches!(
            Direction::parse("forward"),
            Some(Direction::Forward)
        ));
        assert!(Direction::parse("backwards").is_none());
        assert!(Direction::parse("").is_none());
    }
}
