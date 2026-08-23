//! End-to-end checks of the project loader and validation pass.

use std::fs;
use std::process::ExitCode;

use kiln::loader::{LoadedProject, Severity};
use kiln::testing;

mod support;

use support::{
    ACCOUNT_EVENTS, REGISTER_ACCOUNT, assert_clean, assert_error, errors, example_dir, findings,
    write_project,
};

/// The warning-severity findings, rendered as `location: message`. The shared
/// harness only exposes the error half, and these cases are about the other one.
fn warnings(project: &LoadedProject) -> Vec<String> {
    findings(project)
        .into_iter()
        .filter(|finding| finding.severity == Severity::Warning)
        .map(|finding| format!("{}: {}", finding.location, finding.message))
        .collect()
}

/// A command body with no boundary, for the cases where only the `load()` or the
/// filename matters.
const TRIVIAL_COMMAND: &str =
    "input = schema(x = text())\n\ndef handle(input, state):\n    return []\n";

/// A shared event file used by the temp-project cases. `note` opts out of tagging,
/// so a query that filters on it is an error.
const EVENTS: &str = r#"
thing_happened = event(
    type = "thing.happened",
    fields = {"thing_id": uuid(), "note": text(indexed = False)},
)
"#;

/// An event with a well-formed subject field, shared by the subject cases.
const SUBJECT_EVENTS: &str = r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "owner", max_length = 50)},
)
"#;

#[test]
fn example_project_checks_clean() {
    let project = LoadedProject::load(&example_dir("users"));

    let errs = errors(&project);
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    // Lower bounds, not exact counts: the point is that no directory silently failed
    // to load (including `commands/internal/`), and the named lookups below pin the
    // specific modules.
    assert!(project.commands.len() >= 4, "every example command loads");
    assert!(
        project.projectors.len() >= 2,
        "every example projector loads"
    );
    assert!(!project.effects.is_empty(), "the example effect loads");
    assert!(
        project.events.by_type.len() >= 4,
        "every example event type loads"
    );

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
    let project = LoadedProject::load(&example_dir("orders"));
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
    assert_error(
        &[
            ("events/thing.star", EVENTS),
            (
                "commands/do-thing.star",
                r#"
load("events/thing.star", "thing_happened")

input = schema(thing_id = uuid(), note = text())

def query(input):
    return thing_happened(note = input.note)

def handle(input, state):
    return thing_happened(thing_id = input.thing_id, note = input.note)
"#,
            ),
        ],
        "is not indexed",
    );
}

#[test]
fn command_cannot_load_another_command() {
    assert_error(
        &[
            (
                "commands/a.star",
                "input = schema(x = text())\n\ndef handle(input, state):\n    return []\n",
            ),
            (
                "commands/b.star",
                "load(\"commands/a.star\", \"handle\")\n\ninput = schema(x = text())\n\ndef handle(input, state):\n    return []\n",
            ),
        ],
        "may only load from events/ or lib/",
    );
}

