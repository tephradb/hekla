//! Command dispatch: the DCB decision cycle for a loaded command.
//!
//! `query` derives the consistency boundary; the events inside it are read and
//! `fold`ed into state; `handle` decides; and any events it emits are appended
//! guarded by that same boundary, so a concurrent write inside the boundary
//! makes the append fail rather than silently violate an invariant.
//!
//! One call is one attempt. A DCB conflict returns [`CommandOutcome::Conflict`]
//! rather than an error, so the runtime can rebuild the decision model by
//! re-running the whole cycle. The events a command emits are wrapped in a
//! host-stamped [`envelope`] at the append seam, and every read unwraps it, so
//! handlers only ever see the payload.

use std::collections::HashMap;

use starlark::environment::Module;
use tephra::{
    AppendCondition, AppendError, ConflictClause, Event, EventType, Position, PositionRange, Query,
    QueryItem, Tag, Tags, WriteHandle,
};
use uuid::Uuid;

use crate::context::{CommandContext, HandleCtx};
use crate::crypto::KeyStore;
use crate::envelope::{self, Envelope};
use crate::hash;
use crate::starlark_builtins::{
    EmittedEvent, EventDef, EventSpec, HandleOutcome, LoadedModule, ModuleDef, alloc_event,
    alloc_input, call_handler, call_handler_with_ctx, call_handler_with_query_ctx,
    check_fold_result, initial_state, parse_event_specs, parse_handle_result, scalar_to_string,
    thaw, validate_command_input,
};

/// The reserved tag-key prefix for the global uniqueness tag of a `unique` field:
/// `_kiln_uniq_<field>`. Host-stamped, so it lives in the reserved namespace a user
/// field can never occupy.
const UNIQUE_TAG_PREFIX: &str = "_kiln_uniq_";

/// The global uniqueness tag key for `field`.
fn unique_tag_key(field: &str) -> String {
    format!("{UNIQUE_TAG_PREFIX}{field}")
}

/// A reserved tag no event ever carries, added to a query clause whose subject key is
/// absent so the clause matches nothing (rather than minting a key on the read path).
const NOMATCH_TAG: &str = "_kiln_nomatch";

/// The event definitions the dispatch layer needs: type name to its declared field
/// metadata, for encryption and for wrapping subject fields as opaque handles.
pub type EventDefs = HashMap<String, EventDef>;

/// Derived tags as `(key, Some(value))` / `(key, None)` pairs, before lowering to
/// tephra tags.
type TagPairs = Vec<(String, Option<String>)>;

/// Per-handler instruction budget. Bounds a runaway script at dispatch time.
const MAX_TICKS: u64 = 10_000_000;

/// The reserved tag-key prefix kiln stamps onto events for host bookkeeping. The
/// loader rejects this namespace on both sides: an event tag field can't emit one
/// (so a handler can't forge a host tag, or an append condition) and a `query()` /
/// `events()` tag can't name one (so a handler can't fold over other requests' host
/// tags).
pub const RESERVED_TAG_PREFIX: &str = "_kiln_";

/// The reserved tag key carrying a command's per-request idempotency identity. Every
/// event a keyed command emits gets this tag, and the append is guarded against it,
/// so exactly-once is enforced by the log itself rather than by op-DB bookkeeping.
const IDEMPOTENCY_TAG_KEY: &str = "_kiln_idem";

/// The idempotency tag for a `(command, key)` pair: `_kiln_idem:<sha256(command\0key)>`.
/// Hashing binds the tag to the command (so the same key on two commands cannot
/// collide) and yields a fixed-length, fixed-charset value regardless of the client's
/// raw key.
///
/// The tag deliberately excludes the request body: the key alone identifies the
/// request, so reusing a key with a different body replays the first outcome rather
/// than running the new body. This is standard idempotency-key semantics; a client
/// that wants a distinct outcome must use a distinct key.
pub fn idempotency_tag(command: &str, key: &str) -> String {
    let mut material = Vec::with_capacity(command.len() + 1 + key.len());
    material.extend_from_slice(command.as_bytes());
    material.push(0);
    material.extend_from_slice(key.as_bytes());
    format!("{IDEMPOTENCY_TAG_KEY}:{}", hash::sha256_hex(&material))
}

