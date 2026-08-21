//! Command dispatch: the DCB decision cycle for a loaded command.
//!
//! `query` derives the consistency boundary; the events inside it are read and
//! `fold`ed into state; `handle` decides; and any events it emits are appended
//! guarded by that same boundary, so a concurrent write inside the boundary
//! makes the append fail rather than silently violate an invariant.

use std::collections::HashMap;

use starlark::environment::Module;
use tephra::{
    AppendCondition, Event, EventType, Position, Query, QueryItem, Tag, Tags, WriteHandle,
};

use crate::read_model::ReadModel;
use crate::starlark_builtins::{
    EmittedEvent, EntityDef, EventSpec, HandleOutcome, LoadedModule, ModuleDef, alloc_event,
    alloc_input, call_handler, check_fold_result, initial_state, parse_entity_ops,
    parse_event_specs, parse_handle_result, thaw,
};

/// Per-handler instruction budget. Bounds a runaway script at dispatch time.
const MAX_TICKS: u64 = 10_000_000;

/// The outcome of running a command.
pub enum CommandResult {
    /// `handle` emitted events and they were appended.
    Committed { appended: usize },
    /// `handle` rejected the command; nothing was written.
    Rejected { code: String, message: String },
}

/// Run one command against the store: read its boundary, fold, handle, append.
pub fn run_command(
    store: &WriteHandle,
    loaded: &LoadedModule,
    payload: serde_json::Value,
) -> anyhow::Result<CommandResult> {
    let ModuleDef::Command { input: schema, .. } = &loaded.def else {
        anyhow::bail!("run_command called on a projector module");
    };
    let frozen = &loaded.module;

    Module::with_temp_heap(|module| {
        let input = alloc_input(&module, schema, &payload)?;

        // 1. Consistency boundary from `query` (optional). May return one spec
        //    or a list of them, OR'd into the boundary.
        let boundary = match frozen.get_option("query")? {
            Some(func) => {
                let result = call_handler(&module, thaw(&func, &module), &[input], MAX_TICKS)
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
                    let data =
                        serde_json::from_slice(seq.event.data()).unwrap_or(serde_json::Value::Null);
                    let event = alloc_event(&module, seq.event.event_type(), &data);
                    state = call_handler(&module, thaw(fold, &module), &[state, event], MAX_TICKS)
                        .map_err(|err| anyhow::anyhow!("fold() failed: {err}"))?;
                    check_fold_result(state)?;
                }
            }
        }

        // 3. Decide.
        let handle_fn = frozen
            .get_option("handle")?
            .ok_or_else(|| anyhow::anyhow!("command has no handle() function"))?;
        let decision = call_handler(
            &module,
            thaw(&handle_fn, &module),
            &[input, state],
            MAX_TICKS,
        )
        .map_err(|err| anyhow::anyhow!("handle() failed: {err}"))?;

        // 4. Append the emitted events, guarded by the same boundary.
        match parse_handle_result(decision)? {
            HandleOutcome::Reject(rejection) => Ok(CommandResult::Rejected {
                code: rejection.code,
                message: rejection.message,
            }),
            HandleOutcome::Emit(events) => {
                if events.is_empty() {
                    return Ok(CommandResult::Committed { appended: 0 });
                }
                let packed = events
                    .iter()
                    .map(build_event)
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let condition = boundary.map(|query| AppendCondition::new(query).after(after));
                store
                    .append(packed, condition)
                    .map_err(|err| anyhow::anyhow!("append failed: {err}"))?;
                Ok(CommandResult::Committed {
                    appended: events.len(),
                })
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
        anyhow::bail!("run_projector called on a command module");
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
            let data = serde_json::from_slice(seq.event.data()).unwrap_or(serde_json::Value::Null);
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

/// Pack an emitted event for the store.
fn build_event(event: &EmittedEvent) -> anyhow::Result<Event> {
    let ty = EventType::new(event.event_type.as_str())
        .map_err(|err| anyhow::anyhow!("invalid event type `{}`: {err}", event.event_type))?;
    let tags = to_tags(&event.tags)?;
    let payload = serde_json::to_vec(&event.data)
        .map_err(|err| anyhow::anyhow!("serialising event data: {err}"))?;
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
