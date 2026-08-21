//! Deploy-time validation over a loaded project.
//!
//! The loader ([`crate::loader`]) already fails a module that will not parse,
//! evaluate, or freeze, and an entity whose index names an undeclared field.
//! This pass adds the checks that need the event registry: a command's `query`
//! and a projector's or effect's `source` must filter on event types that exist
//! and on tags those types actually declare. A query that filters an event type
//! on a field that type does not declare as a tag would silently match nothing
//! at runtime, so catching it here is the point of sharing event definitions.
//!
//! `query` is a pure function of input, so it is evaluated with a placeholder
//! input built from the command's schema and the resulting boundary inspected.
//! The one blind spot is a `query` that branches on input values: only the
//! branch the placeholder takes is seen. A `query` that fails on the placeholder
//! is reported as a warning, not an error, since the failure may be an artefact
//! of the stub rather than a real defect.

use starlark::environment::Module;
use starlark::values::OwnedFrozenValue;

use crate::loader::{CommandUnit, EventRegistry, Finding, LoadedProject};
use crate::starlark_builtins::{
    EventSpec, FieldKind, InputSchema, ModuleDef, alloc_input, call_handler, parse_event_specs,
    thaw,
};

/// Instruction budget for evaluating a `query` during the check.
const MAX_TICKS: u64 = 10_000_000;

/// Run the semantic checks, returning the findings they surface. These are added
/// to the findings the loader already collected.
pub fn check(project: &LoadedProject) -> Vec<Finding> {
    let mut findings = Vec::new();
    for command in &project.commands {
        check_command_query(command, &project.events, &mut findings);
    }
    for projector in &project.projectors {
        if let ModuleDef::Projector { sources, .. } = &projector.loaded.def {
            validate_specs(
                sources,
                &project.events,
                &projector.rel_path,
                "source",
                &mut findings,
            );
        }
    }
    for effect in &project.effects {
        if let ModuleDef::Effect { sources, .. } = &effect.loaded.def {
            validate_specs(
                sources,
                &project.events,
                &effect.rel_path,
                "source",
                &mut findings,
            );
        }
    }
    findings
}

fn check_command_query(command: &CommandUnit, events: &EventRegistry, findings: &mut Vec<Finding>) {
    let ModuleDef::Command { input, .. } = &command.loaded.def else {
        return;
    };
    let query_fn = match command.loaded.module.get_option("query") {
        // A command with no invariants omits `query` entirely.
        Ok(None) => return,
        Ok(Some(func)) => func,
        Err(err) => {
            findings.push(Finding::error(
                &command.rel_path,
                format!("reading query(): {err}"),
            ));
            return;
        }
    };
    match evaluate_query(input, &query_fn) {
        Ok(specs) => validate_specs(&specs, events, &command.rel_path, "query", findings),
        Err(err) => findings.push(Finding::warning(
            &command.rel_path,
            format!("could not statically evaluate query() with placeholder input: {err:#}"),
        )),
    }
}

/// Evaluate `query(input)` with a placeholder input and return the boundary it
/// declares.
fn evaluate_query(
    schema: &InputSchema,
    query_fn: &OwnedFrozenValue,
) -> anyhow::Result<Vec<EventSpec>> {
    Module::with_temp_heap(|module| {
        let stub = stub_payload(schema);
        let input = alloc_input(&module, schema, &stub)?;
        let func = thaw(query_fn, &module);
        let result = call_handler(&module, func, &[input], MAX_TICKS)
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        parse_event_specs(result)
    })
}

fn validate_specs(
    specs: &[EventSpec],
    events: &EventRegistry,
    rel: &str,
    context: &str,
    findings: &mut Vec<Finding>,
) {
    for spec in specs {
        // `all_events()` matches every event, so there is nothing to validate.
        let EventSpec::Filter { types, tags } = spec else {
            continue;
        };
        for event_type in types {
            if !events.by_type.contains_key(event_type) {
                findings.push(Finding::error(
                    rel,
                    format!("{context} references unknown event type `{event_type}`"),
                ));
            }
        }
        for (key, _value) in tags {
            if types.is_empty() {
                // A tag-only filter binds to no specific type, so the strongest
                // check is that some event declares the tag at all.
                let declared = events
                    .by_type
                    .values()
                    .any(|def| def.tags.iter().any(|tag| tag == key));
                if !declared {
                    findings.push(Finding::warning(
                        rel,
                        format!("{context} filters on tag `{key}`, which no event declares"),
                    ));
                }
                continue;
            }
            for event_type in types {
                if let Some(def) = events.by_type.get(event_type)
                    && !def.tags.iter().any(|tag| tag == key)
                {
                    findings.push(Finding::error(
                        rel,
                        format!(
                            "{context} filters event `{event_type}` on `{key}`, which it does not declare as a tag"
                        ),
                    ));
                }
            }
        }
    }
}

fn stub_payload(schema: &InputSchema) -> serde_json::Value {
    let mut obj = serde_json::Map::with_capacity(schema.fields.len());
    for (name, kind) in &schema.fields {
        obj.insert(name.clone(), stub_value(kind));
    }
    serde_json::Value::Object(obj)
}

/// A type-appropriate placeholder so `query` can read every field. `optional`
/// fields get a present value rather than null, to exercise more of `query`.
fn stub_value(kind: &FieldKind) -> serde_json::Value {
    use serde_json::Value;
    match kind.base() {
        FieldKind::Text { .. } => Value::String("example".to_owned()),
        FieldKind::Uuid => Value::String("00000000-0000-0000-0000-000000000000".to_owned()),
        FieldKind::Timestamp => Value::String("1970-01-01T00:00:00Z".to_owned()),
        FieldKind::Money => Value::String("0".to_owned()),
        FieldKind::OneOf(variants) => Value::String(variants.first().cloned().unwrap_or_default()),
        FieldKind::I64 | FieldKind::U64 => Value::from(0),
        FieldKind::Bool => Value::Bool(false),
        FieldKind::Json => Value::Object(serde_json::Map::new()),
        FieldKind::Optional(_) => unreachable!("base() strips Optional"),
    }
}