/// A command outcome recovered from the log by its idempotency tag: a prior committed
/// attempt's events and identity, enough to rebuild the exact success response a
/// replay must return without re-running `handle`.
pub struct RecoveredOutcome {
    pub events: Vec<RecoveredEvent>,
    pub positions: PositionRange,
    pub correlation_id: Uuid,
    pub causation_id: Uuid,
}

/// One recovered event: its type and its derived tags (reserved host tags stripped),
/// already rendered as the `"key:value"` strings the response reports.
pub struct RecoveredEvent {
    pub event_type: String,
    pub tags: Vec<String>,
}

/// The outcome of one command attempt.
pub enum CommandOutcome {
    /// `handle` emitted events (possibly none) and they were appended.
    Committed {
        events: Vec<EmittedEvent>,
        /// The assigned positions, or `None` when nothing was emitted.
        positions: Option<PositionRange>,
    },
    /// `handle` rejected the command on state grounds; nothing was written.
    Rejected { code: String, message: String },
    /// The input was malformed (host-side validation or `invalid_input`); nothing
    /// was written.
    InvalidInput { message: String },
    /// The append hit a concurrent write inside the boundary. The caller should
    /// rebuild state and retry.
    Conflict,
    /// This request already committed under its idempotency tag (a crashed or
    /// concurrent duplicate): the outcome was recovered from the log rather than
    /// re-decided, and the caller returns it verbatim.
    AlreadyCommitted(RecoveredOutcome),
    /// The store could not service the append for a transient reason (the write
    /// coordinator is draining). The caller should surface a retryable status.
    Unavailable { message: String },
}

/// Host-side validation of a command's raw input against its declared schema.
/// A malformed body is the equivalent of `invalid_input(...)` and never reaches a
/// handler. The runtime validates once before the retry loop, so [`run_command`]
/// (re-run per attempt) can assume an already-validated body.
pub fn validate_input(loaded: &LoadedModule, input: &serde_json::Value) -> anyhow::Result<()> {
    let ModuleDef::Command { input: schema, .. } = &loaded.def else {
        anyhow::bail!("validate_input called on a non-command module");
    };
    validate_command_input(schema, input)
}

