//! Deploy-time validation over a loaded project.
//!
//! The loader ([`crate::loader`]) already fails a module that will not parse,
//! evaluate, or freeze, and an entity whose index names an undeclared field. This
//! pass adds the checks that need the event registry. A command's `query`, a command's
//! `fold` keys, and a projector's or effect's `handle` keys are all typed clauses
//! (event definitions called with field constraints), so every constrained field must
//! exist on that event type, be indexed, be well-typed, and, when subject-scoped, have
//! a derivable key. A clause that filtered a field an event does not index would
//! silently match nothing at runtime, so catching it here is the point of sharing
//! event definitions.
//!
//! `query` is a pure function of input, so it is evaluated (in query mode) with a
//! placeholder input built from the command's schema and the resulting clauses
//! inspected. The one blind spot is a `query` that branches on input values: only
//! the branch the placeholder takes is seen. A `query` that fails on the placeholder
//! is reported as a warning, not an error, since the failure may be an artefact of
//! the stub rather than a real defect.
//!
//! A command's `fold` keys are additionally cross-checked against the boundary those
//! clauses describe. A dispatch map is a module-level literal with no branches, so it
//! is always seen in full; the uncertainty is on the `query` side, which is why a dead
//! `fold` entry is a warning rather than an error. A projector's or effect's keys need
//! no such cross-check at all, being the subscription itself.

use starlark::environment::{FrozenModule, Module};
use starlark::values::OwnedFrozenValue;
use uuid::Uuid;

use crate::loader::{EventRegistry, Finding, LoadedProject};
use crate::starlark_builtins::{
    EventDef, EventSpec, FieldKind, InputSchema, ModuleDef, alloc_event, alloc_input,
    call_handler_with_query_ctx, parse_event_dispatch, parse_event_specs, thaw,
};

/// Instruction budget for evaluating a `query` during the check.
const MAX_TICKS: u64 = 10_000_000;
/// The `event.timestamp` a placeholder event carries, matching what `kiln test` pins
/// so an author sees one fixed clock across the check and their scenarios.
const STUB_TIMESTAMP: &str = "1970-01-01T00:00:00Z";

/// Field-name substrings that suggest personal data. A field whose name contains one
/// of these but declares no `subject` is warned about, because a value appended
/// without a subject can never be erased.
const PERSONAL_NAME_HINTS: [&str; 9] = [
    "email", "phone", "address", "name", "dob", "ssn", "postcode", "zip", "birth",
];

/// A query clause that constrains at least this fraction of an event's fields looks
/// like a copied emit call rather than a boundary; warn on it.
const OVER_CONSTRAINT_RATIO: f64 = 0.75;

/// Run the semantic checks, returning the findings they surface. These are added to
/// the findings the loader already collected.
pub fn check(project: &LoadedProject) -> Vec<Finding> {
    let mut findings = Vec::new();
    check_event_definitions(&project.events, &mut findings);
    for command in &project.commands {
        findings.extend(check_module(
            &command.loaded.def,
            &command.loaded.module,
            &command.rel_path,
            &project.events,
        ));
    }
    // Projectors and effects subscribe the same way, so they are checked together:
    // all projectors first, then all effects.
    let subscribers = project
        .projectors
        .iter()
        .map(|unit| (&unit.loaded, unit.rel_path.as_str()))
        .chain(
            project
                .effects
                .iter()
                .map(|unit| (&unit.loaded, unit.rel_path.as_str())),
        );
    for (loaded, rel) in subscribers {
        findings.extend(check_module(
            &loaded.def,
            &loaded.module,
            rel,
            &project.events,
        ));
    }
    findings
}