#[test]
fn missing_handle_is_an_error() {
    assert_error(
        &[("commands/no-handle.star", "input = schema(x = text())\n")],
        "missing required `handle`",
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
    assert_error(
        &[
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
        ],
        "unknown field `note`",
    );
}

#[test]
fn non_scalar_entity_key_is_an_error() {
    assert_error(
        &[
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
        ],
        "must be an orderable scalar",
    );
}

#[test]
fn money_entity_key_is_an_error() {
    // Money is stored as its decimal string, so `ORDER BY` and the cursor comparison
    // would sort it lexicographically (`"2" > "10"`); it cannot key the ordered scan.
    assert_error(
        &[
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
        ],
        "must be an orderable scalar",
    );
}

#[test]
fn filterable_field_colliding_with_a_reserved_query_param_is_an_error() {
    assert_error(
        &[
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
        ],
        "reserved read query param",
    );
}

#[test]
fn duplicate_event_type_is_an_error() {
    assert_error(
        &[("events/a.star", EVENTS), ("events/b.star", EVENTS)],
        "already defined",
    );
}

#[test]
fn re_exporting_an_event_is_not_a_duplicate() {
    // A second events module that `load()`s an event definition re-exports its
    // symbol, but that is one definition referenced twice, not a type collision.
    let dir = write_project(&[
        (
            "events/a.star",
            r#"thing = event(type = "thing.happened", fields = {"thing_id": uuid()})"#,
        ),
        (
            "events/b.star",
            r#"
load("events/a.star", "thing")
other = event(type = "other.happened", fields = {"other_id": uuid()})
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    assert_eq!(project.events.by_type.len(), 2);
}

#[test]
fn event_field_in_the_reserved_namespace_is_an_error() {
    assert_error(
        &[(
            "events/reserved.star",
            r#"
sneaky = event(
    type = "sneaky.happened",
    fields = {"_kiln_idem": text()},
)
"#,
        )],
        "reserved `_kiln_` prefix",
    );
}

#[test]
fn unique_without_subject_is_an_error() {
    assert_error(
        &[(
            "events/thing.star",
            r#"
thing = event(type = "thing.happened", fields = {"n": u64_(unique = True)})
"#,
        )],
        "unique = True requires",
    );
}

#[test]
fn subject_on_a_json_field_is_an_error() {
    assert_error(
        &[(
            "events/thing.star",
            r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "blob": json(subject = "owner")},
)
"#,
        )],
        "json field cannot be subject-encrypted",
    );
}

#[test]
fn subject_text_without_max_length_is_an_error() {
    assert_error(
        &[(
            "events/thing.star",
            r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "owner")},
)
"#,
        )],
        "needs max_length",
    );
}

#[test]
fn optional_subject_id_is_an_error() {
    assert_error(
        &[(
            "events/thing.star",
            r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": optional(u64_()), "secret": text(subject = "owner", max_length = 50)},
)
"#,
        )],
        "must not be optional",
    );
}

#[test]
fn query_on_an_undeclared_field_is_an_error() {
    assert_error(
        &[
            ("events/thing.star", EVENTS),
            (
                "commands/do-thing.star",
                r#"
load("events/thing.star", "thing_happened")

input = schema(x = text())

def query(input):
    return thing_happened(nonexistent = input.x)

def handle(input, state):
    return []
"#,
            ),
        ],
        "does not declare",
    );
}

#[test]
fn subject_referencing_an_unknown_field_is_an_error() {
    assert_error(
        &[(
            "events/thing.star",
            r#"
thing = event(
    type = "thing.happened",
    fields = {"owner": u64_(), "secret": text(subject = "nope", max_length = 50)},
)
"#,
        )],
        "is not a declared field",
    );
}

#[test]
fn a_well_formed_subject_field_checks_clean() {
    assert_clean(&[("events/thing.star", SUBJECT_EVENTS)]);
}

#[test]
fn entity_subject_column_without_its_id_is_an_error() {
    assert_error(
        &[
            ("events/thing.star", SUBJECT_EVENTS),
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
        ],
        "is not a declared field",
    );
}

#[test]
fn index_on_a_subject_encrypted_column_is_an_error() {
    assert_error(
        &[
            ("events/thing.star", SUBJECT_EVENTS),
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
        ],
        "subject-encrypted column",
    );
}

#[test]
fn query_on_a_subject_field_without_its_subject_is_an_error() {
    assert_error(
        &[
            ("events/thing.star", SUBJECT_EVENTS),
            (
                "commands/do-thing.star",
                r#"
load("events/thing.star", "thing")

input = schema(secret = text())

def query(input):
    return thing(secret = input.secret)

def handle(input, state):
    return []
"#,
            ),
        ],
        "without its subject",
    );
}