/// Run one command attempt against the store: read the boundary, fold, handle,
/// append. The caller validates input once via [`validate_input`] before the retry
/// loop, so this per-attempt cycle assumes a well-formed body. `now` is the
/// request's pinned append time, visible to `handle` through `now()` and stamped
/// into each event's envelope.
///
/// When `idem_tag` is set, exactly-once is enforced atomically at the append: every
/// emitted event carries the tag and the append condition's existence clause
/// ([`AppendCondition::fail_if_exists`]) rejects it if the tag exists anywhere. So a
/// duplicate (a crash replay or a concurrent request) fails with
/// [`ConflictClause::Existence`] rather than committing twice, and the runtime then
/// recovers the original outcome via [`find_committed_outcome`]. There is no
/// pre-`handle` read: a fresh request pays nothing, and a duplicate re-runs the pure
/// `handle` but its events never land. The gaps the append can't catch are the
/// decisions that never append at all: a `handle` that rejects, and one that emits
/// nothing. Both are checked against the tag directly (see [`recover_if_committed`]).
#[allow(clippy::too_many_arguments)]
pub fn run_command(
    store: &WriteHandle,
    loaded: &LoadedModule,
    events: &EventDefs,
    keystore: Option<&KeyStore>,
    input: &serde_json::Value,
    ctx: &CommandContext,
    now: &str,
    idem_tag: Option<&str>,
) -> anyhow::Result<CommandOutcome> {
    let ModuleDef::Command { input: schema, .. } = &loaded.def else {
        anyhow::bail!("run_command called on a non-command module");
    };
    let frozen = &loaded.module;

    Module::with_temp_heap(|module| {
        let input_value = alloc_input(&module, schema, input)?;

        // Consistency boundary from `query` (optional). May return one spec or a
        // list of them, OR'd into the boundary.
        let boundary = match frozen.get_option("query")? {
            Some(func) => {
                let result = call_handler_with_query_ctx(
                    &module,
                    thaw(&func, &module),
                    &[input_value],
                    MAX_TICKS,
                )
                .map_err(|err| anyhow::anyhow!("query() failed: {err}"))?;
                let specs =
                    parse_event_specs(result).map_err(|err| anyhow::anyhow!("query() {err}"))?;
                Some(to_query(&specs, events, keystore)?)
            }
            None => None,
        };

        let mut state = initial_state(frozen, &module)
            .map_err(|err| anyhow::anyhow!("initial failed: {err}"))?;
        let mut after = Position::ZERO;
        if let Some(query) = &boundary {
            let fold = frozen.get_option("fold")?;
            let mut reads = store.read(query, Position::ZERO, None);
            while let Some(item) = reads.next() {
                let seq = item.map_err(|err| anyhow::anyhow!("read failed: {err}"))?;
                after = seq.position;
                if let Some(fold) = &fold {
                    let (_envelope, data) = envelope::decode(seq.event.data())
                        .map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
                    let def = events.get(seq.event.event_type());
                    let event = alloc_event(&module, seq.event.event_type(), &data, def);
                    state = call_handler(&module, thaw(fold, &module), &[state, event], MAX_TICKS)
                        .map_err(|err| anyhow::anyhow!("fold() failed: {err}"))?;
                    check_fold_result(state)?;
                }
            }
        }

        // `handle` alone sees the pinned clock.
        let handle_fn = frozen
            .get_option("handle")?
            .ok_or_else(|| anyhow::anyhow!("command has no handle() function"))?;
        let handle_ctx = HandleCtx {
            now: now.to_owned(),
        };
        let decision = call_handler_with_ctx(
            &module,
            thaw(&handle_fn, &module),
            &[input_value, state],
            MAX_TICKS,
            &handle_ctx,
        )
        .map_err(|err| anyhow::anyhow!("handle() failed: {err}"))?;

        match parse_handle_result(decision)? {
            HandleOutcome::Reject(rejection) => {
                if let Some(recovered) =
                    recover_if_committed(store, events, boundary.as_ref(), idem_tag)?
                {
                    return Ok(CommandOutcome::AlreadyCommitted(recovered));
                }
                Ok(CommandOutcome::Rejected {
                    code: rejection.code,
                    message: rejection.message,
                })
            }
            HandleOutcome::InvalidInput(invalid) => Ok(CommandOutcome::InvalidInput {
                message: invalid.message,
            }),
            HandleOutcome::Emit(emitted) => {
                if emitted.is_empty() {
                    if let Some(recovered) =
                        recover_if_committed(store, events, boundary.as_ref(), idem_tag)?
                    {
                        return Ok(CommandOutcome::AlreadyCommitted(recovered));
                    }
                    return Ok(CommandOutcome::Committed {
                        events: emitted,
                        positions: None,
                    });
                }
                let packed = emitted
                    .iter()
                    .map(|event| {
                        build_event(
                            event,
                            events.get(&event.event_type),
                            keystore,
                            ctx,
                            now,
                            idem_tag,
                        )
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let condition = build_condition(boundary, after, idem_tag)?;
                match store.append(packed, condition) {
                    Ok(positions) => Ok(CommandOutcome::Committed {
                        events: emitted,
                        positions: Some(positions),
                    }),
                    // The existence clause fired: this request already committed (a
                    // crash replay or a concurrent duplicate), caught atomically at the
                    // append with no TOCTOU. Recover its original outcome.
                    Err(AppendError::Conflict {
                        clause: ConflictClause::Existence,
                        ..
                    }) => {
                        let tag = idem_tag.expect("the existence clause is only set when keyed");
                        match find_committed_outcome(store, events, tag)? {
                            Some(recovered) => Ok(CommandOutcome::AlreadyCommitted(recovered)),
                            None => Err(anyhow::anyhow!(
                                "idempotency existence guard fired but no committed outcome was found"
                            )),
                        }
                    }
                    // A concurrent write inside the boundary: rebuild state on a fresh
                    // read and retry.
                    Err(AppendError::Conflict {
                        clause: ConflictClause::Boundary,
                        ..
                    }) => Ok(CommandOutcome::Conflict),
                    // The coordinator is draining: the request never landed, and a
                    // retry against a fresh process can succeed, so surface it as a
                    // retryable 503 rather than an opaque 500.
                    Err(AppendError::Shutdown) => Ok(CommandOutcome::Unavailable {
                        message: "the write coordinator is shutting down; retry".to_owned(),
                    }),
                    // A handler emitted a batch too large to ever store: an author
                    // bug, distinct from the integrity and I/O failures below.
                    Err(err @ AppendError::TooLarge { .. }) => Err(anyhow::anyhow!(
                        "command emitted an oversized event batch: {err}"
                    )),
                    // An event already on the log failed to decode during the
                    // condition scan: an integrity failure, not a normal outcome.
                    Err(err @ AppendError::Corrupt(_)) => Err(anyhow::anyhow!(
                        "append aborted on a corrupt event in the boundary: {err}"
                    )),
                    // Empty (guarded above) and AfterBeyondTip (a position kiln
                    // never hands out) are host bugs; Log is a durable write failure.
                    Err(err) => Err(anyhow::anyhow!("append failed: {err}")),
                }
            }
        }
    })
}

/// Lower one or more typed query clauses into a tephra query. `all_events()`
/// becomes `Query::All`; otherwise each clause is a query item (its type AND its
/// constrained fields as tags), and the items are OR'd (`parse_event_specs`
/// guarantees `all_events()` never appears alongside clauses).
///
/// A constraint on a subject-scoped field is encrypted so it matches the ciphertext
/// tag the emit path stored. The key follows the constraint's shape: if the field's
/// subject is also constrained in the clause, the scoped key; otherwise, for a
/// `unique` field, the global key (matching the `_kiln_uniq_<field>` tag that
/// survives erasure). A subject field constrained with neither is an error, because
/// no key can be derived. Plaintext fields (including subject ids) match verbatim.
pub(crate) fn to_query(
    specs: &[EventSpec],
    events: &EventDefs,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<Query> {
    let mut items = Vec::with_capacity(specs.len());
    for spec in specs {
        match spec {
            EventSpec::All => return Ok(Query::all()),
            EventSpec::Filter {
                event_type,
                constraints,
            } => {
                let ty = EventType::new(event_type.as_str())
                    .map_err(|err| anyhow::anyhow!("invalid event type `{event_type}`: {err}"))?;
                // Fail closed: the constructor came from a declared event, so its def
                // must be in the registry, and every constrained field must exist and
                // be indexed. This backstops the static check, whose input-branch blind
                // spot could otherwise let an undeclared, non-indexed, or reserved-name
                // constraint through as a tag that silently matches nothing (or injects
                // into the host namespace).
                let def = events.get(event_type).ok_or_else(|| {
                    anyhow::anyhow!("query references unknown event type `{event_type}`")
                })?;
                let mut tags = Vec::with_capacity(constraints.len());
                let mut unmatchable = false;
                for (field, value) in constraints {
                    let meta = def.field(field).ok_or_else(|| {
                        anyhow::anyhow!(
                            "query filters `{event_type}` on undeclared field `{field}`"
                        )
                    })?;
                    if !meta.indexed {
                        anyhow::bail!(
                            "query filters `{event_type}` on `{field}`, which is not indexed"
                        );
                    }
                    match &meta.subject {
                        Some(subject_field) => {
                            let ks = keystore.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "filtering encrypted field `{field}` needs a master key (set KILN_MASTER_KEY)"
                                )
                            })?;
                            let subject_value = constraints
                                .iter()
                                .find(|(f, _)| f == subject_field)
                                .map(|(_, v)| v);
                            let resolved = match subject_value {
                                // Scoped: encrypt with an existing per-subject key only,
                                // so a query never mints or resurrects one. An absent key
                                // means no matchable events, so the clause matches nothing.
                                Some(subject_value) => ks
                                    .encrypt_subject_existing(
                                        subject_field,
                                        subject_value,
                                        field,
                                        value,
                                    )?
                                    .map(|ct| (field.clone(), ct)),
                                // Global (uniqueness): use the global key, creating it if
                                // this is the first-ever use. The global key is a
                                // never-erased singleton, so creating it on a query is
                                // safe (no resurrection), and a deterministic tag is what
                                // makes concurrent first-writers of the same value conflict
                                // at the DCB boundary instead of both committing.
                                None if meta.unique => {
                                    Some((unique_tag_key(field), ks.encrypt_global(field, value)?))
                                }
                                None => anyhow::bail!(
                                    "cannot filter subject-encrypted field `{field}`: also constrain its subject `{subject_field}` (scoped), or the field is not `unique` for a global match"
                                ),
                            };
                            match resolved {
                                Some((tag_key, ciphertext)) => {
                                    tags.push((tag_key, Some(ciphertext)))
                                }
                                None => unmatchable = true,
                            }
                        }
                        None => tags.push((field.clone(), Some(value.clone()))),
                    }
                }
                if unmatchable {
                    tags.push((NOMATCH_TAG.to_owned(), None));
                }
                items.push(QueryItem::new(vec![ty], to_tags(&tags, &[])?));
            }
        }
    }
    Ok(Query::items(items))
}