/// The semantic checks for one module, given its definition and its frozen form.
/// [`check`] is this over every unit in a project; the language server runs it
/// over the single module being edited, so the two cannot disagree about what a
/// module's own clauses mean.
///
/// The cross-module checks ([`check_event_definitions`]) are not here: they are
/// about the registry as a whole, not about any one file.
pub fn check_module(
    def: &ModuleDef,
    module: &FrozenModule,
    rel: &str,
    events: &EventRegistry,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    match def {
        ModuleDef::Command { input, .. } => {
            check_command(input, module, rel, events, &mut findings)
        }
        // `sources` on the ModuleDef is `handle`'s keys, so checking the map covers
        // the subscription too; validating both would report each clause twice.
        ModuleDef::Projector { .. } => check_dispatch(
            module,
            "handle",
            Context::Handle,
            None,
            events,
            rel,
            &mut findings,
        ),
        ModuleDef::Effect { sources, .. } => {
            check_dispatch(
                module,
                "handle",
                Context::Handle,
                None,
                events,
                rel,
                &mut findings,
            );
            check_effect_boundary(sources, module, rel, events, &mut findings);
        }
    }
    findings
}

/// Which position a clause sits in. All three are query clauses, but they lower
/// differently, so their guardrails differ.
///
/// A `fold` key is lowered with the command's keystore, the way `query` is, so a
/// subject-scoped constraint resolves there; a `handle` key is lowered with no
/// keystore, so it can only filter plaintext. Selectivity is a boundary concern only:
/// a bare `order_placed()` dispatch key legitimately constrains nothing.
#[derive(Clone, Copy, PartialEq)]
enum Context {
    Query,
    Fold,
    Handle,
}

impl Context {
    fn label(self) -> &'static str {
        match self {
            Context::Query => "query",
            Context::Fold => "fold",
            Context::Handle => "handle",
        }
    }

    /// Whether a subject-encrypted field can be filtered here at all. Mirrors whether
    /// the lowering has a keystore to encrypt the filter value with.
    fn can_filter_encrypted(self) -> bool {
        matches!(self, Context::Query | Context::Fold)
    }
}

/// Warn about a personal-looking field with no subject, across every event. Crude,
/// but catching it at authoring time beats discovering an unerasable field later.
fn check_event_definitions(events: &EventRegistry, findings: &mut Vec<Finding>) {
    for def in events.by_type.values() {
        for (name, meta) in &def.fields {
            if meta.subject.is_none() && looks_personal(name) {
                findings.push(Finding::warning(
                    "events",
                    format!(
                        "event `{}` field `{name}` looks like personal data but has no `subject`, so it could never be erased",
                        def.event_type
                    ),
                ));
            }
        }
    }
}

fn looks_personal(field: &str) -> bool {
    let lower = field.to_ascii_lowercase();
    PERSONAL_NAME_HINTS.iter().any(|hint| lower.contains(hint))
}

/// Check a command's boundary and its fold dispatch.
///
/// `query` is evaluated once and the clauses it declares are reused for both, so a
/// command with no `query` is checked for the one thing that is still wrong without
/// it: a `fold` that can never run.
fn check_command(
    input: &InputSchema,
    module: &FrozenModule,
    rel: &str,
    events: &EventRegistry,
    findings: &mut Vec<Finding>,
) {
    let query_fn = match module.get_option("query") {
        // A command with no invariants omits `query` entirely.
        Ok(None) => {
            if matches!(module.get_option("fold"), Ok(Some(_))) {
                findings.push(Finding::warning(
                    rel,
                    "command defines `fold` but no `query`, so there is no boundary to fold and handle() only ever sees `initial`; add a query(), or drop `fold`".to_owned(),
                ));
            }
            return;
        }
        Ok(Some(func)) => func,
        Err(err) => {
            findings.push(Finding::error(rel, format!("reading query(): {err}")));
            return;
        }
    };
    let specs = match evaluate_query(input, &query_fn) {
        Ok(specs) => {
            validate_specs(&specs, events, rel, Context::Query, findings);
            Some(specs)
        }
        Err(err) => {
            findings.push(Finding::warning(
                rel,
                format!("could not statically evaluate query() with placeholder input: {err:#}"),
            ));
            None
        }
    };
    check_dispatch(
        module,
        "fold",
        Context::Fold,
        specs.as_deref(),
        events,
        rel,
        findings,
    );
}