#[test]
fn source_filtering_a_subject_encrypted_field_is_an_error() {
    assert_error(
        &[
            ("events/thing.star", SUBJECT_EVENTS),
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
        ],
        "can only filter plaintext fields",
    );
}

#[test]
fn parse_error_is_reported_without_crashing() {
    let dir = write_project(&[("commands/broken.star", "def handle(input, state)\n")]);
    let project = LoadedProject::load(dir.path());
    assert!(project.has_errors());
}

#[test]
fn event_defined_in_lib_is_an_error() {
    // Only `events/` modules feed the registry, so an event defined anywhere else is
    // invisible to dispatch: its `subject` fields would reach the log as plaintext.
    let dir = write_project(&[("lib/thing.star", SUBJECT_EVENTS)]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("lib/thing.star") && err.contains("`thing.happened`")),
        "{errs:?}"
    );
    assert!(project.events.by_type.is_empty());
}

#[test]
fn event_defined_in_a_command_is_an_error() {
    let dir = write_project(&[(
        "commands/do-thing.star",
        r#"
thing = event(type = "thing.happened", fields = {"thing_id": uuid()})

input = schema(thing_id = uuid())

def handle(input, state):
    return thing(thing_id = input.thing_id)
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.contains("commands/do-thing.star") && err.contains("`thing.happened`")),
        "{errs:?}"
    );
}

#[test]
fn an_event_may_be_re_bound_under_a_second_name() {
    // Binding a loaded definition to another name looks exactly like a fresh
    // definition from the outside: the frozen module exports an `EventDef` under a
    // name that is not the `load()` local. It is still the one definition in
    // `events/`, so the registry has it and nothing is invisible to dispatch.
    let dir = write_project(&[
        (
            "events/thing.star",
            r#"thing_done = event(type = "thing.done", fields = {"thing_id": uuid()})"#,
        ),
        (
            "lib/alias.star",
            r#"
load("events/thing.star", "thing_done")

ThingDone = thing_done
"#,
        ),
        (
            "commands/do-thing.star",
            r#"
load("events/thing.star", "thing_done")

EVT = thing_done

input = schema(thing_id = uuid())

def handle(input, state):
    return EVT(thing_id = input.thing_id)
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    assert!(errors(&project).is_empty(), "{:?}", project.findings);
    assert!(project.events.by_type.contains_key("thing.done"));
}

#[test]
fn a_lib_module_may_re_export_an_event_it_loads() {
    // A `lib/` file that `load()`s an event re-exports the symbol. That is a
    // reference to the one definition in `events/`, not a second definition.
    let dir = write_project(&[
        (
            "events/thing.star",
            r#"thing = event(type = "thing.happened", fields = {"thing_id": uuid()})"#,
        ),
        (
            "lib/helpers.star",
            r#"
load("events/thing.star", "thing")

def blank(value):
    return value.strip() == ""
"#,
        ),
        (
            "commands/do-thing.star",
            r#"
load("lib/helpers.star", "thing", "blank")

input = schema(thing_id = uuid(), note = text())

def handle(input, state):
    if blank(input.note):
        return reject("invalid_note", "note must not be blank")
    return thing(thing_id = input.thing_id)
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    assert_eq!(project.events.by_type.len(), 1);
}

/// A subtree the loader cannot walk used to vanish from the load, so `kiln check`
/// reported success on a project whose commands were only partly read.
#[cfg(unix)]
#[test]
fn an_unwalkable_project_subdirectory_is_reported() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let dir = write_project(&[(
        "commands/a.star",
        "input = schema(x = text())\n\ndef handle(input, state):\n    return []\n",
    )]);
    let blocked = dir.path().join("commands/internal");
    fs::create_dir_all(&blocked).unwrap();
    fs::write(blocked.join("b.star"), "input = schema()\n").unwrap();
    fs::set_permissions(&blocked, Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&blocked).is_ok() {
        return; // running as root, where the permission bits deny nothing
    }

    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    // Restore before the assertion so the temp dir can still be cleaned up.
    fs::set_permissions(&blocked, Permissions::from_mode(0o755)).unwrap();

    assert!(
        errs.iter()
            .any(|err| err.contains("walking the project tree")),
        "{errs:?}"
    );
}

#[test]
fn load_paths_cannot_escape_the_project_root() {
    // Path normalisation is the only thing keeping `load()` inside the project
    // tree; without it a project file becomes an arbitrary read of the host.
    let dir = write_project(&[
        (
            "commands/a.star",
            &format!("load(\"../secrets.star\", \"x\")\n\n{TRIVIAL_COMMAND}"),
        ),
        (
            "commands/b.star",
            &format!("load(\"/etc/passwd\", \"x\")\n\n{TRIVIAL_COMMAND}"),
        ),
        (
            "commands/c.star",
            &format!("load(\"lib/../commands/b.star\", \"x\")\n\n{TRIVIAL_COMMAND}"),
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);

    for (file, needle) in [
        ("commands/a.star", "must not contain `..`"),
        ("commands/b.star", "must be relative to the project root"),
        ("commands/c.star", "must not contain `..`"),
    ] {
        assert!(
            errs.iter()
                .any(|err| err.starts_with(file) && err.contains(needle)),
            "expected `{needle}` for {file}, got {errs:?}"
        );
    }
    // A file with an illegal dependency is never evaluated, so none of the three
    // reaches the deployment even though each would otherwise be a valid command.
    assert!(
        project.commands.is_empty(),
        "a command with an escaping load() must not load"
    );
}

#[test]
fn a_load_cycle_and_a_broken_library_are_reported() {
    // (a) A cycle: neither module can ever become ready, so the readiness loop must
    // terminate and report both rather than spin or drop them silently.
    let cyclic = write_project(&[
        ("lib/a.star", "load(\"lib/b.star\", \"y\")\nx = 1\n"),
        ("lib/b.star", "load(\"lib/a.star\", \"x\")\ny = 2\n"),
    ]);
    let project = LoadedProject::load(cyclic.path());
    let errs = errors(&project);
    for file in ["lib/a.star", "lib/b.star"] {
        assert!(
            errs.iter()
                .any(|err| err.starts_with(file) && err.contains("unresolved or cyclic load()")),
            "expected a cycle error for {file}, got {errs:?}"
        );
    }

    // (b) A library that fails to evaluate must take its dependents down with a
    // reported error, not leave them quietly missing from the deployment.
    let broken = write_project(&[
        ("lib/bad.star", "x = fail(\"boom\")\n"),
        (
            "commands/a.star",
            &format!("load(\"lib/bad.star\", \"x\")\n\n{TRIVIAL_COMMAND}"),
        ),
    ]);
    let project = LoadedProject::load(broken.path());
    let errs = errors(&project);
    assert!(
        errs.iter()
            .any(|err| err.starts_with("lib/bad.star") && err.contains("boom")),
        "expected the library's evaluation failure, got {errs:?}"
    );
    assert!(
        errs.iter().any(|err| err.starts_with("commands/a.star")
            && err.contains("could not be resolved to a known module")),
        "expected the dependent to be reported, got {errs:?}"
    );
    assert!(project.commands.is_empty());
}

#[test]
fn duplicate_module_names_and_bad_filenames_are_errors() {
    // Names come from the file stem alone, so a nested file can collide with a
    // top-level one; unreported, the internal (non-routed) module would shadow a
    // publicly routed command in the runtime's name map.
    let dir = write_project(&[
        ("commands/a.star", TRIVIAL_COMMAND),
        ("commands/internal/a.star", TRIVIAL_COMMAND),
        ("commands/Place_Order.star", TRIVIAL_COMMAND),
    ]);
    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);

    assert!(
        errs.iter()
            .any(|err| err.contains("command name `a` is already used by commands/a.star")),
        "expected the duplicate-name error, got {errs:?}"
    );
    assert!(
        errs.iter()
            .any(|err| err.starts_with("commands/Place_Order.star")
                && err.contains("must be lowercase letters, digits and single hyphens")),
        "expected the slug error, got {errs:?}"
    );
    assert!(
        project
            .commands
            .iter()
            .all(|unit| unit.loaded.def.name() != "Place_Order"),
        "a file with an invalid slug must not produce a command"
    );
}

/// An event whose fields exercise every kind [`kiln::validate`] type-checks a query
/// constraint against.
const TYPED_EVENTS: &str = r#"
thing = event(
    type = "thing.happened",
    fields = {
        "thing_id": uuid(),
        "active": boolean(),
        "count": u64_(),
        "status": one_of(["open", "closed"]),
    },
)
"#;

#[test]
fn a_query_constraint_with_an_ill_typed_value_is_an_error() {
    // Each of these lowers to a tag no stored event can carry, so the boundary
    // would guard nothing at all.
    let dir = write_project(&[
        ("events/thing.star", TYPED_EVENTS),
        (
            "commands/do-thing.star",
            r#"
load("events/thing.star", "thing")

input = schema(thing_id = uuid())

def query(input):
    return thing(thing_id = input.thing_id, active = "yes", count = "-1", status = "archived")

def handle(input, state):
    return []
"#,
        ),
    ]);
    let errs = errors(&LoadedProject::load(dir.path()));
    for field in ["active", "count", "status"] {
        assert!(
            errs.iter()
                .any(|err| err.contains(&format!("`{field}`")) && err.contains("is not a valid")),
            "expected an ill-typed-value error for `{field}`, got {errs:?}"
        );
    }
}

#[test]
fn a_query_constraint_with_a_well_typed_value_checks_clean() {
    // The sibling of the case above: the type check must accept the canonical
    // scalar strings (`true`, `7`, a declared variant), not reject everything.
    assert_clean(&[
        ("events/thing.star", TYPED_EVENTS),
        (
            "commands/do-thing.star",
            r#"
load("events/thing.star", "thing")

input = schema(thing_id = uuid())

def query(input):
    return thing(thing_id = input.thing_id, active = True, count = 7, status = "open")

def handle(input, state):
    return []
"#,
        ),
    ]);
}

#[test]
fn personal_data_and_weak_boundaries_warn_without_failing_the_check() {
    // (a) An unsubjected personal-looking field is the warning that tells an author
    // their PII can never be erased. It must stay a warning: an error here would
    // stop a valid project from deploying.
    let dir = write_project(&[(
        "events/person.star",
        r#"
signed_up = event(
    type = "person.signed_up",
    fields = {"person_id": uuid(), "email": text()},
)
"#,
    )]);
    let project = LoadedProject::load(dir.path());
    let warns = warnings(&project);
    assert!(
        warns
            .iter()
            .any(|warn| warn.contains("`email`") && warn.contains("looks like personal data")),
        "expected the personal-data warning, got {warns:?}"
    );
    assert!(errors(&project).is_empty(), "must not fail the check");

    // (b) A boundary with no constraint on a high-cardinality field guards on a
    // broad set of events, defeating the append fast-reject.
    let dir = write_project(&[
        (
            "events/person.star",
            r#"
signed_up = event(
    type = "person.signed_up",
    fields = {"person_id": uuid(), "email": text()},
)
"#,
        ),
        (
            "commands/sign-up.star",
            r#"
load("events/person.star", "signed_up")

input = schema(person_id = uuid())

def query(input):
    return signed_up()

def handle(input, state):
    return []
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let warns = warnings(&project);
    assert!(
        warns
            .iter()
            .any(|warn| warn.starts_with("commands/sign-up.star")
                && warn.contains("high-cardinality")),
        "expected the selectivity warning, got {warns:?}"
    );
    assert!(errors(&project).is_empty(), "must not fail the check");

    // (c) A `query` the placeholder input cannot drive is a warning, not an error:
    // the failure may be an artefact of the stub rather than a real defect.
    let dir = write_project(&[
        ("events/thing.star", EVENTS),
        (
            "commands/do-thing.star",
            r#"
load("events/thing.star", "thing_happened")

input = schema(thing_id = uuid())

def query(input):
    fail("boom")

def handle(input, state):
    return []
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let warns = warnings(&project);
    assert!(
        warns
            .iter()
            .any(|warn| warn.contains("could not statically evaluate query()")),
        "expected the unevaluable-query warning, got {warns:?}"
    );
    assert!(!project.has_errors());
    assert!(errors(&project).is_empty(), "must not fail the check");

    // (d) The shipped example carries unsubjected `email` and `name` fields, so it
    // pins the hint list itself against a silent removal.
    let example = LoadedProject::load(&example_dir("users"));
    let warns = warnings(&example);
    for field in ["`email`", "`name`"] {
        assert!(
            warns
                .iter()
                .any(|warn| warn.contains(field) && warn.contains("looks like personal data")),
            "expected the example's personal-data warning for {field}, got {warns:?}"
        );
    }
    assert!(errors(&example).is_empty());
}

/// A `kiln test` file over the accounts project. The second case is the load-bearing
/// one: it passes only if the seeded event's global-unique tag (written under the
/// runner's fixed test master key) matches the tag the query lowers to, and only if
/// `expect` is compared against plaintext.
const ACCOUNT_SCENARIOS: &str = r#"
load("events/account.star", "account_registered")

cases = [
    case(
        name = "registers a new email",
        command = "register-account",
        input = {
            "account_id": "11111111-1111-1111-1111-111111111111",
            "email": "alice@example.com",
        },
        expect = account_registered(
            account_id = "11111111-1111-1111-1111-111111111111",
            email = "alice@example.com",
        ),
    ),
    case(
        name = "rejects an email already taken by another account",
        command = "register-account",
        given = [account_registered(
            account_id = "22222222-2222-2222-2222-222222222222",
            email = "alice@example.com",
        )],
        input = {
            "account_id": "33333333-3333-3333-3333-333333333333",
            "email": "alice@example.com",
        },
        expect = reject("email_taken", "that email is already registered"),
    ),
]
"#;

#[test]
fn kiln_test_runs_a_scenario_over_a_subject_encrypted_event() {
    let dir = write_project(&[
        ("events/account.star", ACCOUNT_EVENTS),
        ("commands/register-account.star", REGISTER_ACCOUNT),
        ("tests/register-account.star", ACCOUNT_SCENARIOS),
    ]);
    assert_eq!(
        format!("{:?}", testing::run(dir.path())),
        format!("{:?}", ExitCode::SUCCESS),
        "the scenario suite should pass"
    );
}

#[test]
fn kiln_test_reports_a_scenario_whose_expectation_does_not_hold() {
    // The negative control for the case above: without it, a runner that silently
    // executed nothing would still report success.
    let wrong = r#"
load("events/account.star", "account_registered")

cases = [
    case(
        name = "expects the wrong email",
        command = "register-account",
        input = {
            "account_id": "11111111-1111-1111-1111-111111111111",
            "email": "alice@example.com",
        },
        expect = account_registered(
            account_id = "11111111-1111-1111-1111-111111111111",
            email = "bob@example.com",
        ),
    ),
]
"#;
    let dir = write_project(&[
        ("events/account.star", ACCOUNT_EVENTS),
        ("commands/register-account.star", REGISTER_ACCOUNT),
        ("tests/register-account.star", wrong),
    ]);
    assert_eq!(
        format!("{:?}", testing::run(dir.path())),
        format!("{:?}", ExitCode::FAILURE),
        "a mismatched expectation must fail the suite"
    );
}