/// Pack an emitted event for the store: its payload wrapped in a host-stamped
/// envelope, with the derived tags kept separate as tephra tags so the DCB index
/// still matches on them. When `idem_tag` is set it is added as an extra host tag,
/// so the append condition and a later recovery read can find this request's events.
/// This is the only place enveloping happens.
pub fn build_event(
    event: &EmittedEvent,
    event_def: Option<&EventDef>,
    keystore: Option<&KeyStore>,
    ctx: &CommandContext,
    now: &str,
    idem_tag: Option<&str>,
) -> anyhow::Result<Event> {
    let ty = EventType::new(event.event_type.as_str())
        .map_err(|err| anyhow::anyhow!("invalid event type `{}`: {err}", event.event_type))?;
    let (data, derived) = lower_event(event, event_def, keystore)?;
    let extra = idem_tag.as_slice();
    let tags = to_tags(&derived, extra)?;
    let envelope = Envelope {
        event_id: Uuid::new_v4(),
        timestamp: now.to_owned(),
        correlation_id: ctx.correlation_id,
        causation_id: ctx.causation_id,
        triggering_event_id: ctx.triggering_event_id,
    };
    let payload = envelope::encode(&envelope, &data)?;
    Event::new(&ty, &tags, &payload)
        .map_err(|err| anyhow::anyhow!("encoding event `{}`: {err}", event.event_type))
}