/// Check an effect's boundary and its fold dispatch, the mirror of [`check_command`].
///
/// The difference is where the placeholder comes from. A command's `query` takes
/// `input`, so there is one schema to stub; an effect's takes the triggering event,
/// and its `handle` keys may name several types, so `query` is evaluated once per
/// subscribed type and the clauses are unioned. A type whose evaluation fails is a
/// warning for the same reason it is on a command: the placeholder may be the problem.
fn check_effect_boundary(
    sources: &[EventSpec],
    module: &FrozenModule,
    rel: &str,
    events: &EventRegistry,
    findings: &mut Vec<Finding>,
) {
    let query_fn = match module.get_option("query") {
        Ok(None) => {
            if matches!(module.get_option("fold"), Ok(Some(_))) {
                findings.push(Finding::warning(
                    rel,
                    "effect defines `fold` but no `query`, so there is no boundary to fold and handle() only ever sees `initial`; add a query(), or drop `fold`".to_owned(),
                ));
            }
            return;
        }
        Ok(Some(func)) => func,
        Err(err) => {
            findings.push(Finding::error(rel, format!("reading query(): {err}")));
            return;
        }
    };
    // Unlike a command, whose `query` still guards the append, an effect never
    // appends. A boundary with nothing folding it is read and discarded.
    if !matches!(module.get_option("fold"), Ok(Some(_))) {
        findings.push(Finding::warning(
            rel,
            "effect defines `query` but no `fold`, so the boundary is read and discarded and handle() only ever sees `initial`; add a fold, or drop `query`".to_owned(),
        ));
    }

    let mut declared: Vec<EventSpec> = Vec::new();
    for source in sources {
        // `all_events()` names no type, so there is no shape to build a placeholder
        // from; that effect's `query` is checked at runtime only.
        let EventSpec::Filter { event_type, .. } = source else {
            continue;
        };
        let Some(def) = events.by_type.get(event_type) else {
            continue; // Already reported by the `handle` pass.
        };
        match evaluate_effect_query(def, &query_fn) {
            Ok(specs) => {
                validate_specs(&specs, events, rel, Context::Query, findings);
                declared.extend(specs);
            }
            Err(err) => findings.push(Finding::warning(
                rel,
                format!(
                    "could not statically evaluate query() with a placeholder `{event_type}`: {err:#}"
                ),
            )),
        }
    }
    check_dispatch(
        module,
        "fold",
        Context::Fold,
        Some(&declared),
        events,
        rel,
        findings,
    );
}

/// Validate a dispatch map's keys, and for a command cross-check them against the
/// boundary.
///
/// The keys are query clauses, so they get the same constraint checks a `query` clause
/// gets, through [`validate_specs`]. That is not optional now that a key can carry a
/// filter: a `fold` arm constraining an unindexed field would otherwise match nothing
/// at runtime with nothing said at check time.
///
/// On top of that sits the one thing `validate_specs` cannot see, a key built by
/// calling `event(...)` inline. The loader's module-scope scan ([`crate::loader`]) only
/// sees definitions bound to a name, so an inline one inside a dict literal reaches
/// dispatch unregistered and would quietly work. An unregistered *type* needs no
/// separate report here, since `validate_specs` already rejects it.
///
/// A command's `fold` is also cross-checked against its boundary, but in one direction
/// only. An entry the boundary never returns is dead code, so it is worth reporting
/// (as a warning: `query` is evaluated with a placeholder input, so a branch the
/// placeholder did not take could legitimately name that type). The reverse is not
/// reported at all: the boundary is also the append condition, so a type can belong
/// there to make a concurrent write conflict without telling the decision anything new.
/// The cross-check is by type only, because `query`'s constraint *values* come from a
/// placeholder, so an arm made dead by its filter rather than its type is not visible.
fn check_dispatch(
    module: &FrozenModule,
    global: &str,
    context: Context,
    declared: Option<&[EventSpec]>,
    events: &EventRegistry,
    rel: &str,
    findings: &mut Vec<Finding>,
) {
    let Ok(Some(owned)) = module.get_option(global) else {
        return;
    };
    // A malformed map already failed the load with a precise message; repeating a
    // vaguer version of it here would only double the output.
    let Ok(dispatch) = parse_event_dispatch(owned.value()) else {
        return;
    };
    let specs = dispatch.specs();
    validate_specs(&specs, events, rel, context, findings);
    for spec in &specs {
        let (Some(event_type), Some(def_id)) = (spec.event_type(), spec.def_id()) else {
            continue; // `all_events()` is a builtin, not a definition.
        };
        if events
            .by_type
            .get(event_type)
            .is_some_and(|def| def.id != def_id)
        {
            findings.push(Finding::error(
                rel,
                format!(
                    "`{global}` maps event `{event_type}` through a definition declared outside events/; load() the events/ definition instead of calling event(type = \"{event_type}\", ...) again"
                ),
            ));
        }
    }
    let Some(declared) = declared else {
        return;
    };
    // `all_events()` names no types, so there is nothing to cross-check against.
    if declared.iter().any(|spec| matches!(spec, EventSpec::All)) {
        return;
    }
    for spec in &specs {
        let Some(event_type) = spec.event_type() else {
            continue;
        };
        if declared
            .iter()
            .any(|clause| clause.event_type() == Some(event_type))
        {
            continue;
        }
        findings.push(Finding::warning(
            rel,
            format!(
                "`{global}` has an entry for `{event_type}`, which query does not include, so it never runs; add a `{event_type}(...)` clause to query(), or drop the entry"
            ),
        ));
    }
}

