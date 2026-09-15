//! The checks heklang does not make, because they are hekla's to make.
//!
//! Almost everything `validate.rs` used to do is gone, and it is gone because heklang
//! now decides it. An unknown event type, an undeclared field, a constraint value of
//! the wrong type, a filter on an unindexed field and a filter on sealed content are
//! all parse-time errors there, and a `fold` without a `query` is not expressible at
//! all: a `state` **is** its own slice declaration.
//!
//! What is left is two kinds of thing. A declaration hekla cannot serve is an error,
//! because only hekla knows its own tag namespace and what an erasure does to a column.
//! The rest is advice: a judgement about a design that parses perfectly well and that
//! hekla has no business refusing, which is why those stay warnings.

use heklang::ir::{Command, Slice, Type};
use heklang::{Defs, Program};

use crate::loader::{Finding, LoadedProject, ProjectorUnit, Severity, Span};
use crate::schema::{EventDef, FieldKind, event_type};
use crate::tags::RESERVED_TAG_PREFIX;

/// How much of an event a clause may pin before it looks like a copied `emit`. A
/// boundary is a subset match, and one that names nearly every field usually matches
/// nothing.
const OVER_CONSTRAINT_RATIO: f64 = 0.75;

/// Everything wrong with a loaded project: the loader's findings plus [`check`]'s,
/// sorted by location and then by position.
///
/// Here rather than in `cli`, because it is not a CLI concern. `hekla check` reports
/// these, `hekla test` refuses over them, and `POST /admin/projections` hands them back
/// to whoever posted the source; a server reaching into the CLI module to format a
/// diagnostic would have the dependency the wrong way round.
pub fn findings(project: &LoadedProject) -> Vec<Finding> {
    let mut findings = project.findings.clone();
    findings.extend(check(project));
    findings.sort_by(|left, right| {
        let position = |finding: &Finding| finding.span.map(|span| (span.line, span.column));
        left.location
            .cmp(&right.location)
            .then_with(|| position(left).cmp(&position(right)))
    });
    findings
}

/// One finding as a line: `<severity>: <location>[:line:col]: <message>`, with the
/// compiler's hint on a following `  = ` line when there is one.
///
/// One rendering, wherever a finding surfaces: `hekla check` on stdout, `hekla openapi`
/// and `hekla project` on stderr so their stdout stays parseable, and the `findings`
/// array of a 400 from `POST /admin/projections`.
pub fn render(finding: &Finding) -> String {
    let severity = match finding.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    // Spans are 0-based; editors and humans count from one.
    let at = match finding.span {
        Some(span) => format!(":{}:{}", span.line + 1, span.column + 1),
        None => String::new(),
    };
    let line = format!("{severity}: {}{at}: {}", finding.location, finding.message);
    // heklang carries the fix on a separate hint, and a diagnostic that names the
    // problem without it is the worse half of the message.
    match &finding.hint {
        Some(hint) => format!("{line}\n  = {hint}"),
        None => line,
    }
}

/// How many findings are errors, which is what every load-and-refuse path branches on.
pub fn errors(findings: &[Finding]) -> usize {
    findings
        .iter()
        .filter(|finding| finding.severity == Severity::Error)
        .count()
}

/// Every finding for a loaded project. The loader has already reported anything that
/// stopped a declaration parsing; these are the ones that need the whole picture.
pub fn check(project: &LoadedProject) -> Vec<Finding> {
    let mut findings = Vec::new();
    check_events(&project.events, &mut findings);
    check_entities(&project.projectors, &mut findings);
    check_secrets(project, &mut findings);
    let defs = Defs::of(&project.program);
    for command in &project.program.commands {
        let location = command.module.clone().unwrap_or_default();
        check_boundary(command, &project.program, defs, &location, &mut findings);
    }
    findings
}