/// Lower an emitted event to its stored form: encrypt every subject-scoped field
/// (in the payload and in its tag), add the global-key tag for a `unique` field, and
/// derive the plaintext tags of the remaining indexed fields. Returns the payload to
/// envelope and the tag pairs.
///
/// Fails closed on an event type the registry does not know. The loader rejects an
/// `event(...)` bound outside `events/`, but a definition built inside a function
/// body reaches here unregistered, and passing it through would store a `subject`
/// field as plaintext in both the payload and its tag: unerasable, and silent.
fn lower_event(
    event: &EmittedEvent,
    event_def: Option<&EventDef>,
    keystore: Option<&KeyStore>,
) -> anyhow::Result<(serde_json::Value, TagPairs)> {
    let def = event_def.ok_or_else(|| {
        anyhow::anyhow!(
            "event type `{}` is not declared in events/; define it there and load() it, so its schema (and any `subject` encryption) is applied",
            event.event_type
        )
    })?;
    let Some(obj) = event.data.as_object() else {
        return Ok((event.data.clone(), event.tags.clone()));
    };
    if !def.fields.iter().any(|(_, meta)| meta.subject.is_some()) {
        // No subjects: the constructor's plaintext tags are already correct.
        return Ok((event.data.clone(), event.tags.clone()));
    }
    let mut payload = obj.clone();
    let mut tags = Vec::with_capacity(def.fields.len());
    for (name, meta) in &def.fields {
        let Some(value) = obj.get(name) else { continue };
        if value.is_null() {
            continue;
        }
        match &meta.subject {
            Some(subject_field) => {
                let plaintext = scalar_to_string(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "event `{}`: subject field `{name}` must be a scalar",
                        event.event_type
                    )
                })?;
                let subject_value = obj.get(subject_field).and_then(scalar_to_string).ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "event `{}`: subject id `{subject_field}` for `{name}` is missing or not scalar",
                            event.event_type
                        )
                    },
                )?;
                let ks = keystore.ok_or_else(|| {
                    anyhow::anyhow!(
                        "event `{}` has subject-encrypted field `{name}` but no master key is configured (set KILN_MASTER_KEY)",
                        event.event_type
                    )
                })?;
                let ciphertext =
                    ks.encrypt_subject(subject_field, &subject_value, name, &plaintext)?;
                payload.insert(name.clone(), serde_json::Value::String(ciphertext.clone()));
                if meta.indexed {
                    tags.push((name.clone(), Some(ciphertext)));
                }
                if meta.unique {
                    let global = ks.encrypt_global(name, &plaintext)?;
                    tags.push((unique_tag_key(name), Some(global)));
                }
            }
            None if meta.indexed => {
                let text = scalar_to_string(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "event `{}`: indexed field `{name}` must be a scalar",
                        event.event_type
                    )
                })?;
                tags.push((name.clone(), Some(text)));
            }
            None => {}
        }
    }
    Ok((serde_json::Value::Object(payload), tags))
}

