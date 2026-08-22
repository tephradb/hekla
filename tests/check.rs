//! End-to-end checks of the project loader and validation pass.

use std::fs;
use std::path::Path;

use kiln::loader::{Finding, LoadedProject, Severity};
use kiln::validate;
use tempfile::TempDir;

/// Write a throwaway project from `(relative path, contents)` pairs.
fn write_project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

/// All findings the loader and the validation pass produce together.
fn findings(project: &LoadedProject) -> Vec<Finding> {
    let mut all = project.findings.clone();
    all.extend(validate::check(project));
    all
}

fn errors(project: &LoadedProject) -> Vec<String> {
    findings(project)
        .into_iter()
        .filter(|finding| finding.severity == Severity::Error)
        .map(|finding| format!("{}: {}", finding.location, finding.message))
        .collect()
}

/// A shared event file used by the temp-project cases. `note` opts out of tagging,
/// so a query that filters on it is an error.
const EVENTS: &str = r#"
thing_happened = event(
    type = "thing.happened",
    fields = {"thing_id": uuid(), "note": text(indexed = False)},
)
"#;

#[test]
fn example_project_checks_clean() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);

    let errs = errors(&project);
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    assert_eq!(project.commands.len(), 4);
    assert_eq!(project.projectors.len(), 2);
    assert_eq!(project.effects.len(), 1);
    assert_eq!(project.events.by_type.len(), 4);

    let internal = project
        .commands
        .iter()
        .find(|unit| unit.loaded.def.name() == "record-welcome")
        .expect("record-welcome command");
    assert!(internal.internal, "record-welcome should be internal");

    let public = project
        .commands
        .iter()
        .find(|unit| unit.loaded.def.name() == "register-user")
        .expect("register-user command");
    assert!(!public.internal, "register-user should be public");
}

#[test]
fn orders_example_checks_clean() {
    // The subject-encryption example checks statically with no master key: check is
    // a load-and-validate pass, not a runtime.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/orders");
    let project = LoadedProject::load(&root);
    let errs = errors(&project);
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    assert_eq!(project.commands.len(), 1);
    assert_eq!(project.projectors.len(), 1);
    assert_eq!(project.effects.len(), 1);
}

#[test]
fn query_on_non_indexed_field_is_an_error() {
    // `note` is declared `indexed = False`, so it never becomes a tag; a query that
    // filters on it would silently match nothing at runtime.
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "commands/do-thing.star",
            r#"
load("events/thing.star", "thing_happened")

input = schema(thing_id = uuid(), note = text())

def query(input):
    return thing_happened(note = input.note)

def handle(input, state):
    return emit(thing_happened(thing_id = input.thing_id, note = input.note))
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter().any(|err| err.contains("is not indexed")),
        "{errs:?}"
    );
}

#[test]
fn command_cannot_load_another_command() {
    let dir = write_project(&[
        (
            "commands/a.star",
            "input = schema(x = text())\n\ndef handle(input, state):\n    return emit([])\n",
        ),
        (
            "commands/b.star",
            "load(\"commands/a.star\", \"handle\")\n\ninput = schema(x = text())\n\ndef handle(input, state):\n    return emit([])\n",
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("may only load from events/ or lib/")),
        "{errs:?}"
    );
}

#[test]
fn missing_handle_is_an_error() {
    let dir = write_project(&[("commands/no-handle.star", "input = schema(x = text())\n")]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("missing required `handle`")),
        "{errs:?}"
    );
}

#[test]
fn projector_source_on_an_undefined_event_is_an_error() {
    // A typed source names event types by calling their definitions, so a typo is an
    // undefined-variable error rather than a string that silently matches nothing.
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "thing_happened")

things = entity(key = "thing_id", fields = {"thing_id": uuid()})

source = [thing_happend()]

def handle(event):
    return [put(things, {"thing_id": event.data["thing_id"]})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    assert!(
        project.has_errors(),
        "expected a load error for the undefined event"
    );
}

#[test]
fn projector_index_on_unknown_field_is_an_error() {
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "thing_happened")

things = entity(
    key = "thing_id",
    fields = {"thing_id": uuid()},
    indexes = [index("by_note", ["note"])],
)

source = [thing_happened()]

def handle(event):
    return [put(things, {"thing_id": event.data["thing_id"]})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter().any(|err| err.contains("unknown field `note`")),
        "{errs:?}"
    );
}

#[test]
fn non_scalar_entity_key_is_an_error() {
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "thing_happened")

things = entity(
    key = "active",
    fields = {"thing_id": uuid(), "active": boolean()},
)

source = [thing_happened()]

def handle(event):
    return [put(things, {"thing_id": event.data["thing_id"], "active": True})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("must be an orderable scalar")),
        "{errs:?}"
    );
}

#[test]
fn money_entity_key_is_an_error() {
    // Money is stored as its decimal string, so `ORDER BY` and the cursor comparison
    // would sort it lexicographically (`"2" > "10"`); it cannot key the ordered scan.
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "thing_happened")

things = entity(
    key = "price",
    fields = {"thing_id": uuid(), "price": money()},
)

source = [thing_happened()]

def handle(event):
    return [put(things, {"thing_id": event.data["thing_id"], "price": "1.00"})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("must be an orderable scalar")),
        "{errs:?}"
    );
}