/// Evaluate `query(input)` in query mode with a placeholder input, returning the
/// clauses it declares.
fn evaluate_query(
    schema: &InputSchema,
    query_fn: &OwnedFrozenValue,
) -> anyhow::Result<Vec<EventSpec>> {
    Module::with_temp_heap(|module| {
        let stub = stub_payload(schema);
        let input = alloc_input(&module, schema, &stub)?;
        let func = thaw(query_fn, &module);
        let result = call_handler_with_query_ctx(&module, func, &[input], MAX_TICKS)
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        parse_event_specs(result)
    })
}

/// Evaluate an effect's `query` against a placeholder event of one subscribed type.
///
/// The same three calls the effect runtime makes, which is what keeps the check and
/// the runtime from disagreeing about what a clause means. The placeholder is built
/// from the event's own declared fields, so a `query` reading `event.data.shop_id`
/// resolves; a subject field arrives as a handle here exactly as it does live.
fn evaluate_effect_query(
    def: &EventDef,
    query_fn: &OwnedFrozenValue,
) -> anyhow::Result<Vec<EventSpec>> {
    Module::with_temp_heap(|module| {
        let stub = stub_event_payload(def);
        let event = alloc_event(
            &module,
            Uuid::nil(),
            STUB_TIMESTAMP,
            &def.event_type,
            &stub,
            Some(def),
        );
        let func = thaw(query_fn, &module);
        let result = call_handler_with_query_ctx(&module, func, &[event], MAX_TICKS)
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        parse_event_specs(result)
    })
}

fn validate_specs(
    specs: &[EventSpec],
    events: &EventRegistry,
    rel: &str,
    context: Context,
    findings: &mut Vec<Finding>,
) {
    let ctx = context.label();
    for spec in specs {
        let EventSpec::Filter {
            event_type,
            constraints,
            ..
        } = spec
        else {
            continue; // `all_events()` matches everything; nothing to validate.
        };
        let Some(def) = events.by_type.get(event_type) else {
            findings.push(Finding::error(
                rel,
                format!("{ctx} references unknown event type `{event_type}`"),
            ));
            continue;
        };
        for (field, value) in constraints {
            validate_constraint(def, field, value, constraints, rel, context, findings);
        }
        if context == Context::Query {
            check_selectivity(def, constraints, event_type, rel, findings);
        }
    }
}