/// The append guard for one attempt: the DCB boundary check (fail if a matching event
/// landed after the fold read) plus, when keyed, tephra's independent existence check
/// (fail if this request's idempotency tag exists anywhere, at an implicit `after = 0`).
/// The two clauses have separate positions, so a single append atomically asserts both
/// the moving decision boundary and the whole-log uniqueness of the request; a
/// duplicate that committed anywhere is caught even when the boundary's `after` has
/// advanced past it. A boundaryless keyed command is the pure-existence case.
fn build_condition(
    boundary: Option<Query>,
    after: Position,
    idem_tag: Option<&str>,
) -> anyhow::Result<Option<AppendCondition>> {
    let existence = match idem_tag {
        Some(tag) => Some(Query::item(idem_item(tag)?)),
        None => None,
    };
    match (boundary, existence) {
        (None, None) => Ok(None),
        (None, Some(exists)) => Ok(Some(AppendCondition::exists_only(exists))),
        (Some(query), None) => Ok(Some(AppendCondition::new(query).after(after))),
        (Some(query), Some(exists)) => Ok(Some(
            AppendCondition::new(query)
                .after(after)
                .fail_if_exists(exists),
        )),
    }
}

/// A query item matching any event carrying the idempotency tag.
fn idem_item(tag: &str) -> anyhow::Result<QueryItem> {
    let tag = Tag::new(tag.to_owned())
        .map_err(|err| anyhow::anyhow!("invalid idempotency tag `{tag}`: {err}"))?;
    let tags =
        Tags::new([tag]).map_err(|err| anyhow::anyhow!("invalid idempotency tag set: {err}"))?;
    Ok(QueryItem::with_tags(tags))
}

/// A keyed request's own prior commit, checked when this attempt is about to return
/// without appending anything. Both such decisions (a `handle` that rejects, and one
/// that emits nothing) can be spurious under a crashed or concurrent same-key
/// duplicate: this attempt folded the duplicate's just-committed events and concluded
/// the work was already done. Neither appends, so the append's existence clause can't
/// catch it, and only a boundaried command folds state at all, which is why the tag
/// re-read is confined to that case.
fn recover_if_committed(
    store: &WriteHandle,
    event_defs: &EventDefs,
    boundary: Option<&Query>,
    idem_tag: Option<&str>,
) -> anyhow::Result<Option<RecoveredOutcome>> {
    match (boundary, idem_tag) {
        (Some(_), Some(tag)) => find_committed_outcome(store, event_defs, tag),
        _ => Ok(None),
    }
}

