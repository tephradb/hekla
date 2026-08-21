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
    AppendCondition, AppendError, Event, EventType, Position, PositionRange, Query, QueryItem, Tag,
    Tags, WriteHandle,
};
use uuid::Uuid;

use crate::context::{CommandContext, HandleCtx};
use crate::envelope::{self, Envelope};
use crate::read_model::ReadModel;
use crate::starlark_builtins::{
    EmittedEvent, EntityDef, EventSpec, HandleOutcome, LoadedModule, ModuleDef, alloc_event,
    alloc_input, call_handler, call_handler_with_ctx, check_fold_result, initial_state,
    parse_entity_ops, parse_event_specs, parse_handle_result, thaw, validate_command_input,
};

/// Per-handler instruction budget. Bounds a runaway script at dispatch time.
const MAX_TICKS: u64 = 10_000_000;

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
}

/// Run one command attempt against the store: validate input, read the boundary,
/// fold, handle, append. `now` is the request's pinned append time, visible to
/// `handle` through `now()` and stamped into each event's envelope.
pub fn run_command(
    store: &WriteHandle,
    loaded: &LoadedModule,
    input: &serde_json::Value,
    ctx: &CommandContext,
    now: &str,
) -> anyhow::Result<CommandOutcome> {
    let ModuleDef::Command { input: schema, .. } = &loaded.def else {
        anyhow::bail!("run_command called on a non-command module");
    };
    let frozen = &loaded.module;

    // 0. Host-side input validation. A malformed body is the equivalent of
    //    `invalid_input(...)`: it never reaches a handler.
    if let Err(err) = validate_command_input(schema, input) {
        return Ok(CommandOutcome::InvalidInput {
            message: format!("{err}"),
        });
    }

    Module::with_temp_heap(|module| {
        let input_value = alloc_input(&module, schema, input)?;

        // 1. Consistency boundary from `query` (optional). May return one spec or
        //    a list of them, OR'd into the boundary.
        let boundary = match frozen.get_option("query")? {
            Some(func) => {
                let result = call_handler(&module, thaw(&func, &module), &[input_value], MAX_TICKS)
                    .map_err(|err| anyhow::anyhow!("query() failed: {err}"))?;
                let specs =
                    parse_event_specs(result).map_err(|err| anyhow::anyhow!("query() {err}"))?;
                Some(to_query(&specs)?)
            }
            None => None,
        };

        // 2. Initial state, then fold the events inside the boundary.
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
                    let event = alloc_event(&module, seq.event.event_type(), &data);
                    state = call_handler(&module, thaw(fold, &module), &[state, event], MAX_TICKS)
                        .map_err(|err| anyhow::anyhow!("fold() failed: {err}"))?;
                    check_fold_result(state)?;
                }
            }
        }

        // 3. Decide. `handle` alone sees the pinned clock.
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

        // 4. Append the emitted events, guarded by the same boundary.
        match parse_handle_result(decision)? {
            HandleOutcome::Reject(rejection) => Ok(CommandOutcome::Rejected {
                code: rejection.code,
                message: rejection.message,
            }),
            HandleOutcome::InvalidInput(invalid) => Ok(CommandOutcome::InvalidInput {
                message: invalid.message,
            }),
            HandleOutcome::Emit(events) => {
                if events.is_empty() {
                    return Ok(CommandOutcome::Committed {
                        events,
                        positions: None,
                    });
                }
                let packed = events
                    .iter()
                    .map(|event| build_event(event, ctx, now))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let condition = boundary.map(|query| AppendCondition::new(query).after(after));
                match store.append(packed, condition) {
                    Ok(positions) => Ok(CommandOutcome::Committed {
                        events,
                        positions: Some(positions),
                    }),
                    // A concurrent write inside the boundary. Both conflict sites
                    // resolve by rebuilding state on a fresh read, so both are a
                    // retry signal to the runtime.
                    Err(AppendError::Conflict { .. }) => Ok(CommandOutcome::Conflict),
                    Err(err) => Err(anyhow::anyhow!("append failed: {err}")),
                }
            }
        }
    })
}