#[test]
fn filterable_field_colliding_with_a_reserved_query_param_is_an_error() {
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "thing_happened")

things = entity(
    key = "thing_id",
    fields = {"thing_id": uuid(), "cursor": text()},
    indexes = [index("by_cursor", ["cursor"])],
)

source = [thing_happened()]

def handle(event):
    return [put(things, {"thing_id": event.data["thing_id"]})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("reserved read query param")),
        "{errs:?}"
    );
}

#[test]
fn duplicate_event_type_is_an_error() {
    let dir = write_project(&[("events/a.star", EVENTS), ("events/b.star", EVENTS)]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter().any(|err| err.contains("already defined")),
        "{errs:?}"
    );
}

#[test]
fn event_field_in_the_reserved_namespace_is_an_error() {
    let dir = write_project(&[(
        "events/reserved.star",
        r#"
sneaky = event(
    type = "sneaky.happened",
    fields = {"_kiln_idem": text()},
)
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("reserved `_kiln_` prefix")),
        "{errs:?}"
    );
}

#[test]
fn unique_without_subject_is_an_error() {
    let dir = write_project(&[(
        "events/thing.star",
        r#"
thing = event(type = "thing.happened", fields = {"n": u64_(unique = True)})
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("unique = True requires")),
        "{errs:?}"
    );
}

#[test]
fn subject_on_a_json_field_is_an_error() {
    let dir = write_project(&[(
        "events/thing.star",
        r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "blob": json(subject = "owner")},
)
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("json field cannot be subject-encrypted")),
        "{errs:?}"
    );
}

#[test]
fn subject_text_without_max_length_is_an_error() {
    let dir = write_project(&[(
        "events/thing.star",
        r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "owner")},
)
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter().any(|err| err.contains("needs max_length")),
        "{errs:?}"
    );
}

#[test]
fn optional_subject_id_is_an_error() {
    let dir = write_project(&[(
        "events/thing.star",
        r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": optional(u64_()), "secret": text(subject = "owner", max_length = 50)},
)
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter().any(|err| err.contains("must not be optional")),
        "{errs:?}"
    );
}

#[test]
fn query_on_an_undeclared_field_is_an_error() {
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "commands/do-thing.star",
            r#"
load("events/thing.star", "thing_happened")

input = schema(x = text())

def query(input):
    return thing_happened(nonexistent = input.x)

def handle(input, state):
    return emit([])
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter().any(|err| err.contains("does not declare")),
        "{errs:?}"
    );
}

#[test]
fn subject_referencing_an_unknown_field_is_an_error() {
    let dir = write_project(&[(
        "events/thing.star",
        r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "nope", max_length = 50)},
)
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("is not a declared field")),
        "{errs:?}"
    );
}

#[test]
fn a_well_formed_subject_field_checks_clean() {
    let dir = write_project(&[(
        "events/thing.star",
        r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "owner", max_length = 50)},
)
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");
}

#[test]
fn entity_subject_column_without_its_id_is_an_error() {
    let dir = write_project(&[
        (
            "events/thing.star",
            r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "owner", max_length = 50)},
)
"#,
        ),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "thing")

things = entity(
    key = "id",
    fields = {"id": uuid(), "secret": text(subject = "owner", max_length = 50)},
)

source = [thing()]

def handle(event):
    return [put(things, {"id": event.data["owner"]})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("is not a declared field")),
        "{errs:?}"
    );
}

#[test]
fn index_on_a_subject_encrypted_column_is_an_error() {
    let dir = write_project(&[
        (
            "events/thing.star",
            r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "owner", max_length = 50)},
)
"#,
        ),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "thing")

things = entity(
    key = "owner",
    fields = {"owner": u64_(), "secret": text(subject = "owner", max_length = 50)},
    indexes = [index("by_secret", ["secret"])],
)

source = [thing()]

def handle(event):
    return [put(things, {"owner": event.data["owner"], "secret": event.data["secret"]})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("subject-encrypted column")),
        "{errs:?}"
    );
}

#[test]
fn query_on_a_subject_field_without_its_subject_is_an_error() {
    let dir = write_project(&[
        (
            "events/thing.star",
            r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "owner", max_length = 50)},
)
"#,
        ),
        (
            "commands/do-thing.star",
            r#"
load("events/thing.star", "thing")

input = schema(secret = text())

def query(input):
    return thing(secret = input.secret)

def handle(input, state):
    return emit([])
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter().any(|err| err.contains("without its subject")),
        "{errs:?}"
    );
}

#[test]
fn source_filtering_a_subject_encrypted_field_is_an_error() {
    let dir = write_project(&[
        (
            "events/thing.star",
            r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "owner", max_length = 50)},
)
"#,
        ),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "thing")

things = entity(key = "owner", fields = {"owner": u64_()})

source = [thing(secret = "x")]

def handle(event):
    return [put(things, {"owner": event.data["owner"]})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("can only filter plaintext fields")),
        "{errs:?}"
    );
}

#[test]
fn parse_error_is_reported_without_crashing() {
    let dir = write_project(&[("commands/broken.star", "def handle(input, state)\n")]);
    let project = LoadedProject::load(dir.path());
    assert!(project.has_errors());
}
