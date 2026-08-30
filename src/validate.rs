//! The checks heklang does not make, because they are hekla's to make.
//!
//! Almost everything `validate.rs` used to do is gone, and it is gone because heklang
//! now decides it. An unknown event type, an undeclared field, a constraint value of
//! the wrong type, a filter on an unindexed field and a filter on sealed content are
//! all parse-time errors there, and a `fold` without a `query` is not expressible at
//! all: a `state` **is** its own slice declaration.
//!
//! What is left is advice rather than law. Each of these is a judgement about a design
//! that parses perfectly well, which is why every one is a warning and why they live
//! here rather than in the language.

use heklang::ir::{Command, Slice, Type};
use heklang::{Defs, Program};

use crate::loader::{Finding, LoadedProject, ProjectorUnit};
use crate::schema::{EventDef, FieldKind, event_type};
use crate::tags::RESERVED_TAG_PREFIX;

/// Field names that usually mean personal data. A field whose name matches one and
/// which carries no `@subject` can never be erased, which is worth saying out loud
/// even though nothing about it is wrong.
const PERSONAL_NAME_HINTS: [&str; 9] = [
    "email", "phone", "address", "name", "dob", "ssn", "postcode", "zip", "birth",
];

/// How much of an event a clause may pin before it looks like a copied `emit`. A
/// boundary is a subset match, and one that names nearly every field usually matches
/// nothing.
const OVER_CONSTRAINT_RATIO: f64 = 0.75;

/// Every finding for a loaded project. The loader has already reported anything that
/// stopped a declaration parsing; these are the ones that need the whole picture.
pub fn check(project: &LoadedProject) -> Vec<Finding> {
    let mut findings = Vec::new();
    check_events(&project.events, &mut findings);
    check_entities(&project.projectors, &mut findings);
    let defs = Defs::of(&project.program);
    for command in &project.program.commands {
        let location = command.module.clone().unwrap_or_default();
        check_boundary(command, &project.program, defs, &location, &mut findings);
    }
    findings
}

/// Two things about an event's fields: that none of them occupies hekla's tag
/// namespace, and that a personal-looking one can be erased.
fn check_events(events: &crate::schema::EventDefs, findings: &mut Vec<Finding>) {
    let mut sorted: Vec<(&String, &EventDef)> = events.iter().collect();
    sorted.sort_by_key(|(event_type, _)| *event_type);
    for (event_type, def) in sorted {
        for (name, meta) in &def.fields {
            // An indexed field becomes a tag named after it, and hekla's own tags live
            // under this prefix. A field here could forge the idempotency tag an append
            // condition is guarded against, so the namespace is closed to programs.
            // heklang has no idea the prefix means anything, which is why this is here.
            if name.starts_with(RESERVED_TAG_PREFIX) {
                findings.push(Finding::error(
                    "events",
                    format!(
                        "event `{event_type}` field `{name}` uses the reserved \
                         `{RESERVED_TAG_PREFIX}` prefix, which is hekla's own tag namespace"
                    ),
                ));
            }
            if meta.subject.is_some() {
                continue;
            }
            let lowered = name.to_lowercase();
            if PERSONAL_NAME_HINTS
                .iter()
                .any(|hint| lowered.contains(hint))
            {
                findings.push(Finding::warning(
                    "events",
                    format!(
                        "event `{event_type}` field `{name}` looks like personal data but has no \
                         `@subject`, so it could never be erased"
                    ),
                ));
            }
        }
    }
}

/// A sealed column has to be able to say it is absent.
///
/// Erasure destroys the key and rewrites nothing, so hekla answers a column it cannot
/// decrypt with absence: `read_api` drops it from the response and `Rows::row` reads it
/// back as null. A column whose type cannot hold that breaks both boundaries at once,
/// and only once a real erasure has happened: the projector stalls for good on
/// `expected String, stored null` the next time a handler loads that row, and the read
/// API serves a body missing a field its own OpenAPI schema marks required.
///
/// An error rather than a warning, and the only one here, because the author has no
/// local signal to go on. `docs/projectors.md` rule 9 makes a subject *propagate* onto a
/// column rather than be declared on it, so the declaration that breaks reads as an
/// ordinary `String` and names no subject at all.
fn check_entities(projectors: &[ProjectorUnit], findings: &mut Vec<Finding>) {
    for unit in projectors {
        for entity in &unit.entities {
            for (name, meta) in &entity.fields {
                let Some(subject_field) = &meta.subject else {
                    continue;
                };
                if matches!(meta.kind, FieldKind::Optional(_)) {
                    continue;
                }
                findings.push(Finding::error(
                    unit.rel_path.clone(),
                    format!(
                        "column `{name}` of entity `{}` is sealed under `{subject_field}`, so \
                         erasing that subject leaves it absent, but its declared type cannot \
                         be absent: make it optional",
                        entity.name
                    ),
                ));
            }
        }
    }
}

/// Two judgements about a command's boundary: that it is narrow enough to be worth
/// having, and that it is not so narrow it can never match.
fn check_boundary(
    command: &Command,
    program: &Program,
    defs: Defs<'_>,
    location: &str,
    findings: &mut Vec<Finding>,
) {
    for slice in &command.slices {
        let Some(declared) = program.event(&slice.event) else {
            continue;
        };
        let path = event_type(&slice.event);

        if !slice
            .filters
            .iter()
            .any(|filter| is_selective(declared, &filter.field, defs))
        {
            findings.push(Finding::warning(
                location,
                format!(
                    "`{}` folds `{path}` with no constraint on a high-cardinality field, so it \
                     guards a broad set of events; a boundary is best keyed on an entity id",
                    command.name
                ),
            ));
        }

        if over_constrained(declared, slice) {
            findings.push(Finding::warning(
                location,
                format!(
                    "`{}` constrains most of `{path}`'s fields, which looks like a copied `emit`; \
                     a slice is a subset match and over-constraining can match nothing",
                    command.name
                ),
            ));
        }
    }
}

/// Whether narrowing on this field meaningfully narrows the log. A bool or a small
/// enum does not; an id does.
fn is_selective(declared: &heklang::ir::EventDef, field: &str, defs: Defs<'_>) -> bool {
    let Some(def) = declared.field(field) else {
        return false;
    };
    matches!(
        FieldKind::of(&def.ty, defs),
        FieldKind::Uuid
            | FieldKind::I64
            | FieldKind::Text { .. }
            | FieldKind::Money { .. }
            | FieldKind::Timestamp
    ) && !matches!(def.ty, Type::Bool)
}

fn over_constrained(declared: &heklang::ir::EventDef, slice: &Slice) -> bool {
    let fields = declared.fields.len();
    if fields < 4 {
        return false;
    }
    slice.filters.len() as f64 / fields as f64 >= OVER_CONSTRAINT_RATIO
}