/// Check one field constraint of a clause against the event's schema.
fn validate_constraint(
    def: &EventDef,
    field: &str,
    value: &str,
    constraints: &[(String, String)],
    rel: &str,
    context: Context,
    findings: &mut Vec<Finding>,
) {
    let ctx = context.label();
    let Some(meta) = def.field(field) else {
        findings.push(Finding::error(
            rel,
            format!(
                "{ctx} filters event `{}` on `{field}`, which it does not declare",
                def.event_type
            ),
        ));
        return;
    };
    if !meta.indexed {
        findings.push(Finding::error(
            rel,
            format!(
                "{ctx} filters event `{}` on `{field}`, which is not indexed (indexed = False)",
                def.event_type
            ),
        ));
        return;
    }
    if !value_matches_kind(&meta.kind, value) {
        findings.push(Finding::error(
            rel,
            format!(
                "{ctx} filters event `{}` on `{field}` with a value that is not a valid {:?}",
                def.event_type,
                meta.kind.base()
            ),
        ));
    }
    if let Some(subject_field) = &meta.subject {
        if !context.can_filter_encrypted() {
            findings.push(Finding::error(
                rel,
                format!(
                    "{ctx} filters event `{}` on subject-encrypted `{field}`; a subscription can only filter plaintext fields (filter by the subject id `{subject_field}` instead)",
                    def.event_type
                ),
            ));
            return;
        }
        let subject_constrained = constraints.iter().any(|(f, _)| f == subject_field);
        if !subject_constrained && !meta.unique {
            findings.push(Finding::error(
                rel,
                format!(
                    "{ctx} filters event `{}` on subject-encrypted `{field}` without its subject `{subject_field}`; also constrain `{subject_field}` (scoped), or mark `{field}` unique (global)",
                    def.event_type
                ),
            ));
        }
    }
}

/// Warn on a command boundary that guards weakly: no constraint on a high-cardinality
/// field (so it fires on a broad set of events, defeating the append fast-reject), or
/// a clause that pins most of an event's fields (a copied emit call, likely matching
/// nothing).
fn check_selectivity(
    def: &EventDef,
    constraints: &[(String, String)],
    event_type: &str,
    rel: &str,
    findings: &mut Vec<Finding>,
) {
    let selective = constraints.iter().any(|(field, _)| {
        def.field(field)
            .is_some_and(|meta| is_high_cardinality(&meta.kind))
    });
    if !selective {
        findings.push(Finding::warning(
            rel,
            format!(
                "query clause on `{event_type}` has no constraint on a high-cardinality field, so it guards on a broad set of events; boundaries are best keyed on an entity id"
            ),
        ));
    }
    // Only meaningful once an event has several fields; pinning the one field of a
    // single-field event is the normal way to query it, not a copied emit call.
    if def.fields.len() >= 4
        && (constraints.len() as f64) >= (def.fields.len() as f64) * OVER_CONSTRAINT_RATIO
    {
        findings.push(Finding::warning(
            rel,
            format!(
                "query clause on `{event_type}` constrains {} of {} fields, which looks like a copied emit call; a query is a subset match and over-constraining can match nothing",
                constraints.len(),
                def.fields.len()
            ),
        ));
    }
}

/// A field kind whose values are numerous enough to make a good, selective guard.
fn is_high_cardinality(kind: &FieldKind) -> bool {
    matches!(
        kind.base(),
        FieldKind::Uuid
            | FieldKind::U64
            | FieldKind::I64
            | FieldKind::Text { .. }
            | FieldKind::Money
            | FieldKind::Timestamp
    )
}

/// A rough type check of a constraint's string value against a field kind. Query
/// constraint values arrive as the scalar's canonical string (so a bool is
/// `true`/`false`, a `u64` its full decimal), which is a different convention from a
/// URL filter, so this is intentionally not [`crate::read_model::coerce_value`].
fn value_matches_kind(kind: &FieldKind, value: &str) -> bool {
    match kind.base() {
        FieldKind::I64 => value.parse::<i64>().is_ok(),
        FieldKind::U64 => value.parse::<u64>().is_ok(),
        FieldKind::Bool => matches!(value, "true" | "false"),
        FieldKind::OneOf(variants) => variants.iter().any(|variant| variant == value),
        _ => true,
    }
}

fn stub_payload(schema: &InputSchema) -> serde_json::Value {
    let mut obj = serde_json::Map::with_capacity(schema.fields.len());
    for (name, kind) in &schema.fields {
        obj.insert(name.clone(), stub_value(kind));
    }
    serde_json::Value::Object(obj)
}

/// The event-shaped counterpart of [`stub_payload`], for an effect's `query`.
fn stub_event_payload(def: &EventDef) -> serde_json::Value {
    let mut obj = serde_json::Map::with_capacity(def.fields.len());
    for (name, meta) in &def.fields {
        obj.insert(name.clone(), stub_value(&meta.kind));
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