/// Look for a prior committed attempt of this request in the log, by its idempotency
/// tag, so a replay returns the original outcome without re-running `handle`. Returns
/// `None` when the request has not committed yet. The read is an indexed existence
/// check on a unique, high-cardinality tag at `after = 0`: a term-dictionary probe per
/// segment, no posting-list decode (the single position inlines into the FST value).
fn find_committed_outcome(
    store: &WriteHandle,
    event_defs: &EventDefs,
    idem_tag: &str,
) -> anyhow::Result<Option<RecoveredOutcome>> {
    let query = Query::item(idem_item(idem_tag)?);
    let mut reads = store.read(&query, Position::ZERO, None);
    let mut events = Vec::new();
    let mut range: Option<(Position, Position)> = None;
    let mut ids: Option<(Uuid, Uuid)> = None;
    while let Some(item) = reads.next() {
        let seq = item.map_err(|err| anyhow::anyhow!("reading idempotent replay: {err}"))?;
        range = Some(match range {
            Some((first, _)) => (first, seq.position),
            None => (seq.position, seq.position),
        });
        let (envelope, _data) = envelope::decode(seq.event.data())
            .map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
        match ids {
            None => ids = Some((envelope.correlation_id, envelope.causation_id)),
            // Every event of one command execution shares its causation id. A second
            // distinct one means two logical requests matched this tag (a double
            // commit, or an astronomically unlikely hash collision): surface it rather
            // than splice both commits into one bogus recovered outcome.
            Some((_, causation)) if envelope.causation_id != causation => {
                anyhow::bail!("idempotency tag matches events from more than one command execution")
            }
            Some(_) => {}
        }
        // Report the same tags a fresh commit does: plaintext, non-subject indexed
        // fields only. Subject fields are stored as ciphertext (and the recovery path
        // deliberately cannot decrypt), and the reserved host tags (`_kiln_idem`, the
        // `_kiln_uniq_` global tags) are internal, so both are dropped.
        let def = event_defs.get(seq.event.event_type());
        let mut tags: Vec<String> = seq
            .event
            .tags()
            .filter(|tag| !tag.starts_with(RESERVED_TAG_PREFIX))
            .filter(|tag| {
                let key = tag.split(':').next().unwrap_or(tag);
                !def.is_some_and(|def| def.is_subject(key))
            })
            .map(str::to_owned)
            .collect();
        // Stored tag sets are sorted; sorting the response tags too keeps recovery
        // byte-identical to the live outcome, which also sorts (see `tag_strings`).
        tags.sort();
        events.push(RecoveredEvent {
            event_type: seq.event.event_type().to_owned(),
            tags,
        });
    }
    let Some((first, last)) = range else {
        return Ok(None);
    };
    let (correlation_id, causation_id) = ids.expect("a matched event carries an envelope");
    Ok(Some(RecoveredOutcome {
        events,
        positions: PositionRange { first, last },
        correlation_id,
        causation_id,
    }))
}

/// `(key, Some(value))` → `"key:value"`, `(key, None)` → `"key"`, plus any `extra`
/// raw tag strings appended verbatim (the host's reserved tags). Query and event go
/// through the same pair mapping, so a keyed tag matches only a keyed tag.
fn to_tags(pairs: &[(String, Option<String>)], extra: &[&str]) -> anyhow::Result<Tags> {
    let mut tags = pairs
        .iter()
        .map(|(key, value)| {
            let raw = match value {
                Some(value) => format!("{key}:{value}"),
                None => key.clone(),
            };
            Tag::new(raw).map_err(|err| anyhow::anyhow!("invalid tag `{key}`: {err}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for raw in extra {
        tags.push(
            Tag::new((*raw).to_owned())
                .map_err(|err| anyhow::anyhow!("invalid tag `{raw}`: {err}"))?,
        );
    }
    Tags::new(tags).map_err(|err| anyhow::anyhow!("invalid tag set: {err}"))
}