/// Run a projector across the store: read every event in its `source`, hand each
/// to `handle`, and apply the emitted `put`/`patch`/`delete` ops to the SQLite
/// read model. Returns the number of events processed.
pub fn run_projector(
    store: &WriteHandle,
    loaded: &LoadedModule,
    model: &ReadModel,
) -> anyhow::Result<usize> {
    let ModuleDef::Projector {
        entities, sources, ..
    } = &loaded.def
    else {
        anyhow::bail!("run_projector called on a non-projector module");
    };
    let frozen = &loaded.module;
    let query = to_query(sources)?;

    // Resolve the by-value references in `put`/`patch`/`delete` back to entities.
    let by_id: HashMap<u64, &EntityDef> =
        entities.iter().map(|entity| (entity.id, entity)).collect();

    let mut events_seen = 0usize;
    Module::with_temp_heap(|module| {
        let handle_fn = frozen
            .get_option("handle")?
            .ok_or_else(|| anyhow::anyhow!("projector has no handle() function"))?;
        let mut reads = store.read(&query, Position::ZERO, None);
        while let Some(item) = reads.next() {
            let seq = item.map_err(|err| anyhow::anyhow!("read failed: {err}"))?;
            events_seen += 1;
            let (_envelope, data) = envelope::decode(seq.event.data())
                .map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
            let event = alloc_event(&module, seq.event.event_type(), &data);
            let result = call_handler(&module, thaw(&handle_fn, &module), &[event], MAX_TICKS)
                .map_err(|err| anyhow::anyhow!("handle() failed: {err}"))?;
            for op in parse_entity_ops(result)? {
                let entity = by_id.get(&op.entity_id).ok_or_else(|| {
                    anyhow::anyhow!("op references an entity the projector didn't declare")
                })?;
                model.apply(entity, op.kind)?;
            }
        }
        anyhow::Ok(())
    })?;

    Ok(events_seen)
}

/// Lower one or more event specs into a tephra query. `all_events()` becomes
/// `Query::All` (a full scan that bypasses the index); otherwise each filter is
/// a query item and the items are OR'd together (`parse_event_specs` guarantees
/// `all_events()` never appears alongside filters).
fn to_query(specs: &[EventSpec]) -> anyhow::Result<Query> {
    let mut items = Vec::with_capacity(specs.len());
    for spec in specs {
        match spec {
            EventSpec::All => return Ok(Query::all()),
            EventSpec::Filter { types, tags } => {
                let types = types
                    .iter()
                    .map(|t| {
                        EventType::new(t.as_str())
                            .map_err(|err| anyhow::anyhow!("invalid event type `{t}`: {err}"))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                items.push(QueryItem::new(types, to_tags(tags)?));
            }
        }
    }
    Ok(Query::items(items))
}

/// Pack an emitted event for the store: its payload wrapped in a host-stamped
/// envelope, with the derived tags kept separate as tephra tags so the DCB index
/// still matches on them. This is the only place enveloping happens.
pub fn build_event(event: &EmittedEvent, ctx: &CommandContext, now: &str) -> anyhow::Result<Event> {
    let ty = EventType::new(event.event_type.as_str())
        .map_err(|err| anyhow::anyhow!("invalid event type `{}`: {err}", event.event_type))?;
    let tags = to_tags(&event.tags)?;
    let envelope = Envelope {
        event_id: Uuid::new_v4(),
        timestamp: now.to_owned(),
        correlation_id: ctx.correlation_id,
        causation_id: ctx.causation_id,
        triggering_event_id: ctx.triggering_event_id,
    };
    let payload = envelope::encode(&envelope, &event.data)?;
    Event::new(&ty, &tags, &payload)
        .map_err(|err| anyhow::anyhow!("encoding event `{}`: {err}", event.event_type))
}

/// `(key, Some(value))` → `"key:value"`, `(key, None)` → `"key"`. Query and
/// event go through the same mapping, so a keyed tag matches only a keyed tag.
fn to_tags(pairs: &[(String, Option<String>)]) -> anyhow::Result<Tags> {
    let tags = pairs
        .iter()
        .map(|(key, value)| {
            let raw = match value {
                Some(value) => format!("{key}:{value}"),
                None => key.clone(),
            };
            Tag::new(raw).map_err(|err| anyhow::anyhow!("invalid tag `{key}`: {err}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Tags::new(tags).map_err(|err| anyhow::anyhow!("invalid tag set: {err}"))
}