/// One thing about an event's fields: that none of them occupies hekla's tag
/// namespace.
fn check_events(events: &crate::schema::EventDefs, findings: &mut Vec<Finding>) {
    let mut sorted: Vec<(&String, &EventDef)> = events.iter().collect();
    sorted.sort_by_key(|(event_type, _)| *event_type);
    for (event_type, def) in sorted {
        for (name, _) in &def.fields {
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

/// Three judgements about a command's boundary: that it is narrow enough to be worth
/// having, that it is not so narrow it can never match, and that it is not keyed on a
/// field the log's older events were never tagged with.
fn check_boundary(
    command: &Command,
    program: &Program,
    defs: Defs<'_>,
    location: &str,
    findings: &mut Vec<Finding>,
) {
    // Every slice the command declares, whichever run declared it. A lint about the
    // shape of a boundary does not care which read resolved it.
    for slice in command.stages.iter().flat_map(|stage| &stage.slices) {
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

        // Every field is auto-tagged, so an event written before a field existed carries
        // no tag for it and a filter on it matches none of them. The absent value does
        // not help: it is read from the decoded payload, and a slice never gets that far.
        //
        // Keyed on the annotation because it is the only thing in the source that says a
        // field is younger than the log; whether the log *has* older events is a question
        // this pass has no data directory to ask. So it can say so about a field that has
        // always been there and carries `@absent` against a future it never needed, and
        // it cannot say so about one added later as `T?`, which has the same defect and
        // no annotation to give it away.
        let younger: Vec<&str> = slice
            .filters
            .iter()
            .filter(|filter| {
                declared
                    .fields
                    .iter()
                    .any(|field| field.name == filter.field && field.absent.is_some())
            })
            .map(|filter| filter.field.as_str())
            .collect();
        // One finding per slice, not per filter: two such filters are one design and one
        // thing to say about it.
        if !younger.is_empty() {
            findings.push(Finding::warning(
                location,
                format!(
                    "`{}` folds `{path}` on {}, which `@absent` marks as younger than the log; an \
                     event written before that existed carries no tag for it, so this slice can \
                     only ever match events appended since",
                    command.name,
                    younger
                        .iter()
                        .map(|field| format!("`{field}`"))
                        .collect::<Vec<_>>()
                        .join(" and "),
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

/// Two things about the credentials a project declares, and deliberately not a third.
///
/// **Neither of these reads the environment.** `hekla check` is the CI gate, and a gate
/// that needed production credentials to pass would either be run with them, which is
/// worse than the problem, or skipped. Whether a declared credential is actually *set* is
/// `hekla plan`'s question against a target, and `Runtime::open`'s refusal at boot.
///
/// Both are warnings: each names a line that parses and does nothing, which is advice
/// about a project rather than something hekla cannot serve.
fn check_secrets(project: &LoadedProject, findings: &mut Vec<Finding>) {
    // What runs, with local names and layout taken out. A read site renders as
    // `(secret NAME)`, and a `secret` declaration has no entry of its own, so this
    // answers "does anything read it" without walking the expression arena. Tests are
    // excluded on purpose: a credential only a `test` reaches is one production never
    // uses, which is exactly what the warning is for.
    let packed = project.digest.packed();
    for def in &project.program.secrets {
        if read_somewhere(&packed, &def.name) {
            continue;
        }
        let location = def.module.clone().unwrap_or_default();
        findings.push(
            Finding::warning(
                location,
                format!(
                    "`{}` is declared and nothing reads it, so this deployment is asked for a credential the program never uses",
                    def.name
                ),
            )
            .with_span(Span {
                line: def.span.start.line,
                column: def.span.start.col,
            })
            .with_hint("delete the declaration, or use it in an effect arm"),
        );
    }
    // Only when the program actually checked. `LoadedProject::load` substitutes an empty
    // `Program` when it did not, so every configured credential would otherwise be
    // reported as undeclared and the author would be told to write a declaration they
    // have already written, once per credential, stacked on top of the real error.
    if project.has_errors() {
        return;
    }
    for name in project.config.secrets.keys() {
        if project.program.secret(name).is_some() {
            continue;
        }
        findings.push(
            Finding::warning(
                crate::config::FILE_NAME,
                format!("[secrets] names `{name}`, which the project does not declare"),
            )
            .with_hint(format!(
                "declare `secret {name}` in a .hk file, or drop the entry: nothing reads it"
            )),
        );
    }
}

/// Whether the packed digest holds a read of this credential.
///
/// A substring search, but not a naive one: `(secret STRIPE_KEY)` must not be found by a
/// search for `STRIPE`, and the atom has two shapes (`(secret NAME)` for a required one,
/// `(secret NAME optional)` for the other), so the character after the name decides.
fn read_somewhere(packed: &str, name: &str) -> bool {
    let needle = format!("(secret {name}");
    packed.match_indices(&needle).any(|(at, _)| {
        matches!(
            packed[at + needle.len()..].chars().next(),
            Some(')') | Some(' ')
        )
    })
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
