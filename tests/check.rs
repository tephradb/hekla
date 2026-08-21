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

/// A shared event file used by the temp-project cases.
const EVENTS: &str = r#"
thing_happened = event(
    type = "thing.happened",
    fields = {"thing_id": uuid(), "note": text()},
    tags = ["thing_id"],
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
fn query_on_undeclared_tag_is_an_error() {
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "commands/do-thing.star",
            r#"
load("events/thing.star", "thing_happened")

input = schema(thing_id = uuid(), note = text())

def query(input):
    return events(types = ["thing.happened"], tags = {"note": input.note})

def handle(input, state):
    return emit(thing_happened(thing_id = input.thing_id, note = input.note))
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("does not declare as a tag")),
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
fn projector_source_on_unknown_type_is_an_error() {
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "projectors/things.star",
            r#"
things = entity(key = "thing_id", fields = {"thing_id": uuid()})

source = events(types = ["thing.happen"])

def handle(event):
    return [put(things, {"thing_id": event.data["thing_id"]})]
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("unknown event type `thing.happen`")),
        "{errs:?}"
    );
}

#[test]
fn projector_index_on_unknown_field_is_an_error() {
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "projectors/things.star",
            r#"
things = entity(
    key = "thing_id",
    fields = {"thing_id": uuid()},
    indexes = [index("by_note", ["note"])],
)

source = events(types = ["thing.happened"])

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
fn parse_error_is_reported_without_crashing() {
    let dir = write_project(&[("commands/broken.star", "def handle(input, state)\n")]);
    let project = LoadedProject::load(dir.path());
    assert!(project.has_errors());
}
